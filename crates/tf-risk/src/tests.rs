use tf_core::{Nanos, Px};
use tf_strategy::{
    Decision, Intent, IntentError, IntentId, OrderId, Pricing, Protective, Purpose, RejectReason,
    Side, StrategyId, Tif,
};
use tf_synth::SplitMix64;

use super::*;

const SEC: Nanos = 1_000_000_000;
/// One dollar in raw price units.
const D: u128 = 1_000_000_000;

fn px(cents: i64) -> Px {
    Px::from_cents(cents)
}

/// $5,000 per order, 500 shares, $8,000 gross, $100 daily loss, 3 orders / 10 s.
fn limits() -> Limits {
    Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 3, 10 * SEC)
        .unwrap()
        .with_gap_rule(generous())
}

/// A gap rule that does not bind: 100% of $1M equity, for a price doubling.
fn generous() -> GapRule {
    GapRule::new(1_000_000 * D, 1_000_000, 1000).unwrap()
}

fn gw() -> Gateway {
    Gateway::new(limits(), 3)
}

fn intent(seq: u64, inst: u32, side: Side, purpose: Purpose, qty: u32, cents: i64) -> Intent {
    let long = side == Side::Buy;
    let open = purpose == Purpose::Open;
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq,
        },
        instrument: inst,
        side,
        qty,
        purpose,
        pricing: Pricing::Limit(px(cents)),
        protect: open.then(|| Protective {
            stop_trigger: px(if long { cents / 2 } else { cents * 2 }),
            stop_limit: None,
            take_profit: None,
        }),
        tif: Tif::Day,
        ts: 0,
        reason: 0,
    }
}

fn open(seq: u64, qty: u32, cents: i64) -> Intent {
    intent(seq, 0, Side::Buy, Purpose::Open, qty, cents)
}

fn close(seq: u64, qty: u32, cents: i64) -> Intent {
    intent(seq, 0, Side::Sell, Purpose::Close, qty, cents)
}

fn rejected(d: Decision) -> RejectReason {
    match d {
        Decision::Rejected(r) => r,
        Decision::Accepted(id) => panic!("expected a rejection, got order {id:?}"),
    }
}

fn accepted(d: Decision) -> OrderId {
    match d {
        Decision::Accepted(id) => id,
        Decision::Rejected(r) => panic!("expected acceptance, got {r:?}"),
    }
}

/// Take a long of `qty` at `cents` through the gateway and fill it.
fn long_position(g: &mut Gateway, seq: u64, qty: u32, cents: i64, t: Nanos) -> OrderId {
    let id = accepted(g.decide(&open(seq, qty, cents), t));
    g.on_fill(id, qty, px(cents)).unwrap();
    id
}

// ---- limits ----

#[test]
fn limits_must_all_be_positive_and_consistent() {
    let ok = |i: usize| {
        let mut v: [u128; 6] = [5_000 * D, 500, 8_000 * D, 100 * D, 3, u128::from(10 * SEC)];
        v[i] = 0;
        Limits::new(v[0], v[1] as u32, v[2], v[3], v[4] as u32, v[5] as Nanos)
    };
    let names = [
        "max_order_notional",
        "max_position_shares",
        "max_gross_notional",
        "max_daily_loss",
        "max_orders_per_window",
        "rate_window_ns",
    ];
    for (i, n) in names.iter().enumerate() {
        assert_eq!(ok(i), Err(LimitsError::Zero(n)), "zero {n}");
    }
    assert_eq!(
        Limits::new(9 * D, 1, 8 * D, 1, 1, 1),
        Err(LimitsError::GrossBelowOrder)
    );
    assert_eq!(limits().max_position_shares(), 500);
}

// ---- decisions ----

#[test]
fn accepted_orders_get_dense_ids_and_every_decision_is_audited() {
    let mut g = gw();
    let a = accepted(g.decide(&open(0, 10, 1000), 5));
    let mut bad = open(1, 10, 1000);
    bad.protect = None;
    let r = rejected(g.decide(&bad, 6));
    let b = accepted(g.decide(&open(2, 10, 1000), 7));
    assert_eq!((a, b), (OrderId(0), OrderId(1)));
    assert_eq!(r, RejectReason::Invalid(IntentError::MissingProtection));
    let log = g.drain_audit();
    assert_eq!(
        log.iter()
            .map(|e| (e.ts, e.intent.seq, e.outcome))
            .collect::<Vec<_>>(),
        [
            (5, 0, Decision::Accepted(a)),
            (6, 1, Decision::Rejected(r)),
            (7, 2, Decision::Accepted(b)),
        ]
    );
    assert!(g.drain_audit().is_empty());
    assert_eq!(g.accepted_count(), 2);
    assert_eq!(g.rejected_count("invalid"), 1);
}

#[test]
fn an_unknown_instrument_is_refused() {
    let mut g = gw();
    let i = intent(0, 7, Side::Buy, Purpose::Open, 1, 1000);
    assert_eq!(rejected(g.decide(&i, 0)), RejectReason::UnknownInstrument);
}

#[test]
fn the_order_notional_cap_is_inclusive() {
    let mut g = gw();
    // 500 x $10 = $5,000 exactly.
    assert!(matches!(
        g.decide(&open(0, 500, 1000), 0),
        Decision::Accepted(_)
    ));
    let mut g = gw();
    assert_eq!(
        rejected(g.decide(&open(0, 500, 1001), 0)),
        RejectReason::MaxNotional
    );
    assert_eq!(g.rejected_count("max_notional"), 1);
}

#[test]
fn position_cap_counts_held_working_and_the_new_order() {
    let mut g = gw();
    long_position(&mut g, 0, 200, 100, 0); // 200 held ($2)
    accepted(g.decide(&open(1, 200, 100), 1)); // 200 working
    assert!(
        matches!(g.decide(&open(2, 100, 100), 2), Decision::Accepted(_)),
        "500 total is allowed"
    );
    assert_eq!(
        rejected(g.decide(&open(3, 1, 100), 3)),
        RejectReason::OrderRate,
        "rate trips first here"
    );
    let mut g = Gateway::new(
        Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 99, 10 * SEC).unwrap(),
        3,
    );
    long_position(&mut g, 0, 200, 100, 0);
    accepted(g.decide(&open(1, 200, 100), 1));
    accepted(g.decide(&open(2, 100, 100), 2));
    assert_eq!(
        rejected(g.decide(&open(3, 1, 100), 3)),
        RejectReason::MaxPosition
    );
}

#[test]
fn gross_notional_spans_instruments_and_includes_working_opens() {
    let mut g = Gateway::new(
        Limits::new(5_000 * D, 10_000, 8_000 * D, 10_000 * D, 99, 10 * SEC).unwrap(),
        3,
    );
    for (seq, inst) in [(0, 0), (1, 1)] {
        let id = accepted(g.decide(&intent(seq, inst, Side::Buy, Purpose::Open, 400, 1000), 0));
        g.on_fill(id, 400, px(1000)).unwrap(); // $4,000 each
    }
    // $8,000 held: nothing more fits.
    assert_eq!(
        rejected(g.decide(&intent(2, 2, Side::Buy, Purpose::Open, 1, 1000), 0)),
        RejectReason::MaxNotional
    );
    // A position is measured at the higher of cost and mark.
    let mut g = Gateway::new(
        Limits::new(5_000 * D, 10_000, 8_000 * D, 10_000 * D, 99, 10 * SEC).unwrap(),
        3,
    );
    long_position(&mut g, 0, 400, 1000, 0); // $4,000 at cost
    g.mark(0, px(1500)); // now $6,000
    assert_eq!(
        rejected(g.decide(&intent(1, 1, Side::Buy, Purpose::Open, 250, 1000), 0)),
        RejectReason::MaxNotional
    );
    assert!(matches!(
        g.decide(&intent(2, 1, Side::Buy, Purpose::Open, 200, 1000), 0),
        Decision::Accepted(_)
    ));
}

#[test]
fn unfilled_working_opens_count_toward_gross_notional() {
    let mut g = Gateway::new(
        Limits::new(5_000 * D, 10_000, 8_000 * D, 10_000 * D, 99, 10 * SEC).unwrap(),
        3,
    );
    accepted(g.decide(&intent(0, 0, Side::Buy, Purpose::Open, 400, 1000), 0)); // $4,000, unfilled
    accepted(g.decide(&intent(1, 1, Side::Buy, Purpose::Open, 400, 1000), 0)); // $4,000, unfilled
    assert_eq!(
        rejected(g.decide(&intent(2, 2, Side::Buy, Purpose::Open, 1, 1000), 0)),
        RejectReason::MaxNotional
    );
}

#[test]
fn opens_against_an_existing_or_working_position_are_refused() {
    let mut g = gw();
    long_position(&mut g, 0, 10, 1000, 0);
    let short = intent(1, 0, Side::SellShort, Purpose::Open, 10, 1000);
    assert_eq!(
        rejected(g.decide(&short, 1)),
        RejectReason::OpposingPosition
    );
    // A working long also blocks a short.
    let mut g = gw();
    accepted(g.decide(&open(0, 10, 1000), 0));
    assert_eq!(
        rejected(g.decide(&short, 1)),
        RejectReason::OpposingPosition
    );
    // And the other way round.
    let mut g = gw();
    let id = accepted(g.decide(&short, 0));
    g.on_fill(id, 10, px(1000)).unwrap();
    assert_eq!(g.position(0), -10);
    assert_eq!(
        rejected(g.decide(&open(1, 10, 1000), 1)),
        RejectReason::OpposingPosition
    );
}

