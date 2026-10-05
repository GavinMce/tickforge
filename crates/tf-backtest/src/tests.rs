use tf_core::Event;
use tf_risk::{Gateway, Limits};
use tf_strategy::{Host, MomentumLong, MomentumParams, SimBroker, StrategyId, TrendParams};

use super::*;

const SEC: Nanos = NANOS_PER_SEC;

fn go(events: Vec<Event>, labels: Vec<String>, cfg: &BacktestConfig) -> BacktestResult {
    momentum_backtest(events, labels, cfg).unwrap()
}

fn limits(order_notional: u64, daily_loss: u64, orders: u32, window_secs: u64) -> Limits {
    Limits::new(
        dollars(order_notional),
        5_000,
        dollars(20_000),
        dollars(daily_loss),
        orders,
        window_secs * SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(dollars(100_000), 20_000, 1000).unwrap())
}

fn with_limits(l: Limits) -> BacktestConfig {
    BacktestConfig {
        limits: l,
        ..BacktestConfig::default()
    }
}

#[test]
fn a_healthy_runner_goes_through_the_gateway_and_both_books_agree() {
    let (events, labels) = demo_session(1, 260, 1, 0, 0);
    let r = go(events, labels, &BacktestConfig::default());
    assert_eq!(
        (r.intents, r.accepted, r.fills),
        (2, 2, 2),
        "an entry and an exit, both accepted and filled"
    );
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert_eq!(r.bookkeeping_errors, 0);
    assert!(r.books_agree());
    assert_eq!(r.gateway_positions, [0]);
    assert_eq!((r.report.total.trades, r.report.total.open_pnl), (1, 0));
    assert_eq!(
        r.audit.len() as u64,
        r.intents,
        "every decision is on the audit log"
    );
}

#[test]
fn the_gateway_and_the_report_agree_on_pnl() {
    let (events, labels) = demo_session(2, 300, 2, 1, 1);
    let r = go(events, labels, &BacktestConfig::default());
    assert!(r.fills >= 4);
    let diff = r.gateway_pnl - r.report.total.net_pnl();
    assert!(
        diff.abs() <= i128::from(r.report.total.shares),
        "gateway {} vs report {}",
        r.gateway_pnl,
        r.report.total.net_pnl()
    );
}

#[test]
fn a_mixed_session_trades_only_the_healthy_names() {
    let (events, labels) = demo_session(3, 320, 2, 2, 2);
    let r = go(events, labels, &BacktestConfig::default());
    let t = |l: &str| r.report.by_label.get(l).map_or(0, |s| s.trades);
    assert_eq!((t("healthy"), t("dangerous"), t("quiet")), (2, 0, 0));
    assert!(r.books_agree() && r.bookkeeping_errors == 0);
    assert!(r.rejections.is_empty());
}

#[test]
fn the_same_session_gives_the_same_outcome() {
    let a = go(
        demo_session(4, 260, 1, 1, 0).0,
        demo_session(4, 260, 1, 1, 0).1,
        &BacktestConfig::default(),
    );
    let b = go(
        demo_session(4, 260, 1, 1, 0).0,
        demo_session(4, 260, 1, 1, 0).1,
        &BacktestConfig::default(),
    );
    assert_eq!(a, b);
    assert_ne!(a.outcome_hash, 0);
    let c = go(
        demo_session(5, 260, 1, 1, 0).0,
        demo_session(5, 260, 1, 1, 0).1,
        &BacktestConfig::default(),
    );
    assert_ne!(
        a.outcome_hash, c.outcome_hash,
        "a different session hashes differently"
    );
}

// ---- the gateway really is in the way ----

#[test]
fn a_kill_switch_stops_the_entry_and_the_strategy_hears_about_it() {
    let (events, labels) = demo_session(1, 260, 1, 0, 0);
    let n = labels.len();
    let mut host = Host::new(
        MomentumLong::new(StrategyId(1), MomentumParams::default(), n).unwrap(),
        n,
    );
    let mut broker = SimBroker::new(BacktestConfig::default().sim, n);
    let mut gateway = Gateway::new(default_limits(), n);
    gateway.engage_kill_switch();
    let r = run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        events,
        |_, _| {},
    )
    .unwrap();
    assert_eq!((r.fills, r.accepted), (0, 0));
    assert!(r.rejections["kill_switch"] >= 1, "{:?}", r.rejections);
    assert_eq!(
        host.strategy().positions(),
        0,
        "the strategy freed the slot when told no"
    );
    assert!(host.strategy().stats().entries_failed >= 1);
    assert_eq!(r.report.total.trades, 0);
}

