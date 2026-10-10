//! Strategies that watch many symbols (E18-S03).
//!
//! A strategy over a universe of hundreds or thousands of symbols does not want a callback per event.
//! A [`CrossStrategy`] gets a periodic [`CrossStrategy::on_review`] with a read-only [`MemberView`] of
//! its members, and helpers to rank them, and optionally [`CrossStrategy::on_member_event`] for the
//! events of its members only.
//!
//! The runner ([`CrossRunner`]) does not own market state. The engine owns one Tier 0 and every
//! runner reads it (`Market`), so twenty strategies cost one Tier 0 update per event plus a bitset
//! test each. Membership is a bitset over dense instrument ids ([`Members`]).
//!
//! # Time
//!
//! Reviews fall on a grid of event time: the first event at or after each multiple of the period
//! triggers one review (a gap of several periods gives one review, not a catch-up burst), so a replay
//! of the same events reviews at the same instants. Timers behave as in [`crate::Host`].

use tf_core::TierChange;
use tf_core::{Event, InstrumentId, Nanos, SymbolTable};
use tf_engine::{Promoter, SharedBars, SymbolState, Tier0};
use tf_universe::{Change, LiveFeature, LiveView, RefInfo, Selection, Tier0View};

use crate::intent::{Intent, StrategyId};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, CtxState, MAX_TIMER_FIRES_PER_STEP, TimerId};
use crate::trace::Trace;

/// A set of instrument ids as a bitset: a test is one load and a mask.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Members {
    words: Vec<u64>,
    len: usize,
}

impl Members {
    pub fn new() -> Self {
        Members::default()
    }

    pub fn from_ids(ids: impl IntoIterator<Item = InstrumentId>) -> Self {
        let mut m = Members::new();
        for id in ids {
            m.insert(id);
        }
        m
    }

    /// The members of a stored selection, by symbol name. Names the table does not know are
    /// returned, so a stale list is noticed instead of silently shrinking.
    pub fn from_selection(sel: &Selection, table: &SymbolTable) -> (Members, Vec<String>) {
        let mut m = Members::new();
        let mut unknown = Vec::new();
        for s in &sel.symbols {
            match table.get(s) {
                Some(id) => {
                    m.insert(id);
                }
                None => unknown.push(s.clone()),
            }
        }
        (m, unknown)
    }

    pub fn contains(&self, id: InstrumentId) -> bool {
        self.words
            .get(id as usize / 64)
            .is_some_and(|w| w >> (id % 64) & 1 == 1)
    }

    /// True if it was not already a member.
    pub fn insert(&mut self, id: InstrumentId) -> bool {
        let w = id as usize / 64;
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        let bit = 1u64 << (id % 64);
        let fresh = self.words[w] & bit == 0;
        self.words[w] |= bit;
        self.len += usize::from(fresh);
        fresh
    }

    /// True if it was a member.
    pub fn remove(&mut self, id: InstrumentId) -> bool {
        let Some(word) = self.words.get_mut(id as usize / 64) else {
            return false;
        };
        let bit = 1u64 << (id % 64);
        let had = *word & bit != 0;
        *word &= !bit;
        self.len -= usize::from(had);
        had
    }

    /// Join and leave as a dynamic selector decided.
    pub fn apply(&mut self, change: &Change) {
        for id in &change.left {
            self.remove(*id);
        }
        for id in &change.entered {
            self.insert(*id);
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Ascending.
    pub fn iter(&self) -> impl Iterator<Item = InstrumentId> + '_ {
        self.words.iter().enumerate().flat_map(|(i, w)| {
            let mut w = *w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let b = w.trailing_zeros();
                w &= w - 1;
                Some((i * 64) as InstrumentId + b)
            })
        })
    }
}

/// What the engine shares with every runner: Tier 0 and, for gap and volume ratio, the reference
/// row of each symbol by instrument id.
#[derive(Clone, Copy)]
pub struct Market<'a> {
    pub tier0: &'a Tier0,
    pub refs: &'a [RefInfo],
}

/// The members of one strategy and their state, read-only.
pub struct MemberView<'a> {
    market: Market<'a>,
    members: &'a Members,
}

impl<'a> MemberView<'a> {
    /// A view of `members` over `market`. A runner builds one for each callback; strategies' tests
    /// can build one to call `on_review` directly.
    pub fn new(market: Market<'a>, members: &'a Members) -> Self {
        MemberView { market, members }
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn contains(&self, id: InstrumentId) -> bool {
        self.members.contains(id)
    }

    /// Member ids, ascending.
    pub fn ids(&self) -> impl Iterator<Item = InstrumentId> + 'a {
        self.members.iter()
    }

