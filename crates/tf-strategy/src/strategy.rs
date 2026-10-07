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
use tf_engine::{
    BarClose, BarsGrant, BarsRefused, MtfBars, Promoter, RollingBars, SharedBars, SymbolBars,
    SymbolState, TfBar, Tier0, Tier1Symbol, Timeframe, TrackError,
};
use tf_params::ParamStore;

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
pub(crate) struct Timers {
    by_time: BTreeSet<(Nanos, TimerId)>,
    by_id: BTreeMap<TimerId, Nanos>,
}

impl Timers {
    pub(crate) fn set(&mut self, id: TimerId, at: Nanos) {
        self.cancel(id);
        self.by_time.insert((at, id));
        self.by_id.insert(id, at);
    }

    pub(crate) fn cancel(&mut self, id: TimerId) -> bool {
        match self.by_id.remove(&id) {
            Some(at) => self.by_time.remove(&(at, id)),
            None => false,
        }
    }

    /// The earliest timer due at or before `limit`, removed.
    /// The time of the earliest pending timer.
    pub(crate) fn first_at(&self) -> Option<Nanos> {
        self.by_time.first().map(|t| t.0)
    }

    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    pub(crate) fn pop_due(&mut self, limit: Nanos) -> Option<(Nanos, TimerId)> {
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
    bars: BarsAccess<'a>,
    params: &'a Option<ParamStore>,
    promoter: Option<&'a mut Promoter>,
    tier_out: &'a mut Vec<tf_core::TierChange>,
}

/// Where a context gets its multi-timeframe bars: a host's own aggregator (one strategy, its own symbols), or
/// the engine's shared one, in which the strategy's requests are claims counted against its number.
enum BarsAccess<'a> {
    Own(&'a mut Option<MtfBars>),
    Shared {
        owner: u16,
        bars: Option<&'a mut SharedBars>,
    },
}

/// Why a request about multi-timeframe bars failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarsError {
    /// The host was built without a bar aggregator ([`Host::with_bars`]).
    NotConfigured,
    Track(TrackError),
}

impl<'a> Ctx<'a> {
    /// A context for a strategy that has no parameters, promoter or bars of its own (the
    /// cross-sectional runner, which shares the engine's Tier 0, Tier 1 and bars).
    pub(crate) fn shared(
        now: Nanos,
        strategy: StrategyId,
        tier0: &'a Tier0,
        state: &'a mut CtxState,
        promoter: Option<&'a mut Promoter>,
        bars: Option<&'a mut SharedBars>,
    ) -> Ctx<'a> {
        Ctx {
            now,
            strategy,
            tier0,
            next_seq: &mut state.next_seq,
            out: &mut state.out,
            invalid: &mut state.invalid,
            timers: &mut state.timers,
            bars: BarsAccess::Shared {
                owner: strategy.0,
                bars,
            },
            params: &state.params,
            promoter,
            tier_out: &mut state.tier_out,
        }
    }
}

/// What a context borrows from its owner, for owners outside this module.
#[derive(Default)]
pub(crate) struct CtxState {
    pub(crate) next_seq: u64,
    pub(crate) out: Vec<Intent>,
    pub(crate) invalid: u64,
    pub(crate) timers: Timers,
    params: Option<ParamStore>,
    /// Tier changes the strategy's requests caused, for the host to put on the tape.
    pub(crate) tier_out: Vec<tf_core::TierChange>,
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

    /// Tier 1 state (rings and pullback features) of `id`, if the host has a promoter and
    /// the symbol is promoted ([`Host::with_promoter`]).
    pub fn tier1(&self, id: InstrumentId) -> Option<&Tier1Symbol> {
        self.promoter.as_deref()?.symbol(id)
    }

    /// Whether the host has a promoter at all ([`Host::with_promoter`]).
    pub fn has_promoter(&self) -> bool {
        self.promoter.is_some()
    }

    /// Whether `id` is in Tier 1.
    pub fn is_promoted(&self, id: InstrumentId) -> bool {
        self.promoter.as_deref().is_some_and(|p| p.is_promoted(id))
    }

