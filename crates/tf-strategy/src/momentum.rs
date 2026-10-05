//! Strategy 1, long side: buy a healthy pullback in a low-float runner, ride it.
//!
//! Per symbol: `Idle -> Watching -> Entering -> Holding -> Exiting -> Cooldown ->
//! Idle` (DESIGN.md). The short side (dangerous pullbacks) is a separate story.
//!
//! - **Scan** (`Idle`): Tier 0's rolling window shows a price spike with volume,
//!   inside the price and spread filters. The symbol is promoted to the strategy's
//!   own [`Tier1`], so history starts as the spike does.
//! - **Watch**: once a second, read the pullback features. Skip the symbol for now
//!   if the impulse is too small. Once the pullback is old enough, enter if it is
//!   *healthy* (shallow, drying volume, not too far below the high, enough higher
//!   lows, bid support) and give up if it is *dangerous* (too deep, or volume that
//!   has not dried up) or has dragged on too long.
//! - **Enter**: an IOC marketable limit (collar around the ask) with a broker-side
//!   stop under the pullback low. Sized from a notional budget.
//! - **Ride**: exit on a trailing stop from the high since entry, or after a
//!   maximum hold, whichever comes first.
//! - **Cool down**, then wait for a fresh spike.
//!
//! The decision itself (when to give up, when it is ready to judge, what is dangerous,
//! what is healthy) is a [`RuleSet`], by default [`RuleSet::momentum`]; every threshold in
//! it is a literal or a field of [`MomentumParams`]. There are no numeric constants in the logic. Defaults were chosen on the synthetic runner
//! scenarios and are a starting point for real data, not a result. In
//! particular `min_higher_lows` matters: with 0 the strategy buys in the middle of
//! the pullback and is stopped out; with 1 it waits for the bounce.
//!
//! Positions are tracked from order updates (terminal states only), so a partly
//! filled IOC entry holds what it got. Decisions that depend on features are made
//! at most once per second per symbol.

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_engine::{PromoteError, PullbackFeatures, Quote1, Tier1};
use tf_params::{ParamId, ParamSpec, Scope};

use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::rules::{Evaluation, RuleSet};
use crate::strategy::{Ctx, Request, Strategy, TimerId};

/// Reasons recorded on intents, for the audit trail.
pub mod reason {
    pub const ENTRY_HEALTHY_PULLBACK: u16 = 1;
    pub const EXIT_TRAILING_STOP: u16 = 2;
    pub const EXIT_MAX_HOLD: u16 = 3;
}

/// One parameter an agent may tune through a [`tf_params::ParamStore`]: how to read and
/// write it, and the bounds it may move within. The bounds of related parameters do not
/// overlap (a minimum depth never exceeds the lowest maximum depth), so no combination of
/// allowed values is an invalid parameter set.
pub(crate) struct Tunable {
    pub(crate) name: &'static str,
    pub(crate) get: fn(&MomentumParams) -> i64,
    pub(crate) set: fn(&mut MomentumParams, i64),
    pub(crate) min: i64,
    pub(crate) max: i64,
    pub(crate) max_step: i64,
    pub(crate) cooldown_secs: u64,
}

macro_rules! tunable {
    ($field:ident, $min:expr, $max:expr, $step:expr, $cooldown:expr) => {
        Tunable {
            name: stringify!($field),
            get: |p| p.$field as i64,
            set: |p, v| p.$field = v.max(0) as _,
            min: $min,
            max: $max,
            max_step: $step,
            cooldown_secs: $cooldown,
        }
    };
}

const BILLION: i64 = 1_000_000_000;

/// What an agent may tune, and within what. Everything else (the price filter, caps
/// on positions and watched names, the cooldown) is fixed for the run, and the risk
/// limits are not here at all.
pub(crate) const TUNABLES: [Tunable; 17] = [
    tunable!(spike_permille, 10, 100, 10, 60),
    tunable!(spike_min_volume, 500, 20_000, 1_000, 60),
    tunable!(min_impulse_permille, 100, 1_000, 100, 60),
    tunable!(min_pullback_secs, 5, 30, 5, 60),
    tunable!(max_pullback_secs, 30, 120, 15, 60),
    tunable!(min_depth_permille, 0, 200, 30, 60),
    tunable!(max_depth_permille, 200, 500, 50, 60),
    tunable!(max_volume_ratio_permille, 100, 500, 50, 60),
    tunable!(max_retrace_now_permille, 100, 500, 50, 60),
    tunable!(min_higher_lows, 0, 3, 1, 60),
    tunable!(min_bid_support_permille, 0, 500, 50, 60),
    tunable!(
        entry_notional,
        100 * BILLION,
        2_000 * BILLION,
        250 * BILLION,
        300
    ),
    tunable!(max_qty, 100, 2_000, 250, 300),
    tunable!(collar_permille, 5, 50, 5, 60),
    tunable!(stop_buffer_permille, 5, 100, 15, 60),
    tunable!(trail_permille, 10, 100, 10, 60),
    tunable!(max_hold_secs, 30, 600, 60, 60),
];