#[test]
fn closes_cannot_exceed_what_is_held_less_closes_already_working() {
    let mut g = gw();
    assert_eq!(
        rejected(g.decide(&close(0, 1, 1000), 0)),
        RejectReason::NothingToClose,
        "flat"
    );
    long_position(&mut g, 1, 100, 1000, 1);
    assert_eq!(
        rejected(g.decide(&close(2, 101, 1000), 2)),
        RejectReason::NothingToClose
    );
    accepted(g.decide(&close(3, 60, 1000), 3));
    assert_eq!(
        rejected(g.decide(&close(4, 41, 1000), 4)),
        RejectReason::NothingToClose,
        "60 are already on their way out"
    );
    accepted(g.decide(&close(5, 40, 1000), 5));
    // Covering a short is a Buy close; a Sell close does not apply to it.
    let mut g = gw();
    let id = accepted(g.decide(&intent(0, 0, Side::SellShort, Purpose::Open, 50, 1000), 0));
    g.on_fill(id, 50, px(1000)).unwrap();
    assert_eq!(
        rejected(g.decide(&close(1, 10, 1000), 1)),
        RejectReason::NothingToClose,
        "selling does not cover a short"
    );
    accepted(g.decide(&intent(2, 0, Side::Buy, Purpose::Close, 50, 1000), 2));
}

#[test]
fn a_cancelled_order_releases_what_it_was_holding() {
    let mut g = gw();
    long_position(&mut g, 0, 100, 1000, 0);
    let c = accepted(g.decide(&close(1, 100, 1000), 1));
    assert_eq!(
        rejected(g.decide(&close(2, 1, 1000), 2)),
        RejectReason::NothingToClose
    );
    g.on_closed(c).unwrap();
    accepted(g.decide(&close(3, 100, 1000), 3));
    assert_eq!(
        g.on_closed(c),
        Err(GatewayError::UnknownOrder(c)),
        "already gone"
    );
}

// ---- kill switch and daily loss ----

#[test]
fn the_kill_switch_stops_opens_but_never_closes_and_new_day_does_not_release_it() {
    let mut g = gw();
    long_position(&mut g, 0, 100, 1000, 0);
    g.engage_kill_switch();
    assert!(g.kill_switch_engaged());
    assert_eq!(
        rejected(g.decide(&open(1, 1, 1000), 1)),
        RejectReason::KillSwitch
    );
    accepted(g.decide(&close(2, 100, 1000), 2));
    g.new_day();
    assert_eq!(
        rejected(g.decide(&open(3, 1, 1000), 3)),
        RejectReason::KillSwitch
    );
    assert_eq!(g.rejected_count("kill_switch"), 2);
}

#[test]
fn daily_loss_trips_at_the_limit_and_latches_until_the_next_day() {
    let mut g = Gateway::new(
        Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 99, 10 * SEC).unwrap(),
        3,
    );
    long_position(&mut g, 0, 100, 1000, 0); // $1,000 at $10
    g.mark(0, px(901)); // -$99: under the limit
    assert_eq!(g.daily_pnl(), -99 * D as i128);
    accepted(g.decide(&open(1, 1, 1000), 1));
    g.mark(0, px(900)); // -$100: at the limit
    assert_eq!(
        rejected(g.decide(&open(2, 1, 1000), 2)),
        RejectReason::DailyLossLimit
    );
    // Recovering does not reopen the day.
    g.mark(0, px(1100));
    assert_eq!(
        rejected(g.decide(&open(3, 1, 1000), 3)),
        RejectReason::DailyLossLimit
    );
    // But the position can still be closed.
    accepted(g.decide(&close(4, 100, 1000), 4));
    // A new day measures from where it stands now.
    g.new_day();
    assert_eq!(g.daily_pnl(), 0);
    accepted(g.decide(&open(5, 1, 1000), 5));
}

#[test]
fn realised_losses_count_and_a_short_loses_when_the_price_rises() {
    let mut g = Gateway::new(
        Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 99, 10 * SEC)
            .unwrap()
            .with_gap_rule(generous()),
        3,
    );
    let id = accepted(g.decide(&intent(0, 0, Side::SellShort, Purpose::Open, 100, 1000), 0));
    g.on_fill(id, 100, px(1000)).unwrap();
    g.mark(0, px(1050));
    assert_eq!(
        g.daily_pnl(),
        -50 * D as i128,
        "short 100 from $10 to $10.50"
    );
    let c = accepted(g.decide(&intent(1, 0, Side::Buy, Purpose::Close, 100, 1100), 1));
    g.on_fill(c, 100, px(1100)).unwrap(); // covers at $11: realised -$100
    assert_eq!((g.position(0), g.daily_pnl()), (0, -100 * D as i128));
    assert_eq!(
        rejected(g.decide(&open(2, 1, 1000), 2)),
        RejectReason::DailyLossLimit
    );
}

// ---- rate ----

#[test]
fn order_rate_is_a_sliding_window_of_event_time_and_rejections_do_not_count() {
    let mut g = gw(); // 3 orders per 10 s
    for (i, t) in [0, 2 * SEC, 4 * SEC].into_iter().enumerate() {
        accepted(g.decide(&open(i as u64, 1, 1000), t));
    }
    assert_eq!(
        rejected(g.decide(&open(3, 1, 1000), 9 * SEC)),
        RejectReason::OrderRate
    );
    assert_eq!(
        rejected(g.decide(&open(4, 1, 1000), 10 * SEC - 1)),
        RejectReason::OrderRate
    );
    // Exactly one window after the first, it has left.
    accepted(g.decide(&open(5, 1, 1000), 10 * SEC));
    assert_eq!(
        rejected(g.decide(&open(6, 1, 1000), 10 * SEC)),
        RejectReason::OrderRate,
        "2 s, 4 s, 10 s are in"
    );
    accepted(g.decide(&open(7, 1, 1000), 12 * SEC));
    assert_eq!(g.rejected_count("order_rate"), 3);
}

// ---- books ----

#[test]
fn fills_are_validated() {
    let mut g = gw();
    let id = accepted(g.decide(&open(0, 10, 1000), 0));
    assert_eq!(
        g.on_fill(OrderId(99), 1, px(1000)),
        Err(GatewayError::UnknownOrder(OrderId(99)))
    );
    assert_eq!(g.on_fill(id, 0, px(1000)), Err(GatewayError::BadFill(id)));
    assert_eq!(g.on_fill(id, 11, px(1000)), Err(GatewayError::BadFill(id)));
    g.on_fill(id, 4, px(1000)).unwrap();
    g.on_fill(id, 6, px(1000)).unwrap();
    assert_eq!((g.position(0), g.working_orders()), (10, 0));
}

#[test]
fn position_averaging_realisation_and_flips() {
    let mut p = Position::default();
    assert_eq!(apply(&mut p, 100, 1000), 0);
    assert_eq!(apply(&mut p, 100, 1200), 0);
    assert_eq!((p.qty, p.avg), (200, 1100)); // average cost
    // Sell 50 at 1300: +200 x 50.
    assert_eq!(apply(&mut p, -50, 1300), 200 * 50);
    assert_eq!((p.qty, p.avg), (150, 1100));
    // Sell 200 at 1000: closes 150 at -100 each, flips short 50 at 1000.
    assert_eq!(apply(&mut p, -200, 1000), -100 * 150);
    assert_eq!((p.qty, p.avg), (-50, 1000));
    // Cover 50 at 900: +100 x 50, flat.
    assert_eq!(apply(&mut p, 50, 900), 100 * 50);
    assert_eq!((p.qty, p.avg), (0, 0));
}

// ---- the invariants, under random traffic ----

#[test]
fn whatever_is_asked_the_book_stays_inside_the_limits() {
    let lim = Limits::new(2_000 * D, 300, 3_000 * D, 10_000_000 * D, 40, 5 * SEC)
        .unwrap()
        .with_gap_rule(generous());
    for seed in 0..20u64 {
        let mut rng = SplitMix64::new(seed);
        let mut g = Gateway::new(lim, 3);
        let mut live: Vec<(OrderId, u32)> = Vec::new();
        let mut seq = 0u64;
        let mut now: Nanos = 0;
        for _ in 0..400 {
            now += rng.next_u64() % SEC;
            let inst = (rng.next_u64() % 3) as u32;
            let qty = 1 + (rng.next_u64() % 250) as u32;
            let cents = 100 + (rng.next_u64() % 2000) as i64;
            let (side, purpose) = match rng.next_u64() % 4 {
                0 => (Side::Buy, Purpose::Open),
                1 => (Side::SellShort, Purpose::Open),
                2 => (Side::Sell, Purpose::Close),
                _ => (Side::Buy, Purpose::Close),
            };
            seq += 1;
            let i = intent(seq, inst, side, purpose, qty, cents);
            if let Decision::Accepted(id) = g.decide(&i, now) {
                live.push((id, qty));
            }
            // Randomly fill part of, or finish, some working order.
            if !live.is_empty() && rng.next_u64() % 2 == 0 {
                let k = (rng.next_u64() % live.len() as u64) as usize;
                let (id, left) = live[k];
                if rng.next_u64() % 4 == 0 {
                    g.on_closed(id).unwrap();
                    live.remove(k);
                } else {
                    let f = 1 + (rng.next_u64() % u64::from(left)) as u32;
                    g.on_fill(id, f, px(cents)).unwrap();
                    if f == left {
                        live.remove(k);
                    } else {
                        live[k].1 = left - f;
                    }
                }
            }
            for inst in 0..3 {
                assert!(
                    g.position(inst).unsigned_abs() <= 300,
                    "seed {seed}: position {}",
                    g.position(inst)
                );
            }
            assert!(
                g.gross_notional() <= 3_000 * D + 3 * 300 * 2_100 * 10_000_000,
                "sanity bound"
            );
        }
        assert!(
            g.accepted_count() > 20 && g.rejected.values().sum::<u64>() > 20,
            "seed {seed} exercised both paths"
        );
    }
}