    /// Tier 0 state of a member; `None` for a symbol that is not one.
    pub fn state(&self, id: InstrumentId) -> Option<&'a SymbolState> {
        self.members
            .contains(id)
            .then(|| self.market.tier0.symbol(id))
            .flatten()
    }

    /// What Tier 0 keeps of a member by session (E19-S02): the premarket's and the regular session's high, low, volume and VWAP, the
    /// open (the first regular-session trade), the first minute's and first five minutes' volume and the 5 and 15 minute ranges.
    /// `None` for a symbol that is not a member, or when the host has not told Tier 0 the day.
    pub fn session(&self, id: InstrumentId) -> Option<&'a tf_engine::SessionState> {
        self.members
            .contains(id)
            .then(|| self.market.tier0.session(id))
            .flatten()
    }

    /// What the reference snapshot says about a member: the prior close, average volume and, from one-minute
    /// history, the previous session's high, low and close, the ATR, the volume baselines and the hourly EMA state
    /// (`None` for a symbol that is not a member; a column the snapshot lacks reads as unknown in it).
    pub fn reference(&self, id: InstrumentId) -> Option<&'a RefInfo> {
        self.members
            .contains(id)
            .then(|| self.market.refs.get(id as usize))
            .flatten()
    }

    /// A live measurement of a member (see [`LiveFeature`]).
    pub fn feature(&self, id: InstrumentId, f: LiveFeature) -> Option<i64> {
        if !self.members.contains(id) {
            return None;
        }
        Tier0View {
            tier0: self.market.tier0,
            refs: self.market.refs,
        }
        .value(id, f)
    }

    /// The `k` members with the highest (or lowest) value of `key`, best first; members for which
    /// `key` gives `None` are left out, and ties go to the lower id. Costs one pass over the members and
    /// room for `k` entries.
    pub fn top_k(
        &self,
        k: usize,
        descending: bool,
        key: impl Fn(InstrumentId, &SymbolState) -> Option<i64>,
    ) -> Vec<(i64, InstrumentId)> {
        // Best first; an entry is better than another by value (higher if descending), then lower id.
        let better = |a: &(i64, InstrumentId), b: &(i64, InstrumentId)| {
            if descending {
                (b.0, a.1) < (a.0, b.1)
            } else {
                (a.0, a.1) < (b.0, b.1)
            }
        };
        let mut best: Vec<(i64, InstrumentId)> = Vec::with_capacity(k.min(self.len()));
        if k == 0 {
            return best;
        }
        for id in self.members.iter() {
            let Some(st) = self.market.tier0.symbol(id) else {
                continue;
            };
            let Some(v) = key(id, st) else { continue };
            let cand = (v, id);
            if best.len() == k && !better(&cand, &best[k - 1]) {
                continue;
            }
            let at = best.partition_point(|b| better(b, &cand));
            if best.len() == k {
                best.pop();
            }
            best.insert(at, cand);
        }
        best
    }

    /// [`MemberView::top_k`] by a [`LiveFeature`].
    pub fn top_by(&self, f: LiveFeature, k: usize, descending: bool) -> Vec<(i64, InstrumentId)> {
        let view = Tier0View {
            tier0: self.market.tier0,
            refs: self.market.refs,
        };
        self.top_k(k, descending, |id, _| view.value(id, f))
    }
}

/// A strategy over a set of symbols.
pub trait CrossStrategy: Send {
    /// Whether [`CrossStrategy::on_member_event`] does anything. When false (the default) the runner
    /// does no work at all for an event between reviews and timers, which is what keeps twenty
    /// strategies cheap at the opening burst.
    const WANTS_MEMBER_EVENTS: bool = false;

    fn id(&self) -> StrategyId;

    /// How often to review, in event-time nanoseconds.
    fn period(&self) -> Nanos;

