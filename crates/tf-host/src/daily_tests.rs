use tf_budget::{Group, LossLimits, Strategy as BudgetStrategy, Tree};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Trade, TradeFlags};
use tf_ingest::{Lost, Stats};
use tf_ledger::MemStore;
use tf_risk::Budgets;
use tf_universe::Spec;

use crate::daily::ns;
use crate::tests::*;
use crate::{
    CaptureFacts, DailyReport, GapNote, Host, HostConfig, Route, StrategyDef, SystemInputs, money,
    replay_events, report, runner,
};

fn run_day(cfg: &HostConfig, defs: &[StrategyDef], tape: &[Event]) -> Host<MemStore> {
    let mut h = host(cfg).record();
    certify_all(&mut h, cfg, defs, tape);
    for e in tape {
        h.on_event(e).unwrap();
    }
    h.end_of_day(tape.last().unwrap().ts_recv()).unwrap();
    h
}

#[test]
fn money_is_dollars_and_cents_cut_toward_zero() {
    for (raw, shown) in [
        (0i128, "$0.00"),
        (1_000_000_000, "$1.00"),
        (1_239_999_999, "$1.23"),
        (-1_239_999_999, "-$1.23"),
        (9_999_999, "$0.00"),
        (-9_999_999, "$0.00"),
        (10_000_000, "$0.01"),
        (-10_000_000, "-$0.01"),
        (100_000_000_000, "$100.00"),
        (1_000_000_000_000, "$1,000.00"),
        (1_234_567_890_000_000, "$1,234,567.89"),
        (-123_456_789_000_000_000, "-$123,456,789.00"),
    ] {
        assert_eq!(money(raw), shown, "{raw}");
    }
}

#[test]
fn durations_are_shown_in_the_unit_that_fits() {
    for (n, shown) in [
        (0, "0 ns"),
        (999, "999 ns"),
        (1_000, "1.000 us"),
        (1_500, "1.500 us"),
        (2_097_151, "2.097 ms"),
        (1_000_000, "1.000 ms"),
        (999_999_999, "999.999 ms"),
        (1_000_000_000, "1.000 s"),
        (12_345_678_901, "12.345 s"),
        (u64::MAX, "more than 9 s"),
    ] {
        assert_eq!(ns(n), shown, "{n}");
    }
}

fn two() -> Vec<StrategyDef> {
    vec![
        def(1, LOW, Plan::Buy { qty: 100, n: 6 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 6 }, Route::Sim),
    ]
}

#[test]
fn the_report_says_what_each_strategy_did_and_where_the_fills_came_from() {
    let cfg = config(2);
    let tape = market(12, flat);
    let h = run_day(&cfg, &two(), &tape);
    let r = DailyReport::build(&h, "2026-10-02", SystemInputs::default(), None);
    assert_eq!(r.strategies.len(), 2);
    let (a, b) = (&r.strategies[0], &r.strategies[1]);
    assert_eq!((a.id, a.name.as_str(), a.route), (1, "trader1", Route::Sim));
    assert_eq!((a.intents, a.accepted, a.fills, a.shares), (6, 6, 6, 600));
    assert_eq!((b.intents, b.accepted, b.fills, b.shares), (6, 6, 6, 300));
    // 600 shares at $20.01 (the ask) and 300 likewise.
    assert_eq!(a.turnover, 600 * 20_010_000_000);
    assert_eq!(b.turnover, 300 * 20_010_000_000);
    assert_eq!((a.static_symbols, a.members_now, a.ever_held), (6, 6, 0));
    assert_eq!(a.top_symbols, [("S02".to_owned(), 600)]);
    assert_eq!(b.top_symbols, [("S08".to_owned(), 300)]);
    // P&L is what the gateway says: the position marked at the last trade ($20.00) against its cost.
    let g = h.journal().gateway();
    assert_eq!(
        (a.realized, a.unrealized),
        (g.strategy_realized(1), g.strategy_unrealized(1))
    );
    assert_eq!(a.unrealized, -(600 * 10_000_000));
    assert_eq!(a.state, "ran all day");
    let text = r.render();
    assert!(text.starts_with("Daily report: 2026-10-02\n"));
    assert!(text.contains("No fill in this report is real"), "{text}");
    assert!(text.contains("1 trader1 [simulated fills] ran all day"));
    assert!(
        text.contains(
            "6 intents, 6 accepted by the gateway, 6 fills of 600 shares, $12,006.00 traded"
        ),
        "{text}"
    );
    assert!(text.contains("refused:  none by the gateway"));
    assert!(
        text.contains("p&l:      $0.00 realized, -$6.00 unrealized, -$6.00 in all"),
        "{text}"
    );
    assert!(text.contains("traded most (shares): S02 600"));
    assert!(
        text.contains("universe ")
            && text.contains("6 symbols chosen before the open, 6 held at the end, 11 reviews"),
        "{text}"
    );
}

