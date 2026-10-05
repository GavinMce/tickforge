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
    ShortSaleRestriction = 3,
}

impl StatusKind {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(StatusKind::TradingHalt),
            1 => Some(StatusKind::TradingResume),
            2 => Some(StatusKind::LuldBand),
            3 => Some(StatusKind::ShortSaleRestriction),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Trade(Trade),
    Quote(Quote),
    Status(Status),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Trade,
    Quote,
    Status,
}

impl Event {
    pub const fn hdr(&self) -> &Header {
        match self {
            Event::Trade(t) => &t.hdr,
            Event::Quote(q) => &q.hdr,
            Event::Status(s) => &s.hdr,
        }
    }

    pub const fn hdr_mut(&mut self) -> &mut Header {
        match self {
            Event::Trade(t) => &mut t.hdr,
            Event::Quote(q) => &mut q.hdr,
            Event::Status(s) => &mut s.hdr,
        }
    }

    pub const fn kind(&self) -> EventKind {
        match self {
            Event::Trade(_) => EventKind::Trade,
            Event::Quote(_) => EventKind::Quote,
            Event::Status(_) => EventKind::Status,
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
}
