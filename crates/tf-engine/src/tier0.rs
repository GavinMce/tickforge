//! Tier 0: cheap per-symbol state for the whole universe, in RAM.
//!
//! One slot per `InstrumentId`, in arrays allocated once in [`Tier0::new`]. The
//! per-event path is an index into those arrays: no hashing, no allocation, no
//! locks. An id outside the table is counted and skipped, never a panic.
//!
//! The arrays are parallel (struct of arrays): compact scalars in one, the 4 KB
//! [`RollingBars`] in the other, so reading last price or VWAP across thousands
//! of symbols does not drag the windows through the cache.
//!
//! Conventions, all deliberate and testable:
//! - Every trade counts toward last, day high/low, volume, VWAP and the windows,
//!   including extended-hours and odd-lot prints. Special-casing them is a
//!   decision for when a strategy needs it.
//! - Windows are fed `ts_recv`, which is non-decreasing.
//! - A halt voids the LULD band (a new one arrives on the reopen).
//! - Corrections and cancels adjust volume, notional and trade count, with
//!   saturating arithmetic. They cannot rewind the day high/low, last price or
//!   the windows, since the original trade is not kept.

use tf_core::{Event, InstrumentId, Nanos, Px, StatusKind};

use crate::{RollingBars, SessionState, Sessions};
use tf_calendar::SessionTimes;

/// A quote side: price and size.
pub type Level = (Px, u32);

/// Everything Tier 0 knows about one symbol. Plain data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SymbolState {
    pub last_px: Option<Px>,
    pub last_size: u32,
    /// `ts_recv` of the last trade.
    pub last_ts: Nanos,
    pub bid: Option<Level>,
    pub ask: Option<Level>,
    pub day_high: Option<Px>,
    pub day_low: Option<Px>,
    /// Cumulative shares traded today.
    pub volume: u64,
    /// Cumulative price (raw units) x shares; with `volume` gives VWAP. A `u128`
    /// because 1e-9 prices times billions of shares overflow 64 bits.
    pub notional: u128,
    pub trades: u32,
    pub halted: bool,
    /// The LULD band `(lo, hi)` last announced, voided by a halt.
    pub luld: Option<(Px, Px)>,
    pub ssr: bool,
}

impl SymbolState {
    /// Volume-weighted average price today, rounded down.
    pub fn vwap(&self) -> Option<Px> {
        if self.volume == 0 {
            return None;
        }
        let v = self.notional / u128::from(self.volume);
        i64::try_from(v).ok().map(Px::from_raw)
    }

    /// Ask minus bid in raw price units, if both sides are quoted.
    pub fn spread(&self) -> Option<i64> {
        Some(self.ask?.0.raw() - self.bid?.0.raw())
    }
}

/// State for every instrument in an id space.
#[derive(Clone, Debug)]
pub struct Tier0 {
    states: Vec<SymbolState>,
    windows: Vec<RollingBars>,
    sessions: Sessions,
    unknown: u64,
}