#[test]
fn what_the_gateway_refused_is_listed_by_reason_and_a_paper_strategy_is_labelled_optimistic() {
    // Strategy 1 has a 1% share, so its own budget refuses most of what it asks for.
    let mut cfg = config(2);
    let ids = [(1u16, "s1".to_owned()), (2, "s2".to_owned())];
    let tree = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            BudgetStrategy {
                id: "s1".into(),
                share: 100,
            },
            BudgetStrategy {
                id: "s2".into(),
                share: 9_900,
            },
        ],
    }])
    .unwrap();
    cfg.budgets = Some(Budgets::new(tree, 100_000 * D, ids).unwrap());
    let tape = market(12, flat);
    let defs = vec![
        def(1, LOW, Plan::Buy { qty: 100, n: 6 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 100, n: 6 }, Route::Paper),
    ];
    let mut h = Host::new(cfg.clone(), reference(), MemStore::from_records(vec![]))
        .unwrap()
        .with_paper(Box::new(tf_strategy::sim::SimBroker::new(
            cfg.sim,
            SYMBOLS as usize,
        )));
    for d in &defs {
        h.install_for_test(d);
    }
    for e in &tape {
        h.on_event(e).unwrap();
    }
    let r = DailyReport::build(&h, "x", SystemInputs::default(), None);
    let a = &r.strategies[0];
    assert!(a.accepted < 6 && !a.rejections.is_empty(), "{a:?}");
    assert_eq!(
        a.rejections.iter().map(|x| x.1).sum::<u64>(),
        a.intents - a.accepted
    );
    assert!(
        a.rejections
            .iter()
            .all(|(reason, _)| reason == "strategy_budget" || reason == "group_budget"),
        "{:?}",
        a.rejections
    );
    let text = r.render();
    assert!(
        text.contains(&format!(
            "refused:  {} by the gateway (",
            a.intents - a.accepted
        )),
        "{text}"
    );
    assert!(
        text.contains("2 trader2 [paper fills, optimistic]"),
        "{text}"
    );
    assert!(text.contains("paper fills are optimistic"));
    assert!(text.contains("1 trader1 [simulated fills]"));
}

