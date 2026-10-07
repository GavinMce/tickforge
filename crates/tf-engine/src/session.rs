//! Per-symbol state by trading session: premarket, the regular session and after-hours.
//!
//! Tier 0's high, low, volume and VWAP count every trade of the day, extended hours included (its own
//! convention, and the scanner and the momentum strategy rely on it), so it cannot give a VWAP anchored at
//! the open, a premarket high or an opening range. This is a second array beside it, one slot per instrument,
//! that places each trade in a session and keeps what the strategies in `docs/research` ask for. Tier 0's
//! fields and everything built on them are unchanged.
//!
//! Rules, all deliberate and tested:
//! - **Which session.** By the trade's *arrival* time (`ts_recv`), against the day's boundaries from
//!   `tf-calendar` ([`Sessions::set_day`]). Databento's one-minute bars place trades the same way: on a real
//!   day every figure below agrees with them for 25 symbols (275 comparisons), and agrees for all but one
//!   if the event time is used instead (a trade whose event time is in the first minute and which arrived
//!   after it). Arrival time is also what was known when a strategy decides, and it does not move: a late
//!   report from a trade reporting facility never changes a window that has already closed. A start belongs
//!   to its session and an end to the next.
//! - **No day, no state.** Until a day is set, and for trades outside every session, nothing is kept.
//! - **The open** is the price of the first regular-session trade to arrive: the bars agree, and it is close
//!   to the opening cross (the earliest event time would often be a late facility print instead). It is
//!   still not the official opening cross price, which is a statistics record.
//! - **Windows** from the open: the first minute and the first 5 and 15 minutes.
//! - Corrections and cancels are ignored, as in `MtfBars`: a closed figure is not rewound.
//! - Integer arithmetic only, `Copy` state, no allocation per event.

use tf_calendar::SessionTimes;
use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};

const MINUTE: Nanos = 60 * NANOS_PER_SEC;

/// High, low, volume and price-times-volume of the trades of one session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub high: Option<Px>,
    pub low: Option<Px>,
    pub volume: u64,
    pub notional: u128,
}

impl Span {
    fn add(&mut self, px: Px, size: u32) {
        self.high = Some(self.high.map_or(px, |h| h.max(px)));
        self.low = Some(self.low.map_or(px, |l| l.min(px)));
        self.volume += u64::from(size);
        self.notional += u128::try_from(px.raw()).unwrap_or(0) * u128::from(size);
    }

    /// Volume-weighted average price, rounded down.
    pub fn vwap(&self) -> Option<Px> {
        if self.volume == 0 {
            return None;
        }
        i64::try_from(self.notional / u128::from(self.volume))
            .ok()
            .map(Px::from_raw)
    }
}

/// High and low over a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub high: Option<Px>,
    pub low: Option<Px>,
}

impl Range {
    fn add(&mut self, px: Px) {
        self.high = Some(self.high.map_or(px, |h| h.max(px)));
        self.low = Some(self.low.map_or(px, |l| l.min(px)));
    }
}

/// What is known about one symbol today, by session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionState {
    pub premarket: Span,
    /// The regular session, from the open: its VWAP is anchored there.
    pub regular: Span,
    pub after_hours: Span,
    /// Price and arrival time of the first regular-session trade.
    pub open: Option<(Px, Nanos)>,
    /// Volume in the first minute and the first five minutes after the open.
    pub first_minute_volume: u64,
    pub first_5m_volume: u64,
    /// High and low of the first 5 and 15 minutes after the open.
    pub range_5m: Range,
    pub range_15m: Range,
}

/// [`SessionState`] for every instrument of an id space.
#[derive(Clone, Debug)]
pub struct Sessions {
    states: Vec<SessionState>,
    day: Option<SessionTimes>,
    unknown: u64,
}

impl Sessions {
    /// Room for ids `0..id_space`. The only allocation this type makes.
    pub fn new(id_space: usize) -> Self {
        Sessions {
            states: vec![SessionState::default(); id_space],
            day: None,
            unknown: 0,
        }
    }

    /// Start a day: clears every symbol and sets the session boundaries (from
    /// `tf_calendar::Calendar::times`).
    pub fn set_day(&mut self, times: SessionTimes) {
        self.states.fill(SessionState::default());
        self.day = Some(times);
    }

    /// Forget the day: no boundaries, no state.
    pub fn clear(&mut self) {
        self.states.fill(SessionState::default());
        self.day = None;
    }

    pub fn day(&self) -> Option<&SessionTimes> {
        self.day.as_ref()
    }

    pub fn state(&self, id: InstrumentId) -> Option<&SessionState> {
        self.states.get(id as usize)
    }

    /// Trades skipped because their instrument is outside the table.
    pub fn unknown_events(&self) -> u64 {
        self.unknown
    }

    pub fn on_event(&mut self, ev: &Event) {
        let Event::Trade(t) = ev else { return };
        let Some(day) = self.day else { return };
        let Some(s) = self.states.get_mut(t.hdr.instrument as usize) else {
            self.unknown += 1;
            return;
        };
        let at = t.hdr.ts_recv;
        if at < day.premarket || at >= day.after_hours_end {
            return;
        }
        if at < day.open {
            s.premarket.add(t.px, t.size);
        } else if at < day.close {
            s.regular.add(t.px, t.size);
            if s.open.is_none() {
                s.open = Some((t.px, at));
            }
            let since = at - day.open;
            if since < MINUTE {
                s.first_minute_volume += u64::from(t.size);
            }
            if since < 5 * MINUTE {
                s.first_5m_volume += u64::from(t.size);
                s.range_5m.add(t.px);
            }
            if since < 15 * MINUTE {
                s.range_15m.add(t.px);
            }
        } else {
            s.after_hours.add(t.px, t.size);
        }
    }
}
