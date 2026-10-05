//! An example strategy built on the indicator and bar APIs: EMA cross and VWAP
//! reclaim, long only.
//!
//! It exists to show the pattern for new strategies, not to claim an edge:
//! - ask for one-minute bars of each symbol it sees ([`Ctx::track_bars`]);
//! - feed the closed bars to indicators it owns per symbol (two [`Ema`]s, an
//!   [`Atr`]) and the trades to a session [`Vwap`];
//! - decide only when a bar closes ([`Strategy::on_bar`]);
//! - size, price and protect the order from the same numbers.
//!
//! **Entry** (flat, indicators ready, a volume surge on the bar, the fast EMA rising
//! by at least `min_slope_permille`, price inside the filter, positions below the
//! cap) on any enabled trigger: the fast EMA crosses above the slow with the close
//! above the VWAP; the close reclaims the VWAP from below while the fast EMA is above
//! the slow; or simply the fast EMA above the slow and the close above the VWAP on
//! the surge bar.
//! **Protection**: a broker-side stop `atr_stop_mult_permille` / 1000 ATRs under the
//! entry. **Exit**: the fast EMA falls below the slow, or the close falls below the
//! VWAP by more than `exit_below_vwap_permille`. Then a cooldown of whole bars.
//!
//! Every threshold is a field of [`TrendParams`]. Positions are tracked from final
//! order updates, as in the momentum strategy.

use tf_core::{Event, InstrumentId, Px};
use tf_engine::{Atr, Ema, Seed, TfBar, Timeframe, Vwap};

use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, Strategy, TimerId};

