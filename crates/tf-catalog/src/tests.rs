use tf_budget::{Bounds, Group, LossLimits, Strategy as S, Tree};
use tf_core::{Nanos, Px};
use tf_ledger::{Journal, MemStore};
use tf_manifest::{DataRange, Manifest, RunResult};
use tf_risk::{Budgets, GapRule, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{Decision, RejectReason};

use super::*;

const P: i64 = 1_000_000_000;
const SEC: Nanos = 1_000_000_000;

fn limits() -> Limits {
    Limits::new(
        5_000 * 1_000_000_000u128,
        1_000,
        20_000 * 1_000_000_000u128,
        400 * 1_000_000_000u128,
        6,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(100_000 * 1_000_000_000u128, 20_000, 1000).unwrap())
}

fn journal() -> Journal<MemStore> {
    Journal::open(MemStore::from_records(vec![]), limits(), 1)
        .unwrap()
        .0
}

fn budgets() -> Budgets {
    let tree = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            S {
                id: "alpha".into(),
                share: 5_000,
            },
            S {
                id: "beta".into(),
                share: 3_000,
            },
            S {
                id: "gamma".into(),
                share: 2_000,
            },
        ],
    }])
    .unwrap();
    Budgets::new(
        tree,
        30_000 * P as u128,
        [
            (1, "alpha".to_owned()),
            (2, "beta".to_owned()),
            (3, "gamma".to_owned()),
        ],
    )
    .unwrap()
}

fn intent(strategy: u16, seq: u64, side: Side, purpose: Purpose, px: i64) -> Intent {
    let protect = (purpose == Purpose::Open).then(|| Protective {
        stop_trigger: Px::from_raw(px * 9 / 10),
        stop_limit: None,
        take_profit: None,
    });
    Intent {
        id: IntentId {
            strategy: StrategyId(strategy),
            seq,
        },
        instrument: 0,
        side,
        qty: 100,
        purpose,
        pricing: Pricing::Limit(Px::from_raw(px)),
        protect,
        tif: Tif::Day,
        ts: seq * SEC,
        reason: 7,
    }
}

/// Fills a decision completely at its limit; returns the order.
fn trade(j: &mut Journal<MemStore>, i: &Intent, px: i64) {
    let now = i.ts;
    let Decision::Accepted(o) = j.decide(i, now).unwrap() else {
        panic!("{i:?}")
    };
    j.ack(o, now).unwrap();
    j.fill(o, 100, Px::from_raw(px), now).unwrap();
}

fn open(j: &mut Journal<MemStore>, strategy: u16, at: u64) {
    trade(
        j,
        &intent(strategy, at, Side::Buy, Purpose::Open, 5 * P),
        5 * P,
    );
}

/// Sells at `exit` dollars what `open` bought.
fn close(j: &mut Journal<MemStore>, strategy: u16, at: u64, exit: i64) {
    trade(
        j,
        &intent(strategy, at, Side::Sell, Purpose::Close, exit * P),
        exit * P,
    );
}

fn read(j: &Journal<MemStore>, kind: Kind) -> Vec<Run> {
    sessions(
        MemStore::from_records(j.store().records().to_vec()),
        "paper-1",
        kind,
    )
    .unwrap()
}

fn find<'a>(runs: &'a [Run], strategy: &str, day: u32) -> &'a Run {
    runs.iter()
        .find(|r| {
            r.strategy == strategy
                && matches!(&r.source, Source::Ledger { session, .. } if *session == day)
        })
        .unwrap_or_else(|| panic!("no run of {strategy} on day {day} in {runs:?}"))
}

