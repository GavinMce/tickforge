//! The strategy trait and the host that drives it.
//!
//! A strategy is a deterministic state machine: **events and timers in, intents
//! out**. It reacts to market events ([`Strategy::on_event`]), to timers it set
//! earlier ([`Strategy::on_timer`]) and to what became of its orders
//! ([`Strategy::on_order_update`]), and everything it can do goes through the
//! [`Ctx`] it is handed.
//!
//! # No wall clock, no I/O
//!
//! The [`Ctx`] gives a strategy: the current *event* time, read-only market state,
//! `submit` for intents and a timer API. It offers no clock, no file, no socket,
//! no randomness. Because a strategy author could still import `std::time`, the
//! crate's `clippy.toml` bans the wall clock, the filesystem, the network,
//! processes, the environment, `HashMap` / `HashSet` (iteration order) and the
//! print macros; CI's `clippy -D warnings` turns any use into a build failure. A
//! new strategy crate copies that file.
//!
//! # Time
//!
//! Time is the `ts_recv` of the events fed to the [`Host`]. A timer set for time
//! `T` fires before the first event at or after `T`, with `now` equal to `T`
//! exactly, so a backtest reproduces to the nanosecond and a replay at any speed
//! is identical.

use std::collections::{BTreeMap, BTreeSet};

use tf_core::{Event, InstrumentId, Nanos};
use tf_engine::{RollingBars, SymbolState, Tier0};

use crate::intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
use crate::lifecycle::OrderUpdate;

/// Names one of a strategy's timers. Setting a timer with an id that is already
/// pending reschedules it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(pub u32);

/// What a strategy wants to submit. The [`Ctx`] adds the strategy id, a sequence
/// number and the time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub side: Side,
    pub qty: u32,
    pub purpose: Purpose,
    pub pricing: Pricing,
    pub protect: Option<Protective>,
    pub tif: Tif,
    pub reason: u16,
}

/// Pending timers, fired in `(time, id)` order.
#[derive(Debug, Default)]
struct Timers {
    by_time: BTreeSet<(Nanos, TimerId)>,
    by_id: BTreeMap<TimerId, Nanos>,
}

impl Timers {
    fn set(&mut self, id: TimerId, at: Nanos) {
        self.cancel(id);
        self.by_time.insert((at, id));
        self.by_id.insert(id, at);
    }

    fn cancel(&mut self, id: TimerId) -> bool {
        match self.by_id.remove(&id) {
            Some(at) => self.by_time.remove(&(at, id)),
            None => false,
        }
    }

    /// The earliest timer due at or before `limit`, removed.
    fn pop_due(&mut self, limit: Nanos) -> Option<(Nanos, TimerId)> {
        let first = *self.by_time.first()?;
        if first.0 > limit {
            return None;
        }
        self.by_time.remove(&first);
        self.by_id.remove(&first.1);
        Some(first)
    }
}

/// Everything a strategy may touch while handling one event, timer or update.
pub struct Ctx<'a> {
    now: Nanos,
    strategy: StrategyId,
    tier0: &'a Tier0,
    next_seq: &'a mut u64,
    out: &'a mut Vec<Intent>,
    invalid: &'a mut u64,
    timers: &'a mut Timers,
}

impl Ctx<'_> {
    /// The current event time. Never the wall clock.
    pub fn now(&self) -> Nanos {
        self.now
    }

    /// Tier 0 state for an instrument, including the event being handled.
    pub fn state(&self, id: InstrumentId) -> Option<&SymbolState> {
        self.tier0.symbol(id)
    }

    /// Rolling windows and one-second bars for an instrument.
    pub fn windows(&self, id: InstrumentId) -> Option<&RollingBars> {
        self.tier0.windows(id)
    }

    /// Submit an intent. It is validated here: a malformed one is refused with
    /// the reason, counted, and never leaves the strategy. A good one gets the
    /// strategy's next sequence number and the current event time.
    pub fn submit(
        &mut self,
        instrument: InstrumentId,
        req: Request,
    ) -> Result<IntentId, IntentError> {
        let id = IntentId {
            strategy: self.strategy,
            seq: *self.next_seq,
        };
        let intent = Intent {
            id,
            instrument,
            side: req.side,
            qty: req.qty,
            purpose: req.purpose,
            pricing: req.pricing,
            protect: req.protect,
            tif: req.tif,
            ts: self.now,
            reason: req.reason,
        };
        if let Err(e) = intent.validate() {
            *self.invalid += 1;
            return Err(e);
        }
        *self.next_seq += 1;
        self.out.push(intent);
        Ok(id)
    }

    /// Fire timer `id` at event time `at`. A time not after `now` becomes the
    /// next nanosecond, so a timer can never fire in the instant that set it.
    pub fn set_timer(&mut self, id: TimerId, at: Nanos) {
        self.timers.set(id, at.max(self.now.saturating_add(1)));
    }

    /// Fire timer `id` `delay` nanoseconds from now.
    pub fn set_timer_in(&mut self, id: TimerId, delay: Nanos) {
        self.set_timer(id, self.now.saturating_add(delay));
    }

    /// Cancel a pending timer; true if there was one.
    pub fn cancel_timer(&mut self, id: TimerId) -> bool {
        self.timers.cancel(id)
    }
}