    /// Keep `id` in Tier 1 while the strategy holds a position or a working order in it: a held
    /// symbol is never demoted or evicted. Holds are counted per strategy, so another strategy
    /// letting go of the symbol does not release this one's. Release it with
    /// [`Ctx::unpin_tier1`] when the position is closed and the orders are done.
    pub fn pin_tier1(&mut self, id: InstrumentId) {
        let owner = self.strategy.0;
        if let Some(p) = self.promoter.as_deref_mut() {
            p.pin(owner, id);
        }
    }

    pub fn unpin_tier1(&mut self, id: InstrumentId) {
        let owner = self.strategy.0;
        if let Some(p) = self.promoter.as_deref_mut() {
            p.unpin(owner, id);
        }
    }

    /// Ask for `id` to be in Tier 1 for as long as the strategy wants it. The answer says whether it
    /// was granted (already there, promoted, or promoted by evicting a symbol nobody holds and of
    /// lower priority) or why not; a denial is counted against this strategy. `None` if the host has
    /// no promoter. See `tf_engine::claims` for the priority rule.
    pub fn request_tier1(&mut self, id: InstrumentId) -> Option<tf_engine::Grant> {
        let owner = self.strategy.0;
        let now = self.now;
        let p = self.promoter.as_deref_mut()?;
        Some(p.request(owner, id, now, self.tier_out))
    }

    /// The strategy no longer wants `id` in Tier 1 (its hold, if any, is separate: see `unpin_tier1`).
    pub fn release_tier1(&mut self, id: InstrumentId) {
        let owner = self.strategy.0;
        if let Some(p) = self.promoter.as_deref_mut() {
            p.release(owner, id);
        }
    }

    /// Whether this strategy has an interest in `id` (it is lost when the symbol is evicted).
    pub fn wants_tier1(&self, id: InstrumentId) -> bool {
        self.promoter
            .as_deref()
            .is_some_and(|p| p.is_wanted_by(self.strategy.0, id))
    }

    /// The parameter store, if the host has one ([`Host::with_params`]). Changes arrive as
    /// events and take effect before the strategy sees the event that carried them; a
    /// strategy should freeze what governs a position at entry (see
    /// [`ParamStore::revision`]) so a change applies to new entries only.
    pub fn params(&self) -> Option<&ParamStore> {
        self.params.as_ref()
    }

    /// Bars on 1m, 5m, 15m, 1h and day timeframes for a tracked instrument. `None`
    /// if there is no aggregator or the instrument is not tracked (for a strategy on the engine's
    /// shared bars: not tracked *for this strategy*, so what it sees does not depend on which others run).
    pub fn bars(&self, id: InstrumentId) -> Option<&SymbolBars> {
        match &self.bars {
            BarsAccess::Own(b) => b.as_ref()?.symbol(id),
            BarsAccess::Shared { owner, bars } => bars.as_deref()?.symbol(*owner, id),
        }
    }

    /// Start building multi-timeframe bars for `id`. Bars begin with the next trade;
    /// nothing is back-filled. The tracked set is bounded by the aggregator. On the engine's shared
    /// bars this is a claim counted for this strategy: `AlreadyTracked` if it already had one, `Full`
    /// when the bound is reached (also counted and exported), and a symbol another strategy already
    /// tracks is always granted.
    pub fn track_bars(&mut self, id: InstrumentId) -> Result<(), BarsError> {
        match &mut self.bars {
            BarsAccess::Own(b) => b
                .as_mut()
                .ok_or(BarsError::NotConfigured)?
                .track(id)
                .map_err(BarsError::Track),
            BarsAccess::Shared { owner, bars } => {
                let bars = bars.as_deref_mut().ok_or(BarsError::NotConfigured)?;
                match bars.claim(*owner, id) {
                    Ok(BarsGrant::Started | BarsGrant::Joined) => Ok(()),
                    Ok(BarsGrant::Already) => Err(BarsError::Track(TrackError::AlreadyTracked)),
                    Err(BarsRefused::Full) => Err(BarsError::Track(TrackError::Full)),
                    Err(BarsRefused::Unknown) => Err(BarsError::Track(TrackError::Unknown)),
                }
            }
        }
    }

