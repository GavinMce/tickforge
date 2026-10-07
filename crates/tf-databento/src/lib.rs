//! Databento DBN records as canonical events.
//!
//! [`Decoder`] reads DBN (plain or zstd-compressed, from a file, a capture or a live stream's bytes)
//! and yields [`Item`]s: canonical [`Event`]s for the schemas we use, symbol mappings, and the
//! gateway's own notices (an error such as "records were skipped because you read too slowly" is a
//! gap and must reach the ingest policy, so it is never swallowed). It uses the `dbn` crate, which is
//! synchronous: no async runtime reaches the engine (ADR 0038).
//!
//! What it maps, and how:
//! - **Trades** (`trades`, and the trade records of `mbp-1`, `tbbo`, `cmbp-1`, `tcbbo`) become
//!   [`Event::Trade`]. A trade that arrives with the book around it (`tbbo`, `tcbbo`, `mbp-1`,
//!   `cmbp-1`) is two events, the quote first and then the trade, which share their sequence.
//! - **Quotes** (the book of an `mbp-1`, `cmbp-1`, `cbbo-*`, `bbo-*` record) become [`Event::Quote`]. A
//!   side with no price (Databento's undefined price) becomes price 0 and size 0: an empty side.
//! - **Status** records become halts, resumes and short-sale-restriction changes. LULD bands are not
//!   in a status record (they are `statistics`); that is E06-S06.
//! - Prices are copied raw: Databento's scale is ours (1e-9 dollars) and nothing is rounded.
//! - A trade with an undefined or non-positive price is counted (`bad_trades`) and dropped, not turned
//!   into a price. A print of zero shares at a real price is a different thing: it is real, adds no
//!   volume, and is counted (`zero_size`) and dropped by default, because a strategy's last price
//!   should not move on it; `keep_zero_size` lets it through.
//!
//! The header's `seq` is the record's `sequence` where the schema has one, made unique across
//! publishers by putting the publisher id in the top 32 bits (each venue numbers its own); where it
//! has none (the consolidated schemas) it is `ts_recv`, which a replay delivers again unchanged.
//! Events made from the same record share it, so a dedupe has to include the kind of event as well.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::Path;

use dbn::decode::{DbnDecoder, DecodeRecordRef};
use dbn::{
    CbboMsg, Cmbp1Msg, ConsolidatedBidAskPair, ErrorMsg, Mbp1Msg, RecordRef, StatusAction,
    StatusMsg, SymbolMappingMsg, SystemMsg, TradeMsg, UNDEF_PRICE,
};
use tf_core::{
    Event, Header, InstrumentId, ProviderId, Px, Quote, Status, StatusKind, Trade, TradeFlags,
};

/// What a decoder yields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Event(Event),
    /// `symbol` is `instrument` from here on (a live session sends one per subscribed symbol).
    Mapping {
        instrument: InstrumentId,
        symbol: String,
    },
    Notice(Notice),
    /// A record of a kind this decoder does not map (a bar, a definition, an imbalance ...).
    Ignored {
        rtype: u8,
    },
}

/// A message from the gateway about the session, not market data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// `code` is Databento's `ErrorCode` (7 is "skipped records after slow reading").
    Error { code: u8, text: String },
    /// `code` is Databento's `SystemCode` (0 heartbeat, 2 slow-reader warning, 3 replay completed).
    System { code: u8, text: String },
}

impl Notice {
    /// The gateway dropped records because we read too slowly: a gap.
    pub fn is_skip(&self) -> bool {
        matches!(self, Notice::Error { code: 7, .. })
    }

    /// We are falling behind real time (records are still all coming).
    pub fn is_slow_reader_warning(&self) -> bool {
        matches!(self, Notice::System { code: 2, .. })
    }

    pub fn is_heartbeat(&self) -> bool {
        matches!(self, Notice::System { code: 0, .. })
    }

    pub fn is_replay_completed(&self) -> bool {
        matches!(self, Notice::System { code: 3, .. })
    }
}