// ---- the gap rule ----

/// $100,000 of equity; a price doubling may cost at most 2% of it.
fn gap_gw() -> Gateway {
    let rule = GapRule::new(100_000 * D, 20_000, 1000).unwrap();
    let l = Limits::new(
        50_000 * D,
        100_000,
        500_000 * D,
        10_000_000 * D,
        99,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(rule);
    Gateway::new(l, 3)
}

fn short(seq: u64, inst: u32, qty: u32, cents: i64) -> Intent {
    intent(seq, inst, Side::SellShort, Purpose::Open, qty, cents)
}

#[test]
fn rule_validation() {
    assert_eq!(GapRule::new(0, 1, 1), Err(LimitsError::Zero("equity")));
    assert_eq!(GapRule::new(1, 0, 1), Err(LimitsError::BadGapRule));
    assert_eq!(GapRule::new(1, 1_000_001, 1), Err(LimitsError::BadGapRule));
    assert_eq!(
        GapRule::new(1, 1, 0),
        Err(LimitsError::Zero("gap_permille"))
    );
    assert!(GapRule::new(1, 1_000_000, 1).is_ok());
}

#[test]
fn shorts_are_refused_without_a_gap_rule() {
    let l = Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 3, 10 * SEC).unwrap();
    let mut g = Gateway::new(l, 3);
    assert_eq!(
        rejected(g.decide(&short(0, 0, 1, 1000), 0)),
        RejectReason::GapRisk
    );
    accepted(g.decide(&open(1, 1, 1000), 0)); // longs do not need one
}

#[test]
fn a_double_on_a_short_loses_at_most_the_configured_fraction_of_equity() {
    let mut g = gap_gw();
    let n = max_short_shares(100_000 * D, 20_000, 1000, px(1000));
    assert_eq!(
        n, 200,
        "2% of $100,000 is $2,000; doubling a $10 stock costs $10 a share"
    );
    assert_eq!(
        rejected(g.decide(&short(0, 0, n + 1, 1000), 0)),
        RejectReason::GapRisk,
        "one share too many"
    );
    let id = accepted(g.decide(&short(1, 0, n, 1000), 1));
    g.on_fill(id, n, px(1000)).unwrap();
    g.mark(0, px(2000)); // the gap
    assert_eq!(g.daily_pnl(), -2_000 * D as i128, "exactly the allowance");
}

#[test]
fn the_rule_covers_all_shorts_gapping_together_including_working_ones() {
    let mut g = gap_gw(); // $2,000 allowance: 200 shares' worth at $10
    let a = accepted(g.decide(&short(0, 0, 100, 1000), 0));
    accepted(g.decide(&short(1, 1, 100, 1000), 0)); // still unfilled: counts
    assert_eq!(
        rejected(g.decide(&short(2, 2, 1, 1000), 0)),
        RejectReason::GapRisk
    );
    g.on_fill(a, 100, px(1000)).unwrap(); // filling does not change the sum
    assert_eq!(
        rejected(g.decide(&short(3, 2, 1, 1000), 0)),
        RejectReason::GapRisk
    );
    // Covering one frees its share of the allowance.
    let c = accepted(g.decide(&intent(4, 0, Side::Buy, Purpose::Close, 100, 1000), 1));
    g.on_fill(c, 100, px(1000)).unwrap();
    accepted(g.decide(&short(5, 2, 100, 1000), 2));
}

#[test]
fn a_rising_price_and_lost_equity_both_shrink_what_may_be_added() {
    let mut g = gap_gw();
    let id = accepted(g.decide(&short(0, 0, 100, 1000), 0)); // $1,000 of a $2,000 allowance
    g.on_fill(id, 100, px(1000)).unwrap();
    g.mark(0, px(1500)); // the short is now $1,500, and equity is down $500 to $99,500
    // Allowance is 2% of $99,500 = $1,990; held is $1,500, so $490 remains: 32 shares at $15.
    assert_eq!(
        rejected(g.decide(&short(1, 1, 33, 1500), 1)),
        RejectReason::GapRisk
    );
    accepted(g.decide(&short(2, 1, 32, 1500), 1));
}

#[test]
fn the_rule_is_never_looser_than_stated_after_rounding() {
    // One raw unit short of $100,000 allows one raw unit short of $2,000: not 200 shares.
    assert_eq!(
        max_short_shares(100_000 * D - 1, 20_000, 1000, px(1000)),
        199
    );
    assert_eq!(max_short_shares(100_000 * D, 20_000, 1000, px(1000)), 200);
    assert_eq!(max_short_shares(100_000 * D, 20_000, 1000, Px::ZERO), 0);
    assert_eq!(max_short_shares(0, 20_000, 1000, px(1000)), 0);
}

#[test]
fn sizing_and_the_gateway_agree_exactly() {
    for cents in [1, 37, 250, 1000, 4999, 123_456] {
        let n = max_short_shares(100_000 * D, 20_000, 1000, px(cents));
        let capped = n.min(100_000);
        if capped == 0 {
            continue;
        }
        let mut g = gap_gw();
        assert!(
            matches!(
                g.decide(&short(0, 0, capped, cents), 0),
                Decision::Accepted(_)
            ),
            "{cents}c: {capped} shares"
        );
        if n < 100_000 {
            let mut g = gap_gw();
            // Notional caps may bind first for big sizes; the gap rule is the one that must
            // refuse `n + 1` when it is the binding constraint.
            let r = g.decide(&short(0, 0, n + 1, cents), 0);
            assert!(
                matches!(r, Decision::Rejected(_)),
                "{cents}c: {} shares",
                n + 1
            );
        }
    }
}

#[test]
fn a_negative_equity_account_cannot_short() {
    let mut g = gap_gw();
    let s = accepted(g.decide(&short(0, 0, 200, 1000), 0));
    g.on_fill(s, 200, px(1000)).unwrap();
    g.mark(0, px(100_000)); // a 100x squeeze: equity goes negative
    assert!(g.daily_pnl() < -100_000 * D as i128);
    assert_eq!(
        rejected(g.decide(&short(1, 1, 1, 1000), 1)),
        RejectReason::GapRisk
    );
}

#[test]
fn whatever_is_shorted_a_gap_costs_no_more_than_the_allowance() {
    for seed in 0..30u64 {
        let mut rng = SplitMix64::new(seed);
        let mut g = gap_gw();
        let mut accepted_any = 0;
        for step in 0..300u64 {
            let inst = (rng.next_u64() % 3) as u32;
            let cents = 50 + (rng.next_u64() % 3000) as i64;
            let qty = 1 + (rng.next_u64() % 400) as u32;
            let i = if rng.next_u64() % 4 == 0 {
                intent(step, inst, Side::Buy, Purpose::Close, qty, cents)
            } else {
                short(step, inst, qty, cents)
            };
            g.mark(inst, px(cents)); // the price is known when the decision is made
            let Decision::Accepted(id) = g.decide(&i, step) else {
                continue;
            };
            g.on_fill(id, i.qty, px(cents)).unwrap();
            if i.purpose == Purpose::Close {
                continue; // reducing risk is always allowed; the market may have moved since
            }
            accepted_any += 1;
            // The price of every instrument now doubles. What it costs, from here:
            let before = g.total_pnl();
            let equity = 100_000 * D as i128 + before;
            let marks: Vec<(u32, i64)> = (0..3).map(|k| (k, g.marks[k as usize])).collect();
            for (k, m) in marks {
                if m > 0 {
                    g.mark(k, Px::from_raw(m * 2));
                }
            }
            let cost = before - g.total_pnl();
            assert!(
                cost <= equity * 20_000 / 1_000_000,
                "seed {seed} step {step}: cost {cost} of equity {equity}"
            );
            // Put the marks back so the walk continues from real prices.
            for k in 0..3u32 {
                let m = g.marks[k as usize];
                if m > 0 {
                    g.mark(k, Px::from_raw(m / 2));
                }
            }
        }
        assert!(accepted_any > 5, "seed {seed} accepted {accepted_any}");
    }
}

