//! The bar-level screening pass (E19-S13): the first stage of a research run.
//!
//! Running a strategy over every symbol of a day through the engine is the expensive part of research. A screen reads
//! the cheap schemas of a store, one-minute bars (`ohlcv-1m`) and, where quotes are kept, the quoted spread, and says
//! which symbols are worth the full pass: priced in a range, traded enough, tight enough. It reads a file once and keeps a
//! few numbers per symbol; it decides nothing about a strategy.
//!
//! - **Bars**: per symbol over the days given, the number of one-minute bars with trades, the first open and last close,
//!   the shares and the dollars traded (shares times the bar's close, in raw price units), and the lowest and highest
//!   price seen. The highs and lows are the feed's own, which carry off-market prints (ADR 0055): a screen therefore
//!   never selects on them. Whole days, every session the file holds (premarket and after-hours included).
//! - **Spreads**: the quoted spread over the mid at each quote of a quote schema (`tcbbo` here, the consolidated best bid
//!   and offer at each trade), in hundredths of a basis point, as a mean over those quotes; a locked, crossed or
//!   one-sided quote is not counted. It is weighted by quotes, not by time.
//! - **A symbol is named by the file's own symbology** (its metadata mappings): the same name on two days is the same row.
//! - **Selection** ([`select`]) keeps the symbols that meet every limit that is set; a spread limit needs the symbol to
//!   have quotes, and a symbol with none does not pass it.
//!
//! Integers only; averages divide down.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use dbn::decode::{DbnDecoder, DbnMetadata, DecodeRecordRef};
use dbn::{OhlcvMsg, UNDEF_PRICE};
use tf_capture::CaptureReplay;
use tf_core::Event;
use tf_provider::{Poll, Provider};

use crate::Error;

/// What the bars of one symbol say, over the days read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BarRow {
    pub symbol: String,
    /// One-minute bars with at least one share.
    pub bars: u64,
    /// The open of the first bar read, and the close of the last (files in date order).
    pub first_open: i64,
    pub last_close: i64,
    pub low: i64,
    pub high: i64,
    pub shares: u64,
    /// Shares times each bar's close, in raw price units.
    pub dollars: u128,
}

/// What the quotes of one symbol say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpreadRow {
    pub symbol: String,
    /// Quotes counted (two-sided and not crossed).
    pub quotes: u64,
    /// The sum over them of spread over mid, in hundredths of a basis point.
    pub sum_bp_x100: u128,
}

impl SpreadRow {
    /// The mean spread in hundredths of a basis point; `None` if there were no quotes.
    pub fn mean_bp_x100(&self) -> Option<u64> {
        (self.quotes > 0).then(|| (self.sum_bp_x100 / u128::from(self.quotes)) as u64)
    }
}

/// Read `ohlcv-1m` files in date order into one row per symbol, ordered by name.
pub fn bars(files: &[impl AsRef<Path>]) -> Result<Vec<BarRow>, Error> {
    let mut rows: BTreeMap<String, BarRow> = BTreeMap::new();
    for f in files {
        let path = f.as_ref();
        let dbn = |e: dbn::Error| Error::Dbn(format!("{}: {e}", path.display()));
        let file = File::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        let mut dec = DbnDecoder::with_zstd(file).map_err(dbn)?;
        // The numeric id in a record, and the name the file gives it.
        let mut names: BTreeMap<u32, String> = BTreeMap::new();
        for m in &dec.metadata().mappings {
            for i in &m.intervals {
                if let Ok(id) = i.symbol.parse::<u32>() {
                    names.insert(id, m.raw_symbol.clone());
                }
            }
        }
        while let Some(rec) = dec.decode_record_ref().map_err(dbn)? {
            let Some(b) = rec.get::<OhlcvMsg>() else {
                continue;
            };
            let Some(name) = names.get(&b.hd.instrument_id) else {
                continue;
            };
            if b.volume == 0 || [b.open, b.high, b.low, b.close].contains(&UNDEF_PRICE) {
                continue;
            }
            let r = rows.entry(name.clone()).or_insert_with(|| BarRow {
                symbol: name.clone(),
                first_open: b.open,
                low: b.low,
                high: b.high,
                ..BarRow::default()
            });
            r.bars += 1;
            r.last_close = b.close;
            r.low = r.low.min(b.low);
            r.high = r.high.max(b.high);
            r.shares += b.volume;
            r.dollars += u128::from(b.volume) * u128::try_from(b.close).unwrap_or(0);
        }
    }
    Ok(rows.into_values().collect())
}

/// Read quote files (the days of a quote schema, in order) into one row per symbol, ordered by name.
pub fn spreads(files: &[impl AsRef<Path>]) -> Result<Vec<SpreadRow>, Error> {
    let mut replay =
        CaptureReplay::from_files(files.iter().map(|f| f.as_ref().to_owned()).collect());
    let mut by_id: BTreeMap<u32, (u64, u128)> = BTreeMap::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match replay.poll(&mut buf, 65_536) {
            Poll::Events(_) => {}
            _ => break,
        }
        for e in &buf {
            let Event::Quote(q) = e else { continue };
            let (bid, ask) = (i128::from(q.bid_px.raw()), i128::from(q.ask_px.raw()));
            if bid <= 0 || ask <= bid {
                continue;
            }
            let (mid2, spread) = (bid + ask, ask - bid);
            let s = by_id.entry(q.hdr.instrument).or_default();
            s.0 += 1;
            // spread / mid = 2 x spread / (bid + ask), in hundredths of a basis point.
            s.1 += (spread * 2 * 1_000_000 / mid2) as u128;
        }
    }
    if let Some(why) = replay.failure() {
        return Err(Error::Unusable(format!("the quotes cannot be read: {why}")));
    }
    let map = replay.instruments();
    let mut rows: BTreeMap<String, SpreadRow> = BTreeMap::new();
    for (id, (n, sum)) in by_id {
        let name = map
            .symbol(id)
            .map_or_else(|| format!("#{id}"), str::to_owned);
        let r = rows.entry(name.clone()).or_insert_with(|| SpreadRow {
            symbol: name,
            ..SpreadRow::default()
        });
        r.quotes += n;
        r.sum_bp_x100 += sum;
    }
    Ok(rows.into_values().collect())
}

/// Limits a symbol must meet to go on to the full pass. A limit that is `None` is not applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    /// The last close, raw price units.
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    /// Dollars traded over the days read, raw price units.
    pub min_dollars: Option<u128>,
    /// One-minute bars with trades over the days read.
    pub min_bars: Option<u64>,
    /// The mean spread, hundredths of a basis point. A symbol with no quotes does not pass.
    pub max_spread_bp_x100: Option<u64>,
}

/// The symbols that meet every limit, by name.
pub fn select(bars: &[BarRow], spreads: &[SpreadRow], limits: &Limits) -> Vec<String> {
    let by_name: BTreeMap<&str, &SpreadRow> =
        spreads.iter().map(|s| (s.symbol.as_str(), s)).collect();
    bars.iter()
        .filter(|b| {
            limits.min_price.is_none_or(|p| b.last_close >= p)
                && limits.max_price.is_none_or(|p| b.last_close <= p)
                && limits.min_dollars.is_none_or(|d| b.dollars >= d)
                && limits.min_bars.is_none_or(|n| b.bars >= n)
                && limits.max_spread_bp_x100.is_none_or(|m| {
                    by_name
                        .get(b.symbol.as_str())
                        .and_then(|s| s.mean_bp_x100())
                        .is_some_and(|mean| mean <= m)
                })
        })
        .map(|b| b.symbol.clone())
        .collect()
}