#[test]
fn each_day_of_a_ledger_is_one_session_of_each_strategy_that_did_anything() {
    let mut j = journal();
    j.set_budgets(Some(budgets()), SEC).unwrap();
    open(&mut j, 1, 10);
    close(&mut j, 1, 20, 7); // alpha +$200
    open(&mut j, 2, 30);
    close(&mut j, 2, 40, 4); // beta -$100
    j.rebalance(50 * SEC, Bounds::default(), None).unwrap();
    j.new_day(60 * SEC).unwrap();
    open(&mut j, 1, 100);
    close(&mut j, 1, 110, 4); // alpha -$100
    j.new_day(120 * SEC).unwrap();

    let runs = read(&j, Kind::Paper);
    assert_eq!(runs.len(), 3, "gamma never acted: {runs:?}");
    let a1 = find(&runs, "alpha", 1);
    assert_eq!(a1.net_pnl, Some(200 * P as i128));
    assert_eq!(a1.trades, Some(1));
    assert_eq!(a1.started, 10 * SEC);
    assert_eq!(a1.kind, Kind::Paper);
    assert!(!a1.explorable());
    assert_eq!(
        a1.source,
        Source::Ledger {
            ledger: "paper-1".into(),
            session: 1
        }
    );
    let b1 = find(&runs, "beta", 1);
    assert_eq!(b1.net_pnl, Some(-100 * P as i128));
    let a2 = find(&runs, "alpha", 2);
    assert_eq!(a2.net_pnl, Some(-100 * P as i128));
    assert_eq!(a2.started, 100 * SEC);
    // A strategy's sessions add up to the ledger's own total for it.
    let end = j.gateway();
    for (name, n) in [("alpha", 1u16), ("beta", 2)] {
        let sum: i128 = runs
            .iter()
            .filter(|r| r.strategy == name)
            .map(|r| r.net_pnl.unwrap())
            .sum();
        assert_eq!(sum, end.strategy_realized(n), "{name}");
    }
    // The budget is the one in force when the strategy last decided in the session.
    let day1_budget = a1.budget.unwrap();
    assert_eq!(day1_budget, 15_000 * P as u128);
    assert_eq!(b1.budget, Some(9_000 * P as u128));
    assert_ne!(
        a2.budget,
        Some(day1_budget),
        "the rebalance moved alpha's budget before day 2"
    );
}

#[test]
fn strategies_are_named_s_n_when_no_budget_tree_names_them() {
    let mut j = journal();
    open(&mut j, 4, 10);
    close(&mut j, 4, 20, 6);
    let runs = read(&j, Kind::Live);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].strategy, "s4");
    assert_eq!(runs[0].budget, None);
    assert_eq!(runs[0].kind, Kind::Live);
    assert_eq!(runs[0].net_pnl, Some(100 * P as i128));
}

#[test]
fn a_position_carried_overnight_counts_as_profit_on_the_day_it_closes_and_a_trade_on_the_day_it_opens()
 {
    let mut j = journal();
    open(&mut j, 1, 10);
    j.new_day(30 * SEC).unwrap();
    close(&mut j, 1, 40, 8);
    let runs = read(&j, Kind::Paper);
    let d1 = find(&runs, "s1", 1);
    assert_eq!((d1.net_pnl, d1.trades), (Some(0), Some(1)));
    let d2 = find(&runs, "s1", 2);
    assert_eq!((d2.net_pnl, d2.trades), (Some(300 * P as i128), Some(0)));
}

#[test]
fn a_strategy_whose_only_activity_was_refused_still_has_a_session_with_no_trades() {
    let mut j = journal();
    // Too big for the order notional limit.
    let mut big = intent(1, 10, Side::Buy, Purpose::Open, 5 * P);
    big.qty = 100_000;
    assert!(matches!(
        j.decide(&big, 10 * SEC).unwrap(),
        Decision::Rejected(RejectReason::MaxNotional)
    ));
    let runs = read(&j, Kind::Paper);
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].net_pnl, runs[0].trades), (Some(0), Some(0)));
    assert_eq!(runs[0].started, 10 * SEC);
}

#[test]
fn a_ledger_with_no_activity_has_no_sessions() {
    let mut j = journal();
    j.new_day(5 * SEC).unwrap();
    assert!(read(&j, Kind::Paper).is_empty());
}

#[test]
fn a_ledger_that_cannot_be_replayed_is_an_error_not_an_empty_catalog() {
    assert!(sessions(MemStore::from_records(vec![]), "x", Kind::Paper).is_err());
}