#[test]
fn rounding_in_the_gateway_never_loosens_the_rule() {
    let gw = |equity: u128, ppm: u32, gap: u32| {
        let l = Limits::new(
            50_000 * D,
            100_000,
            500_000 * D,
            10_000_000 * D,
            99,
            10 * SEC,
        )
        .unwrap()
        .with_gap_rule(GapRule::new(equity, ppm, gap).unwrap());
        Gateway::new(l, 1)
    };
    let raw_short = |qty: u32, raw_px: i64| {
        let mut i = short(0, 0, qty, 0);
        i.pricing = Pricing::Limit(Px::from_raw(raw_px));
        i.protect = Some(Protective {
            stop_trigger: Px::from_raw(raw_px * 2),
            stop_limit: None,
            take_profit: None,
        });
        i
    };
    // The allowance (50% of $1,000.000000001) is $500.0000000005: floored to ...000.
    // A loss of one raw unit more must be refused.
    let mut g = gw(1000 * D + 1, 500_000, 1000);
    assert_eq!(
        rejected(g.decide(&raw_short(1, 500 * D as i64 + 1), 0)),
        RejectReason::GapRisk
    );
    assert!(matches!(
        g.decide(&raw_short(1, 500 * D as i64), 0),
        Decision::Accepted(_)
    ));
    // The loss (33.3% of 5 raw = 1.665) is rounded up to 2 and the allowance (1% of 100 raw) is 1.
    let mut g = gw(100, 10_000, 333);
    assert_eq!(
        rejected(g.decide(&raw_short(1, 5), 0)),
        RejectReason::GapRisk
    );
    assert!(
        matches!(g.decide(&raw_short(1, 3), 0), Decision::Accepted(_)),
        "0.999 rounds up to 1, which fits"
    );
}

#[test]
fn every_limit_is_recorded_once_and_the_gap_rule_shows() {
    let plain = Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 3, 10 * SEC).unwrap();
    let with_rule = plain.with_gap_rule(generous());
    let (a, b) = (plain.pairs(), with_rule.pairs());
    for v in [&a, &b] {
        let mut names: Vec<_> = v.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), v.len(), "names are unique");
    }
    assert!(a.contains(&("gap_rule", "none".to_owned())));
    assert!(b.iter().any(|(n, _)| *n == "gap_permille"));
    assert_ne!(a, b);
    assert!(a.contains(&("max_orders_per_window", "3".to_owned())));
}

#[test]
fn limits_round_trip_through_their_pairs_and_bad_pairs_are_refused() {
    let owned = |l: &Limits| -> Vec<(String, String)> {
        l.pairs()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    };
    let plain = Limits::new(5_000, 100, 20_000, 400, 6, 10).unwrap();
    let gap = plain.with_gap_rule(GapRule::new(100_000, 20_000, 1000).unwrap());
    for l in [plain, gap] {
        assert_eq!(Limits::from_pairs(&owned(&l)).unwrap().pairs(), l.pairs());
    }
    let base = owned(&plain);
    let without = |k: &str| -> Vec<(String, String)> {
        base.iter().filter(|(n, _)| n != k).cloned().collect()
    };
    for k in [
        "max_order_notional_raw",
        "max_position_shares",
        "max_gross_notional_raw",
        "max_daily_loss_raw",
        "max_orders_per_window",
        "rate_window_ns",
        "gap_rule",
    ] {
        assert!(
            Limits::from_pairs(&without(k))
                .unwrap_err()
                .contains("missing"),
            "{k}"
        );
    }
    let mut extra = base.clone();
    extra.push(("surprise".into(), "1".into()));
    assert!(
        Limits::from_pairs(&extra)
            .unwrap_err()
            .contains("unknown limit `surprise`")
    );
    let mut twice = base.clone();
    twice.push(base[0].clone());
    assert!(Limits::from_pairs(&twice).unwrap_err().contains("twice"));
    let mut bad = base.clone();
    bad[1].1 = "many".into();
    assert!(
        Limits::from_pairs(&bad)
            .unwrap_err()
            .contains("not a whole number")
    );
    let mut zero = base.clone();
    zero[1].1 = "0".into();
    assert!(
        Limits::from_pairs(&zero).is_err(),
        "a zero limit is still refused"
    );
    // A gap rule given in part is refused.
    let mut partial = owned(&gap);
    partial.retain(|(n, _)| n != "gap_permille");
    assert!(
        Limits::from_pairs(&partial)
            .unwrap_err()
            .contains("gap_permille")
    );
}

#[test]
fn a_snapshot_shows_every_part_of_the_state_that_decisions_depend_on() {
    let l = Limits::new(
        5_000 * 1_000_000_000,
        1_000,
        20_000 * 1_000_000_000,
        400 * 1_000_000_000,
        6,
        10_000_000_000,
    )
    .unwrap();
    let mut g = Gateway::new(l, 2);
    let empty = g.snapshot();
    assert!(empty.positions.is_empty() && empty.working.is_empty() && !empty.killed);
    g.mark(1, Px::from_raw(7)); // a flat instrument's mark is not state
    assert_eq!(g.snapshot(), empty);
    assert_eq!(g.mark_of(1), 7);
    assert_eq!(g.instruments(), 2);
    g.engage_kill_switch();
    assert_ne!(g.snapshot(), empty);
}

// ---- strategies sharing one broker account ----

fn by(strategy: u16, mut i: Intent) -> Intent {
    i.id.strategy = StrategyId(strategy);
    i
}