/// The declarations of the parameters an agent may tune, with `base`'s values as their
/// baselines. Build a [`tf_params::ParamStore`] from them and give it to the host.
/// Fails (when the store is built) if a base value lies outside its bounds.
pub fn tunable_specs(base: &MomentumParams) -> Vec<ParamSpec> {
    TUNABLES
        .iter()
        .map(|t| ParamSpec {
            name: t.name,
            baseline: (t.get)(base),
            min: t.min,
            max: t.max,
            max_step: t.max_step as u64,
            cooldown: t.cooldown_secs * NANOS_PER_SEC,
            scope: Scope::PerInstrument,
        })
        .collect()
}

/// Every tunable of the strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MomentumParams {
    // -- scan --
    /// Window for the spike, seconds (1 to 60).
    pub spike_secs: u32,
    /// Rise over that window that counts as a spike, permille.
    pub spike_permille: i64,
    /// Shares traded in that window.
    pub spike_min_volume: u64,
    pub min_price: Px,
    pub max_price: Px,
    /// Widest acceptable spread, permille of the ask.
    pub max_spread_permille: u32,
    /// How many symbols may be watched at once.
    pub max_watched: u32,
    // -- classification --
    /// Smallest impulse (swing low to swing high) worth trading, permille of the low.
    pub min_impulse_permille: i64,
    /// Seconds after the high before a pullback is judged.
    pub min_pullback_secs: u32,
    /// Seconds after the high at which a pullback that has not triggered is dropped.
    pub max_pullback_secs: u32,
    pub min_depth_permille: i64,
    pub max_depth_permille: i64,
    /// Pullback volume rate against the impulse's, permille.
    pub max_volume_ratio_permille: u64,
    /// How far below the high the price may be right now, permille of the impulse.
    pub max_retrace_now_permille: i64,
    pub min_higher_lows: u32,
    /// Latest bid size as permille of bid + ask size.
    pub min_bid_support_permille: u32,
    // -- execution --
    /// Budget per entry, raw price units x shares.
    pub entry_notional: u128,
    pub max_qty: u32,
    /// How far above the ask the entry may pay, permille.
    pub collar_permille: u32,
    /// The stop sits this far under the pullback low, permille.
    pub stop_buffer_permille: u32,
    // -- riding --
    /// Exit when the price falls this far from its high since entry, permille.
    pub trail_permille: u32,
    pub max_hold_secs: u32,
    pub cooldown_secs: u32,
    /// Positions open at once.
    pub max_positions: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamError(pub &'static str);

impl Default for MomentumParams {
    fn default() -> Self {
        MomentumParams {
            spike_secs: 10,
            spike_permille: 30,
            spike_min_volume: 2_000,
            min_price: Px::from_cents(200),
            max_price: Px::from_cents(2_000),
            max_spread_permille: 50,
            max_watched: 16,
            min_impulse_permille: 300,
            min_pullback_secs: 10,
            max_pullback_secs: 60,
            min_depth_permille: 30,
            max_depth_permille: 350,
            max_volume_ratio_permille: 250,
            max_retrace_now_permille: 350,
            min_higher_lows: 1,
            min_bid_support_permille: 150,
            entry_notional: 1_000 * 1_000_000_000,
            max_qty: 1_000,
            collar_permille: 20,
            stop_buffer_permille: 20,
            trail_permille: 30,
            max_hold_secs: 120,
            cooldown_secs: 120,
            max_positions: 3,
        }
    }
}

impl MomentumParams {
    /// Every parameter as a name and a value, for recording in a run manifest. It
    /// destructures the struct, so adding a field without recording it is a
    /// compile error: a result must never be reused across different parameters.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let MomentumParams {
            spike_secs,
            spike_permille,
            spike_min_volume,
            min_price,
            max_price,
            max_spread_permille,
            max_watched,
            min_impulse_permille,
            min_pullback_secs,
            max_pullback_secs,
            min_depth_permille,
            max_depth_permille,
            max_volume_ratio_permille,
            max_retrace_now_permille,
            min_higher_lows,
            min_bid_support_permille,
            entry_notional,
            max_qty,
            collar_permille,
            stop_buffer_permille,
            trail_permille,
            max_hold_secs,
            cooldown_secs,
            max_positions,
        } = *self;
        vec![
            ("spike_secs", spike_secs.to_string()),
            ("spike_permille", spike_permille.to_string()),
            ("spike_min_volume", spike_min_volume.to_string()),
            ("min_price_raw", min_price.raw().to_string()),
            ("max_price_raw", max_price.raw().to_string()),
            ("max_spread_permille", max_spread_permille.to_string()),
            ("max_watched", max_watched.to_string()),
            ("min_impulse_permille", min_impulse_permille.to_string()),
            ("min_pullback_secs", min_pullback_secs.to_string()),
            ("max_pullback_secs", max_pullback_secs.to_string()),
            ("min_depth_permille", min_depth_permille.to_string()),
            ("max_depth_permille", max_depth_permille.to_string()),
            (
                "max_volume_ratio_permille",
                max_volume_ratio_permille.to_string(),
            ),
            (
                "max_retrace_now_permille",
                max_retrace_now_permille.to_string(),
            ),
            ("min_higher_lows", min_higher_lows.to_string()),
            (
                "min_bid_support_permille",
                min_bid_support_permille.to_string(),
            ),
            ("entry_notional_raw", entry_notional.to_string()),
            ("max_qty", max_qty.to_string()),
            ("collar_permille", collar_permille.to_string()),
            ("stop_buffer_permille", stop_buffer_permille.to_string()),
            ("trail_permille", trail_permille.to_string()),
            ("max_hold_secs", max_hold_secs.to_string()),
            ("cooldown_secs", cooldown_secs.to_string()),
            ("max_positions", max_positions.to_string()),
        ]
    }

    pub fn validate(&self) -> Result<(), ParamError> {
        let bad = |m| Err(ParamError(m));
        if self.spike_secs == 0 || self.spike_secs > 60 {
            return bad("spike_secs must be 1 to 60");
        }
        if self.spike_permille <= 0 {
            return bad("spike_permille must be positive");
        }
        if self.min_price.raw() <= 0 || self.max_price < self.min_price {
            return bad("price filter must be positive and ordered");
        }
        if self.max_watched == 0 || self.max_positions == 0 {
            return bad("max_watched and max_positions must be positive");
        }
        if self.min_impulse_permille <= 0 {
            return bad("min_impulse_permille must be positive");
        }
        if self.min_pullback_secs > self.max_pullback_secs {
            return bad("min_pullback_secs must not exceed max_pullback_secs");
        }
        if self.min_depth_permille > self.max_depth_permille {
            return bad("min_depth_permille must not exceed max_depth_permille");
        }
        if self.entry_notional == 0 || self.max_qty == 0 {
            return bad("entry_notional and max_qty must be positive");
        }
        if self.collar_permille > 999
            || self.stop_buffer_permille >= 1000
            || self.trail_permille == 0
            || self.trail_permille >= 1000
        {
            return bad(
                "collar, stop buffer and trail must be permille below 1000 (trail above 0)",
            );
        }
        if self.max_hold_secs == 0 {
            return bad("max_hold_secs must be positive");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Promoted at this second; features read once per second after `last_eval`.
    Watching {
        last_eval: u64,
    },
    Entering {
        intent: IntentId,
        riding: Riding,
    },
    Holding {
        qty: u32,
        high: i64,
        riding: Riding,
    },
    Exiting {
        intent: IntentId,
        qty: u32,
        high: i64,
        riding: Riding,
    },
    Cooldown,
}

/// The parameters that govern a position after it is entered, frozen when the entry
/// is decided: a later parameter change applies to new entries only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Riding {
    trail_permille: u32,
    max_hold_secs: u32,
    collar_permille: u32,
}

