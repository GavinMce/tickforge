use tf_core::Event;
use tf_risk::{Gateway, Limits};
use tf_strategy::{MomentumLong, MomentumParams, SimBroker, StrategyId, TrendParams};

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
    let (events, labels) = demo_session(1, 330, 1, 0, 0);
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
        demo_session(4, 330, 1, 1, 0).0,
        demo_session(4, 330, 1, 1, 0).1,
        &BacktestConfig::default(),
    );
    let b = go(
        demo_session(4, 330, 1, 1, 0).0,
        demo_session(4, 330, 1, 1, 0).1,
        &BacktestConfig::default(),
    );
    assert_eq!(a, b);
    assert_ne!(a.outcome_hash, 0);
    let c = go(
        demo_session(5, 330, 1, 1, 0).0,
        demo_session(5, 330, 1, 1, 0).1,
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
    let (events, labels) = demo_session(1, 330, 1, 0, 0);
    let n = labels.len();
    let mut host = MomentumLong::new(StrategyId(1), MomentumParams::default(), n)
        .unwrap()
        .host(n);
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
    let (events, labels) = demo_session(1, 330, 1, 0, 0);
    let n = labels.len();
    let mut host = MomentumLong::new(StrategyId(1), MomentumParams::default(), n)
        .unwrap()
        .host(n);
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
    let (events, labels) = demo_session(1, 330, 1, 0, 0);
    let r = go(events, labels, &with_limits(limits(100, 1_000, 20, 10)));
    assert_eq!(r.accepted, 0);
    assert!(r.rejections["max_notional"] >= 1, "{:?}", r.rejections);
    assert_eq!(r.fills, 0);
}