#[test]
fn a_kill_switch_engaged_mid_trade_still_lets_the_exit_through() {
    let (events, labels) = demo_session(1, 260, 1, 0, 0);
    let n = labels.len();
    let mut host = Host::new(
        MomentumLong::new(StrategyId(1), MomentumParams::default(), n).unwrap(),
        n,
    );
    let mut broker = SimBroker::new(BacktestConfig::default().sim, n);
    let mut gateway = Gateway::new(default_limits(), n);
    // The moment the gateway's book shows a position, pull the switch.
    let r = run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        events,
        |_, g| {
            if g.position(0) > 0 {
                g.engage_kill_switch();
            }
        },
    )
    .unwrap();
    assert!(gateway.kill_switch_engaged());
    assert_eq!((r.accepted, r.fills), (2, 2), "entry and exit");
    assert!(
        r.rejections.is_empty(),
        "closes are never blocked: {:?}",
        r.rejections
    );
    assert_eq!(r.gateway_positions, [0]);
}

#[test]
fn an_order_cap_below_the_entry_size_rejects_it() {
    let (events, labels) = demo_session(1, 260, 1, 0, 0);
    let r = go(events, labels, &with_limits(limits(100, 1_000, 20, 10)));
    assert_eq!(r.accepted, 0);
    assert!(r.rejections["max_notional"] >= 1, "{:?}", r.rejections);
    assert_eq!(r.fills, 0);
}

#[test]
fn the_rate_limit_applies_to_exits_and_the_book_stays_consistent_when_one_is_refused() {
    let (events, labels) = demo_session(1, 260, 1, 0, 0);
    // One order in ten thousand seconds: the entry gets through, the exits cannot.
    let r = go(
        events,
        labels,
        &with_limits(limits(5_000, 1_000, 1, 10_000)),
    );
    assert_eq!((r.accepted, r.fills), (1, 1));
    assert!(r.rejections["order_rate"] >= 1, "{:?}", r.rejections);
    assert!(
        r.gateway_positions[0] > 0,
        "stuck long: the exit was refused"
    );
    assert!(r.books_agree());
    assert!(r.report.total.open_pnl != 0 || r.report.total.trades == 0);
    // The open position is marked the same way by the gateway and by the report.
    assert!(r.report.total.open_pnl != 0);
    let diff = r.gateway_pnl - r.report.total.net_pnl();
    assert!(
        diff.abs() <= i128::from(r.report.total.shares),
        "gateway {} vs report {}",
        r.gateway_pnl,
        r.report.total.net_pnl()
    );
    assert_eq!(r.bookkeeping_errors, 0);
}

#[test]
fn an_entry_that_expires_unfilled_frees_the_gateway_and_the_strategy() {
    // Fifteen seconds in flight: by the time the order arrives, the ask is far outside its collar.
    let cfg = BacktestConfig {
        sim: SimConfig {
            latency_ns: 15 * SEC,
            borrow_bps_per_year: 0,
        },
        ..BacktestConfig::default()
    };
    let (events, labels) = demo_session(1, 260, 1, 0, 0);
    let n = labels.len();
    let mut host = Host::new(MomentumLong::new(StrategyId(1), cfg.params, n).unwrap(), n);
    let mut broker = SimBroker::new(cfg.sim, n);
    let mut gateway = Gateway::new(cfg.limits, n);
    let r = run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        events,
        |_, _| {},
    )
    .unwrap();
    assert!(r.accepted >= 1);
    assert_eq!(r.fills, 0, "it never filled");
    assert_eq!(
        r.gateway_working, 0,
        "the gateway let go of the expired order"
    );
    assert_eq!(host.strategy().positions(), 0);
    assert!(host.strategy().stats().entries_failed >= 1);
    assert_eq!(r.bookkeeping_errors, 0);
}