/// Why an entry happened: what the strategy saw when it decided, and the thresholds in force.
/// One is recorded per entry intent, so a person (or a viewer) can see the evidence without
/// recomputing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryTrace {
    pub intent: IntentId,
    pub instrument: InstrumentId,
    pub ts: Nanos,
    /// The pullback features at the decision.
    pub features: PullbackFeatures,
    /// Swing low to swing high, permille of the low.
    pub impulse_permille: i64,
    /// The effective parameters used for this decision.
    pub params: MomentumParams,
    /// Every condition of the rule set as it evaluated.
    pub evaluations: Vec<Evaluation>,
    /// [`RuleSet::fingerprint`] of the rules that decided.
    pub rules: u64,
}

/// Why a watched symbol was given up on without an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclineReason {
    /// Too deep, or volume that had not dried up.
    Dangerous,
    /// The pullback went on too long without turning healthy.
    TooOld,
}

/// A symbol the strategy watched and decided not to enter, with what it saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decline {
    pub instrument: InstrumentId,
    pub ts: Nanos,
    pub reason: DeclineReason,
    pub features: PullbackFeatures,
    pub impulse_permille: i64,
    pub params: MomentumParams,
    pub evaluations: Vec<Evaluation>,
    pub rules: u64,
}

/// Counters for what the strategy decided and why not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MomentumStats {
    pub promoted: u64,
    pub entries: u64,
    pub exits: u64,
    pub rejected_dangerous: u64,
    pub rejected_too_old: u64,
    pub entries_failed: u64,
    pub promotions_refused: u64,
}

