//! The risk gateway: the only path from a strategy's intent to a broker order.
//!
//! A strategy can only *ask* (ADR 0010). [`Gateway::decide`] answers, and an
//! accepted intent becomes an order with an id; anything else is an explicit
//! [`RejectReason`] that is counted and written to an audit log. The gateway owns
//! the position book and working orders it decides from, fed only by fills and
//! closes reported back to it.
//!
//! Checks, in order (a close skips the kill switch, daily loss and size caps,
//! because reducing risk must always be possible):
//! 1. the intent is well formed and the instrument is known;
//! 2. opens only: manual **kill switch**, then **daily loss** (equity change since
//!    the day began, realised plus marked; reaching the limit latches until
//!    [`Gateway::new_day`]);
//! 3. **order rate**: at most N accepted orders in a sliding window of event time;
//! 4. a close must not exceed what is held less closes already working;
//! 5. an open must not go against a position or working open the other way;
//! 6. opens: **order notional**, **position size** (held + working + this) and
//!    **gross notional** over all instruments, each measured at the order's limit;
//! 7. short opens: the **gap rule**. If every short, held or working, including
//!    this one, gapped up by the rule's percentage at once, the loss must not exceed
//!    the rule's fraction of current equity. This is stricter than one short at a
//!    time, and it is what makes size depend on the worst-case gap rather than on a
//!    stop that a halt or squeeze can jump over. Without a rule, shorts are refused.
//!
//! [`Limits`] are fixed when the gateway is built and nothing on the gateway or
//! its inputs changes them, so a strategy or agent holding a `Gateway` handle
//! still cannot loosen them. That is the shape of the API, not a sandbox: the
//! limits come from operator-owned configuration and the process boundary
//! (E09's gateway process) is what keeps agents from editing it.
//!
//! The kill switch can be engaged but not released; releasing it means building
//! a new gateway. Everything is integer arithmetic on event time.

use std::collections::{BTreeMap, VecDeque};

use tf_core::{InstrumentId, Nanos, Px};
use tf_strategy::{Decision, Intent, IntentId, OrderId, Purpose, RejectReason, Side};

/// Hard limits. Notionals are shares x price in raw price units (1e-9 dollars).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    max_order_notional: u128,
    max_position_shares: u32,
    max_gross_notional: u128,
    max_daily_loss: u128,
    max_orders_per_window: u32,
    rate_window_ns: Nanos,
    gap: Option<GapRule>,
}

/// How much a short book may lose to a gap. Equity is what the account starts
/// with; the gateway adds its profit and loss to get current equity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GapRule {
    equity: u128,
    max_loss_ppm: u32,
    gap_permille: u32,
}

impl GapRule {
    /// `max_loss_ppm`: the largest loss as parts per million of equity (1 to
    /// 1,000,000). `gap_permille`: the jump to survive, 1000 = the price doubles.
    pub fn new(equity: u128, max_loss_ppm: u32, gap_permille: u32) -> Result<GapRule, LimitsError> {
        if equity == 0 {
            return Err(LimitsError::Zero("equity"));
        }
        if max_loss_ppm == 0 || max_loss_ppm > 1_000_000 {
            return Err(LimitsError::BadGapRule);
        }
        if gap_permille == 0 {
            return Err(LimitsError::Zero("gap_permille"));
        }
        Ok(GapRule {
            equity,
            max_loss_ppm,
            gap_permille,
        })
    }

    pub fn equity(&self) -> u128 {
        self.equity
    }
    pub fn max_loss_ppm(&self) -> u32 {
        self.max_loss_ppm
    }
    pub fn gap_permille(&self) -> u32 {
        self.gap_permille
    }
}