fn roomy() -> Limits {
    Limits::new(
        100_000 * D,
        500,
        10_000_000 * D,
        1_000_000 * D,
        1000,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(generous())
}

/// An opening order from `strategy` through the gateway, filled in full at its price.
fn fill_open(g: &mut Gateway, strategy: u16, seq: u64, i: Intent) -> OrderId {
    let id = accepted(g.decide(
        &by(
            strategy,
            Intent {
                id: IntentId {
                    strategy: StrategyId(strategy),
                    seq,
                },
                ..i
            },
        ),
        seq * SEC,
    ));
    g.on_fill(id, i.qty, i.pricing.reference_price()).unwrap();
    id
}

#[test]
fn two_strategies_in_one_symbol_keep_their_own_cost_and_profit_and_the_broker_sees_the_sum() {
    let mut g = Gateway::new(roomy(), 2);
    fill_open(&mut g, 1, 1, open(0, 100, 500));
    fill_open(&mut g, 2, 2, open(0, 50, 600));
    assert_eq!(g.position(0), 150, "what the broker holds");
    assert_eq!(
        (g.strategy_position(1, 0), g.strategy_position(2, 0)),
        (100, 50)
    );
    assert_eq!(g.strategy_position(3, 0), 0);
    g.mark(0, px(700));
    // Each at its own average cost, not the blend.
    assert_eq!(
        g.strategy_positions(1),
        vec![(0, 100, px(500).raw(), px(700).raw())]
    );
    assert_eq!(
        g.strategy_positions(2),
        vec![(0, 50, px(600).raw(), px(700).raw())]
    );
    assert_eq!(g.strategy_unrealized(1), 200 * D as i128);
    assert_eq!(g.strategy_unrealized(2), 50 * D as i128);
    assert_eq!(
        g.daily_pnl(),
        250 * D as i128,
        "the account's profit is the sum"
    );
    // Strategy 1 sells out at the mark: its profit is realised, strategy 2's is not touched.
    let c = accepted(g.decide(&by(1, close(3, 100, 700)), 3 * SEC));
    g.on_fill(c, 100, px(700)).unwrap();
    assert_eq!(
        (g.strategy_realized(1), g.strategy_realized(2)),
        (200 * D as i128, 0)
    );
    assert_eq!(g.strategy_position(1, 0), 0);
    assert!(
        g.strategy_positions(1).is_empty(),
        "a closed position is not kept"
    );
    assert_eq!(g.position(0), 50);
    assert_eq!(g.strategy_unrealized(1), 0);
    assert_eq!(g.strategy_unrealized(2), 50 * D as i128);
    assert_eq!(
        g.daily_pnl(),
        250 * D as i128,
        "moving profit from paper to realised changes nothing"
    );
    let snap = g.snapshot();
    assert_eq!(
        snap.positions,
        vec![(2, 0, 50, px(600).raw(), px(700).raw())],
        "a flat position is not kept"
    );
    assert_eq!(snap.strategy_realized, vec![(1, 200 * D as i128), (2, 0)]);
    assert_eq!(snap.realized, 200 * D as i128);
}

#[test]
fn a_strategy_can_close_only_what_it_holds_and_not_what_another_strategy_holds() {
    let mut g = Gateway::new(roomy(), 1);
    fill_open(&mut g, 1, 1, open(0, 100, 500));
    fill_open(&mut g, 2, 2, open(0, 50, 500));
    // The broker holds 150, but strategy 2 holds 50.
    assert_eq!(
        rejected(g.decide(&by(2, close(3, 100, 500)), 3 * SEC)),
        RejectReason::NothingToClose
    );
    assert_eq!(
        rejected(g.decide(&by(3, close(4, 1, 500)), 3 * SEC)),
        RejectReason::NothingToClose,
        "a strategy with nothing"
    );
    // Strategy 1 has a working close of 60; its own remaining 40 is all it can add, and strategy 2's
    // closing room is its own.
    accepted(g.decide(&by(1, close(5, 60, 500)), 4 * SEC));
    assert_eq!(
        rejected(g.decide(&by(1, close(6, 41, 500)), 4 * SEC)),
        RejectReason::NothingToClose
    );
    accepted(g.decide(&by(1, close(7, 40, 500)), 4 * SEC));
    accepted(g.decide(&by(2, close(8, 50, 500)), 4 * SEC));
}

#[test]
fn opposing_positions_are_checked_per_strategy() {
    let mut g = Gateway::new(roomy(), 1);
    fill_open(&mut g, 1, 1, open(0, 100, 500));
    // Another strategy may go the other way in the same symbol: each has its own sub-account.
    let short = |seq, st, qty| by(st, intent(seq, 0, Side::SellShort, Purpose::Open, qty, 500));
    accepted(g.decide(&short(2, 2, 40), 2 * SEC));
    // But not the strategy that is long, and not a long that holds a short.
    assert_eq!(
        rejected(g.decide(&short(3, 1, 10), 3 * SEC)),
        RejectReason::OpposingPosition
    );
    let id = accepted(g.decide(&short(4, 2, 10), 3 * SEC));
    g.on_fill(id, 10, px(500)).unwrap();
    assert_eq!(
        rejected(g.decide(&by(2, open(5, 10, 500)), 4 * SEC)),
        RejectReason::OpposingPosition
    );
    assert_eq!(g.strategy_position(2, 0), -10);
}

#[test]
fn the_cap_on_a_symbol_counts_every_strategy_on_that_side_but_not_the_other_side() {
    let mut g = Gateway::new(roomy(), 1); // 500 shares
    fill_open(&mut g, 1, 1, open(0, 300, 200));
    fill_open(&mut g, 2, 2, open(0, 150, 200));
    assert_eq!(
        rejected(g.decide(&by(3, open(3, 51, 200)), 3 * SEC)),
        RejectReason::MaxPosition,
        "500 - 450 = 50 left"
    );
    accepted(g.decide(&by(3, open(4, 50, 200)), 3 * SEC));
    // Working orders count too: strategy 4 cannot add one more share on the long side.
    assert_eq!(
        rejected(g.decide(&by(4, open(5, 1, 200)), 4 * SEC)),
        RejectReason::MaxPosition
    );
    // Shorts are a different side: 500 of them fit even with 500 long.
    let short = by(5, intent(6, 0, Side::SellShort, Purpose::Open, 500, 200));
    accepted(g.decide(&short, 5 * SEC));
}

#[test]
fn shorts_count_toward_the_gap_rule_even_when_the_broker_is_flat() {
    // $10,000 equity, 10% may be lost if every short doubles: $1,000 of short notional.
    let l = Limits::new(
        100_000 * D,
        5_000,
        10_000_000 * D,
        1_000_000 * D,
        1000,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(10_000 * D, 100_000, 1000).unwrap());
    let mut g = Gateway::new(l, 1);
    fill_open(&mut g, 1, 1, open(0, 100, 500)); // long $500
    let short = |seq, st, qty| by(st, intent(seq, 0, Side::SellShort, Purpose::Open, qty, 500));
    let id = accepted(g.decide(&short(2, 2, 100), 2 * SEC)); // short $500: fits
    g.on_fill(id, 100, px(500)).unwrap();
    assert_eq!(g.position(0), 0, "the broker nets to flat");
    // Another $600 of short notional: $1,100 of virtual shorts, over the $1,000 allowed. A rule that
    // looked at the broker's net (flat) would let it through.
    assert_eq!(
        rejected(g.decide(&short(3, 3, 120), 3 * SEC)),
        RejectReason::GapRisk
    );
    accepted(g.decide(&short(4, 3, 100), 3 * SEC));
}

#[test]
fn a_strategys_working_opens_are_its_own() {
    let mut g = Gateway::new(roomy(), 1);
    accepted(g.decide(&by(1, open(1, 100, 500)), SEC));
    accepted(g.decide(&by(2, open(2, 50, 400)), SEC));
    assert_eq!(g.strategy_working_open_notional(1), 500 * D);
    assert_eq!(g.strategy_working_open_notional(2), 200 * D);
    assert_eq!(g.strategy_working_open_notional(3), 0);
    let snap = g.snapshot();
    assert_eq!(
        snap.working.iter().map(|w| (w.1, w.3)).collect::<Vec<_>>(),
        vec![(1, 100), (2, 50)]
    );
}

#[test]
fn across_random_trading_by_several_strategies_the_parts_always_add_up_to_the_account() {
    let mut rng = SplitMix64::new(21);
    let l = Limits::new(
        1_000_000 * D,
        100_000,
        100_000_000 * D,
        100_000_000 * D,
        100_000,
        SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(100_000_000 * D, 1_000_000, 1000).unwrap());
    let mut g = Gateway::new(l, 3);
    // A shadow account: signed shares and cash per instrument, from every fill the gateway took.
    let (mut net, mut cash) = ([0i64; 3], [0i128; 3]);
    let mut marks = [px(500).raw(); 3];
    for (i, m) in marks.iter().enumerate() {
        g.mark(i as u32, Px::from_raw(*m));
    }
    let (mut seq, mut fills) = (0u64, 0i128);
    for step in 0..1500u64 {
        let now = step * SEC;
        let inst = rng.below(3) as u32;
        let strat = 1 + rng.below(3) as u16;
        match rng.below(10) {
            0..=4 => {
                seq += 1;
                let qty = 10 + rng.below(90) as u32;
                let cents = 400 + rng.below(200) as i64;
                let held = g.strategy_position(strat, inst);
                let i = match rng.below(3) {
                    0 => intent(seq, inst, Side::SellShort, Purpose::Open, qty, cents),
                    1 if held > 0 => intent(
                        seq,
                        inst,
                        Side::Sell,
                        Purpose::Close,
                        (held as u32).min(qty),
                        cents,
                    ),
                    2 if held < 0 => intent(
                        seq,
                        inst,
                        Side::Buy,
                        Purpose::Close,
                        ((-held) as u32).min(qty),
                        cents,
                    ),
                    _ => intent(seq, inst, Side::Buy, Purpose::Open, qty, cents),
                };
                let _ = g.decide(&by(strat, i), now);
            }
            5..=7 => {
                // Fill part or all of a random working order, at a price near the market.
                let snap = g.snapshot();
                if !snap.working.is_empty() {
                    let w = snap.working[rng.below(snap.working.len() as u64) as usize];
                    let qty = 1 + rng.below(u64::from(w.3)) as u32;
                    let cents = 400 + rng.below(200) as i64;
                    let buy = g.working_is_buy(w.0);
                    g.on_fill(w.0, qty, px(cents)).unwrap();
                    let signed = if buy { i64::from(qty) } else { -i64::from(qty) };
                    net[w.2 as usize] += signed;
                    cash[w.2 as usize] -= i128::from(signed) * i128::from(px(cents).raw());
                    fills += 1;
                }
            }
            8 => {
                let snap = g.snapshot();
                if !snap.working.is_empty() {
                    let w = snap.working[rng.below(snap.working.len() as u64) as usize];
                    g.on_closed(w.0).unwrap();
                }
            }
            _ => {
                marks[inst as usize] = px(400 + rng.below(200) as i64).raw();
                g.mark(inst, Px::from_raw(marks[inst as usize]));
            }
        }
        // The strategies' positions add up to the broker's, in every instrument, at every step.
        for i in 0..3u32 {
            let sum: i64 = (1..=3).map(|s| g.strategy_position(s, i)).sum();
            assert_eq!(sum, g.position(i), "step {step} instrument {i}");
            assert_eq!(sum, net[i as usize], "and to what the fills said");
        }
        // And their profit adds up to the account's: cash plus the broker's shares at the marks.
        // (Average costs round to a raw unit, so allow a unit per share per fill.)
        let account: i128 = (0..3)
            .map(|i| cash[i] + i128::from(net[i]) * i128::from(marks[i]))
            .sum();
        let parts: i128 = (1..=3u16)
            .map(|s| g.strategy_realized(s) + g.strategy_unrealized(s))
            .sum();
        assert!(
            (account - parts).abs() <= fills * 100,
            "step {step}: account {account} parts {parts}"
        );
        assert_eq!(
            g.daily_pnl(),
            parts,
            "the gateway's own total is the sum of the parts"
        );
        let snap = g.snapshot();
        assert!(snap.positions.iter().all(|p| p.2 != 0));
        assert_eq!(
            snap.realized,
            snap.strategy_realized.iter().map(|x| x.1).sum::<i128>()
        );
    }
    assert!(fills > 200, "{fills} fills");
}

// ---- budgets ----

use tf_budget::{Group, LossLimits as BudgetLoss, Strategy as BudgetStrategy, Tree};

fn bs(id: &str, share: u32) -> BudgetStrategy {
    BudgetStrategy {
        id: id.to_owned(),
        share,
    }
}

/// $100,000: group g1 50% ($50,000: a 40% = $20,000, b 60% = $30,000), group g2 50% (c, all of it).
fn tree() -> Tree {
    let g = |id: &str, share, strategies| Group {
        id: id.to_owned(),
        share,
        loss: BudgetLoss::default(),
        strategies,
    };
    Tree::new(vec![
        g("g1", 5000, vec![bs("a", 4000), bs("b", 6000)]),
        g("g2", 5000, vec![bs("c", 10_000)]),
    ])
    .unwrap()
}

fn budgets_for(t: Tree) -> Budgets {
    Budgets::new(
        t,
        100_000 * D,
        [
            (1, "a".to_owned()),
            (2, "b".to_owned()),
            (3, "c".to_owned()),
        ],
    )
    .unwrap()
}

/// A gateway with the budgets above and nothing else in the way.
fn budgeted(gap_permille: u32) -> Gateway {
    let l = Limits::new(
        1_000_000 * D,
        1_000_000,
        1_000_000_000 * D,
        1_000_000_000 * D,
        100_000,
        SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(1_000_000_000 * D, 1_000_000, gap_permille).unwrap());
    let mut g = Gateway::new(l, 2);
    g.set_budgets(Some(budgets_for(tree())));
    g
}

fn buy_by(strategy: u16, seq: u64, qty: u32, dollars: i64) -> Intent {
    by(
        strategy,
        intent(seq, 0, Side::Buy, Purpose::Open, qty, dollars * 100),
    )
}

fn short_by(strategy: u16, seq: u64, qty: u32, dollars: i64) -> Intent {
    by(
        strategy,
        intent(seq, 0, Side::SellShort, Purpose::Open, qty, dollars * 100),
    )
}

fn take(g: &mut Gateway, i: Intent, t: Nanos) -> OrderId {
    let id = accepted(g.decide(&i, t));
    g.on_fill(id, i.qty, i.pricing.reference_price()).unwrap();
    id
}

#[test]
fn budgets_must_name_real_strategies_once_each() {
    assert_eq!(
        Budgets::new(tree(), 1, [(1, "nope".to_owned())]).unwrap_err(),
        BudgetsError::UnknownStrategy("nope".into())
    );
    assert_eq!(
        Budgets::new(tree(), 1, [(1, "a".to_owned()), (2, "a".to_owned())]).unwrap_err(),
        BudgetsError::Duplicate("a".into())
    );
    let b = budgets_for(tree());
    assert_eq!(
        (
            b.strategy_budget(1),
            b.strategy_budget(2),
            b.strategy_budget(3)
        ),
        (Some(20_000 * D), Some(30_000 * D), Some(50_000 * D))
    );
    assert_eq!(b.strategy_budget(9), None);
    assert_eq!(
        (b.group_of(1), b.group_of(3), b.group_of(9)),
        (Some("g1"), Some("g2"), None)
    );
    assert_eq!(b.balance(), 100_000 * D);
    assert_eq!(b.ids().len(), 3);
    assert_eq!(b.tree().groups().len(), 2);
}

#[test]
fn an_opening_order_must_fit_the_strategys_own_budget_to_the_last_unit() {
    let mut g = budgeted(1000);
    take(&mut g, buy_by(1, 1, 3_000, 6), SEC); // $18,000 of $20,000
    assert_eq!(g.strategy_charge(1), 18_000 * D);
    assert_eq!(
        rejected(g.decide(&buy_by(1, 2, 401, 5), 2 * SEC)),
        RejectReason::StrategyBudget,
        "$2,005 would be $20,005"
    );
    accepted(g.decide(&buy_by(1, 3, 400, 5), 2 * SEC)); // exactly $20,000: allowed
    assert_eq!(g.strategy_charge(1), 20_000 * D, "a working order counts");
    assert_eq!(
        rejected(g.decide(&buy_by(1, 4, 1, 1), 3 * SEC)),
        RejectReason::StrategyBudget
    );
    // Another strategy's budget is its own.
    accepted(g.decide(&buy_by(2, 5, 6_000, 5), 3 * SEC)); // $30,000 of b's $30,000
    assert_eq!(g.rejected_count("strategy_budget"), 2);
}

#[test]
fn a_group_budget_binds_when_a_sibling_has_run_over_its_own() {
    let mut g = budgeted(1000);
    take(&mut g, buy_by(2, 1, 5_000, 5), SEC); // b holds $25,000 of its $30,000
    g.mark(0, px(700)); // b's position is now worth $35,000: over its budget by $5,000
    assert_eq!(g.strategy_charge(2), 35_000 * D);
    // a is far inside its own $20,000, but the group's $50,000 is $35,000 used.
    accepted(g.decide(&buy_by(1, 2, 2_000, 7), 2 * SEC)); // $14,000: group $49,000
    assert_eq!(
        rejected(g.decide(&buy_by(1, 3, 143, 7), 3 * SEC)),
        RejectReason::GroupBudget,
        "$1,001 more is $50,001"
    );
    assert_eq!(g.group_charge("g1"), 49_000 * D);
    assert_eq!(g.group_charge("g2"), 0);
    assert_eq!(g.group_charge("nope"), 0);
    accepted(g.decide(&buy_by(1, 4, 142, 7), 3 * SEC)); // $994: group $49,994
}

#[test]
fn a_strategy_without_a_budget_cannot_open_but_can_still_close() {
    let mut g = Gateway::new(
        Limits::new(
            1_000_000 * D,
            1_000_000,
            1_000_000_000 * D,
            1_000_000_000 * D,
            100_000,
            SEC,
        )
        .unwrap()
        .with_gap_rule(generous()),
        2,
    );
    take(&mut g, buy_by(9, 1, 100, 5), SEC); // before budgets are in force
    g.set_budgets(Some(budgets_for(tree())));
    assert_eq!(
        rejected(g.decide(&buy_by(9, 2, 1, 5), 2 * SEC)),
        RejectReason::NoBudget
    );
    accepted(g.decide(&by(9, close(3, 100, 500)), 3 * SEC));
    assert_eq!(g.rejected_count("no_budget"), 1);
}

#[test]
fn a_short_is_charged_its_notional_times_the_gap_assumption_and_never_less_than_its_notional() {
    // 150%: a $5 short of n shares is charged 7.5 n.
    let mut g = budgeted(1500);
    take(&mut g, short_by(1, 1, 2_666, 5), SEC);
    assert_eq!(g.strategy_charge(1), 19_995 * D);
    assert_eq!(
        rejected(g.decide(&short_by(1, 2, 2, 5), 2 * SEC)),
        RejectReason::StrategyBudget,
        "2,668 shares would be $20,010"
    );
    assert_eq!(
        rejected(g.decide(&short_by(1, 3, 1, 5), 2 * SEC)),
        RejectReason::StrategyBudget,
        "one more share is $20,002.50"
    );
    // The same size under a gap assumption below 100% is still charged as a full notional.
    let mut low = budgeted(500);
    accepted(low.decide(&short_by(1, 1, 4_000, 5), SEC)); // $20,000 exactly
    assert_eq!(
        rejected(low.decide(&short_by(1, 2, 1, 5), 2 * SEC)),
        RejectReason::StrategyBudget
    );
    // A long of the same notional is charged just the notional.
    let mut long = budgeted(1500);
    accepted(long.decide(&buy_by(1, 1, 4_000, 5), SEC));
    assert_eq!(long.strategy_charge(1), 20_000 * D);
}

#[test]
fn working_orders_count_until_they_end_and_partial_fills_count_what_remains() {
    let mut g = budgeted(1000);
    let w = accepted(g.decide(&buy_by(1, 1, 3_000, 5), SEC)); // $15,000 working
    assert_eq!(
        rejected(g.decide(&buy_by(1, 2, 1_001, 5), 2 * SEC)),
        RejectReason::StrategyBudget
    );
    g.on_fill(w, 1_000, px(500)).unwrap(); // $5,000 held, $10,000 still working
    assert_eq!(g.strategy_charge(1), 15_000 * D);
    g.on_closed(w).unwrap(); // the rest is cancelled
    assert_eq!(g.strategy_charge(1), 5_000 * D);
    accepted(g.decide(&buy_by(1, 3, 3_000, 5), 3 * SEC)); // room again: $5,000 + $15,000
}

#[test]
fn a_position_is_charged_at_the_higher_of_cost_and_mark() {
    let mut g = budgeted(1000);
    take(&mut g, buy_by(1, 1, 1_000, 5), SEC);
    assert_eq!(g.strategy_charge(1), 5_000 * D);
    g.mark(0, px(800));
    assert_eq!(
        g.strategy_charge(1),
        8_000 * D,
        "a gain uses more of the budget"
    );
    g.mark(0, px(200));
    assert_eq!(
        g.strategy_charge(1),
        5_000 * D,
        "a loss does not give budget back"
    );
}

#[test]
fn closes_always_pass_even_when_a_strategy_is_over_budget() {
    let mut g = budgeted(1000);
    take(&mut g, buy_by(1, 1, 3_000, 6), SEC);
    g.mark(0, px(900)); // $27,000 against a $20,000 budget
    assert_eq!(
        rejected(g.decide(&buy_by(1, 2, 1, 9), 2 * SEC)),
        RejectReason::StrategyBudget
    );
    let c = accepted(g.decide(&by(1, close(3, 3_000, 900)), 3 * SEC));
    g.on_fill(c, 3_000, px(900)).unwrap();
    assert_eq!(g.strategy_charge(1), 0);
}

#[test]
fn replacing_or_removing_budgets_changes_what_may_open_and_touches_no_position() {
    let mut g = budgeted(1000);
    take(&mut g, buy_by(1, 1, 3_000, 5), SEC); // $15,000
    let half = Tree::new(
        tree()
            .groups()
            .iter()
            .map(|gr| Group {
                share: gr.share / 2,
                ..gr.clone()
            })
            .collect(),
    )
    .unwrap(); // a's budget is now $10,000
    g.set_budgets(Some(budgets_for(half)));
    assert_eq!(g.position(0), 3_000, "the position stays");
    assert_eq!(
        rejected(g.decide(&buy_by(1, 2, 1, 5), 2 * SEC)),
        RejectReason::StrategyBudget
    );
    g.set_budgets(None);
    assert!(g.budgets().is_none());
    accepted(g.decide(&buy_by(1, 3, 900_000, 1), 3 * SEC));
    assert_eq!(
        g.strategy_charge(1),
        915_000 * D,
        "with no budgets anything the limits allow may open, and the charge is still computed"
    );
}

#[test]
fn the_snapshot_says_which_budgets_were_in_force() {
    let a = budgeted(1000).snapshot();
    let b = budgeted(1000).snapshot();
    assert_eq!(a, b);
    assert!(a.budgets.is_some());
    let mut other = budgeted(1000);
    other.set_budgets(Some(
        Budgets::new(tree(), 99_000 * D, [(1, "a".to_owned())]).unwrap(),
    ));
    assert_ne!(other.snapshot(), a);
    other.set_budgets(None);
    assert_eq!(other.snapshot().budgets, None);
    let mut moved = budgeted(1000);
    moved.set_budgets(Some(budgets_for(
        tree()
            .with_group_share("g1", 4000, 100_000 * D, &tf_budget::Usage::new())
            .unwrap(),
    )));
    assert_ne!(
        moved.snapshot().budgets,
        a.budgets,
        "a different tree is a different fingerprint"
    );
}

#[test]
fn an_accepted_opening_always_fits_its_strategy_and_group_across_random_trading() {
    let mut rng = SplitMix64::new(33);
    let mut g = budgeted(1500);
    // Loss limits out of the way: this is about the budgets themselves.
    let out_of_the_way = BudgetLoss {
        soft: 9_000,
        hard: 10_000,
    };
    let wide = tree()
        .with_loss("g1", out_of_the_way)
        .unwrap()
        .with_loss("g2", out_of_the_way)
        .unwrap();
    g.set_budgets(Some(budgets_for(wide)));
    let mut accepted_n = 0;
    let mut seq = 0;
    for step in 0..2_000u64 {
        let now = step * SEC;
        seq += 1;
        let strat = 1 + rng.below(3) as u16;
        let qty = 10 + rng.below(800) as u32;
        let dollars = 3 + rng.below(8) as i64;
        match rng.below(10) {
            0..=5 => {
                let i = if rng.below(4) == 0 {
                    short_by(strat, seq, qty, dollars)
                } else {
                    buy_by(strat, seq, qty, dollars)
                };
                if let Decision::Accepted(_) = g.decide(&i, now) {
                    accepted_n += 1;
                    // The check included this order, so right now it fits both budgets.
                    let b = g.budgets().unwrap().clone();
                    assert!(
                        g.strategy_charge(strat) <= b.strategy_budget(strat).unwrap(),
                        "step {step}"
                    );
                    let grp = b.group_of(strat).unwrap().to_owned();
                    assert!(
                        g.group_charge(&grp) <= b.tree().group_budget(b.balance(), &grp).unwrap(),
                        "step {step}"
                    );
                }
            }
            6..=7 => {
                let snap = g.snapshot();
                if !snap.working.is_empty() {
                    let w = snap.working[rng.below(snap.working.len() as u64) as usize];
                    let q = 1 + rng.below(u64::from(w.3)) as u32;
                    g.on_fill(w.0, q, px(300 + rng.below(800) as i64)).unwrap();
                }
            }
            8 => {
                let snap = g.snapshot();
                if !snap.working.is_empty() {
                    g.on_closed(snap.working[rng.below(snap.working.len() as u64) as usize].0)
                        .unwrap();
                }
            }
            _ => g.mark(0, px(300 + rng.below(800) as i64)),
        }
    }
    assert!(accepted_n > 50, "{accepted_n} accepted");
    assert!(
        g.rejected_count("strategy_budget") + g.rejected_count("group_budget") > 50,
        "the budgets did bind: {:?}",
        g.rejection_counts()
    );
}

#[test]
fn a_short_is_charged_rounded_up_so_the_budget_is_never_looser_than_stated() {
    // A balance of 4 raw units gives the only strategy a budget of 4. One share at a raw price of 3
    // is a notional of 3, which at 150% is 4.5: charged as 5, so it does not fit.
    let t = Tree::new(vec![Group {
        id: "x".into(),
        share: 10_000,
        loss: BudgetLoss::default(),
        strategies: vec![bs("s", 10_000)],
    }])
    .unwrap();
    let l = Limits::new(
        1_000_000 * D,
        1_000_000,
        1_000_000_000 * D,
        1_000_000_000 * D,
        100_000,
        SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(1_000_000_000 * D, 1_000_000, 1500).unwrap());
    let mut g = Gateway::new(l, 1);
    g.set_budgets(Some(Budgets::new(t, 4, [(1, "s".to_owned())]).unwrap()));
    let short = Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq: 1,
        },
        instrument: 0,
        side: Side::SellShort,
        qty: 1,
        purpose: Purpose::Open,
        pricing: Pricing::Limit(Px::from_raw(3)),
        protect: Some(Protective {
            stop_trigger: Px::from_raw(6),
            stop_limit: None,
            take_profit: None,
        }),
        tif: Tif::Day,
        ts: 0,
        reason: 0,
    };
    assert_eq!(
        rejected(g.decide(&short, SEC)),
        RejectReason::StrategyBudget
    );
    // At a raw price of 2 it is 3 exactly, and fits.
    let fits = Intent {
        pricing: Pricing::Limit(Px::from_raw(2)),
        protect: Some(Protective {
            stop_trigger: Px::from_raw(5),
            stop_limit: None,
            take_profit: None,
        }),
        ..short
    };
    accepted(g.decide(&fits, 2 * SEC));
}