pub struct MomentumLong {
    id: StrategyId,
    p: MomentumParams,
    tier1: Tier1,
    phase: Vec<Phase>,
    positions: u32,
    stats: MomentumStats,
    /// The store's ids for [`TUNABLES`], found the first time a store is seen.
    bound: Option<Vec<Option<ParamId>>>,
    /// Per instrument: the effective parameters and the store revision (plus one) they
    /// were built at, so they are rebuilt only when something changed.
    effective: Vec<(u64, MomentumParams)>,
    /// Effective parameter sets that failed validation and were replaced by the base.
    conflicts: u64,
    traces: Vec<EntryTrace>,
    declines: Vec<Decline>,
    rules: RuleSet,
    rules_id: u64,
}

impl MomentumLong {
    pub fn new(
        id: StrategyId,
        params: MomentumParams,
        id_space: usize,
    ) -> Result<MomentumLong, ParamError> {
        params.validate()?;
        let rules = RuleSet::momentum();
        Ok(MomentumLong {
            rules_id: rules.fingerprint(),
            rules,
            id,
            p: params,
            tier1: Tier1::new(id_space, params.max_watched as usize),
            phase: vec![Phase::Idle; id_space],
            positions: 0,
            stats: MomentumStats::default(),
            bound: None,
            effective: vec![(0, params); id_space],
            conflicts: 0,
            traces: Vec::new(),
            declines: Vec::new(),
        })
    }

    /// Decide with `rules` instead of [`RuleSet::momentum`].
    pub fn with_rules(mut self, rules: RuleSet) -> MomentumLong {
        self.rules_id = rules.fingerprint();
        self.rules = rules;
        self
    }

    pub fn rules(&self) -> &RuleSet {
        &self.rules
    }

    /// The symbols it watched and declined, in order.
    pub fn declines(&self) -> &[Decline] {
        &self.declines
    }

    /// The evidence behind each entry intent, in order.
    pub fn entry_traces(&self) -> &[EntryTrace] {
        &self.traces
    }

    /// Times a store's overrides, combined, made an invalid parameter set and the base
    /// parameters were used instead. Should stay zero with [`tunable_specs`] bounds.
    pub fn parameter_conflicts(&self) -> u64 {
        self.conflicts
    }

    /// The parameters in force for `i` now: the base, with the store's values for the
    /// tunable ones. Rebuilt only when the store's revision changes.
    fn effective(&mut self, ctx: &Ctx<'_>, i: InstrumentId) -> MomentumParams {
        let Some(store) = ctx.params() else {
            return self.p;
        };
        let rev = store.revision() + 1;
        if self.effective[i as usize].0 == rev {
            return self.effective[i as usize].1;
        }
        let ids = self
            .bound
            .get_or_insert_with(|| TUNABLES.iter().map(|t| store.id_of(t.name)).collect());
        let mut p = self.p;
        for (t, id) in TUNABLES.iter().zip(ids.iter()) {
            if let Some(id) = id {
                (t.set)(&mut p, store.value_for(*id, i));
            }
        }
        if p.validate().is_err() {
            self.conflicts += 1;
            p = self.p;
        }
        self.effective[i as usize] = (rev, p);
        p
    }

    pub fn stats(&self) -> MomentumStats {
        self.stats
    }

