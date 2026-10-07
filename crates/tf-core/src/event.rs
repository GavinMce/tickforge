use crate::ids::{InstrumentId, Nanos, ProviderId};
use crate::px::Px;

/// Fields common to every event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// When the event happened at the venue/SIP.
    pub ts_event: Nanos,
    /// When we received it. Arrival order is `ts_recv` order, ties broken by `seq`.
    pub ts_recv: Nanos,
    /// Per-provider arrival sequence, assigned at the provider boundary.
    /// Replays after a reconnect re-deliver the same `seq`, which is what
    /// dedupe keys on.
    pub seq: u64,
    pub instrument: InstrumentId,
    pub provider: ProviderId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TradeFlags(pub u16);

impl TradeFlags {
    pub const NONE: TradeFlags = TradeFlags(0);
    pub const EXT_HOURS: TradeFlags = TradeFlags(1 << 0);
    pub const ODD_LOT: TradeFlags = TradeFlags(1 << 1);

    pub const fn contains(self, other: TradeFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trade {
    pub hdr: Header,
    pub px: Px,
    pub size: u32,
    pub flags: TradeFlags,
}

/// Top-of-book (L1) update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub hdr: Header,
    pub bid_px: Px,
    pub ask_px: Px,
    pub bid_sz: u32,
    pub ask_sz: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusKind {
    TradingHalt = 0,
    TradingResume = 1,
    /// LULD band update; `lo`/`hi` carry the band.
    LuldBand = 2,
    /// The short-sale restriction (Rule 201) is in force for the instrument.
    ShortSaleRestriction = 3,
    /// The restriction is no longer in force (schema v5; earlier streams only ever said when it began).
    ShortSaleRestrictionLifted = 4,
}

impl StatusKind {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(StatusKind::TradingHalt),
            1 => Some(StatusKind::TradingResume),
            2 => Some(StatusKind::LuldBand),
            3 => Some(StatusKind::ShortSaleRestriction),
            4 => Some(StatusKind::ShortSaleRestrictionLifted),
            _ => None,
        }
    }
}

/// Halts, LULD bands and SSR. Existential for the low-float strategies, so
/// part of the canonical model from the start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub hdr: Header,
    pub kind: StatusKind,
    pub lo: Px,
    pub hi: Px,
}

/// A correction to an earlier trade (Alpaca `corrections`): the print that was
/// `orig_px` x `orig_size` is now `px` x `size`.
///
/// The canonical model has no trade id, so the original is identified by
/// `(instrument, hdr.ts_event, orig_px, orig_size)`. `hdr.ts_event` is the
/// *original trade's* event time; `hdr.ts_recv` is when the correction arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Correction {
    pub hdr: Header,
    pub orig_px: Px,
    pub orig_size: u32,
    pub px: Px,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelErrorKind {
    /// The trade was cancelled.
    Cancel = 0,
    /// The trade was reported in error.
    Error = 1,
}

impl CancelErrorKind {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(CancelErrorKind::Cancel),
            1 => Some(CancelErrorKind::Error),
            _ => None,
        }
    }
}

/// A trade that was cancelled or reported in error (Alpaca `cancelErrors`).
/// The trade is identified like a [`Correction`]'s original: `hdr.ts_event` is
/// its event time, and `px` and `size` are what it printed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CancelError {
    pub hdr: Header,
    pub kind: CancelErrorKind,
    pub px: Px,
    pub size: u32,
}

/// A news article tagged with one instrument.
///
/// `hdr.ts_event` is when the article was published and `hdr.ts_recv` when we
/// received it. Anything that must be point-in-time correct (backtests,
/// features) may use the article only from `ts_recv` onward. An article tagged
/// with several symbols arrives as one `News` per symbol, all sharing
/// `article_id`. The text is not part of the event, since events are `Copy` and
/// at most 64 bytes; it is stored separately, keyed by `article_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct News {
    pub hdr: Header,
    pub article_id: u64,
}

/// What a parameter change applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ParamScope {
    /// Every instrument. `hdr.instrument` is zero and means nothing.
    Global = 0,
    /// Only the instrument in `hdr.instrument`.
    Instrument = 1,
}

impl ParamScope {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ParamScope::Global),
            1 => Some(ParamScope::Instrument),
            _ => None,
        }
    }
}

/// A strategy parameter was changed (see `tf-params`). It is an event so that a
/// replay of the tape reproduces the exact session, parameters included.
///
/// `param` is the index of the parameter in the store's declaration list;
/// `proposer` identifies who asked (an agent, an operator); `reason` is a code
/// from the proposer's vocabulary; `evidence` is an id of the supporting
/// artifact (for example the hash prefix of a backtest result). The old value is
/// not carried: it is the value the previous change, or the baseline, left.
/// Free text lives in the journal beside the tape, keyed by `hdr.seq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamChange {
    pub hdr: Header,
    pub param: u16,
    pub scope: ParamScope,
    pub proposer: u16,
    pub reason: u16,
    pub new_value: i64,
    pub evidence: u64,
}