#[test]
fn the_rate_limit_applies_to_exits_and_the_book_stays_consistent_when_one_is_refused() {
    let (events, labels) = demo_session(1, 330, 1, 0, 0);
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
    let (events, labels) = demo_session(1, 330, 1, 0, 0);
    let n = labels.len();
    let mut host = MomentumLong::new(StrategyId(1), cfg.params, n)
        .unwrap()
        .host(n);
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

#[test]
fn traced_run_matches_plain_run_and_explains_the_entry() {
    let (events, labels) = demo_session_with_lead(1, 520, 2, 2, 2, 120);
    let cfg = BacktestConfig::default();
    let plain = go(events.clone(), labels.clone(), &cfg);
    let t = momentum_backtest_traced(events, labels, &cfg).unwrap();
    assert_eq!(
        (t.result.intents, t.result.accepted, t.result.fills),
        (plain.intents, plain.accepted, plain.fills),
        "tracing must not change the run"
    );
    assert_eq!(t.entries.len(), 2, "both healthy runners enter");
    assert!(
        !t.declines.is_empty(),
        "the dangerous runners are watched then declined"
    );
    assert_eq!(t.result.decisions.len() as u64, plain.intents);
    assert_eq!(t.result.fill_log.len() as u64, plain.fills);
    let e = &t.entries[0];
    assert!(e.impulse_permille >= 300, "entry only after a real impulse");
}

#[test]
fn export_is_deterministic_and_carries_the_decision_record() {
    let (events, labels) = demo_session_with_lead(1, 520, 2, 2, 2, 120);
    let cfg = BacktestConfig::default();
    let run = |ev: &[Event]| {
        let t = momentum_backtest_traced(ev.iter().copied(), labels.clone(), &cfg).unwrap();
        export::export_json(
            ev,
            &labels,
            &cfg,
            &t.result,
            &t.entries,
            &t.declines,
            &export::ExportMeta {
                strategy: "momentum",
                seed: 1,
                secs: 520,
            },
        )
    };
    let a = run(&events);
    assert_eq!(a, run(&events), "same input, same bytes");
    for key in [
        "\"trades\"",
        "\"orders\"",
        "\"stop_path\"",
        "\"conditions\"",
        "\"declines\"",
        "\"hits\"",
    ] {
        assert!(a.contains(key), "missing {key}");
    }
    assert!(a.contains("pullback went on too long") || a.contains("dangerous"));
}

#[test]
fn rules_in_the_config_decide_and_the_export_records_them() {
    let (events, labels) = demo_session_with_lead(1, 520, 2, 2, 2, 120);
    let base = BacktestConfig::default();
    let never = BacktestConfig {
        rules: Some(
            RuleSet::parse(
                &tf_strategy::rules::MOMENTUM_RULES
                    .replace("higher_lows >= @min_higher_lows", "higher_lows >= 99"),
            )
            .unwrap(),
        ),
        ..BacktestConfig::default()
    };
    let a = go(events.clone(), labels.clone(), &base);
    let b = go(events.clone(), labels.clone(), &never);
    assert!(a.intents > 0);
    assert_eq!(b.intents, 0, "the edited rules never enter");

    let export = |cfg: &BacktestConfig| {
        let t = momentum_backtest_traced(events.iter().copied(), labels.clone(), cfg).unwrap();
        export::export_json(
            &events,
            &labels,
            cfg,
            &t.result,
            &t.entries,
            &t.declines,
            &export::ExportMeta {
                strategy: "momentum",
                seed: 1,
                secs: 520,
            },
        )
    };
    let ja = export(&base);
    let id = format!("{:016x}", RuleSet::momentum().fingerprint());
    assert!(
        ja.contains(&format!("\"rules\":{{\"id\":\"{id}\"")),
        "the rule version is recorded"
    );
    assert!(
        ja.contains("enter all: depth >= @min_depth_permille;"),
        "with its text"
    );
    assert!(
        ja.contains("\\nenter all"),
        "newlines are escaped, not dropped"
    );
    for key in [
        "\"stage\":\"enter\"",
        "\"feature\":\"higher_lows\"",
        "\"param\":\"max_depth_permille\"",
        "\"param\":null",
    ] {
        assert!(ja.contains(key), "missing {key}");
    }
    let jb = export(&never);
    assert_ne!(ja, jb);
    assert!(!jb.contains(&id), "the edited rules have their own id");
    assert!(jb.contains("higher_lows >= 99"));
}

// ---- comparing runs ----

fn rt(
    instrument: u32,
    t_in: u64,
    t_out: Option<u64>,
    entry_px: i64,
    exit_px: Option<i64>,
) -> export::RoundTrip {
    export::RoundTrip {
        instrument,
        order: 0,
        exit_order: None,
        qty: 100,
        entry_px,
        exit_px,
        t_in: t_in * SEC,
        t_out: t_out.map(|t| t * SEC),
        pnl: None,
    }
}

#[test]
fn trades_line_up_by_instrument_and_overlap_and_read_as_same_changed_or_one_sided() {
    use compare::{Kind, compare};
    let a = [
        rt(1, 100, Some(200), 10, Some(12)), // same in B
        rt(2, 100, Some(200), 10, Some(12)), // changed (exit price)
        rt(3, 100, Some(200), 10, Some(12)), // only in A
        rt(4, 100, Some(200), 10, Some(12)), // B's is on another symbol: both one-sided
        rt(5, 100, None, 10, None),          // still open in both
    ];
    let b = [
        rt(1, 100, Some(200), 10, Some(12)),
        rt(2, 100, Some(200), 10, Some(13)),
        rt(6, 120, Some(180), 10, Some(12)), // only in B
        rt(5, 150, None, 11, None),          // overlaps an open trade
        rt(1, 300, Some(400), 10, Some(12)), // later trade on 1: does not overlap, so only in B
    ];
    let pairs = compare(&a, &[], &b, &[], 0);
    let find = |f: &dyn Fn(&compare::Pair) -> bool| pairs.iter().filter(|p| f(p)).count();
    assert_eq!(find(&|p| p.kind == Kind::Same), 1);
    let changed: Vec<_> = pairs.iter().filter(|p| p.kind == Kind::Changed).collect();
    assert_eq!(changed.len(), 2);
    assert_eq!(changed[0].diff, ["exit price"]);
    assert_eq!(
        changed[1].diff,
        ["entry time", "entry price"],
        "open vs open on 5"
    );
    assert_eq!(find(&|p| p.kind == Kind::OnlyA), 2, "3 and 4");
    assert_eq!(
        find(&|p| p.kind == Kind::OnlyB),
        2,
        "6 and the later trade on 1"
    );
    // Every trade of each run appears in exactly one pair.
    let mut seen_a: Vec<_> = pairs.iter().filter_map(|p| p.a).collect();
    let mut seen_b: Vec<_> = pairs.iter().filter_map(|p| p.b).collect();
    seen_a.sort_unstable();
    seen_b.sort_unstable();
    assert_eq!(seen_a, [0, 1, 2, 3, 4]);
    assert_eq!(seen_b, [0, 1, 2, 3, 4]);
    // Ordered by time.
    let times: Vec<u64> = pairs
        .iter()
        .map(|p| p.a.map_or_else(|| b[p.b.unwrap()].t_in, |i| a[i].t_in))
        .collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    // Two trades in one run cannot both claim the same trade in the other.
    let a2 = [
        rt(1, 100, Some(200), 10, Some(12)),
        rt(1, 150, Some(250), 10, Some(12)),
    ];
    let b2 = [rt(1, 100, Some(250), 10, Some(12))];
    let p2 = compare(&a2, &[], &b2, &[], 0);
    assert_eq!(p2.iter().filter(|p| p.b.is_some()).count(), 1);
    assert_eq!(p2.iter().filter(|p| p.kind == Kind::OnlyA).count(), 1);
}

#[test]
fn a_rule_edit_shows_which_trades_vanished_and_what_the_other_rules_did_instead() {
    use compare::{Kind, compare};
    let (events, labels) = demo_session_with_lead(1, 520, 2, 2, 2, 120);
    let a = BacktestConfig::default();
    let b = BacktestConfig {
        rules: Some(
            RuleSet::parse(
                &tf_strategy::rules::MOMENTUM_RULES
                    .replace("higher_lows >= @min_higher_lows", "higher_lows >= 99"),
            )
            .unwrap(),
        ),
        ..BacktestConfig::default()
    };
    let ta = momentum_backtest_traced(events.iter().copied(), labels.clone(), &a).unwrap();
    let tb = momentum_backtest_traced(events.iter().copied(), labels, &b).unwrap();
    let (ra, rb) = (
        export::round_trips(&ta.result),
        export::round_trips(&tb.result),
    );
    assert_eq!(
        (ra.len(), rb.len()),
        (2, 0),
        "both healthy runners trade under the built-in rules"
    );
    let t0 = events[0].ts_recv();
    let pairs = compare(&ra, &ta.declines, &rb, &tb.declines, t0);
    assert_eq!(pairs.len(), 2);
    for p in &pairs {
        assert_eq!(p.kind, Kind::OnlyA);
        assert!(p.note.contains("never entered or gave up"), "{}", p.note);
    }
    // Swap: the trade appears in B. The other run (A, as A) has no decline on it either.
    let flipped = compare(&rb, &tb.declines, &ra, &ta.declines, t0);
    assert_eq!(flipped[0].kind, Kind::OnlyB);
    // With a decline on that symbol before the entry, the note says what it was.
    let d = tb
        .declines
        .iter()
        .chain(ta.declines.iter())
        .next()
        .expect("the dangerous runners are declined");
    let fake = [rt(
        d.instrument,
        (d.ts - t0) / SEC + 5,
        Some((d.ts - t0) / SEC + 50),
        10,
        Some(12),
    )];
    let fake = [export::RoundTrip {
        t_in: d.ts + 5 * SEC,
        t_out: Some(d.ts + 50 * SEC),
        ..fake[0]
    }];
    let n = compare(&fake, &[], &[], std::slice::from_ref(d), t0);
    assert_eq!(n[0].kind, Kind::OnlyA);
    assert!(n[0].note.contains("gave up on it at"), "{}", n[0].note);
    let json = compare::to_json(&n);
    assert!(
        json.starts_with("{\"a\":0,\"b\":1,\"pairs\":[{\"a\":0,\"b\":null,\"kind\":\"only_a\""),
        "{json}"
    );
}

#[test]
fn the_page_embeds_the_data_once_and_cannot_be_broken_out_of() {
    let data = "{\"label\":\"</script><b>\"}";
    let frag = export::fragment(data);
    assert!(!frag.contains("__TF_DATA__"));
    assert!(
        frag.contains("{\"label\":\"<\\/script><b>\"}"),
        "closing tags are escaped"
    );
    assert_eq!(
        frag.matches("</script>").count(),
        export::VIEWER.matches("</script>").count()
    );
    let page = export::page(data);
    assert!(page.starts_with("<!doctype html>"));
    assert!(page.contains("<title>Trade Explorer</title>"));
    // One run stays itself; two become runs plus the comparison.
    assert_eq!(export::bundle(&["{\"x\":1}".into()], None), "{\"x\":1}");
    assert_eq!(
        export::bundle(&["{\"x\":1}".into(), "{\"y\":2}".into()], Some("{\"c\":3}")),
        "{\"runs\":[{\"x\":1},{\"y\":2}],\"compare\":{\"c\":3}}"
    );
}

#[test]
fn matching_prefers_the_overlapping_trade_and_each_difference_is_reported_on_its_own() {
    use compare::{Kind, compare};
    // Two trades on one symbol, listed in opposite orders: overlap decides, not position.
    let a = [
        rt(1, 100, Some(200), 10, Some(12)),
        rt(1, 300, Some(400), 11, Some(13)),
    ];
    let b = [
        rt(1, 300, Some(400), 11, Some(13)),
        rt(1, 100, Some(200), 10, Some(12)),
    ];
    let p = compare(&a, &[], &b, &[], 0);
    assert_eq!(p.len(), 2);
    assert!(
        p.iter().all(|p| p.kind == Kind::Same && p.note.is_empty()),
        "{p:?}"
    );
    assert_eq!(
        (p[0].a, p[0].b, p[1].a, p[1].b),
        (Some(0), Some(1), Some(1), Some(0))
    );
    // A trade that ended before this one began is not its match, whatever the order.
    let late = [rt(1, 300, Some(400), 11, Some(13))];
    let both = [
        rt(1, 100, Some(200), 10, Some(12)),
        rt(1, 300, Some(400), 11, Some(13)),
    ];
    let p = compare(&late, &[], &both, &[], 0);
    let paired = p.iter().find(|p| p.a == Some(0)).unwrap();
    assert_eq!((paired.b, paired.kind), (Some(1), Kind::Same), "{p:?}");
    assert_eq!(p.iter().filter(|p| p.kind == Kind::OnlyB).count(), 1);
    // One difference at a time.
    let base = rt(1, 100, Some(200), 10, Some(12));
    let cases = [
        (export::RoundTrip { qty: 90, ..base }, "size"),
        (
            export::RoundTrip {
                t_out: Some(210 * SEC),
                ..base
            },
            "exit time",
        ),
        (
            export::RoundTrip {
                t_in: 101 * SEC,
                ..base
            },
            "entry time",
        ),
        (
            export::RoundTrip {
                entry_px: 11,
                ..base
            },
            "entry price",
        ),
        (
            export::RoundTrip {
                exit_px: Some(13),
                ..base
            },
            "exit price",
        ),
        (
            export::RoundTrip {
                t_out: None,
                exit_px: None,
                ..base
            },
            "exit time",
        ),
    ];
    for (changed, first) in cases {
        let p = compare(&[base], &[], &[changed], &[], 0);
        assert_eq!(p[0].kind, Kind::Changed);
        assert_eq!(p[0].diff[0], first, "{p:?}");
        if first != "exit time" || changed.exit_px.is_some() {
            assert_eq!(p[0].diff.len(), 1, "{first}: {p:?}");
        }
    }
}

#[test]
fn the_note_names_the_latest_decline_on_that_symbol_before_the_entry() {
    use compare::compare;
    let (events, labels) = demo_session_with_lead(1, 520, 2, 2, 2, 120);
    let t = momentum_backtest_traced(events.iter().copied(), labels, &BacktestConfig::default())
        .unwrap();
    let t0 = events[0].ts_recv();
    let base = t.declines[0].clone();
    let early = tf_strategy::Decline {
        ts: t0 + 100 * SEC,
        reason: tf_strategy::DeclineReason::TooOld,
        ..base.clone()
    };
    let late = tf_strategy::Decline {
        ts: t0 + 150 * SEC,
        reason: tf_strategy::DeclineReason::Dangerous,
        ..base.clone()
    };
    let after = tf_strategy::Decline {
        ts: t0 + 400 * SEC,
        ..base.clone()
    };
    let other = tf_strategy::Decline {
        instrument: base.instrument + 1,
        ts: t0 + 160 * SEC,
        ..base.clone()
    };
    let trade = export::RoundTrip {
        t_in: t0 + 200 * SEC,
        t_out: Some(t0 + 260 * SEC),
        ..rt(base.instrument, 0, None, 10, None)
    };
    let p = compare(&[trade], &[], &[], &[early, late, after, other], t0);
    assert!(
        p[0].note.contains("at 150.0 s: dangerous pullback"),
        "{}",
        p[0].note
    );
}