fn run(strategy: &str, kind: Kind, started: Nanos, hash: &str) -> Run {
    Run {
        strategy: strategy.into(),
        kind,
        started,
        net_pnl: None,
        trades: None,
        rules: None,
        budget: None,
        source: Source::Stored { hash: hash.into() },
    }
}

#[test]
fn runs_are_listed_newest_first_and_the_latest_is_the_newest_of_that_strategy() {
    let c = Catalog::new(vec![
        run("a", Kind::Backtest, 10, "h1"),
        run("b", Kind::Backtest, 99, "h2"),
        run("a", Kind::Paper, 30, "h3"),
        run("a", Kind::Live, 20, "h4"),
    ]);
    let order: Vec<_> = c.of("a").map(|r| r.started).collect();
    assert_eq!(order, [30, 20, 10]);
    assert_eq!(c.latest("a").unwrap().kind, Kind::Paper);
    assert_eq!(c.latest("b").unwrap().started, 99);
    assert!(c.latest("nobody").is_none());
    assert_eq!(c.strategies(), ["a", "b"]);
    assert_eq!(c.runs().len(), 4);
}

#[test]
fn runs_that_began_together_are_ordered_the_same_way_every_time() {
    let a = vec![
        run("a", Kind::Paper, 5, "h1"),
        run("a", Kind::Backtest, 5, "h2"),
        run("a", Kind::Backtest, 5, "h3"),
        run("0", Kind::Live, 5, "h4"),
    ];
    let mut b = a.clone();
    b.reverse();
    assert_eq!(Catalog::new(a), Catalog::new(b));
}

fn stored(
    strategy: Option<&str>,
    rules: Option<&str>,
    from: Nanos,
    metrics: &[(&str, i64)],
) -> RunResult {
    let mut m = Manifest::new(
        "abc1234",
        "backtest",
        7,
        DataRange {
            source: "synth:universe".into(),
            from,
            to: from + 100,
        },
    )
    .unwrap();
    if let Some(s) = strategy {
        m = m.with_config("strategy", s).unwrap();
    }
    if let Some(r) = rules {
        m = m.with_config("rules", r).unwrap();
    }
    let mut r = RunResult::new(m, 10, 99);
    for (k, v) in metrics {
        r = r.with_metric(k, *v).unwrap();
    }
    r
}

#[test]
fn a_stored_backtest_is_a_backtest_run_that_can_be_explored() {
    let r = stored(
        Some("momentum"),
        None,
        42,
        &[("trades", 5), ("pnl_net", -7)],
    );
    let run = run_of(&r);
    assert_eq!(run.strategy, "momentum");
    assert_eq!(run.kind, Kind::Backtest);
    assert_eq!(run.started, 42);
    assert_eq!(run.net_pnl, Some(-7));
    assert_eq!(run.trades, Some(5));
    assert_eq!(run.rules.as_deref(), Some("built-in"));
    assert_eq!(run.budget, None);
    assert_eq!(
        run.source,
        Source::Stored {
            hash: r.key().hex()
        }
    );
    assert!(run.explorable());
}

#[test]
fn stored_runs_say_what_rules_they_used_and_tolerate_missing_pieces() {
    let custom = run_of(&stored(Some("momentum"), Some("deadbeef"), 1, &[]));
    assert_eq!(custom.rules.as_deref(), Some("deadbeef"));
    assert_eq!((custom.net_pnl, custom.trades), (None, None));
    let other = run_of(&stored(Some("other"), None, 1, &[]));
    assert_eq!(other.rules, None);
    let bare = run_of(&stored(None, None, 1, &[("trades", -1)]));
    assert_eq!(bare.strategy, "-");
    assert_eq!(bare.trades, None, "a negative count is not a count");
    let cat = from_results(&[
        stored(Some("momentum"), None, 5, &[]),
        stored(Some("momentum"), None, 9, &[]),
    ]);
    assert_eq!(cat.latest("momentum").unwrap().started, 9);
}