    /// Stop building bars for `id` and drop them (on the shared bars: give up this strategy's claim; the
    /// bars go with the last one). True if it was tracked.
    pub fn untrack_bars(&mut self, id: InstrumentId) -> bool {
        match &mut self.bars {
            BarsAccess::Own(b) => b.as_mut().is_some_and(|b| b.untrack(id)),
            BarsAccess::Shared { owner, bars } => {
                bars.as_deref_mut().is_some_and(|b| b.release(*owner, id))
            }
        }
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

    /// A multi-timeframe bar has just closed for a tracked instrument. Called before
    /// [`Strategy::on_event`] for the trade (or the passing of time) that closed it,
    /// in time order, with the bar that closed. (When a gap is filled several bars close at
    /// once, so `ctx.bars(id)?.closed(timeframe, 0)` may be a later one than the bar given.)
    fn on_bar(
        &mut self,
        _ctx: &mut Ctx<'_>,
        _instrument: InstrumentId,
        _timeframe: Timeframe,
        _bar: &TfBar,
    ) {
    }

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
    bars: Option<MtfBars>,
    closes: Vec<BarClose>,
    params: Option<ParamStore>,
    param_errors: u64,
    promoter: Option<Promoter>,
    tier_events: Vec<tf_core::TierChange>,
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
            bars: None,
            closes: Vec::new(),
            params: None,
            param_errors: 0,
            promoter: None,
            tier_events: Vec::new(),
        }
    }

    /// Give the host a promoter: the scanner's hits move symbols into Tier 1 (and back),
    /// strategies read promoted symbols through [`Ctx::tier1`], and every move is a
    /// `TierChange` event to collect with [`Host::drain_tier_events`]. A promoter built with
    /// [`Promoter::follower`] instead applies `TierChange` events found in the stream.
    pub fn with_promoter(mut self, promoter: Promoter) -> Self {
        self.promoter = Some(promoter);
        self
    }

    pub fn promoter(&self) -> Option<&Promoter> {
        self.promoter.as_ref()
    }

    /// The tier changes made since the last call, in order: what to write to the tape.
    pub fn drain_tier_events(&mut self) -> Vec<tf_core::TierChange> {
        std::mem::take(&mut self.tier_events)
    }

    /// Give the strategy a parameter store. [`Event::ParamChange`] events in the stream are
    /// applied to it, so a replay of a tape reproduces the session's parameters.
    pub fn with_params(mut self, params: ParamStore) -> Self {
        self.params = Some(params);
        self
    }

    /// The parameter store, to check proposals against (see [`ParamStore::check`]).
    pub fn params(&self) -> Option<&ParamStore> {
        self.params.as_ref()
    }

    /// Parameter-change events the store refused when applying them. Zero in a faithful
    /// replay; non-zero means the tape and the store's declarations disagree.
    pub fn param_errors(&self) -> u64 {
        self.param_errors
    }

    /// Give the strategy multi-timeframe bars (see [`Ctx::track_bars`]).
    pub fn with_bars(mut self, bars: MtfBars) -> Self {
        self.bars = Some(bars);
        self
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
            bars: BarsAccess::Own(&mut self.bars),
            params: &self.params,
            promoter: self.promoter.as_mut(),
            tier_out: &mut self.tier_events,
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
        if let Some(p) = &mut self.promoter {
            p.on_event(&self.tier0, ev, &mut self.tier_events);
        }
        self.now = self.now.max(ts);
        let now = self.now;
        if let (Some(store), Event::ParamChange(c)) = (&mut self.params, ev) {
            if store.apply(c).is_err() {
                self.param_errors += 1;
            }
        }
        if let Some(b) = &mut self.bars {
            b.on_event(ev, &mut self.closes);
        }
        self.deliver_bar_closes(now);
        self.call(now, |s, c| s.on_event(c, ev));
    }

    /// Let time pass with no event (the end of the data, or an idle stretch):
    /// timers due by `ts` fire.
    pub fn advance_to(&mut self, ts: Nanos) {
        self.fire_timers(ts);
        self.now = self.now.max(ts);
        if let Some(b) = &mut self.bars {
            b.advance_to(ts, &mut self.closes);
        }
        let now = self.now;
        self.deliver_bar_closes(now);
    }

    /// Tell the strategy about the bars that just closed, in order.
    fn deliver_bar_closes(&mut self, now: Nanos) {
        if self.closes.is_empty() {
            return;
        }
        let closes = std::mem::take(&mut self.closes);
        for c in &closes {
            self.call(now, |s, ctx| {
                s.on_bar(ctx, c.instrument, c.timeframe, &c.bar)
            });
        }
        self.closes = closes;
        self.closes.clear();
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