    pub fn params(&self) -> &MomentumParams {
        &self.p
    }

    pub fn watched(&self) -> usize {
        self.tier1.live()
    }

    pub fn positions(&self) -> u32 {
        self.positions
    }

    fn cooldown_timer(&self, i: InstrumentId) -> TimerId {
        TimerId(self.phase.len() as u32 + i)
    }

    fn decline(
        &mut self,
        ctx: &Ctx<'_>,
        i: InstrumentId,
        reason: DeclineReason,
        f: &PullbackFeatures,
        impulse_permille: i64,
        p: &MomentumParams,
    ) {
        self.declines.push(Decline {
            instrument: i,
            ts: ctx.now(),
            reason,
            features: *f,
            impulse_permille,
            params: *p,
            evaluations: self.rules.evaluate(f, impulse_permille, p),
            rules: self.rules_id,
        });
    }

    fn stop_watching(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId) {
        self.tier1.demote(i);
        self.phase[i as usize] = Phase::Cooldown;
        let t = self.cooldown_timer(i);
        ctx.set_timer_in(t, Nanos::from(self.p.cooldown_secs) * NANOS_PER_SEC);
    }

    fn scan(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, now_sec: u64) {
        let p = self.effective(ctx, i);
        let Some(w) = ctx.windows(i) else { return };
        let Some(change) = w.price_change_permille(p.spike_secs as usize) else {
            return;
        };
        if change < p.spike_permille || w.volume(p.spike_secs as usize) < p.spike_min_volume {
            return;
        }
        let Some(st) = ctx.state(i) else { return };
        let (Some(last), Some((ask, _))) = (st.last_px, st.ask) else {
            return;
        };
        if last < p.min_price || last > p.max_price {
            return;
        }
        let spread = st.spread().unwrap_or(i64::MAX);
        if ask.raw() <= 0
            || i128::from(spread) * 1000 > i128::from(ask.raw()) * i128::from(p.max_spread_permille)
        {
            return;
        }
        match self.tier1.promote(i) {
            Ok(()) => {
                self.stats.promoted += 1;
                self.phase[i as usize] = Phase::Watching { last_eval: now_sec };
            }
            Err(PromoteError::Full) => self.stats.promotions_refused += 1,
            Err(_) => {}
        }
    }