#[test]
fn kinds_have_names_and_only_paper_and_live_are_ledger_kinds() {
    assert_eq!(Kind::Backtest.name(), "backtest");
    assert_eq!(Kind::Paper.name(), "paper");
    assert_eq!(Kind::Live.name(), "live");
    assert_eq!(Kind::parse_session("paper"), Some(Kind::Paper));
    assert_eq!(Kind::parse_session("live"), Some(Kind::Live));
    assert_eq!(Kind::parse_session("backtest"), None);
}

fn detailed(j: &Journal<MemStore>) -> Vec<(Run, Detail)> {
    sessions_detailed(
        MemStore::from_records(j.store().records().to_vec()),
        "paper-1",
        Kind::Paper,
    )
    .unwrap()
}

#[test]
fn a_session_lists_its_fills_with_the_profit_each_made_and_what_was_refused() {
    let mut j = journal();
    j.set_budgets(Some(budgets()), SEC).unwrap();
    open(&mut j, 1, 10);
    // Refused twice: far too big.
    for at in [15, 16] {
        let mut big = intent(1, at, Side::Buy, Purpose::Open, 5 * P);
        big.qty = 100_000;
        assert!(matches!(
            j.decide(&big, big.ts).unwrap(),
            Decision::Rejected(RejectReason::MaxNotional)
        ));
    }
    close(&mut j, 1, 20, 7);
    open(&mut j, 2, 30);
    j.new_day(60 * SEC).unwrap();
    close(&mut j, 2, 100, 3); // beta, next day: -$200 from yesterday's buy

    // alpha trades again the next day, so its profit then is measured from yesterday's.
    open(&mut j, 1, 110);
    close(&mut j, 1, 120, 8);
    let runs = detailed(&j);
    let get = |name: &str, day: u32| {
        runs.iter()
            .find(|(r, _)| {
                r.strategy == name
                    && matches!(&r.source, Source::Ledger { session, .. } if *session == day)
            })
            .unwrap()
    };
    let (a, ad) = get("alpha", 1);
    assert_eq!(
        ad.fills,
        [
            FillLine {
                ts: 10 * SEC,
                instrument: 0,
                side: Side::Buy,
                purpose: Purpose::Open,
                qty: 100,
                px: 5 * P,
                pnl: 0
            },
            FillLine {
                ts: 20 * SEC,
                instrument: 0,
                side: Side::Sell,
                purpose: Purpose::Close,
                qty: 100,
                px: 7 * P,
                pnl: 200 * P as i128
            },
        ]
    );
    assert_eq!(ad.refused, [("max_notional".to_owned(), 2)]);
    assert_eq!(
        ad.fills.iter().map(|f| f.pnl).sum::<i128>(),
        a.net_pnl.unwrap(),
        "the fills add up to the session"
    );
    let (b2, bd2) = get("beta", 2);
    assert_eq!(bd2.fills.len(), 1);
    assert_eq!(bd2.fills[0].pnl, -200 * P as i128);
    assert_eq!(b2.net_pnl, Some(-200 * P as i128));
    assert!(bd2.refused.is_empty());
    assert_eq!(get("beta", 1).1.fills.len(), 1);
    let (a2, ad2) = get("alpha", 2);
    assert_eq!(
        ad2.fills.iter().map(|f| f.pnl).collect::<Vec<_>>(),
        [0, 300 * P as i128]
    );
    assert_eq!(a2.net_pnl, Some(300 * P as i128));
    // `sessions` is the same runs without the detail.
    assert_eq!(
        read(&j, Kind::Paper),
        runs.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn a_strategy_that_only_passed_has_no_fills_and_says_why() {
    let mut j = journal();
    let mut big = intent(1, 10, Side::Buy, Purpose::Open, 5 * P);
    big.qty = 100_000;
    j.decide(&big, big.ts).unwrap();
    let runs = detailed(&j);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].1.fills.is_empty());
    assert_eq!(runs[0].1.refused, [("max_notional".to_owned(), 1)]);
}