/// A trading strategy. See the module docs for what it can and cannot do.
pub trait Strategy: Send {
    fn id(&self) -> StrategyId;

    /// A market event, after Tier 0 has absorbed it. Called for every event the
    /// host is fed; the strategy picks the instruments it cares about.
    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event);

    /// A timer set with [`Ctx::set_timer`] has come due.
    fn on_timer(&mut self, ctx: &mut Ctx<'_>, timer: TimerId);

    /// What became of one of its intents.
    fn on_order_update(&mut self, _ctx: &mut Ctx<'_>, _update: &OrderUpdate) {}
}

/// Timers fired in one step before the host gives up on it, so a strategy that
/// re-arms itself every nanosecond cannot hang a backtest.
pub const MAX_TIMER_FIRES_PER_STEP: u32 = 100_000;

/// Drives a strategy from a stream of events: keeps Tier 0, fires timers in
/// order, stamps intents, and collects them.
pub struct Host<S: Strategy> {
    strategy: S,
    tier0: Tier0,
    timers: Timers,
    out: Vec<Intent>,
    next_seq: u64,
    invalid: u64,
    storms: u64,
    now: Nanos,
}

impl<S: Strategy> Host<S> {
    /// `id_space` is the number of instrument ids (see `Session::id_space`).
    pub fn new(strategy: S, id_space: usize) -> Self {
        Host {
            strategy,
            tier0: Tier0::new(id_space),
            timers: Timers::default(),
            out: Vec::new(),
            next_seq: 0,
            invalid: 0,
            storms: 0,
            now: 0,
        }
    }

    fn call<R>(&mut self, now: Nanos, f: impl FnOnce(&mut S, &mut Ctx<'_>) -> R) -> R {
        let mut ctx = Ctx {
            now,
            strategy: self.strategy.id(),
            tier0: &self.tier0,
            next_seq: &mut self.next_seq,
            out: &mut self.out,
            invalid: &mut self.invalid,
            timers: &mut self.timers,
        };
        f(&mut self.strategy, &mut ctx)
    }

    /// Fire every timer due at or before `limit`, in order, each at its own time.
    fn fire_timers(&mut self, limit: Nanos) {
        let mut fires = 0;
        while let Some((at, id)) = self.timers.pop_due(limit) {
            if fires == MAX_TIMER_FIRES_PER_STEP {
                // Put it back; it fires on the next step, late (at the then-current
                // time). Count the storm.
                self.timers.set(id, at);
                self.storms += 1;
                return;
            }
            fires += 1;
            let when = at.max(self.now);
            self.now = when;
            self.call(when, |s, c| s.on_timer(c, id));
        }
    }

    /// Feed one market event: due timers fire first, then Tier 0 absorbs the
    /// event, then the strategy sees it.
    pub fn on_event(&mut self, ev: &Event) {
        let ts = ev.ts_recv();
        self.fire_timers(ts);
        self.tier0.on_event(ev);
        self.now = self.now.max(ts);
        let now = self.now;
        self.call(now, |s, c| s.on_event(c, ev));
    }

    /// Let time pass with no event (the end of the data, or an idle stretch):
    /// timers due by `ts` fire.
    pub fn advance_to(&mut self, ts: Nanos) {
        self.fire_timers(ts);
        self.now = self.now.max(ts);
    }

    /// Tell the strategy what became of an order.
    pub fn on_order_update(&mut self, update: &OrderUpdate) {
        let now = self.now;
        self.call(now, |s, c| s.on_order_update(c, update));
    }

    /// The intents emitted since the last call, in order.
    pub fn drain_intents(&mut self) -> Vec<Intent> {
        std::mem::take(&mut self.out)
    }

    /// Feed a whole stream and return every intent it produced, including those
    /// from timers that fired along the way.
    pub fn run(&mut self, events: impl IntoIterator<Item = Event>) -> Vec<Intent> {
        let mut all = Vec::new();
        for ev in events {
            self.on_event(&ev);
            all.append(&mut self.out);
        }
        all
    }

    pub fn strategy(&self) -> &S {
        &self.strategy
    }

    pub fn tier0(&self) -> &Tier0 {
        &self.tier0
    }

    pub fn now(&self) -> Nanos {
        self.now
    }

    /// Intents refused by [`Ctx::submit`] because they were malformed.
    pub fn invalid_intents(&self) -> u64 {
        self.invalid
    }

    /// Steps cut short by [`MAX_TIMER_FIRES_PER_STEP`].
    pub fn timer_storms(&self) -> u64 {
        self.storms
    }

    /// Timers still pending.
    pub fn pending_timers(&self) -> usize {
        self.timers.by_id.len()
    }
}