    fn watch(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, now_sec: u64) {
        let p = self.effective(ctx, i);
        let Phase::Watching { last_eval } = self.phase[i as usize] else {
            return;
        };
        if now_sec <= last_eval {
            return; // once a second
        }
        self.phase[i as usize] = Phase::Watching { last_eval: now_sec };
        let Some(sym) = self.tier1.symbol(i) else {
            return;
        };
        let Some(f) = sym.features() else { return };
        let low = f.impulse_low.raw();
        let impulse = i128::from(f.impulse_high.raw() - low) * 1000 / i128::from(low.max(1));
        let impulse = impulse.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        if self.rules.too_old.holds(&f, impulse, &p) {
            self.stats.rejected_too_old += 1;
            self.decline(ctx, i, DeclineReason::TooOld, &f, impulse, &p);
            self.stop_watching(ctx, i);
            return;
        }
        if !self.rules.armed.holds(&f, impulse, &p) {
            return;
        }
        if self.rules.dangerous.holds(&f, impulse, &p) {
            self.stats.rejected_dangerous += 1;
            self.decline(ctx, i, DeclineReason::Dangerous, &f, impulse, &p);
            self.stop_watching(ctx, i);
            return;
        }
        if !self.rules.enter.holds(&f, impulse, &p) || self.positions >= p.max_positions {
            return;
        }
        let Some(Quote1 { ask, .. }) = sym.quote_ago(0) else {
            return;
        };
        if ask.raw() <= 0 {
            return;
        }
        let qty = u32::try_from(p.entry_notional / ask.raw() as u128)
            .unwrap_or(u32::MAX)
            .min(p.max_qty);
        let stop = Px::from_raw(
            (i128::from(f.pullback_low.raw()) * i128::from(1000 - p.stop_buffer_permille) / 1000)
                as i64,
        );
        if qty == 0 {
            return;
        }
        let req = Request {
            side: Side::Buy,
            qty,
            purpose: Purpose::Open,
            pricing: Pricing::Collar {
                reference: ask,
                collar_permille: p.collar_permille,
            },
            protect: Some(Protective {
                stop_trigger: stop,
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Ioc,
            reason: reason::ENTRY_HEALTHY_PULLBACK,
        };
        match ctx.submit(i, req) {
            Ok(intent) => {
                self.traces.push(EntryTrace {
                    intent,
                    instrument: i,
                    ts: ctx.now(),
                    features: f,
                    impulse_permille: impulse,
                    params: p,
                    evaluations: self.rules.evaluate(&f, impulse, &p),
                    rules: self.rules_id,
                });
                self.stats.entries += 1;
                self.positions += 1;
                self.phase[i as usize] = Phase::Entering {
                    intent,
                    riding: Riding {
                        trail_permille: p.trail_permille,
                        max_hold_secs: p.max_hold_secs,
                        collar_permille: p.collar_permille,
                    },
                };
            }
            Err(_) => self.stats.entries_failed += 1,
        }
    }

    fn ride(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, px: Px) {
        let Phase::Holding { qty, high, riding } = self.phase[i as usize] else {
            return;
        };
        let high = high.max(px.raw());
        self.phase[i as usize] = Phase::Holding { qty, high, riding };
        let floor = i128::from(high) * i128::from(1000 - riding.trail_permille) / 1000;
        if i128::from(px.raw()) <= floor {
            self.exit(ctx, i, px, reason::EXIT_TRAILING_STOP);
        }
    }

    fn exit(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, last: Px, why: u16) {
        let Phase::Holding { qty, high, riding } = self.phase[i as usize] else {
            return;
        };
        let req = Request {
            side: Side::Sell,
            qty,
            purpose: Purpose::Close,
            pricing: Pricing::Collar {
                reference: last,
                collar_permille: riding.collar_permille,
            },
            protect: None,
            tif: Tif::Ioc,
            reason: why,
        };
        if let Ok(intent) = ctx.submit(i, req) {
            self.stats.exits += 1;
            self.phase[i as usize] = Phase::Exiting {
                intent,
                qty,
                high,
                riding,
            };
        }
    }
}

impl Strategy for MomentumLong {
    fn id(&self) -> StrategyId {
        self.id
    }

    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
        self.tier1.on_event(ev);
        let Event::Trade(t) = ev else { return };
        let i = t.hdr.instrument;
        if i as usize >= self.phase.len() {
            return;
        }
        let now_sec = ctx.now() / NANOS_PER_SEC;
        match self.phase[i as usize] {
            Phase::Idle => self.scan(ctx, i, now_sec),
            Phase::Watching { .. } => self.watch(ctx, i, now_sec),
            Phase::Holding { .. } => self.ride(ctx, i, t.px),
            _ => {}
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, timer: TimerId) {
        let n = self.phase.len() as u32;
        if timer.0 >= n {
            let i = timer.0 - n;
            if self.phase.get(i as usize) == Some(&Phase::Cooldown) {
                self.phase[i as usize] = Phase::Idle;
            }
            return;
        }
        let last = ctx.state(timer.0).and_then(|s| s.last_px);
        if let Some(last) = last {
            self.exit(ctx, timer.0, last, reason::EXIT_MAX_HOLD);
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, u: &OrderUpdate) {
        if !u.state.is_terminal() {
            return;
        }
        for i in 0..self.phase.len() as u32 {
            match self.phase[i as usize] {
                Phase::Entering { intent, riding } if intent == u.intent => {
                    if u.filled_qty > 0 {
                        let high = u.avg_px.map_or(0, |p| p.raw());
                        self.phase[i as usize] = Phase::Holding {
                            qty: u.filled_qty,
                            high,
                            riding,
                        };
                        ctx.set_timer_in(
                            TimerId(i),
                            Nanos::from(riding.max_hold_secs) * NANOS_PER_SEC,
                        );
                    } else {
                        self.stats.entries_failed += 1;
                        self.positions -= 1;
                        self.stop_watching(ctx, i);
                    }
                    return;
                }
                Phase::Exiting {
                    intent,
                    qty,
                    high,
                    riding,
                } if intent == u.intent => {
                    let left = qty - u.filled_qty.min(qty);
                    if left == 0 {
                        self.positions -= 1;
                        ctx.cancel_timer(TimerId(i));
                        self.stop_watching(ctx, i);
                    } else {
                        // Not all sold: hold the rest and try again shortly.
                        self.phase[i as usize] = Phase::Holding {
                            qty: left,
                            high,
                            riding,
                        };
                        ctx.set_timer_in(TimerId(i), NANOS_PER_SEC);
                    }
                    return;
                }
                _ => {}
            }
        }
    }
}