/// The most shares of a stock at `px` that can be shorted so that a gap of
/// `gap_permille` loses no more than `max_loss_ppm` of `equity`. Rounds down.
/// Sizing with this and checking with the gateway agree exactly.
pub fn max_short_shares(equity: u128, max_loss_ppm: u32, gap_permille: u32, px: Px) -> u32 {
    let per_share = u128::try_from(px.raw()).unwrap_or(0) * u128::from(gap_permille);
    if per_share == 0 {
        return 0;
    }
    let allowed = equity * u128::from(max_loss_ppm) / 1_000_000;
    u32::try_from(allowed * 1000 / per_share).unwrap_or(u32::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitsError {
    /// A gap loss fraction outside (0, 100%].
    BadGapRule,
    /// Every limit must be positive: a zero would silently disable trading or,
    /// worse, be mistaken for "no limit".
    Zero(&'static str),
    /// The gross cap is below the single-order cap, so a maximal order could never pass.
    GrossBelowOrder,
}

impl Limits {
    pub fn new(
        max_order_notional: u128,
        max_position_shares: u32,
        max_gross_notional: u128,
        max_daily_loss: u128,
        max_orders_per_window: u32,
        rate_window_ns: Nanos,
    ) -> Result<Limits, LimitsError> {
        for (name, zero) in [
            ("max_order_notional", max_order_notional == 0),
            ("max_position_shares", max_position_shares == 0),
            ("max_gross_notional", max_gross_notional == 0),
            ("max_daily_loss", max_daily_loss == 0),
            ("max_orders_per_window", max_orders_per_window == 0),
            ("rate_window_ns", rate_window_ns == 0),
        ] {
            if zero {
                return Err(LimitsError::Zero(name));
            }
        }
        if max_gross_notional < max_order_notional {
            return Err(LimitsError::GrossBelowOrder);
        }
        Ok(Limits {
            max_order_notional,
            max_position_shares,
            max_gross_notional,
            max_daily_loss,
            max_orders_per_window,
            rate_window_ns,
            gap: None,
        })
    }

    /// These limits plus a gap rule (consumes `self`: limits are never edited in place).
    pub fn with_gap_rule(mut self, rule: GapRule) -> Limits {
        self.gap = Some(rule);
        self
    }

    pub fn gap_rule(&self) -> Option<&GapRule> {
        self.gap.as_ref()
    }

    /// Every limit as a name and a value, for recording in a run manifest. It
    /// destructures the struct, so a new limit that is not recorded is a compile error.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let Limits {
            max_order_notional,
            max_position_shares,
            max_gross_notional,
            max_daily_loss,
            max_orders_per_window,
            rate_window_ns,
            gap,
        } = *self;
        let mut v = vec![
            ("max_order_notional_raw", max_order_notional.to_string()),
            ("max_position_shares", max_position_shares.to_string()),
            ("max_gross_notional_raw", max_gross_notional.to_string()),
            ("max_daily_loss_raw", max_daily_loss.to_string()),
            ("max_orders_per_window", max_orders_per_window.to_string()),
            ("rate_window_ns", rate_window_ns.to_string()),
        ];
        match gap {
            Some(GapRule {
                equity,
                max_loss_ppm,
                gap_permille,
            }) => {
                v.push(("gap_equity_raw", equity.to_string()));
                v.push(("gap_max_loss_ppm", max_loss_ppm.to_string()));
                v.push(("gap_permille", gap_permille.to_string()));
            }
            None => v.push(("gap_rule", "none".to_owned())),
        }
        v
    }

    pub fn max_order_notional(&self) -> u128 {
        self.max_order_notional
    }
    pub fn max_position_shares(&self) -> u32 {
        self.max_position_shares
    }
    pub fn max_gross_notional(&self) -> u128 {
        self.max_gross_notional
    }
    pub fn max_daily_loss(&self) -> u128 {
        self.max_daily_loss
    }
    pub fn max_orders_per_window(&self) -> u32 {
        self.max_orders_per_window
    }
    pub fn rate_window_ns(&self) -> Nanos {
        self.rate_window_ns
    }
}

/// One line of the audit log: every decision, accepted or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Audit {
    pub ts: Nanos,
    pub intent: IntentId,
    pub outcome: Decision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayError {
    UnknownOrder(OrderId),
    /// A fill for more shares than the order has left, or of zero shares.
    BadFill(OrderId),
}

#[derive(Clone, Copy, Default)]
struct Position {
    qty: i64,
    /// Average cost, raw price units.
    avg: i64,
}

#[derive(Clone, Copy)]
struct Working {
    id: OrderId,
    instrument: InstrumentId,
    side: Side,
    purpose: Purpose,
    remaining: u32,
    limit: i64,
    /// Where a gap would start from: the intent's reference price.
    reference: i64,
}

pub struct Gateway {
    limits: Limits,
    pos: Vec<Position>,
    marks: Vec<i64>,
    working: Vec<Working>,
    next_order: u64,
    realized: i128,
    day_base: i128,
    killed: bool,
    loss_latched: bool,
    recent: VecDeque<Nanos>,
    accepted: u64,
    rejected: BTreeMap<&'static str, u64>,
    audit: Vec<Audit>,
}

/// A stable name for a rejection, used as the metric label.
pub fn reason_name(r: &RejectReason) -> &'static str {
    match r {
        RejectReason::Invalid(_) => "invalid",
        RejectReason::KillSwitch => "kill_switch",
        RejectReason::MaxNotional => "max_notional",
        RejectReason::MaxPosition => "max_position",
        RejectReason::DailyLossLimit => "daily_loss_limit",
        RejectReason::OrderRate => "order_rate",
        RejectReason::NotShortable => "not_shortable",
        RejectReason::ShortSaleRestricted => "short_sale_restricted",
        RejectReason::Halted => "halted",
        RejectReason::OutsideLuldBand => "outside_luld_band",
        RejectReason::SpreadTooWide => "spread_too_wide",
        RejectReason::RunUpTooLarge => "run_up_too_large",
        RejectReason::Broker => "broker",
        RejectReason::OpposingPosition => "opposing_position",
        RejectReason::GapRisk => "gap_risk",
        RejectReason::NothingToClose => "nothing_to_close",
        RejectReason::UnknownInstrument => "unknown_instrument",
        _ => "other",
    }
}

