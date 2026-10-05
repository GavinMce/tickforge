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
    Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 3, 10 * SEC).unwrap()
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
        Limits::new(5_000 * D, 500, 8_000 * D, 100 * D, 99, 10 * SEC).unwrap(),
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
    let lim = Limits::new(2_000 * D, 300, 3_000 * D, 10_000_000 * D, 40, 5 * SEC).unwrap();
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