    /// The periodic callback: look at the members, submit intents.
    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>);

    /// An event of a member symbol, after Tier 0 has absorbed it. Never called for others.
    fn on_member_event(&mut self, _ctx: &mut Ctx<'_>, _view: &MemberView<'_>, _ev: &Event) {}

    fn on_timer(&mut self, _ctx: &mut Ctx<'_>, _view: &MemberView<'_>, _timer: TimerId) {}

    fn on_order_update(&mut self, _ctx: &mut Ctx<'_>, _update: &OrderUpdate) {}

    /// The strategy's interest in `id` (a `request_tier1` that was granted) was lost: the symbol was
    /// evicted for a strategy of higher priority or demoted. Its holds are never lost this way.
    fn on_tier1_revoked(&mut self, _ctx: &mut Ctx<'_>, _view: &MemberView<'_>, _id: InstrumentId) {}

    /// Whether to record [`Trace`]s of the decisions (the host asks before the day starts, and for a day that is to be viewed).
    /// A strategy that traces must decide the same whether it does or not.
    fn set_tracing(&mut self, _on: bool) {}

    /// The traces recorded since the last call; none unless tracing was asked for.
    fn take_traces(&mut self) -> Vec<Trace> {
        Vec::new()
    }
}

/// Drives one [`CrossStrategy`] from events the engine has already applied to the shared Tier 0.
pub struct CrossRunner<S: CrossStrategy> {
    strategy: S,
    members: Members,
    state: CtxState,
    period: Nanos,
    next_review: Option<Nanos>,
    /// Nothing is due before this: the earlier of the next review and the first timer.
    wake: Nanos,
    now: Nanos,
    reviews: u64,
    events: u64,
    storms: u64,
}

impl<S: CrossStrategy> CrossRunner<S> {
    pub fn new(strategy: S, members: Members) -> Self {
        let period = strategy.period().max(1);
        CrossRunner {
            strategy,
            members,
            state: CtxState::default(),
            period,
            next_review: None,
            wake: 0,
            now: 0,
            reviews: 0,
            events: 0,
            storms: 0,
        }
    }

    pub fn members(&self) -> &Members {
        &self.members
    }

    /// Change the membership (a dynamic selector's decision). Takes effect for the next event.
    pub fn members_mut(&mut self) -> &mut Members {
        &mut self.members
    }

