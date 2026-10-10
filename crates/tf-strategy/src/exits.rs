//! Exits a strategy holds itself (E19-S05).
//!
//! A stop, a target and a time exit that live in the strategy, not at the broker. Outside the regular session the
//! broker takes no stop or bracket order (only plain limit orders, [`crate::session_rules`]), and a resting stop is a
//! different order from the one a simulation would model; an exit the strategy holds behaves the same live, in the
//! simulator and in a replay, because it is only the strategy's own decision on the events it is given.
//!
//! An [`ExitBook`] holds one [`ExitPlan`] per instrument. The strategy arms it when an entry fills, forwards trades,
//! timers and order updates to it, and it sends a **closing intent** when:
//! - a trade prints **at or through the stop** (at or below it for a long, at or above for a short): the stop;
//! - a trade prints **at or through the target** (at or above for a long, at or below for a short): the target;
//! - time reaches **`flat_by`**: the time exit. The instant comes from the calendar ([`flat_by`]): so many minutes
//!   before the regular close of that day, which is 13:00 on an early close.
//!
//! The closing intent is a limit order a collar away from the price that triggered it (so it crosses the spread but
//! never trades worse than the collar), time in force day by default: day and good-til-cancelled are accepted in the
//! extended hours, immediate-or-cancel is not. One exit is outstanding per instrument. If it ends without closing the
//! position (cancelled, expired, refused, or filled in part) the book keeps what is left and tries again on a later
//! trade, no sooner than `retry_after`; once the position is closed it forgets the instrument.
//!
//! Deterministic: it reads no clock (the strategy's `ctx.now()` is event time) and keeps its instruments in order.
//! A stop is only as good as the trades the strategy sees: through a gap it fires at the first print on the far side,
//! and the collar then decides whether the order fills (a real gap through the collar rests unfilled).

use std::collections::BTreeMap;

use tf_calendar::Calendar;
use tf_core::{InstrumentId, NANOS_PER_SEC, Nanos, Px};

use crate::intent::{IntentId, Pricing, Purpose, Side, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};

/// The `reason` code of the closing intents (the strategy's own codes are below these).
pub const REASON_STOP: u16 = 0xE501;
pub const REASON_TARGET: u16 = 0xE502;
pub const REASON_TIME: u16 = 0xE503;
/// An exit the strategy decided on its own signal (a bar closing the wrong side of a level), sent by [`ExitBook::exit_now`].
pub const REASON_SIGNAL: u16 = 0xE504;

/// Why an exit was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Stop,
    Target,
    Time,
    Signal,
}

impl ExitReason {
    pub const fn code(self) -> u16 {
        match self {
            ExitReason::Stop => REASON_STOP,
            ExitReason::Target => REASON_TARGET,
            ExitReason::Time => REASON_TIME,
            ExitReason::Signal => REASON_SIGNAL,
        }
    }
}

/// How a position is to be closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitPlan {
    pub stop: Option<Px>,
    pub target: Option<Px>,
    /// Flat by this event time (see [`flat_by`]).
    pub flat_by: Option<Nanos>,
    /// How far the closing limit may be from the price that triggered it, in permille (0 to 999).
    pub collar_permille: u32,
    /// The least time between two attempts to close the same position.
    pub retry_after: Nanos,
    /// Time in force of the closing orders: day or good-til-cancelled (immediate-or-cancel is refused in the
    /// extended hours).
    pub tif: Tif,
}

impl ExitPlan {
    /// No exits, a 0.5% collar, a second between attempts, day orders.
    pub const fn new() -> ExitPlan {
        ExitPlan {
            stop: None,
            target: None,
            flat_by: None,
            collar_permille: 5,
            retry_after: NANOS_PER_SEC,
            tif: Tif::Day,
        }
    }
}

impl Default for ExitPlan {
    fn default() -> Self {
        ExitPlan::new()
    }
}