#[test]
fn the_system_section_has_the_rates_the_busiest_second_and_the_lag_of_the_feed() {
    let cfg = config(1);
    let mut tape = market(10, flat);
    // Seconds 0 to 9 have 24 quotes... and 12 symbols x (1 + sym % 3) trades: 12 + 24 = 36 events each.
    // Give second 4 a burst of 100 more trades of S00.
    for k in 0..100u64 {
        let ts = T0 + 4 * SEC + 500 * MS + k;
        tape.push(Event::Trade(Trade {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(2_000),
            size: 1,
            flags: TradeFlags::NONE,
        }));
    }
    tape.sort_by_key(Event::ts_recv);
    let n = tape.len() as u64;
    let mut h = host(&cfg);
    h.install_for_test(&def(1, LOW, Plan::Buy { qty: 10, n: 1 }, Route::Sim));
    for e in &tape {
        h.on_event(e).unwrap();
    }
    let r = DailyReport::build(&h, "x", SystemInputs::default(), None);
    let y = &r.system;
    assert_eq!(
        (y.events, y.active_seconds, y.peak_events, y.peak_at),
        (n, 10, 36 + 100, T0 + 4 * SEC)
    );
    assert_eq!((y.feed_lag_p50, y.feed_lag_p99, y.feed_lag_max), (0, 0, 0));
    let text = r.render();
    assert!(
        text.contains(&format!(
            "events:     {n} over 10 active seconds, {} a second on average",
            n / 10
        )),
        "{text}"
    );
    assert!(
        text.contains("busiest second: 136 events at 00:01:44.000000000 UTC"),
        "{text}"
    );
    assert!(text.contains("feed lag (received less event time): half within 0 ns, 99% within 0 ns, all within 0 ns"), "{text}");
    assert!(
        text.contains("engine lag: not measured")
            && text.contains("ingest queue: not measured")
            && text.contains("capture:    not reported")
    );
    assert!(text.contains("gaps:       none reported") && text.contains("REPLAY CHECK\n  not run"));
}

#[test]
fn feed_lag_is_a_quantile_bounded_by_the_power_of_two_it_falls_in() {
    let cfg = config(1);
    // 90% of events arrive at once, 10% 100 ms late.
    let mut tape = market(10, flat);
    for (i, e) in tape.iter_mut().enumerate() {
        if i % 10 == 0 {
            if let Event::Trade(t) = e {
                t.hdr.ts_event -= 100 * MS;
            } else if let Event::Quote(q) = e {
                q.hdr.ts_event -= 100 * MS;
            }
        }
    }
    let mut h = host(&cfg);
    for e in &tape {
        h.on_event(e).unwrap();
    }
    assert_eq!(h.feed_lag_quantile(500), 0);
    assert_eq!(h.feed_lag_quantile(890), 0);
    // 100 ms lies between 2^26 and 2^27 ns: reported as at most 2^27 - 1.
    assert_eq!(h.feed_lag_quantile(990), (1 << 27) - 1);
    assert_eq!(h.feed_lag_quantile(1000), (1 << 27) - 1);
    let r = DailyReport::build(&h, "x", SystemInputs::default(), None);
    assert!(
        r.render()
            .contains("half within 0 ns, 99% within 134.217 ms, all within 134.217 ms"),
        "{}",
        r.render()
    );
}

#[test]
fn what_the_driver_measured_is_shown_and_lost_events_are_called_lost() {
    let cfg = config(1);
    let tape = market(5, flat);
    let mut h = host(&cfg);
    h.install_for_test(&def(1, LOW, Plan::Buy { qty: 10, n: 1 }, Route::Sim));
    for e in &tape {
        h.on_event(e).unwrap();
    }
    h.on_gap(Lost::Trades, 120, T0 + SEC, T0 + 2 * SEC);
    h.on_gap(Lost::Control, 3, T0 + 3 * SEC, T0 + 3 * SEC + 5);
    h.on_gap(Lost::Skipped, 2, T0 + 4 * SEC, T0 + 4 * SEC + 9);
    assert_eq!(h.gaps().len(), 3);
    let inputs = SystemInputs {
        ingest: Some(Stats {
            offered: 1_000,
            queued: 880,
            conflated: 50,
            dropped_trades: 120,
            dropped_quotes: 7,
            dropped_control: 3,
            gaps: 2,
            max_depth: 1_999_000,
            ..Stats::default()
        }),
        engine_lag: Some((2_500, 1_200_000)),
        capture: Some(CaptureFacts {
            segments: 3,
            records: 1_000,
            bytes: 12_345,
        }),
    };
    let text = DailyReport::build(&h, "x", inputs, None).render();
    assert!(text.contains("ingest queue: 1000 offered, 50 conflated, 130 dropped (120 trades, 7 quotes, 3 control), 2 gaps, fullest 1999000"), "{text}");
    assert!(
        text.contains("2 gateway skip notices (an unknown number of records"),
        "{text}"
    );
    assert!(text.contains("EVENTS WERE LOST"), "{text}");
    assert!(text.contains("engine lag: 99% within 2.500 us, worst 1.200 ms"));
    assert!(text.contains("capture:    1000 records in 3 segments, 12345 bytes"));
    assert!(text.contains("gaps:       3\n    120 trades lost between 00:01:41.000000000 UTC and 00:01:42.000000000 UTC\n    3 control events lost between"), "{text}");
    // A queue that lost nothing says nothing about loss.
    let calm = SystemInputs {
        ingest: Some(Stats {
            offered: 5,
            conflated: 2,
            ..Stats::default()
        }),
        ..SystemInputs::default()
    };
    let text = DailyReport::build(&h, "x", calm, None).render();
    assert!(!text.contains("EVENTS WERE LOST") && text.contains("0 dropped"));
    let _ = GapNote {
        lost: Lost::Trades,
        count: 1,
        first_ts: 0,
        last_ts: 0,
    };
}

