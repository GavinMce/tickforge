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
//! Every threshold is a field of [`MomentumParams`]; there are no numeric
//! constants in the logic. Defaults were chosen on the synthetic runner
//! scenarios and are a starting point for real data, not a result. In
//! particular `min_higher_lows` matters: with 0 the strategy buys in the middle of
//! the pullback and is stopped out; with 1 it waits for the bounce.
//!
//! Positions are tracked from order updates (terminal states only), so a partly
//! filled IOC entry holds what it got. Decisions that depend on features are made
//! at most once per second per symbol.

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_engine::{PromoteError, Quote1, Tier1};

use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, Strategy, TimerId};

/// Reasons recorded on intents, for the audit trail.
pub mod reason {
    pub const ENTRY_HEALTHY_PULLBACK: u16 = 1;
    pub const EXIT_TRAILING_STOP: u16 = 2;
    pub const EXIT_MAX_HOLD: u16 = 3;
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
    },
    Holding {
        qty: u32,
        high: i64,
    },
    Exiting {
        intent: IntentId,
        qty: u32,
        high: i64,
    },
    Cooldown,
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
}

impl MomentumLong {
    pub fn new(
        id: StrategyId,
        params: MomentumParams,
        id_space: usize,
    ) -> Result<MomentumLong, ParamError> {
        params.validate()?;
        Ok(MomentumLong {
            id,
            p: params,
            tier1: Tier1::new(id_space, params.max_watched as usize),
            phase: vec![Phase::Idle; id_space],
            positions: 0,
            stats: MomentumStats::default(),
        })
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

    fn stop_watching(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId) {
        self.tier1.demote(i);
        self.phase[i as usize] = Phase::Cooldown;
        let t = self.cooldown_timer(i);
        ctx.set_timer_in(t, Nanos::from(self.p.cooldown_secs) * NANOS_PER_SEC);
    }

    fn scan(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, now_sec: u64) {
        let Some(w) = ctx.windows(i) else { return };
        let Some(change) = w.price_change_permille(self.p.spike_secs as usize) else {
            return;
        };
        if change < self.p.spike_permille
            || w.volume(self.p.spike_secs as usize) < self.p.spike_min_volume
        {
            return;
        }
        let Some(st) = ctx.state(i) else { return };
        let (Some(last), Some((ask, _))) = (st.last_px, st.ask) else {
            return;
        };
        if last < self.p.min_price || last > self.p.max_price {
            return;
        }
        let spread = st.spread().unwrap_or(i64::MAX);
        if ask.raw() <= 0
            || i128::from(spread) * 1000
                > i128::from(ask.raw()) * i128::from(self.p.max_spread_permille)
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
        if impulse < i128::from(self.p.min_impulse_permille)
            || f.secs_since_high < self.p.min_pullback_secs
        {
            if f.secs_since_high > self.p.max_pullback_secs {
                self.stats.rejected_too_old += 1;
                self.stop_watching(ctx, i);
            }
            return;
        }
        if f.secs_since_high > self.p.max_pullback_secs {
            self.stats.rejected_too_old += 1;
            self.stop_watching(ctx, i);
            return;
        }
        let dangerous = f.depth_permille > self.p.max_depth_permille
            || f.volume_ratio_permille
                .is_some_and(|r| r > self.p.max_volume_ratio_permille);
        if dangerous {
            self.stats.rejected_dangerous += 1;
            self.stop_watching(ctx, i);
            return;
        }
        let healthy = f.depth_permille >= self.p.min_depth_permille
            && f.retrace_now_permille <= self.p.max_retrace_now_permille
            && f.volume_ratio_permille.is_some()
            && f.higher_lows >= self.p.min_higher_lows
            && f.bid_support_permille
                .is_some_and(|b| b >= self.p.min_bid_support_permille);
        if !healthy || self.positions >= self.p.max_positions {
            return;
        }
        let Some(Quote1 { ask, .. }) = sym.quote_ago(0) else {
            return;
        };
        if ask.raw() <= 0 {
            return;
        }
        let qty = u32::try_from(self.p.entry_notional / ask.raw() as u128)
            .unwrap_or(u32::MAX)
            .min(self.p.max_qty);
        let stop = Px::from_raw(
            (i128::from(f.pullback_low.raw()) * i128::from(1000 - self.p.stop_buffer_permille)
                / 1000) as i64,
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
                collar_permille: self.p.collar_permille,
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
                self.stats.entries += 1;
                self.positions += 1;
                self.phase[i as usize] = Phase::Entering { intent };
            }
            Err(_) => self.stats.entries_failed += 1,
        }
    }

    fn ride(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, px: Px) {
        let Phase::Holding { qty, high } = self.phase[i as usize] else {
            return;
        };
        let high = high.max(px.raw());
        self.phase[i as usize] = Phase::Holding { qty, high };
        let floor = i128::from(high) * i128::from(1000 - self.p.trail_permille) / 1000;
        if i128::from(px.raw()) <= floor {
            self.exit(ctx, i, px, reason::EXIT_TRAILING_STOP);
        }
    }

    fn exit(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, last: Px, why: u16) {
        let Phase::Holding { qty, high } = self.phase[i as usize] else {
            return;
        };
        let req = Request {
            side: Side::Sell,
            qty,
            purpose: Purpose::Close,
            pricing: Pricing::Collar {
                reference: last,
                collar_permille: self.p.collar_permille,
            },
            protect: None,
            tif: Tif::Ioc,
            reason: why,
        };
        if let Ok(intent) = ctx.submit(i, req) {
            self.stats.exits += 1;
            self.phase[i as usize] = Phase::Exiting { intent, qty, high };
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
                Phase::Entering { intent } if intent == u.intent => {
                    if u.filled_qty > 0 {
                        let high = u.avg_px.map_or(0, |p| p.raw());
                        self.phase[i as usize] = Phase::Holding {
                            qty: u.filled_qty,
                            high,
                        };
                        ctx.set_timer_in(
                            TimerId(i),
                            Nanos::from(self.p.max_hold_secs) * NANOS_PER_SEC,
                        );
                    } else {
                        self.stats.entries_failed += 1;
                        self.positions -= 1;
                        self.stop_watching(ctx, i);
                    }
                    return;
                }
                Phase::Exiting { intent, qty, high } if intent == u.intent => {
                    let left = qty - u.filled_qty.min(qty);
                    if left == 0 {
                        self.positions -= 1;
                        ctx.cancel_timer(TimerId(i));
                        self.stop_watching(ctx, i);
                    } else {
                        // Not all sold: hold the rest and try again shortly.
                        self.phase[i as usize] = Phase::Holding { qty: left, high };
                        ctx.set_timer_in(TimerId(i), NANOS_PER_SEC);
                    }
                    return;
                }
                _ => {}
            }
        }
    }
}