#[test]
fn a_working_short_is_charged_at_its_reference_price_not_its_collar_floor() {
    let mut g = budgeted(1000);
    // A collar of 50% under a $10 reference: the order may sell as low as $5, but it is a $10 short.
    let collar_short = Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq: 1,
        },
        instrument: 0,
        side: Side::SellShort,
        qty: 100,
        purpose: Purpose::Open,
        pricing: Pricing::Collar {
            reference: px(1000),
            collar_permille: 500,
        },
        protect: Some(Protective {
            stop_trigger: px(2000),
            stop_limit: None,
            take_profit: None,
        }),
        tif: Tif::Day,
        ts: 0,
        reason: 0,
    };
    assert_eq!(collar_short.limit_price(), px(500));
    accepted(g.decide(&collar_short, SEC));
    assert_eq!(
        g.strategy_charge(1),
        1_000 * D,
        "100 shares at the $10 reference"
    );
}

// ---- loss limits per strategy ----

/// Strategy 1 ("a", $20,000 budget: soft limit $600, hard limit $1,200) holds 2,000 shares at $5.
fn holding_a() -> Gateway {
    let mut g = budgeted(1000);
    take(&mut g, buy_by(1, 1, 2_000, 5), SEC);
    g
}

#[test]
fn a_strategy_that_loses_its_soft_limit_stops_opening_for_the_day_but_can_still_close() {
    assert_eq!(
        budgets_for(tree()).tree().loss_amounts(100_000 * D, "a"),
        Some((600 * D, 1_200 * D))
    );
    let mut g = holding_a();
    assert_eq!(g.strategy_limits(1), Some((600 * D, 1_200 * D)));
    assert_eq!(g.strategy_limits(9), None);
    g.mark(0, px(471)); // down $0.29 on 2,000 shares: $580
    assert_eq!(g.strategy_loss(1), 580 * D);
    accepted(g.decide(&buy_by(1, 2, 10, 5), 2 * SEC)); // still allowed
    g.mark(0, px(470)); // $600: exactly the limit
    assert_eq!(g.strategy_loss(1), 600 * D);
    assert_eq!(
        rejected(g.decide(&buy_by(1, 3, 10, 5), 3 * SEC)),
        RejectReason::StrategyLossLimit
    );
    // It stays stopped for the day even if the price recovers.
    g.mark(0, px(500));
    assert_eq!(g.strategy_loss(1), 0);
    assert_eq!(
        rejected(g.decide(&buy_by(1, 4, 10, 5), 4 * SEC)),
        RejectReason::StrategyLossLimit
    );
    // Exits are never blocked.
    let c = accepted(g.decide(&by(1, close(5, 2_000, 500)), 5 * SEC));
    g.on_fill(c, 2_000, px(500)).unwrap();
    // Another strategy in the same group, and its own group, are untouched.
    accepted(g.decide(&buy_by(2, 6, 10, 5), 6 * SEC));
    accepted(g.decide(&buy_by(3, 7, 10, 5), 6 * SEC));
    assert_eq!(g.rejected_count("strategy_loss_limit"), 2);
    assert_eq!(g.snapshot().soft_latched, vec![1]);
    assert!(g.snapshot().hard_latched.is_empty());
}