#[test]
fn a_strategy_that_stopped_says_why_and_the_replay_result_is_part_of_the_report() {
    let cfg = config(2);
    let tape = market(14, flat);
    let defs = two();
    let mut h = host(&cfg).record();
    certify_all(&mut h, &cfg, &defs, &tape);
    let cut = tape
        .iter()
        .position(|e| e.ts_recv() >= T0 + 6 * SEC)
        .unwrap();
    for (i, e) in tape.iter().enumerate() {
        if i == cut {
            h.kill_strategy(1, e.ts_recv()).unwrap();
        }
        h.on_event(e).unwrap();
    }
    h.end_of_day(T0 + 20 * SEC).unwrap();
    let log = h.log().unwrap().clone();
    let rep = replay_events(&log, &cfg, &reference(), &defs, &tape).unwrap();
    let ok = report(&log, &rep, &reference().symbols, &defs, 0);
    let r = DailyReport::build(&h, "x", SystemInputs::default(), Some(&ok));
    assert_eq!(
        r.strategies[0].state,
        "STOPPED: killed by an operator and flattened"
    );
    assert!(r.strategies[0].flatten_orders >= 1);
    let text = r.render();
    assert!(
        text.contains("1 trader1 [simulated fills] STOPPED: killed by an operator and flattened"),
        "{text}"
    );
    assert!(text.contains("closed by the host:"), "{text}");
    assert!(
        text.contains("REPLAY CHECK\n  the replay reproduced all"),
        "{text}"
    );
    assert_eq!(r.replay.as_ref().map(|x| x.0), Some(true));
    // A replay that differs is in the report with where.
    let mut other = tape.clone();
    other.remove(other.len() / 3);
    let rep2 = replay_events(&log, &cfg, &reference(), &defs, &other).unwrap();
    let bad = report(&log, &rep2, &reference().symbols, &defs, 9);
    let r2 = DailyReport::build(&h, "x", SystemInputs::default(), Some(&bad));
    assert_eq!(r2.replay.as_ref().map(|x| x.0), Some(false));
    let text = r2.render();
    assert!(
        text.contains("REPLAY CHECK\n  the replay differs at record")
            && text.contains("gave up 9 events"),
        "{text}"
    );
}

#[test]
fn a_dynamic_universe_and_tier_one_requests_are_reported() {
    let mut cfg = config(2);
    cfg.promoter.max_tier1 = 1;
    let tape = market(10, flat);
    let d = StrategyDef {
        id: 1,
        name: "dyn".into(),
        params: String::new(),
        universe: Spec::parse("universe v1\ndynamic top 2 by trades desc keep 2 every 3\n")
            .unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(|| runner(wants(1))),
    };
    let mut h = host(&cfg);
    h.install_for_test(&d);
    for e in &tape {
        h.on_event(e).unwrap();
    }
    let r = DailyReport::build(&h, "x", SystemInputs::default(), None);
    let t = &r.strategies[0];
    assert_eq!((t.static_symbols, t.members_now), (12, 2));
    assert!(t.ever_held >= 2);
    let o = t.tier1.expect("it asked for symbols");
    assert!(o.requests > 0);
    let text = r.render();
    assert!(
        text.contains("held at some time by its dynamic layer"),
        "{text}"
    );
    assert!(text.contains("tier 1:   "), "{text}");
}