#[derive(Debug)]
pub enum DecodeError {
    Dbn(dbn::Error),
    /// A record that is of a kind we map but cannot be read as one.
    Record {
        index: u64,
        why: &'static str,
    },
    /// The tap (see [`Decoder::with_tap`]) refused a record: what it said.
    Tap(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Dbn(e) => write!(f, "DBN: {e}"),
            DecodeError::Record { index, why } => write!(f, "record {index}: {why}"),
            DecodeError::Tap(m) => write!(f, "tap: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<dbn::Error> for DecodeError {
    fn from(e: dbn::Error) -> DecodeError {
        DecodeError::Dbn(e)
    }
}

/// Databento's instrument ids, given dense ids in the order they are first seen.
#[derive(Clone, Debug, Default)]
pub struct InstrumentMap {
    dense: HashMap<u32, InstrumentId>,
    raw: Vec<u32>,
    symbols: Vec<Option<String>>,
    /// The short-sale restriction as the status records last said it, per instrument (`None`: never said). It carries
    /// across files and sessions with the ids, so a restriction that began the day before is still known.
    ssr: Vec<Option<bool>>,
}

impl InstrumentMap {
    pub fn intern(&mut self, raw: u32) -> InstrumentId {
        if let Some(&d) = self.dense.get(&raw) {
            return d;
        }
        let d = InstrumentId::try_from(self.raw.len()).expect("more than u32::MAX instruments");
        self.dense.insert(raw, d);
        self.raw.push(raw);
        self.symbols.push(None);
        self.ssr.push(None);
        d
    }

    /// Whether the status records last said `id` is under a short-sale restriction.
    pub fn short_sale_restricted(&self, id: InstrumentId) -> Option<bool> {
        self.ssr.get(id as usize).copied().flatten()
    }

    pub fn dense(&self, raw: u32) -> Option<InstrumentId> {
        self.dense.get(&raw).copied()
    }

    pub fn raw_of(&self, id: InstrumentId) -> Option<u32> {
        self.raw.get(id as usize).copied()
    }

    pub fn symbol(&self, id: InstrumentId) -> Option<&str> {
        self.symbols.get(id as usize)?.as_deref()
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

/// What a decoder has seen so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub records: u64,
    pub trades: u64,
    pub quotes: u64,
    pub statuses: u64,
    pub mappings: u64,
    pub notices: u64,
    pub ignored: u64,
    /// Trades dropped for an undefined or non-positive price.
    pub bad_trades: u64,
    /// Prints of zero shares at a real price: dropped unless [`Decoder::keep_zero_size`] is set.
    /// On the real feed these are mostly sub-penny prints from one venue, about 4% of trades at
    /// midday and a fifth of them before the open (measured 2026-10-02).
    pub zero_size: u64,
}

/// What a decoder needs from the `dbn` decoders, which are not usable as trait objects themselves.
trait NextRecord {
    fn next_ref(&mut self) -> dbn::Result<Option<RecordRef<'_>>>;
}

impl<T: DecodeRecordRef> NextRecord for T {
    fn next_ref(&mut self) -> dbn::Result<Option<RecordRef<'_>>> {
        self.decode_record_ref()
    }
}

/// Maps Databento records to [`Item`]s, one at a time. A [`Decoder`] is one over a DBN stream; the live
/// adapter uses one directly on the records its client hands it. It owns the instrument ids and the
/// counts, so the same one can be carried across files.
#[derive(Clone, Debug, Default)]
pub struct Mapper {
    ids: InstrumentMap,
    stats: Stats,
    keep_zero_size: bool,
}

impl Mapper {
    pub fn new() -> Mapper {
        Mapper::default()
    }

    /// Start from instrument ids already assigned (a later file of the same session).
    pub fn with_instruments(mut self, ids: InstrumentMap) -> Mapper {
        self.ids = ids;
        self
    }

    /// Pass prints of zero shares through as trades of size 0 (the default drops and counts them).
    pub fn keep_zero_size(mut self, keep: bool) -> Mapper {
        self.keep_zero_size = keep;
        self
    }

    pub fn instruments(&self) -> &InstrumentMap {
        &self.ids
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn into_instruments(self) -> InstrumentMap {
        self.ids
    }

    /// Map one record, appending what it makes (nothing, or one or two items) to `out`.
    pub fn map(&mut self, rec: &RecordRef<'_>, out: &mut Vec<Item>) -> Result<(), DecodeError> {
        let index = self.stats.records;
        self.stats.records += 1;
        map_record(
            rec,
            &mut self.ids,
            &mut self.stats,
            self.keep_zero_size,
            index,
            out,
        )
    }
}

/// Sees every record as it comes off the wire, before it is mapped: for keeping the provider's own bytes.
/// An `Err` ends the stream with [`DecodeError::Tap`].
pub type Tap = Box<dyn FnMut(&RecordRef<'_>) -> Result<(), String> + Send>;

pub struct Decoder<'a> {
    inner: Box<dyn NextRecord + Send + 'a>,
    mapper: Mapper,
    pending: VecDeque<Item>,
    tap: Option<Tap>,
}

fn side_px(px: i64) -> Px {
    if px == UNDEF_PRICE {
        Px::ZERO
    } else {
        Px::from_raw(px)
    }
}

fn side_sz(px: i64, sz: u32) -> u32 {
    if px == UNDEF_PRICE { 0 } else { sz }
}

/// The sequence for a record that has one: the publisher in the top half.
fn seq_of(publisher: u16, sequence: u32) -> u64 {
    (u64::from(publisher) << 32) | u64::from(sequence)
}

impl<'a> Decoder<'a> {
    fn from_inner(inner: Box<dyn NextRecord + Send + 'a>) -> Decoder<'a> {
        Decoder {
            inner,
            mapper: Mapper::default(),
            pending: VecDeque::new(),
            tap: None,
        }
    }

    /// Plain (uncompressed) DBN.
    pub fn new<R: Read + Send + 'a>(reader: R) -> Result<Decoder<'a>, DecodeError> {
        Ok(Decoder::from_inner(Box::new(DbnDecoder::new(reader)?)))
    }

    /// Zstd-compressed DBN, as Databento delivers and as captures are kept.
    pub fn zstd<R: Read + Send + 'a>(reader: R) -> Result<Decoder<'a>, DecodeError> {
        Ok(Decoder::from_inner(Box::new(DbnDecoder::with_zstd(
            reader,
        )?)))
    }

    /// A zstd-compressed DBN file.
    pub fn from_zstd_file(path: impl AsRef<Path>) -> Result<Decoder<'static>, DecodeError> {
        Ok(Decoder::from_inner(Box::new(DbnDecoder::from_zstd_file(
            path,
        )?)))
    }

    /// Pass prints of zero shares through as trades of size 0 (the default drops and counts them).
    pub fn keep_zero_size(mut self, keep: bool) -> Self {
        self.mapper = self.mapper.keep_zero_size(keep);
        self
    }

    /// Show every record to `tap` as it is read (see [`Tap`]).
    pub fn with_tap(mut self, tap: Tap) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Start from instrument ids already assigned (the next file of one session).
    pub fn with_instruments(mut self, ids: InstrumentMap) -> Self {
        self.mapper = self.mapper.with_instruments(ids);
        self
    }

    pub fn instruments(&self) -> &InstrumentMap {
        self.mapper.instruments()
    }

    /// The ids assigned so far, to carry into the next file.
    pub fn into_instruments(self) -> InstrumentMap {
        self.mapper.into_instruments()
    }

    pub fn stats(&self) -> Stats {
        self.mapper.stats()
    }

    /// The next item, or `None` at the end of the stream.
    pub fn next_item(&mut self) -> Result<Option<Item>, DecodeError> {
        loop {
            if let Some(i) = self.pending.pop_front() {
                return Ok(Some(i));
            }
            let Some(rec) = self.inner.next_ref()? else {
                return Ok(None);
            };
            if let Some(tap) = self.tap.as_mut() {
                tap(&rec).map_err(DecodeError::Tap)?;
            }
            let mut out: Vec<Item> = Vec::with_capacity(2);
            self.mapper.map(&rec, &mut out)?;
            self.pending.extend(out);
        }
    }
}

impl Iterator for Decoder<'_> {
    type Item = Result<Item, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_item().transpose()
    }
}

fn header(ids: &mut InstrumentMap, raw: u32, ts_event: u64, ts_recv: u64, seq: u64) -> Header {
    Header {
        ts_event,
        ts_recv,
        seq,
        instrument: ids.intern(raw),
        provider: ProviderId::Databento,
    }
}

/// What to do with a trade's price and size.
fn trade_verdict(px: i64, size: u32, keep_zero: bool) -> Verdict {
    if px == UNDEF_PRICE || px <= 0 {
        Verdict::Bad
    } else if size == 0 && !keep_zero {
        Verdict::ZeroSize
    } else {
        Verdict::Keep
    }
}

enum Verdict {
    Keep,
    Bad,
    ZeroSize,
}

fn book(stats: &mut Stats, out: &mut Vec<Item>, h: Header, level: &Level) {
    stats.quotes += 1;
    out.push(Item::Event(Event::Quote(Quote {
        hdr: h,
        bid_px: side_px(level.bid_px),
        ask_px: side_px(level.ask_px),
        bid_sz: side_sz(level.bid_px, level.bid_sz),
        ask_sz: side_sz(level.ask_px, level.ask_sz),
    })));
}

/// The two sides of a top of book, whichever record it came in.
struct Level {
    bid_px: i64,
    ask_px: i64,
    bid_sz: u32,
    ask_sz: u32,
}

impl From<&ConsolidatedBidAskPair> for Level {
    fn from(l: &ConsolidatedBidAskPair) -> Self {
        Level {
            bid_px: l.bid_px,
            ask_px: l.ask_px,
            bid_sz: l.bid_sz,
            ask_sz: l.ask_sz,
        }
    }
}

fn map_record(
    rec: &RecordRef<'_>,
    ids: &mut InstrumentMap,
    stats: &mut Stats,
    keep_zero: bool,
    index: u64,
    out: &mut Vec<Item>,
) -> Result<(), DecodeError> {
    if let Some(t) = rec.get::<TradeMsg>() {
        match trade_verdict(t.price, t.size, keep_zero) {
            Verdict::Keep => {}
            Verdict::Bad => {
                stats.bad_trades += 1;
                return Ok(());
            }
            Verdict::ZeroSize => {
                stats.zero_size += 1;
                return Ok(());
            }
        }
        let h = header(
            ids,
            t.hd.instrument_id,
            t.hd.ts_event,
            t.ts_recv,
            seq_of(t.hd.publisher_id, t.sequence),
        );
        stats.trades += 1;
        out.push(Item::Event(Event::Trade(Trade {
            hdr: h,
            px: Px::from_raw(t.price),
            size: t.size,
            flags: TradeFlags::NONE,
        })));
    } else if let Some(m) = rec.get::<Mbp1Msg>() {
        let h = header(
            ids,
            m.hd.instrument_id,
            m.hd.ts_event,
            m.ts_recv,
            seq_of(m.hd.publisher_id, m.sequence),
        );
        let l = &m.levels[0];
        let level = Level {
            bid_px: l.bid_px,
            ask_px: l.ask_px,
            bid_sz: l.bid_sz,
            ask_sz: l.ask_sz,
        };
        book(stats, out, h, &level);
        trade_of(m.action as u8, m.price, m.size, h, (stats, keep_zero), out);
    } else if let Some(m) = rec.get::<Cmbp1Msg>() {
        let h = header(ids, m.hd.instrument_id, m.hd.ts_event, m.ts_recv, m.ts_recv);
        book(stats, out, h, &Level::from(&m.levels[0]));
        trade_of(m.action as u8, m.price, m.size, h, (stats, keep_zero), out);
    } else if let Some(m) = rec.get::<CbboMsg>() {
        let h = header(ids, m.hd.instrument_id, m.hd.ts_event, m.ts_recv, m.ts_recv);
        book(stats, out, h, &Level::from(&m.levels[0]));
    } else if let Some(s) = rec.get::<StatusMsg>() {
        let action = s.action().map_err(|_| DecodeError::Record {
            index,
            why: "an unknown status action",
        })?;
        let kind = match action {
            StatusAction::Halt | StatusAction::Pause | StatusAction::Suspend => {
                Some(StatusKind::TradingHalt)
            }
            StatusAction::Trading => Some(StatusKind::TradingResume),
            _ => None,
        };
        let h = header(ids, s.hd.instrument_id, s.hd.ts_event, s.ts_recv, s.ts_recv);
        let status = |kind| {
            Item::Event(Event::Status(Status {
                hdr: h,
                kind,
                lo: Px::ZERO,
                hi: Px::ZERO,
            }))
        };
        let mut produced = false;
        if let Some(kind) = kind {
            stats.statuses += 1;
            out.push(status(kind));
            produced = true;
        }
        // The short-sale restriction. Every status record says whether the instrument is restricted (`Y`, `N`, or
        // `~` for not known), so a restriction carried over from the day before shows on the day's first records
        // although no record changes it. An event is made when the state differs from the last one said; a first
        // `N` changes nothing (not restricted is how an instrument starts), and `~` says nothing. A record whose
        // action is a restriction change but whose flag does not say which way is the start of one, as it was
        // always read.
        let flag = match s.is_short_sell_restricted as u8 {
            b'Y' => Some(true),
            b'N' => Some(false),
            _ if action == StatusAction::SsrChange => Some(true),
            _ => None,
        };
        if let Some(on) = flag {
            let before = ids.ssr[h.instrument as usize];
            if before != Some(on) {
                ids.ssr[h.instrument as usize] = Some(on);
                if on || before.is_some() {
                    stats.statuses += 1;
                    out.push(status(if on {
                        StatusKind::ShortSaleRestriction
                    } else {
                        StatusKind::ShortSaleRestrictionLifted
                    }));
                    produced = true;
                }
            }
        }
        if !produced {
            stats.ignored += 1;
            out.push(Item::Ignored { rtype: s.hd.rtype });
        }
    } else if let Some(m) = rec.get::<SymbolMappingMsg>() {
        let symbol = m
            .stype_out_symbol()
            .map_err(|_| DecodeError::Record {
                index,
                why: "a symbol that is not text",
            })?
            .to_owned();
        let instrument = ids.intern(m.hd.instrument_id);
        ids.symbols[instrument as usize] = Some(symbol.clone());
        stats.mappings += 1;
        out.push(Item::Mapping { instrument, symbol });
    } else if let Some(e) = rec.get::<ErrorMsg>() {
        stats.notices += 1;
        out.push(Item::Notice(Notice::Error {
            code: e.code,
            text: e.err().unwrap_or("").to_owned(),
        }));
    } else if let Some(s) = rec.get::<SystemMsg>() {
        stats.notices += 1;
        out.push(Item::Notice(Notice::System {
            code: s.code,
            text: s.msg().unwrap_or("").to_owned(),
        }));
    } else {
        stats.ignored += 1;
        out.push(Item::Ignored {
            rtype: rec.header().rtype,
        });
    }
    Ok(())
}

/// The trade inside a record whose action is a trade.
fn trade_of(
    action: u8,
    price: i64,
    size: u32,
    h: Header,
    (stats, keep_zero): (&mut Stats, bool),
    out: &mut Vec<Item>,
) {
    if action != b'T' {
        return;
    }
    match trade_verdict(price, size, keep_zero) {
        Verdict::Keep => {}
        Verdict::Bad => {
            stats.bad_trades += 1;
            return;
        }
        Verdict::ZeroSize => {
            stats.zero_size += 1;
            return;
        }
    }
    stats.trades += 1;
    out.push(Item::Event(Event::Trade(Trade {
        hdr: h,
        px: Px::from_raw(price),
        size,
        flags: TradeFlags::NONE,
    })));
}

#[cfg(test)]
mod tests;