#[test]
fn a_loss_counts_realised_and_on_paper_and_a_gain_offsets_it() {
    let mut g = holding_a();
    // Sell half at a $0.30 loss ($300 realised), then lose $0.30 on the other half ($300 on paper).
    let c = accepted(g.decide(&by(1, close(2, 1_000, 470)), 2 * SEC));
    g.on_fill(c, 1_000, px(470)).unwrap();
    assert_eq!(g.strategy_realized(1), -300 * D as i128);
    g.mark(0, px(470));
    assert_eq!(g.strategy_loss(1), 600 * D);
    assert_eq!(
        rejected(g.decide(&buy_by(1, 3, 10, 5), 3 * SEC)),
        RejectReason::StrategyLossLimit
    );
    // A strategy that is ahead has no loss, however much another has lost.
    g.mark(0, px(600));
    assert_eq!(g.strategy_loss(1), 0);
    assert_eq!(g.strategy_loss(2), 0);
}

#[test]
fn crossing_the_hard_limit_is_reported_once_and_lays_out_what_to_flatten() {
    let mut g = holding_a();
    accepted(g.decide(&buy_by(1, 2, 100, 5), 2 * SEC)); // a working open to cancel
    g.mark(0, px(441)); // 2,000 x $0.59 = $1,180: past soft ($600), not yet hard ($1,200)
    assert_eq!(
        g.check_loss_limits(),
        vec![LossEvent {
            strategy: 1,
            tier: LossTier::Soft,
            loss: 1_180 * D,
            limit: 600 * D
        }]
    );
    assert!(g.check_loss_limits().is_empty(), "each tier once");
    g.mark(0, px(440)); // $1,200
    assert_eq!(
        g.check_loss_limits(),
        vec![LossEvent {
            strategy: 1,
            tier: LossTier::Hard,
            loss: 1_200 * D,
            limit: 1_200 * D
        }]
    );
    assert!(g.check_loss_limits().is_empty());
    let snap = g.snapshot();
    assert_eq!((snap.soft_latched, snap.hard_latched), (vec![1], vec![1]));
    // Flatten: sell the 2,000 it holds and cancel the 100-share open that is working.
    let plan = g.flatten_plan(1);
    assert_eq!(plan.closes, vec![(0, Side::Sell, 2_000)]);
    assert_eq!(plan.cancels.len(), 1);
    // A close already working counts toward it.
    accepted(g.decide(&by(1, close(3, 500, 440)), 3 * SEC));
    assert_eq!(g.flatten_plan(1).closes, vec![(0, Side::Sell, 1_500)]);
    assert_eq!(
        g.flatten_plan(1).cancels.len(),
        1,
        "a working close is not cancelled"
    );
    // A strategy with nothing has nothing to flatten.
    assert_eq!(
        g.flatten_plan(2),
        FlattenPlan {
            closes: vec![],
            cancels: vec![]
        }
    );
}