#[test]
fn the_same_day_makes_the_same_report() {
    let cfg = config(2);
    let tape = market(12, flat);
    let a = DailyReport::build(
        &run_day(&cfg, &two(), &tape),
        "d",
        SystemInputs::default(),
        None,
    );
    let b = DailyReport::build(
        &run_day(&cfg, &two(), &tape),
        "d",
        SystemInputs::default(),
        None,
    );
    assert_eq!(a, b);
    assert_eq!(a.render(), b.render());
    let _: Nanos = 0;
    let _ = HostConfig::clone(&cfg);
}

#[test]
fn the_symbols_traded_most_come_first_and_ties_go_to_the_lower_id() {
    let cfg = config(1);
    let tape = market(12, flat);
    let mut h = host(&cfg);
    // Reviews 1 to 5 buy the members ranked 1, 2, 0, 1, 2 by trades: S05 twice, S01 twice, S02 once.
    h.install_for_test(&def(1, LOW, Plan::Rotate { qty: 100, n: 5 }, Route::Sim));
    for e in &tape {
        h.on_event(e).unwrap();
    }
    let r = DailyReport::build(&h, "x", SystemInputs::default(), None);
    assert_eq!(
        r.strategies[0].top_symbols,
        [
            ("S01".to_owned(), 200),
            ("S05".to_owned(), 200),
            ("S02".to_owned(), 100)
        ]
    );
    assert!(
        r.render()
            .contains("traded most (shares): S01 200, S05 200, S02 100")
    );
}

#[test]
fn a_quantile_rounds_up_to_the_event_that_reaches_it() {
    // Three events, one of them on time and two 1 ms late: the median is the second, a late one.
    let cfg = config(1);
    let mut h = host(&cfg);
    let tr = |ts: Nanos, lag: Nanos| {
        Event::Trade(Trade {
            hdr: Header {
                ts_event: ts - lag,
                ts_recv: ts,
                seq: ts,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(2_000),
            size: 1,
            flags: TradeFlags::NONE,
        })
    };
    for e in [tr(T0, 0), tr(T0 + 1, MS), tr(T0 + 2, MS)] {
        h.on_event(&e).unwrap();
    }
    assert_eq!(
        h.feed_lag_quantile(500),
        (1 << 20) - 1,
        "1 ms lies between 2^19 and 2^20 ns"
    );
    assert_eq!(h.feed_lag_quantile(300), 0);
    assert_eq!(h.feed_lag_quantile(1000), (1 << 20) - 1);
    // Nothing yet: nothing to say.
    assert_eq!(host(&cfg).feed_lag_quantile(500), 0);
}

#[test]
fn a_loss_is_signed_and_no_work_is_a_zero_mean() {
    let cfg = config(1);
    let h = host(&cfg);
    let r = DailyReport::build(&h, "empty", SystemInputs::default(), None);
    assert!(
        r.render()
            .contains("events:     0 over 0 active seconds, 0 a second on average"),
        "{}",
        r.render()
    );
    assert!(r.strategies.is_empty() && r.render().contains("STRATEGIES (0)"));
    let tape = market(12, flat);
    let mut h = host(&cfg);
    h.install_for_test(&def(1, LOW, Plan::Buy { qty: 100, n: 6 }, Route::Sim));
    for e in &tape {
        h.on_event(e).unwrap();
    }
    assert!(
        DailyReport::build(&h, "x", SystemInputs::default(), None)
            .render()
            .contains("-$6.00 unrealized")
    );
}