impl Tier0 {
    /// Room for ids `0..id_space`. The only allocation this type ever makes.
    pub fn new(id_space: usize) -> Self {
        Tier0 {
            states: vec![SymbolState::default(); id_space],
            windows: vec![RollingBars::new(); id_space],
            sessions: Sessions::new(id_space),
            unknown: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn symbol(&self, id: InstrumentId) -> Option<&SymbolState> {
        self.states.get(id as usize)
    }

    pub fn windows(&self, id: InstrumentId) -> Option<&RollingBars> {
        self.windows.get(id as usize)
    }

    /// What is known about a symbol by session (premarket, regular, after-hours); nothing until
    /// [`Tier0::set_day`]. Kept apart from the day-wide fields above, which count every trade.
    pub fn session(&self, id: InstrumentId) -> Option<&SessionState> {
        self.sessions.state(id)
    }

    /// Start the session-by-session state for a day with these boundaries (`tf_calendar::Calendar::times`).
    pub fn set_day(&mut self, times: SessionTimes) {
        self.sessions.set_day(times);
    }

    /// The boundaries set by [`Tier0::set_day`].
    pub fn day(&self) -> Option<&SessionTimes> {
        self.sessions.day()
    }

    /// Events skipped because their instrument is outside the table.
    pub fn unknown_events(&self) -> u64 {
        self.unknown
    }

    /// Start a new trading day: clear today's statistics, the quote and the
    /// windows, keeping the table. Does not allocate.
    pub fn reset_day(&mut self) {
        self.states.fill(SymbolState::default());
        self.windows.fill(RollingBars::new());
        self.sessions.clear();
    }

    pub fn on_event(&mut self, ev: &Event) {
        // A parameter change is not market data; its instrument field may mean nothing.
        if matches!(ev, Event::ParamChange(_) | Event::TierChange(_)) {
            return;
        }
        let i = ev.instrument() as usize;
        let (Some(s), Some(w)) = (self.states.get_mut(i), self.windows.get_mut(i)) else {
            self.unknown += 1;
            return;
        };
        self.sessions.on_event(ev);
        match ev {
            Event::Trade(t) => {
                s.last_px = Some(t.px);
                s.last_size = t.size;
                s.last_ts = t.hdr.ts_recv;
                s.day_high = Some(s.day_high.map_or(t.px, |h| h.max(t.px)));
                s.day_low = Some(s.day_low.map_or(t.px, |l| l.min(t.px)));
                s.volume += u64::from(t.size);
                s.notional += notional(t.px, t.size);
                s.trades = s.trades.saturating_add(1);
                w.on_trade(t.hdr.ts_recv, t.px, t.size);
            }
            Event::Quote(q) => {
                s.bid = Some((q.bid_px, q.bid_sz));
                s.ask = Some((q.ask_px, q.ask_sz));
            }
            Event::Status(st) => match st.kind {
                StatusKind::TradingHalt => {
                    s.halted = true;
                    s.luld = None;
                }
                StatusKind::TradingResume => s.halted = false,
                StatusKind::LuldBand => s.luld = Some((st.lo, st.hi)),
                StatusKind::ShortSaleRestriction => s.ssr = true,
                StatusKind::ShortSaleRestrictionLifted => s.ssr = false,
            },
            Event::Correction(c) => {
                s.volume = s.volume.saturating_sub(u64::from(c.orig_size)) + u64::from(c.size);
                s.notional = s.notional.saturating_sub(notional(c.orig_px, c.orig_size))
                    + notional(c.px, c.size);
            }
            Event::CancelError(c) => {
                s.volume = s.volume.saturating_sub(u64::from(c.size));
                s.notional = s.notional.saturating_sub(notional(c.px, c.size));
                s.trades = s.trades.saturating_sub(1);
            }
            Event::News(_) | Event::ParamChange(_) | Event::TierChange(_) => {}
        }
    }
}

fn notional(px: Px, size: u32) -> u128 {
    // Prices are positive; a non-positive one contributes nothing rather than wrapping.
    u128::try_from(px.raw()).unwrap_or(0) * u128::from(size)
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<SymbolState>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_a_few_hundred_bytes_and_the_windows_are_separate() {
        assert!(
            std::mem::size_of::<SymbolState>() <= 256,
            "{}",
            std::mem::size_of::<SymbolState>()
        );
    }

    #[test]
    fn processing_events_never_reallocates() {
        use tf_core::{Header, ProviderId, Trade, TradeFlags};
        let mut t = Tier0::new(8);
        let (cap_s, cap_w) = (t.states.capacity(), t.windows.capacity());
        let (ptr_s, ptr_w) = (t.states.as_ptr(), t.windows.as_ptr());
        for i in 0..50_000u64 {
            t.on_event(&Event::Trade(Trade {
                hdr: Header {
                    ts_event: i,
                    ts_recv: i * 1_000_000,
                    seq: i,
                    instrument: (i % 12) as u32, // 4 of 12 ids are out of range
                    provider: ProviderId::Synthetic,
                },
                px: Px::from_cents(500 + (i % 7) as i64),
                size: 100,
                flags: TradeFlags::NONE,
            }));
        }
        assert_eq!((t.states.capacity(), t.windows.capacity()), (cap_s, cap_w));
        assert_eq!(
            (t.states.as_ptr(), t.windows.as_ptr()),
            (ptr_s, ptr_w),
            "the arrays never moved"
        );
        assert!(t.unknown_events() > 0);
    }
}