#[test]
fn jumping_straight_past_the_hard_limit_reports_both_tiers() {
    let mut g = holding_a();
    g.mark(0, px(300)); // $4,000 down
    assert_eq!(
        g.check_loss_limits()
            .iter()
            .map(|e| (e.strategy, e.tier))
            .collect::<Vec<_>>(),
        vec![(1, LossTier::Soft), (1, LossTier::Hard)]
    );
}

#[test]
fn flattening_a_short_buys_it_back() {
    let mut g = budgeted(1000);
    take(&mut g, short_by(1, 1, 1_000, 10), SEC);
    let plan = g.flatten_plan(1);
    assert_eq!(plan.closes, vec![(0, Side::Buy, 1_000)]);
    g.mark(0, px(1200)); // a short loses $2 a share: $2,000
    assert_eq!(g.strategy_loss(1), 2_000 * D);
    assert_eq!(g.check_loss_limits().len(), 2);
}

#[test]
fn a_new_day_clears_the_latches_and_measures_from_where_the_strategy_stands() {
    let mut g = holding_a();
    g.mark(0, px(440));
    assert_eq!(g.check_loss_limits().len(), 2);
    assert_eq!(
        rejected(g.decide(&buy_by(1, 2, 10, 5), 2 * SEC)),
        RejectReason::StrategyLossLimit
    );
    g.new_day();
    assert!(g.snapshot().soft_latched.is_empty() && g.snapshot().hard_latched.is_empty());
    assert_eq!(
        g.strategy_loss(1),
        0,
        "yesterday's loss is the new starting point"
    );
    assert_eq!(
        g.snapshot().strategy_day_base,
        vec![(1, -1_200 * D as i128), (2, 0), (3, 0)]
    );
    accepted(g.decide(&buy_by(1, 3, 10, 4), 3 * SEC));
    assert!(g.check_loss_limits().is_empty());
    // And it can lose its limits again from there.
    g.mark(0, px(380));
    assert_eq!(g.check_loss_limits().len(), 2);
}

#[test]
fn each_groups_own_loss_limits_apply_to_its_strategies() {
    let t = tree()
        .with_loss(
            "g1",
            BudgetLoss {
                soft: 100,
                hard: 200,
            },
        )
        .unwrap(); // 1% and 2%
    let mut g = budgeted(1000);
    g.set_budgets(Some(budgets_for(t)));
    take(&mut g, buy_by(1, 1, 2_000, 5), SEC);
    assert_eq!(g.strategy_limits(1), Some((200 * D, 400 * D)));
    assert_eq!(
        g.strategy_limits(3),
        Some((1_500 * D, 3_000 * D)),
        "g2 keeps the default 3% and 6% of $50,000"
    );
    g.mark(0, px(490)); // $200 down
    assert_eq!(g.check_loss_limits().len(), 1);
}

#[test]
fn without_budgets_there_are_no_strategy_loss_limits() {
    let mut g = holding_a();
    g.set_budgets(None);
    g.mark(0, px(100));
    assert!(g.strategy_loss(1) > 0);
    assert_eq!(g.strategy_limits(1), None);
    assert!(g.check_loss_limits().is_empty());
    accepted(g.decide(&buy_by(1, 2, 10, 5), 2 * SEC));
}

#[test]
fn across_random_trading_a_latched_strategy_never_opens_again_that_day_and_flattening_empties_it() {
    let mut rng = SplitMix64::new(77);
    let mut g = budgeted(1000);
    let mut seq = 0;
    let mut tripped = 0;
    for step in 0..3_000u64 {
        let now = step * SEC;
        if step % 400 == 399 {
            g.new_day();
        }
        seq += 1;
        let strat = 1 + rng.below(3) as u16;
        match rng.below(10) {
            0..=3 => {
                let i = buy_by(
                    strat,
                    seq,
                    10 + rng.below(400) as u32,
                    3 + rng.below(8) as i64,
                );
                let was_latched = g.snapshot().soft_latched.contains(&strat);
                let d = g.decide(&i, now);
                if was_latched {
                    assert_eq!(
                        d,
                        Decision::Rejected(RejectReason::StrategyLossLimit),
                        "step {step}"
                    );
                }
            }
            4..=5 => {
                let snap = g.snapshot();
                if !snap.working.is_empty() {
                    let w = snap.working[rng.below(snap.working.len() as u64) as usize];
                    g.on_fill(
                        w.0,
                        1 + rng.below(u64::from(w.3)) as u32,
                        px(300 + rng.below(800) as i64),
                    )
                    .unwrap();
                }
            }
            _ => g.mark(0, px(300 + rng.below(800) as i64)),
        }
        for e in g.check_loss_limits() {
            if e.tier == LossTier::Hard {
                tripped += 1;
                // Carry out the plan: cancel the opens, close what is held, at the mark.
                let plan = g.flatten_plan(e.strategy);
                for o in plan.cancels {
                    g.on_closed(o).unwrap();
                }
                for (inst, side, qty) in plan.closes {
                    seq += 1;
                    let cents = g.mark_of(inst) / 10_000_000;
                    let close = by(
                        e.strategy,
                        intent(seq, inst, side, Purpose::Close, qty, cents),
                    );
                    let id = accepted(g.decide(&close, now));
                    g.on_fill(id, qty, px(cents)).unwrap();
                }
                assert!(
                    g.strategy_positions(e.strategy).is_empty(),
                    "step {step}: flattened"
                );
                assert!(g.flatten_plan(e.strategy).closes.is_empty());
            }
        }
    }
    assert!(tripped > 3, "the hard limit was reached {tripped} times");
    assert!(g.rejected_count("strategy_loss_limit") > 10);
}

#[test]
fn a_strategy_with_no_budget_share_is_not_reported_as_having_lost_it() {
    // A strategy sized at zero has a zero limit, which a loss of zero must not trip.
    let zero = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: BudgetLoss::default(),
        strategies: vec![bs("a", 0), bs("b", 10_000)],
    }])
    .unwrap();
    let mut g = budgeted(1000);
    g.set_budgets(Some(
        Budgets::new(
            zero,
            100_000 * D,
            [(1, "a".to_owned()), (2, "b".to_owned())],
        )
        .unwrap(),
    ));
    assert_eq!(g.strategy_limits(1), Some((0, 0)));
    assert!(g.check_loss_limits().is_empty());
    assert!(g.snapshot().soft_latched.is_empty());
}