#[test]
fn a_refused_exit_is_retried_and_goes_through_when_the_rate_window_has_passed() {
    // One order per 200 s: the exit comes 120 s after the entry (the maximum hold), is refused,
    // and is retried every second until the entry has left the window.
    let (events, labels) = demo_session(1, 400, 1, 0, 0);
    let r = go(events, labels, &with_limits(limits(5_000, 1_000, 1, 200)));
    assert_eq!((r.accepted, r.fills), (2, 2), "{:?}", r.rejections);
    assert!(
        r.rejections["order_rate"] >= 1,
        "the exit was turned away at least once"
    );
    assert_eq!(r.gateway_positions, [0]);
    assert!(r.books_agree());
}

#[test]
fn a_daily_loss_limit_latches_and_blocks_later_entries() {
    // Entering mid-pullback loses (see ADR 0015), so use that to produce a loss.
    let params = MomentumParams {
        min_higher_lows: 0,
        ..MomentumParams::default()
    };
    let cfg = BacktestConfig {
        params,
        limits: limits(5_000, 5, 50, 10),
        ..BacktestConfig::default()
    };
    let (events, labels) = demo_session(1, 400, 4, 0, 0);
    let r = go(events, labels, &cfg);
    assert!(
        r.rejections.get("daily_loss_limit").copied().unwrap_or(0) >= 1,
        "{:?} accepted {}",
        r.rejections,
        r.accepted
    );
    assert!(r.report.total.net_pnl() < 0);
    assert!(r.books_agree());
    // A generous limit lets the same session trade every name.
    let free = go(
        demo_session(1, 400, 4, 0, 0).0,
        demo_session(1, 400, 4, 0, 0).1,
        &BacktestConfig {
            params,
            ..BacktestConfig::default()
        },
    );
    assert!(free.accepted > r.accepted);
}

#[test]
fn every_strategy_parameter_error_is_reported_not_run() {
    let cfg = BacktestConfig {
        params: MomentumParams {
            max_qty: 0,
            ..MomentumParams::default()
        },
        ..BacktestConfig::default()
    };
    let (events, labels) = demo_session(1, 30, 1, 0, 0);
    assert!(momentum_backtest(events, labels, &cfg).is_err());
    assert!(
        momentum_backtest(
            Vec::new(),
            vec!["bad label".into()],
            &BacktestConfig::default()
        )
        .is_err()
    );
}

// ---- the indicator example strategy through the same loop ----

#[test]
fn the_trend_example_runs_through_the_gateway_and_the_books_agree() {
    let (events, labels) = demo_session_with_lead(1, 1800, 1, 0, 1, 420);
    let r = trend_backtest(events, labels, &BacktestConfig::default()).unwrap();
    assert_eq!(
        (r.intents, r.accepted, r.fills),
        (2, 2, 2),
        "an entry and an exit"
    );
    assert!(r.rejections.is_empty(), "{:?}", r.rejections);
    assert!(r.books_agree() && r.bookkeeping_errors == 0);
    assert_eq!(r.report.by_label["healthy"].trades, 1);
    assert_eq!(
        r.report.by_label.get("quiet").map_or(0, |s| s.trades),
        0,
        "the quiet name is left alone"
    );
    let diff = r.gateway_pnl - r.report.total.net_pnl();
    assert!(diff.abs() <= i128::from(r.report.total.shares));
    assert_eq!(r.audit.len() as u64, r.intents);
}

#[test]
fn the_trend_example_obeys_the_gateway_too() {
    let (events, labels) = demo_session_with_lead(1, 1800, 1, 0, 0, 420);
    let capped = trend_backtest(
        events.clone(),
        labels.clone(),
        &with_limits(limits(100, 1_000, 20, 10)),
    )
    .unwrap();
    assert_eq!((capped.accepted, capped.fills), (0, 0));
    assert!(capped.rejections["max_notional"] >= 1);
    let same = |c: &BacktestConfig| trend_backtest(events.clone(), labels.clone(), c).unwrap();
    assert_eq!(
        same(&BacktestConfig::default()),
        same(&BacktestConfig::default()),
        "reproducible"
    );
    let bad = BacktestConfig {
        trend: TrendParams {
            fast_period: 9,
            ..TrendParams::default()
        },
        ..BacktestConfig::default()
    };
    assert!(
        trend_backtest(events, labels, &bad).is_err(),
        "fast >= slow is refused, not run"
    );
}