impl Gateway {
    pub fn new(limits: Limits, instruments: usize) -> Gateway {
        Gateway {
            limits,
            pos: vec![Position::default(); instruments],
            marks: vec![0; instruments],
            working: Vec::new(),
            next_order: 0,
            realized: 0,
            day_base: 0,
            killed: false,
            loss_latched: false,
            recent: VecDeque::new(),
            accepted: 0,
            rejected: BTreeMap::new(),
            audit: Vec::new(),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Stop all new exposure, now and until a new gateway is built. Closes still pass.
    pub fn engage_kill_switch(&mut self) {
        self.killed = true;
    }

    pub fn kill_switch_engaged(&self) -> bool {
        self.killed
    }

    /// A new trading day: the daily-loss baseline resets and a loss latch clears.
    /// A manual kill switch stays.
    pub fn new_day(&mut self) {
        self.day_base = self.total_pnl();
        self.loss_latched = false;
        self.recent.clear();
    }

    /// The last price of an instrument, for marking its position.
    pub fn mark(&mut self, instrument: InstrumentId, px: Px) {
        if let Some(m) = self.marks.get_mut(instrument as usize) {
            *m = px.raw();
        }
    }

    /// Decide on an intent at gateway time `now` (event time).
    pub fn decide(&mut self, intent: &Intent, now: Nanos) -> Decision {
        let outcome = match self.check(intent, now) {
            Ok(()) => {
                let id = OrderId(self.next_order);
                self.next_order += 1;
                self.accepted += 1;
                self.recent.push_back(now);
                self.working.push(Working {
                    id,
                    instrument: intent.instrument,
                    side: intent.side,
                    purpose: intent.purpose,
                    remaining: intent.qty,
                    limit: intent.limit_price().raw(),
                    reference: intent.pricing.reference_price().raw(),
                });
                Decision::Accepted(id)
            }
            Err(r) => {
                *self.rejected.entry(reason_name(&r)).or_insert(0) += 1;
                Decision::Rejected(r)
            }
        };
        self.audit.push(Audit {
            ts: now,
            intent: intent.id,
            outcome,
        });
        outcome
    }

    fn check(&mut self, intent: &Intent, now: Nanos) -> Result<(), RejectReason> {
        intent.validate().map_err(RejectReason::Invalid)?;
        let inst = intent.instrument;
        if inst as usize >= self.pos.len() {
            return Err(RejectReason::UnknownInstrument);
        }
        let opening = intent.purpose == Purpose::Open;
        if opening {
            if self.killed {
                return Err(RejectReason::KillSwitch);
            }
            if self.loss_latched || self.daily_loss() >= self.limits.max_daily_loss {
                self.loss_latched = true;
                return Err(RejectReason::DailyLossLimit);
            }
        }
        // Sliding window of event time: an order exactly `window` old has left it.
        while let Some(&t) = self.recent.front() {
            if t.saturating_add(self.limits.rate_window_ns) <= now {
                self.recent.pop_front();
            } else {
                break;
            }
        }
        if self.recent.len() >= self.limits.max_orders_per_window as usize {
            return Err(RejectReason::OrderRate);
        }
        let held = self.pos[inst as usize].qty;
        if !opening {
            // A sell closes a long, a buy covers a short.
            let closable = if intent.side.is_buy() { -held } else { held }.max(0);
            let committed: i64 = self
                .working
                .iter()
                .filter(|w| {
                    w.instrument == inst
                        && w.purpose == Purpose::Close
                        && w.side.is_buy() == intent.side.is_buy()
                })
                .map(|w| i64::from(w.remaining))
                .sum();
            return if i64::from(intent.qty) <= closable - committed {
                Ok(())
            } else {
                Err(RejectReason::NothingToClose)
            };
        }
        let long = intent.side.is_buy();
        let working_open = |want_long: bool| -> i64 {
            self.working
                .iter()
                .filter(|w| {
                    w.instrument == inst
                        && w.purpose == Purpose::Open
                        && w.side.is_buy() == want_long
                })
                .map(|w| i64::from(w.remaining))
                .sum()
        };
        let against = if long { held < 0 } else { held > 0 };
        if against || working_open(!long) > 0 {
            return Err(RejectReason::OpposingPosition);
        }
        let notional = intent.notional_at_limit();
        if notional > self.limits.max_order_notional {
            return Err(RejectReason::MaxNotional);
        }
        let same = held.abs() + working_open(long) + i64::from(intent.qty);
        if same > i64::from(self.limits.max_position_shares) {
            return Err(RejectReason::MaxPosition);
        }
        if self.gross_notional() + notional > self.limits.max_gross_notional {
            return Err(RejectReason::MaxNotional);
        }
        if intent.side == Side::SellShort {
            self.check_gap(intent)?;
        }
        Ok(())
    }

    /// The gap rule: all shorts gapping at once, this one included, must fit.
    fn check_gap(&self, intent: &Intent) -> Result<(), RejectReason> {
        let rule = self.limits.gap.ok_or(RejectReason::GapRisk)?;
        let equity = i128::try_from(rule.equity).unwrap_or(i128::MAX) + self.total_pnl();
        let Ok(equity) = u128::try_from(equity) else {
            return Err(RejectReason::GapRisk);
        };
        let held: u128 = self
            .pos
            .iter()
            .zip(&self.marks)
            .filter(|(p, _)| p.qty < 0)
            .map(|(p, m)| {
                u128::from(p.qty.unsigned_abs()) * u128::try_from(p.avg.max(*m)).unwrap_or(0)
            })
            .sum();
        let working: u128 = self
            .working
            .iter()
            .filter(|w| w.purpose == Purpose::Open && !w.side.is_buy())
            .map(|w| u128::from(w.remaining) * u128::try_from(w.reference).unwrap_or(0))
            .sum();
        let this = u128::from(intent.qty)
            * u128::try_from(intent.pricing.reference_price().raw()).unwrap_or(0);
        // Loss rounds up and the allowance rounds down, so the rule is never looser than stated.
        let loss = ((held + working + this) * u128::from(rule.gap_permille)).div_ceil(1000);
        let allowed = equity * u128::from(rule.max_loss_ppm) / 1_000_000;
        if loss > allowed {
            return Err(RejectReason::GapRisk);
        }
        Ok(())
    }

    /// Exposure over every instrument: positions at the higher of cost and mark,
    /// plus working opens at their limits.
    fn gross_notional(&self) -> u128 {
        let held: u128 = self
            .pos
            .iter()
            .zip(&self.marks)
            .map(|(p, &m)| {
                u128::from(p.qty.unsigned_abs()) * u128::try_from(p.avg.max(m)).unwrap_or(0)
            })
            .sum();
        let working: u128 = self
            .working
            .iter()
            .filter(|w| w.purpose == Purpose::Open)
            .map(|w| u128::from(w.remaining) * u128::try_from(w.limit).unwrap_or(0))
            .sum();
        held + working
    }

    fn unrealized(&self) -> i128 {
        self.pos
            .iter()
            .zip(&self.marks)
            .filter(|(p, m)| p.qty != 0 && **m > 0)
            .map(|(p, &m)| i128::from(m - p.avg) * i128::from(p.qty))
            .sum()
    }

    fn total_pnl(&self) -> i128 {
        self.realized + self.unrealized()
    }

    /// Profit and loss since the day began, raw units (negative = loss).
    pub fn daily_pnl(&self) -> i128 {
        self.total_pnl() - self.day_base
    }

    fn daily_loss(&self) -> u128 {
        u128::try_from(-self.daily_pnl()).unwrap_or(0)
    }

    /// A fill reported by the broker (or the simulator).
    pub fn on_fill(&mut self, order: OrderId, qty: u32, px: Px) -> Result<(), GatewayError> {
        let i = self
            .working
            .iter()
            .position(|w| w.id == order)
            .ok_or(GatewayError::UnknownOrder(order))?;
        let w = &mut self.working[i];
        if qty == 0 || qty > w.remaining || px.raw() <= 0 {
            return Err(GatewayError::BadFill(order));
        }
        w.remaining -= qty;
        let (inst, buy) = (w.instrument, w.side.is_buy());
        if w.remaining == 0 {
            self.working.remove(i);
        }
        let signed = if buy { i64::from(qty) } else { -i64::from(qty) };
        self.realized += apply(&mut self.pos[inst as usize], signed, px.raw());
        Ok(())
    }

    /// An order finished without filling the rest (cancelled, expired, rejected).
    pub fn on_closed(&mut self, order: OrderId) -> Result<(), GatewayError> {
        let i = self
            .working
            .iter()
            .position(|w| w.id == order)
            .ok_or(GatewayError::UnknownOrder(order))?;
        self.working.remove(i);
        Ok(())
    }

    /// Signed position in shares.
    pub fn position(&self, instrument: InstrumentId) -> i64 {
        self.pos.get(instrument as usize).map_or(0, |p| p.qty)
    }

    pub fn working_orders(&self) -> usize {
        self.working.len()
    }

    pub fn accepted_count(&self) -> u64 {
        self.accepted
    }

    /// Rejections by [`reason_name`], in name order.
    pub fn rejection_counts(&self) -> &BTreeMap<&'static str, u64> {
        &self.rejected
    }

    pub fn rejected_count(&self, name: &str) -> u64 {
        self.rejected.get(name).copied().unwrap_or(0)
    }

    /// The decisions made since the last call, for the log sink.
    pub fn drain_audit(&mut self) -> Vec<Audit> {
        std::mem::take(&mut self.audit)
    }
}

/// Apply a signed fill to a position; returns realised profit in raw units.
fn apply(p: &mut Position, signed: i64, px: i64) -> i128 {
    if p.qty == 0 || p.qty.signum() == signed.signum() {
        let (old, add) = (i128::from(p.qty.abs()), i128::from(signed.abs()));
        let total = old + add;
        p.avg = ((old * i128::from(p.avg) + add * i128::from(px) + total / 2) / total) as i64;
        p.qty += signed;
        return 0;
    }
    let closing = p.qty.abs().min(signed.abs());
    let realized = i128::from(px - p.avg) * i128::from(closing) * i128::from(p.qty.signum());
    let before = p.qty.signum();
    p.qty += signed;
    if p.qty == 0 {
        p.avg = 0;
    } else if p.qty.signum() != before {
        p.avg = px; // flipped through zero: the rest is a new position at this price
    }
    realized
}

#[cfg(test)]
mod tests;