/// What happened to a symbol's tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TierAction {
    /// Moved up to Tier 1: full ticks, quotes and features are now kept for it.
    Promote = 0,
    /// Moved back down: its Tier 1 history is dropped.
    Demote = 1,
}

impl TierAction {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(TierAction::Promote),
            1 => Some(TierAction::Demote),
            _ => None,
        }
    }
}

/// A symbol moved between tiers (see `tf-engine`'s `Promoter`). `hdr.instrument` is
/// the symbol. `reason` is the promoter's code for why (see its constants) and
/// `score` the number behind the decision (the volume z-score times 1000 for a
/// promotion, the latest one for a demotion). It is an event so that a replay of the
/// tape follows exactly the tier membership the session had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierChange {
    pub hdr: Header,
    pub action: TierAction,
    pub reason: u8,
    pub score: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Trade(Trade),
    Quote(Quote),
    Status(Status),
    Correction(Correction),
    CancelError(CancelError),
    News(News),
    ParamChange(ParamChange),
    TierChange(TierChange),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Trade,
    Quote,
    Status,
    Correction,
    CancelError,
    News,
    ParamChange,
    TierChange,
}

impl Event {
    pub const fn hdr(&self) -> &Header {
        match self {
            Event::Trade(t) => &t.hdr,
            Event::Quote(q) => &q.hdr,
            Event::Status(s) => &s.hdr,
            Event::Correction(c) => &c.hdr,
            Event::CancelError(c) => &c.hdr,
            Event::News(n) => &n.hdr,
            Event::ParamChange(p) => &p.hdr,
            Event::TierChange(t) => &t.hdr,
        }
    }

    pub const fn hdr_mut(&mut self) -> &mut Header {
        match self {
            Event::Trade(t) => &mut t.hdr,
            Event::Quote(q) => &mut q.hdr,
            Event::Status(s) => &mut s.hdr,
            Event::Correction(c) => &mut c.hdr,
            Event::CancelError(c) => &mut c.hdr,
            Event::News(n) => &mut n.hdr,
            Event::ParamChange(p) => &mut p.hdr,
            Event::TierChange(t) => &mut t.hdr,
        }
    }

    pub const fn kind(&self) -> EventKind {
        match self {
            Event::Trade(_) => EventKind::Trade,
            Event::Quote(_) => EventKind::Quote,
            Event::Status(_) => EventKind::Status,
            Event::Correction(_) => EventKind::Correction,
            Event::CancelError(_) => EventKind::CancelError,
            Event::News(_) => EventKind::News,
            Event::ParamChange(_) => EventKind::ParamChange,
            Event::TierChange(_) => EventKind::TierChange,
        }
    }

    pub const fn instrument(&self) -> InstrumentId {
        self.hdr().instrument
    }

    pub const fn ts_event(&self) -> Nanos {
        self.hdr().ts_event
    }

    pub const fn ts_recv(&self) -> Nanos {
        self.hdr().ts_recv
    }

    pub const fn seq(&self) -> u64 {
        self.hdr().seq
    }
}

// Hot-path buffers are sized in multiples of this; keep it at one cache line.
const _: () = assert!(std::mem::size_of::<Event>() <= 64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_fits_a_cache_line() {
        assert!(std::mem::size_of::<Event>() <= 64);
    }

    #[test]
    fn accessors_see_through_the_enum() {
        let hdr = Header {
            ts_event: 10,
            ts_recv: 20,
            seq: 3,
            instrument: 7,
            provider: ProviderId::Synthetic,
        };
        let mut ev = Event::Trade(Trade {
            hdr,
            px: Px::from_cents(100),
            size: 100,
            flags: TradeFlags::NONE,
        });
        assert_eq!(ev.kind(), EventKind::Trade);
        assert_eq!(
            (ev.ts_event(), ev.ts_recv(), ev.seq(), ev.instrument()),
            (10, 20, 3, 7)
        );
        ev.hdr_mut().seq = 9;
        assert_eq!(ev.seq(), 9);
    }

    #[test]
    fn corrections_cancels_and_news_expose_their_header() {
        let hdr = Header {
            ts_event: 10,
            ts_recv: 20,
            seq: 3,
            instrument: 7,
            provider: ProviderId::Alpaca,
        };
        let mut evs = [
            Event::Correction(Correction {
                hdr,
                orig_px: Px::from_cents(100),
                orig_size: 200,
                px: Px::from_cents(101),
                size: 200,
            }),
            Event::CancelError(CancelError {
                hdr,
                kind: CancelErrorKind::Cancel,
                px: Px::from_cents(100),
                size: 200,
            }),
            Event::News(News {
                hdr,
                article_id: 99,
            }),
        ];
        let kinds = [
            EventKind::Correction,
            EventKind::CancelError,
            EventKind::News,
        ];
        for (ev, kind) in evs.iter_mut().zip(kinds) {
            assert_eq!(ev.kind(), kind);
            assert_eq!(
                (ev.ts_event(), ev.ts_recv(), ev.seq(), ev.instrument()),
                (10, 20, 3, 7)
            );
            ev.hdr_mut().seq = 9;
            assert_eq!(ev.seq(), 9);
        }
    }
}