/// The instant `minutes` before the regular close of the New York trading day containing `ts`; `None` on a day the
/// market is closed or the calendar does not cover. On an early-close day it is before 13:00.
pub fn flat_by(ts: Nanos, minutes: u32) -> Option<Nanos> {
    let cal = Calendar::us_equities();
    let date = cal.date_of(ts).ok()?;
    cal.minutes_before_close(date, minutes).ok()?
}

#[derive(Clone, Copy, Debug)]
struct Held {
    long: bool,
    /// Shares still to close.
    left: u32,
    plan: ExitPlan,
    /// The exit that is out, its size and what it has filled so far.
    working: Option<(IntentId, u32, u32)>,
    /// When the last exit was sent.
    last_sent: Option<Nanos>,
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExitStats {
    pub stops: u64,
    pub targets: u64,
    pub time_exits: u64,
    pub signal_exits: u64,
    /// Exits the strategy's own checks (`submit`) refused.
    pub refused: u64,
    /// Positions closed.
    pub closed: u64,
}

/// The exits of one strategy, by instrument.
pub struct ExitBook {
    timer_base: u32,
    held: BTreeMap<InstrumentId, Held>,
    stats: ExitStats,
}

impl ExitBook {
    /// `timer_base`: the book uses timers `timer_base + instrument id` for the time exits; the strategy keeps those
    /// clear of its own.
    pub fn new(timer_base: u32) -> ExitBook {
        ExitBook {
            timer_base,
            held: BTreeMap::new(),
            stats: ExitStats::default(),
        }
    }

    fn timer(&self, id: InstrumentId) -> Option<TimerId> {
        self.timer_base.checked_add(id).map(TimerId)
    }

    /// An entry has filled for `qty` shares: hold its exits. For an instrument already held the shares are added
    /// and the plan replaced (an add to a position has the new plan).
    pub fn arm(
        &mut self,
        ctx: &mut Ctx<'_>,
        id: InstrumentId,
        long: bool,
        qty: u32,
        plan: ExitPlan,
    ) {
        if qty == 0 {
            return;
        }
        let working = self.held.get(&id).and_then(|h| h.working);
        let last_sent = self.held.get(&id).and_then(|h| h.last_sent);
        let left = self.held.get(&id).map_or(0, |h| h.left) + qty;
        self.held.insert(
            id,
            Held {
                long,
                left,
                plan,
                working,
                last_sent,
            },
        );
        if let (Some(t), Some(at)) = (self.timer(id), plan.flat_by) {
            ctx.set_timer(t, at);
        }
    }

    /// Stop holding exits for `id` (the strategy closed it some other way).
    pub fn disarm(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId) -> bool {
        if let Some(t) = self.timer(id) {
            ctx.cancel_timer(t);
        }
        self.held.remove(&id).is_some()
    }

    /// Raise the stop of a long held in `id` to `stop`, for a trailing stop; a stop that is not above the one held is ignored.
    /// A short's stop is never moved this way. True if the stop was raised (or set where there was none).
    pub fn raise_stop(&mut self, id: InstrumentId, stop: Px) -> bool {
        let Some(h) = self.held.get_mut(&id) else {
            return false;
        };
        if !h.long || h.plan.stop.is_some_and(|s| s >= stop) {
            return false;
        }
        h.plan.stop = Some(stop);
        true
    }

    pub fn is_held(&self, id: InstrumentId) -> bool {
        self.held.contains_key(&id)
    }