pub mod reason {
    pub const ENTRY_EMA_CROSS: u16 = 11;
    pub const ENTRY_VWAP_RECLAIM: u16 = 12;
    pub const EXIT_EMA_CROSS_DOWN: u16 = 13;
    pub const EXIT_BELOW_VWAP: u16 = 14;
    pub const ENTRY_VOLUME_SURGE: u16 = 15;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrendParams {
    pub fast_period: u32,
    pub slow_period: u32,
    pub atr_period: u32,
    /// Stop distance in thousandths of an ATR (2000 = two ATRs).
    pub atr_stop_mult_permille: u32,
    /// The closing bar's volume must be at least this many thousandths of the average
    /// volume of the bars before it (an EMA over `slow_period` bars; 3000 = three times).
    pub min_volume_ratio_permille: u32,
    /// The fast EMA must have risen at least this much (permille) over the bar.
    pub min_slope_permille: i64,
    /// Enter when the fast EMA crosses above the slow with the close above the VWAP.
    pub entry_on_cross: bool,
    /// Enter when the close reclaims the VWAP from below while the fast EMA is above the slow.
    pub entry_on_reclaim: bool,
    /// Enter on a volume surge while the fast EMA is above the slow and the close above the VWAP.
    pub entry_on_surge: bool,
    /// Exit when the close is this far below the VWAP, permille of the VWAP.
    pub exit_below_vwap_permille: u32,
    pub min_price: Px,
    pub max_price: Px,
    pub entry_notional: u128,
    pub max_qty: u32,
    pub collar_permille: u32,
    pub cooldown_bars: u32,
    pub max_positions: u32,
    /// Symbols for which bars are built at once.
    pub max_tracked: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrendParamError(pub &'static str);

impl Default for TrendParams {
    fn default() -> Self {
        TrendParams {
            fast_period: 3,
            slow_period: 6,
            atr_period: 5,
            atr_stop_mult_permille: 2_000,
            min_volume_ratio_permille: 3_000,
            min_slope_permille: 2,
            entry_on_cross: true,
            entry_on_reclaim: true,
            entry_on_surge: true,
            exit_below_vwap_permille: 5,
            min_price: Px::from_cents(200),
            max_price: Px::from_cents(2_000),
            entry_notional: 1_000 * 1_000_000_000,
            max_qty: 1_000,
            collar_permille: 20,
            cooldown_bars: 3,
            max_positions: 3,
            max_tracked: 16,
        }
    }
}

impl TrendParams {
    /// Every parameter as a name and a value, for a run manifest. Destructures the
    /// struct, so adding a field without recording it does not compile.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let TrendParams {
            fast_period,
            slow_period,
            atr_period,
            atr_stop_mult_permille,
            min_volume_ratio_permille,
            min_slope_permille,
            entry_on_cross,
            entry_on_reclaim,
            entry_on_surge,
            exit_below_vwap_permille,
            min_price,
            max_price,
            entry_notional,
            max_qty,
            collar_permille,
            cooldown_bars,
            max_positions,
            max_tracked,
        } = *self;
        vec![
            ("fast_period", fast_period.to_string()),
            ("slow_period", slow_period.to_string()),
            ("atr_period", atr_period.to_string()),
            ("atr_stop_mult_permille", atr_stop_mult_permille.to_string()),
            (
                "min_volume_ratio_permille",
                min_volume_ratio_permille.to_string(),
            ),
            ("min_slope_permille", min_slope_permille.to_string()),
            ("entry_on_cross", u8::from(entry_on_cross).to_string()),
            ("entry_on_reclaim", u8::from(entry_on_reclaim).to_string()),
            ("entry_on_surge", u8::from(entry_on_surge).to_string()),
            (
                "exit_below_vwap_permille",
                exit_below_vwap_permille.to_string(),
            ),
            ("min_price_raw", min_price.raw().to_string()),
            ("max_price_raw", max_price.raw().to_string()),
            ("entry_notional_raw", entry_notional.to_string()),
            ("max_qty", max_qty.to_string()),
            ("collar_permille", collar_permille.to_string()),
            ("cooldown_bars", cooldown_bars.to_string()),
            ("max_positions", max_positions.to_string()),
            ("max_tracked", max_tracked.to_string()),
        ]
    }

    pub fn validate(&self) -> Result<(), TrendParamError> {
        let bad = |m| Err(TrendParamError(m));
        if self.fast_period == 0 || self.fast_period >= self.slow_period {
            return bad("fast_period must be positive and below slow_period");
        }
        if self.atr_period == 0 || self.atr_stop_mult_permille == 0 {
            return bad("atr_period and atr_stop_mult_permille must be positive");
        }
        if self.min_price.raw() <= 0 || self.max_price < self.min_price {
            return bad("price filter must be positive and ordered");
        }
        if self.entry_notional == 0 || self.max_qty == 0 {
            return bad("entry_notional and max_qty must be positive");
        }
        if self.collar_permille > 999 || self.exit_below_vwap_permille >= 1000 {
            return bad("collar and exit_below_vwap must be permille below 1000");
        }
        if self.max_positions == 0 || self.max_tracked == 0 {
            return bad("max_positions and max_tracked must be positive");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Flat,
    Entering {
        intent: IntentId,
    },
    Holding {
        qty: u32,
    },
    Exiting {
        intent: IntentId,
        qty: u32,
    },
    /// Flat again after this many more closed bars.
    Cooldown {
        bars_left: u32,
    },
}

#[derive(Clone, Copy)]
struct Sym {
    fast: Ema,
    slow: Ema,
    atr: Atr,
    vwap: Vwap,
    /// Average volume of the closed bars so far.
    vol_avg: Ema,
    fast_prev: Option<i64>,
    /// Whether the fast EMA was above the slow at the previous bar.
    up_prev: Option<bool>,
    /// Whether the previous close was above the VWAP.
    above_vwap_prev: Option<bool>,
    phase: Phase,
    tracking: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrendStats {
    pub bars: u64,
    pub cross_entries: u64,
    pub reclaim_entries: u64,
    pub surge_entries: u64,
    pub exits: u64,
    pub entries_failed: u64,
    pub tracking_refused: u64,
}

pub struct TrendLong {
    id: StrategyId,
    p: TrendParams,
    syms: Vec<Sym>,
    positions: u32,
    stats: TrendStats,
}

impl TrendLong {
    pub fn new(
        id: StrategyId,
        params: TrendParams,
        id_space: usize,
    ) -> Result<TrendLong, TrendParamError> {
        params.validate()?;
        let sym = Sym {
            fast: Ema::new(params.fast_period, Seed::Sma),
            slow: Ema::new(params.slow_period, Seed::Sma),
            atr: Atr::new(params.atr_period),
            vwap: Vwap::new(),
            vol_avg: Ema::new(params.slow_period, Seed::Sma),
            fast_prev: None,
            up_prev: None,
            above_vwap_prev: None,
            phase: Phase::Flat,
            tracking: false,
        };
        Ok(TrendLong {
            id,
            p: params,
            syms: vec![sym; id_space],
            positions: 0,
            stats: TrendStats::default(),
        })
    }

    pub fn stats(&self) -> TrendStats {
        self.stats
    }

    pub fn params(&self) -> &TrendParams {
        &self.p
    }

    pub fn positions(&self) -> u32 {
        self.positions
    }

    fn enter(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, bar: &TfBar, why: u16) {
        let sym = &mut self.syms[i as usize];
        let reference = ctx
            .state(i)
            .and_then(|s| s.ask)
            .map_or(bar.close, |(ask, _)| ask);
        if reference.raw() <= 0 {
            return;
        }
        let Some(atr) = sym.atr.value() else { return };
        let qty = u32::try_from(self.p.entry_notional / reference.raw() as u128)
            .unwrap_or(u32::MAX)
            .min(self.p.max_qty);
        let dist = i128::from(atr) * i128::from(self.p.atr_stop_mult_permille) / 1000;
        let stop = (i128::from(reference.raw()) - dist).max(1) as i64;
        if qty == 0 || stop >= reference.raw() {
            return;
        }
        let req = Request {
            side: Side::Buy,
            qty,
            purpose: Purpose::Open,
            pricing: Pricing::Collar {
                reference,
                collar_permille: self.p.collar_permille,
            },
            protect: Some(Protective {
                stop_trigger: Px::from_raw(stop),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Ioc,
            reason: why,
        };
        match ctx.submit(i, req) {
            Ok(intent) => {
                self.positions += 1;
                match why {
                    reason::ENTRY_EMA_CROSS => self.stats.cross_entries += 1,
                    reason::ENTRY_VWAP_RECLAIM => self.stats.reclaim_entries += 1,
                    _ => self.stats.surge_entries += 1,
                }
                sym.phase = Phase::Entering { intent };
            }
            Err(_) => self.stats.entries_failed += 1,
        }
    }

    fn exit(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, bar: &TfBar, why: u16) {
        let sym = &mut self.syms[i as usize];
        let Phase::Holding { qty } = sym.phase else {
            return;
        };
        let req = Request {
            side: Side::Sell,
            qty,
            purpose: Purpose::Close,
            pricing: Pricing::Collar {
                reference: bar.close,
                collar_permille: self.p.collar_permille,
            },
            protect: None,
            tif: Tif::Ioc,
            reason: why,
        };
        if let Ok(intent) = ctx.submit(i, req) {
            self.stats.exits += 1;
            sym.phase = Phase::Exiting { intent, qty };
        }
    }
}

impl Strategy for TrendLong {
    fn id(&self) -> StrategyId {
        self.id
    }

    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
        let Event::Trade(t) = ev else { return };
        let i = t.hdr.instrument;
        let Some(sym) = self.syms.get_mut(i as usize) else {
            return;
        };
        sym.vwap.update(t.px.raw(), t.size);
        if !sym.tracking && t.px >= self.p.min_price && t.px <= self.p.max_price {
            // Bars start with the next trade; indicators warm up from there.
            if ctx.track_bars(i).is_ok() {
                sym.tracking = true;
            } else {
                self.stats.tracking_refused += 1;
            }
        }
    }

    fn on_timer(&mut self, _ctx: &mut Ctx<'_>, _timer: TimerId) {}

    fn on_bar(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, tf: Timeframe, bar: &TfBar) {
        if tf != Timeframe::M1 {
            return;
        }
        let bar = *bar;
        if bar.trades == 0 {
            return; // a flat filler bar carries no information
        }
        let close = bar.close.raw();
        let sym = &mut self.syms[i as usize];
        // Compare with the average of the bars before this one, then fold this one in.
        let volume_ok = sym.vol_avg.value().is_some_and(|avg| {
            i128::from(bar.volume) * 1000
                >= i128::from(avg) * i128::from(self.p.min_volume_ratio_permille)
        });
        sym.vol_avg
            .update(i64::try_from(bar.volume).unwrap_or(i64::MAX));
        sym.fast.update(close);
        sym.slow.update(close);
        sym.atr.update_bar(&bar);
        self.stats.bars += 1;

        let (fast, slow) = (sym.fast.value(), sym.slow.value());
        let ready = sym.fast.is_ready() && sym.slow.is_ready() && sym.atr.is_ready();
        let up = match (fast, slow) {
            (Some(f), Some(s)) if ready => Some(f > s),
            _ => None,
        };
        let vwap = sym.vwap.value();
        let above = vwap.map(|v| close > v);
        let slope_ok = match (fast, sym.fast_prev) {
            (Some(f), Some(p)) if p != 0 => {
                i128::from(f - p) * 1000 / i128::from(p) >= i128::from(self.p.min_slope_permille)
            }
            _ => false,
        };
        let (up_prev, above_prev) = (sym.up_prev, sym.above_vwap_prev);
        sym.fast_prev = fast;
        sym.up_prev = up;
        sym.above_vwap_prev = above;

        match sym.phase {
            Phase::Cooldown { bars_left } => {
                sym.phase = if bars_left <= 1 {
                    Phase::Flat
                } else {
                    Phase::Cooldown {
                        bars_left: bars_left - 1,
                    }
                };
            }
            Phase::Flat => {
                let trending = up == Some(true) && above == Some(true);
                let cross = self.p.entry_on_cross && up_prev == Some(false) && trending;
                let reclaim = self.p.entry_on_reclaim && above_prev == Some(false) && trending;
                let surge = self.p.entry_on_surge && trending;
                let eligible = ready
                    && slope_ok
                    && volume_ok
                    && bar.close >= self.p.min_price
                    && bar.close <= self.p.max_price
                    && self.positions < self.p.max_positions;
                if eligible && cross {
                    self.enter(ctx, i, &bar, reason::ENTRY_EMA_CROSS);
                } else if eligible && reclaim {
                    self.enter(ctx, i, &bar, reason::ENTRY_VWAP_RECLAIM);
                } else if eligible && surge {
                    self.enter(ctx, i, &bar, reason::ENTRY_VOLUME_SURGE);
                }
            }
            Phase::Holding { .. } => {
                let below = vwap.is_some_and(|v| {
                    i128::from(close) * 1000
                        < i128::from(v) * i128::from(1000 - self.p.exit_below_vwap_permille)
                });
                if up == Some(false) {
                    self.exit(ctx, i, &bar, reason::EXIT_EMA_CROSS_DOWN);
                } else if below {
                    self.exit(ctx, i, &bar, reason::EXIT_BELOW_VWAP);
                }
            }
            Phase::Entering { .. } | Phase::Exiting { .. } => {}
        }
    }

    fn on_order_update(&mut self, _ctx: &mut Ctx<'_>, u: &OrderUpdate) {
        if !u.state.is_terminal() {
            return;
        }
        for sym in &mut self.syms {
            match sym.phase {
                Phase::Entering { intent } if intent == u.intent => {
                    if u.filled_qty > 0 {
                        sym.phase = Phase::Holding { qty: u.filled_qty };
                    } else {
                        self.stats.entries_failed += 1;
                        self.positions -= 1;
                        sym.phase = Phase::Cooldown {
                            bars_left: self.p.cooldown_bars.max(1),
                        };
                    }
                    return;
                }
                Phase::Exiting { intent, qty } if intent == u.intent => {
                    let left = qty - u.filled_qty.min(qty);
                    if left == 0 {
                        self.positions -= 1;
                        sym.phase = Phase::Cooldown {
                            bars_left: self.p.cooldown_bars.max(1),
                        };
                    } else {
                        // Not all sold: hold the rest; the next bar's exit test tries again.
                        sym.phase = Phase::Holding { qty: left };
                    }
                    return;
                }
                _ => {}
            }
        }
    }
}