    fn call<R>(
        &mut self,
        tier0: &Tier0,
        promoter: Option<&mut Promoter>,
        bars: Option<&mut SharedBars>,
        now: Nanos,
        f: impl FnOnce(&mut S, &mut Ctx<'_>) -> R,
    ) -> R {
        let mut ctx = Ctx::shared(
            now,
            self.strategy.id(),
            tier0,
            &mut self.state,
            promoter,
            bars,
        );
        f(&mut self.strategy, &mut ctx)
    }

    fn fire_timers(
        &mut self,
        m: Market<'_>,
        mut promoter: Option<&mut Promoter>,
        mut bars: Option<&mut SharedBars>,
        limit: Nanos,
    ) {
        let mut fires = 0;
        while let Some((at, id)) = self.state.timers.pop_due(limit) {
            if fires == MAX_TIMER_FIRES_PER_STEP {
                self.state.timers.set(id, at);
                self.storms += 1;
                return;
            }
            fires += 1;
            let when = at.max(self.now);
            self.now = when;
            let view = MemberView {
                market: m,
                members: &self.members,
            };
            let mut ctx = Ctx::shared(
                when,
                self.strategy.id(),
                m.tier0,
                &mut self.state,
                promoter.as_deref_mut(),
                bars.as_deref_mut(),
            );
            self.strategy.on_timer(&mut ctx, &view, id);
        }
    }

    fn review_if_due(
        &mut self,
        m: Market<'_>,
        promoter: Option<&mut Promoter>,
        bars: Option<&mut SharedBars>,
    ) {
        let now = self.now;
        match self.next_review {
            None => self.next_review = Some((now / self.period + 1) * self.period),
            Some(t) if now >= t => {
                self.next_review = Some((now / self.period + 1) * self.period);
                self.reviews += 1;
                let view = MemberView {
                    market: m,
                    members: &self.members,
                };
                let mut ctx = Ctx::shared(
                    now,
                    self.strategy.id(),
                    m.tier0,
                    &mut self.state,
                    promoter,
                    bars,
                );
                self.strategy.on_review(&mut ctx, &view);
            }
            Some(_) => {}
        }
    }

    /// One event the engine has applied to `market.tier0`: due timers fire first, then a member's
    /// event is delivered, then a review if one is due.
    #[inline]
    pub fn on_event(
        &mut self,
        market: Market<'_>,
        promoter: Option<&mut Promoter>,
        bars: Option<&mut SharedBars>,
        ev: &Event,
    ) {
        let ts = ev.ts_recv();
        if S::WANTS_MEMBER_EVENTS || ts >= self.wake {
            self.on_event_slow(market, promoter, bars, ev, ts);
        } else {
            self.now = self.now.max(ts);
        }
    }

    fn on_event_slow(
        &mut self,
        market: Market<'_>,
        mut promoter: Option<&mut Promoter>,
        mut bars: Option<&mut SharedBars>,
        ev: &Event,
        ts: Nanos,
    ) {
        let delivers = S::WANTS_MEMBER_EVENTS
            && !matches!(ev, Event::ParamChange(_) | Event::TierChange(_))
            && self.members.contains(ev.instrument());
        if ts < self.wake && !delivers {
            self.now = self.now.max(ts);
            return;
        }
        self.fire_timers(market, promoter.as_deref_mut(), bars.as_deref_mut(), ts);
        self.now = self.now.max(ts);
        if delivers {
            self.events += 1;
            let view = MemberView {
                market,
                members: &self.members,
            };
            let mut ctx = Ctx::shared(
                self.now,
                self.strategy.id(),
                market.tier0,
                &mut self.state,
                promoter.as_deref_mut(),
                bars.as_deref_mut(),
            );
            self.strategy.on_member_event(&mut ctx, &view, ev);
        }
        self.review_if_due(market, promoter, bars);
        self.rewake();
    }

    fn rewake(&mut self) {
        let review = self.next_review.unwrap_or(0);
        self.wake = review.min(self.state.timers.first_at().unwrap_or(Nanos::MAX));
    }

    /// Let time pass with no event: timers and a due review fire.
    pub fn advance_to(
        &mut self,
        market: Market<'_>,
        mut promoter: Option<&mut Promoter>,
        mut bars: Option<&mut SharedBars>,
        ts: Nanos,
    ) {
        self.fire_timers(market, promoter.as_deref_mut(), bars.as_deref_mut(), ts);
        self.now = self.now.max(ts);
        self.review_if_due(market, promoter, bars);
        self.rewake();
    }

    pub fn on_order_update(
        &mut self,
        tier0: &Tier0,
        promoter: Option<&mut Promoter>,
        bars: Option<&mut SharedBars>,
        update: &OrderUpdate,
    ) {
        let now = self.now;
        self.call(tier0, promoter, bars, now, |s, c| {
            s.on_order_update(c, update)
        });
        self.rewake();
    }

    /// Tell the strategy its interest in `id` was lost because the symbol left Tier 1 (the host takes
    /// these from [`Promoter::drain_revoked`]).
    pub fn on_tier1_revoked(
        &mut self,
        market: Market<'_>,
        promoter: Option<&mut Promoter>,
        bars: Option<&mut SharedBars>,
        id: InstrumentId,
    ) {
        let now = self.now;
        let view = MemberView {
            market,
            members: &self.members,
        };
        let mut ctx = Ctx::shared(
            now,
            self.strategy.id(),
            market.tier0,
            &mut self.state,
            promoter,
            bars,
        );
        self.strategy.on_tier1_revoked(&mut ctx, &view, id);
        self.rewake();
    }

    /// The tier changes this strategy's requests caused, for the tape.
    pub fn drain_tier_events(&mut self) -> Vec<TierChange> {
        std::mem::take(&mut self.state.tier_out)
    }

    pub fn drain_intents(&mut self) -> Vec<Intent> {
        std::mem::take(&mut self.state.out)
    }

    /// Ask the strategy to record traces, or to stop.
    pub fn set_tracing(&mut self, on: bool) {
        self.strategy.set_tracing(on);
    }

    /// The traces the strategy has recorded since the last call.
    pub fn drain_traces(&mut self) -> Vec<Trace> {
        self.strategy.take_traces()
    }

    pub fn strategy(&self) -> &S {
        &self.strategy
    }

    pub fn strategy_mut(&mut self) -> &mut S {
        &mut self.strategy
    }

    pub fn reviews(&self) -> u64 {
        self.reviews
    }

    /// Events delivered to `on_member_event`.
    pub fn member_events(&self) -> u64 {
        self.events
    }

    pub fn invalid_intents(&self) -> u64 {
        self.state.invalid
    }

    pub fn timer_storms(&self) -> u64 {
        self.storms
    }

    /// Timers still pending.
    pub fn pending_timers(&self) -> usize {
        self.state.timers.len()
    }
}