    /// Shares still to close in `id`.
    pub fn left(&self, id: InstrumentId) -> u32 {
        self.held.get(&id).map_or(0, |h| h.left)
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    pub fn stats(&self) -> ExitStats {
        self.stats
    }

    /// A trade of `id` at `px`. If it is at or through the stop or the target and no exit is out (and the last one was
    /// sent at least `retry_after` ago), the closing order is sent; the reason is returned.
    pub fn on_trade(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId, px: Px) -> Option<ExitReason> {
        let h = self.held.get(&id)?;
        if h.working.is_some() {
            return None;
        }
        let reason = if h
            .plan
            .stop
            .is_some_and(|s| if h.long { px <= s } else { px >= s })
        {
            ExitReason::Stop
        } else if h
            .plan
            .target
            .is_some_and(|t| if h.long { px >= t } else { px <= t })
        {
            ExitReason::Target
        } else {
            return None;
        };
        self.send(ctx, id, px, reason).then_some(reason)
    }

    /// Close what is held in `id` now, on the strategy's own signal, with a collar around `px`: for an exit that is a decision
    /// about a bar and not a price a trade can touch. Sent if nothing is out and the last exit was at least `retry_after` ago;
    /// the same one-at-a-time and retry rules as every other exit. False if it was not sent.
    pub fn exit_now(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId, px: Px) -> bool {
        match self.held.get(&id) {
            Some(h) if h.working.is_none() => self.send(ctx, id, px, ExitReason::Signal),
            _ => false,
        }
    }

    /// A timer fired. If it is the time exit of an instrument held, the closing order is sent at the last trade
    /// price (or, with no trade yet, the timer is set again for later). `None` for a timer that is not the book's.
    pub fn on_timer(
        &mut self,
        ctx: &mut Ctx<'_>,
        timer: TimerId,
    ) -> Option<(InstrumentId, ExitReason)> {
        let id = timer.0.checked_sub(self.timer_base)?;
        let h = *self.held.get(&id)?;
        h.plan.flat_by?;
        if h.working.is_some() {
            // An exit is out; look again later in case it ends without closing.
            ctx.set_timer_in(timer, h.plan.retry_after.max(1));
            return None;
        }
        let Some(px) = ctx.state(id).and_then(|s| s.last_px) else {
            ctx.set_timer_in(timer, h.plan.retry_after.max(1));
            return None;
        };
        if self.send(ctx, id, px, ExitReason::Time) {
            Some((id, ExitReason::Time))
        } else {
            ctx.set_timer_in(timer, h.plan.retry_after.max(1));
            None
        }
    }

    /// An order update for the strategy. Keeps the book's accounts: what an exit filled is closed; an exit that
    /// ended short leaves the rest to try again; a closed position is forgotten. True if `u` was an exit's.
    pub fn on_order_update(&mut self, ctx: &mut Ctx<'_>, u: &OrderUpdate) -> bool {
        let Some((&id, h)) = self
            .held
            .iter_mut()
            .find(|(_, h)| h.working.is_some_and(|(i, _, _)| i == u.intent))
        else {
            return false;
        };
        let Some((intent, asked, seen)) = h.working else {
            return false;
        };
        let new = u.filled_qty.saturating_sub(seen).min(h.left);
        h.left -= new;
        h.working = Some((intent, asked, u.filled_qty));
        if h.left == 0 {
            self.held.remove(&id);
            if let Some(t) = self.timer(id) {
                ctx.cancel_timer(t);
            }
            self.stats.closed += 1;
        } else if u.state.is_terminal() {
            h.working = None;
        }
        true
    }

    fn send(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId, px: Px, reason: ExitReason) -> bool {
        let Some(h) = self.held.get_mut(&id) else {
            return false;
        };
        if h.last_sent
            .is_some_and(|t| ctx.now() < t.saturating_add(h.plan.retry_after))
        {
            return false;
        }
        let req = Request {
            side: if h.long { Side::Sell } else { Side::Buy },
            qty: h.left,
            purpose: Purpose::Close,
            pricing: Pricing::Collar {
                reference: px,
                collar_permille: h.plan.collar_permille.min(999),
            },
            protect: None,
            tif: h.plan.tif,
            reason: reason.code(),
        };
        h.last_sent = Some(ctx.now());
        match ctx.submit(id, req) {
            Ok(intent) => {
                h.working = Some((intent, h.left, 0));
                match reason {
                    ExitReason::Stop => self.stats.stops += 1,
                    ExitReason::Target => self.stats.targets += 1,
                    ExitReason::Time => self.stats.time_exits += 1,
                    ExitReason::Signal => self.stats.signal_exits += 1,
                }
                true
            }
            Err(_) => {
                self.stats.refused += 1;
                false
            }
        }
    }
}
