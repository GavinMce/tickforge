use tf_core::{Event, NANOS_PER_SEC, Nanos};
use tf_manifest::{DataRange, Manifest, RunResult};
use tf_params::{ParamStore, Proposal, Reject, Target};
use tf_risk::Gateway;
use tf_strategy::{Host, MomentumLong, MomentumParams, SimBroker, StrategyId, tunable_specs};

use crate::ab::{Scheduled, Side, Snapshot, momentum_ab, run_ab};
use crate::{BacktestConfig, default_limits, demo_session, momentum_backtest, run_gated};

const SEC: Nanos = NANOS_PER_SEC;

fn prop(name: &str, value: i64) -> Proposal {
    let id = ParamStore::new(tunable_specs(&MomentumParams::default()))
        .unwrap()
        .id_of(name)
        .unwrap();
    Proposal {
        param: id,
        target: Target::Global,
        value,
        proposer: 1,
        reason: 1,
        evidence: 0xe,
    }
}

fn at(t0: Nanos, secs: u64, p: Proposal) -> Scheduled {
    Scheduled {
        at: t0 + secs * SEC,
        proposal: p,
    }
}

/// Require more higher lows (one to three, in two permitted steps a minute apart).
fn stricter(t0: Nanos) -> Vec<Scheduled> {
    vec![
        at(t0, 1, prop("min_higher_lows", 2)),
        at(t0, 62, prop("min_higher_lows", 3)),
    ]
}

fn session() -> (Vec<Event>, Vec<String>, Nanos) {
    let (events, labels) = demo_session(1, 400, 2, 2, 1);
    let t0 = events[0].ts_recv();
    (events, labels, t0)
}

#[test]
fn with_no_proposals_both_sides_are_the_plain_run_and_every_difference_is_zero() {
    let (events, labels, _) = session();
    let cfg = BacktestConfig::default();
    let plain = momentum_backtest(events.clone(), labels.clone(), &cfg).unwrap();
    let ab = momentum_ab(events.clone(), labels, &cfg, &[]).unwrap();
    assert_eq!(ab.tuned, plain);
    assert_eq!(ab.shadow, plain);
    let c = &ab.comparison;
    assert_eq!(
        (
            c.pnl_net,
            c.pnl_realized,
            c.max_drawdown,
            c.slippage_cost,
            c.trades,
            c.fills
        ),
        (0, 0, 0, 0, 0, 0)
    );
    assert!(c.pnl_net_by_label.values().all(|d| *d == 0));
    assert!(ab.changes.is_empty() && ab.refused.is_empty());
    assert_eq!(ab.tape, events, "nothing was added to the feed");
    assert!(
        plain.report.total.trades >= 2,
        "the comparison is of something"
    );
}

#[test]
fn a_tuned_side_that_behaves_differently_is_compared_with_an_untouched_shadow() {
    let (events, labels, t0) = session();
    let cfg = BacktestConfig::default();
    let plain = momentum_backtest(events.clone(), labels.clone(), &cfg).unwrap();
    let ab = momentum_ab(events.clone(), labels, &cfg, &stricter(t0)).unwrap();
    assert_eq!(
        ab.shadow, plain,
        "the shadow never sees the change and trades exactly as an untuned run"
    );
    assert_ne!(
        ab.tuned.outcome_hash, ab.shadow.outcome_hash,
        "the stricter entry rule changed what it did"
    );
    assert_eq!(ab.changes.len(), 2);
    assert_eq!(ab.param_errors, 0);
    assert_eq!(
        ab.tuned.report.events, ab.shadow.report.events,
        "parameter changes are not market events"
    );
    let c = &ab.comparison;
    assert_eq!(
        c.pnl_net,
        ab.tuned.report.total.net_pnl() - ab.shadow.report.total.net_pnl()
    );
    assert_eq!(
        c.trades,
        ab.tuned.report.total.trades as i64 - ab.shadow.report.total.trades as i64
    );
    assert_eq!(
        c.pnl_net_by_label.values().sum::<i128>(),
        c.pnl_net,
        "the labels add up"
    );
    assert!(
        c.pnl_net_by_label.contains_key("healthy") && c.pnl_net_by_label.contains_key("dangerous")
    );
    // Both sides' books are consistent and independent.
    for r in [&ab.tuned, &ab.shadow] {
        assert!(r.books_agree() && r.bookkeeping_errors == 0);
    }
}

#[test]
fn the_tape_holds_the_feed_with_the_changes_where_they_took_effect_and_replays_the_tuned_side() {
    let (events, labels, t0) = session();
    let cfg = BacktestConfig::default();
    let ab = momentum_ab(events.clone(), labels.clone(), &cfg, &stricter(t0)).unwrap();
    let changes: Vec<(usize, &tf_core::ParamChange)> = ab
        .tape
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            if let Event::ParamChange(c) = e {
                Some((i, c))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(ab.tape.len(), events.len() + 2);
    for ((i, c), want_at) in changes.iter().zip([t0 + SEC, t0 + 62 * SEC]) {
        let first_at_or_after = events
            .iter()
            .map(Event::ts_recv)
            .find(|t| *t >= want_at)
            .unwrap();
        assert_eq!(
            c.hdr.ts_recv, first_at_or_after,
            "made at the first event at or after its time, no earlier and no later"
        );
        assert!(
            ab.tape[*i + 1].ts_recv() >= c.hdr.ts_recv
                && ab.tape[*i - 1].ts_recv() <= c.hdr.ts_recv,
            "in time order with the feed"
        );
        assert_eq!((c.proposer, c.evidence), (1, 0xe));
    }
    assert_eq!((changes[0].1.hdr.seq, changes[1].1.hdr.seq), (0, 1));
    // Replay: a fresh tuned host with an empty store, fed the tape, ends exactly where the live run did.
    let n = labels.len();
    let store = ParamStore::new(tunable_specs(&cfg.params)).unwrap();
    let mut host =
        Host::new(MomentumLong::new(StrategyId(1), cfg.params, n).unwrap(), n).with_params(store);
    let mut broker = SimBroker::new(cfg.sim, n);
    let mut gateway = Gateway::new(cfg.limits, n);
    let replay = run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        ab.tape.iter().copied(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(
        replay, ab.tuned,
        "the replay reproduces the tuned side exactly"
    );
    assert_eq!(host.param_errors(), 0);
    assert_eq!(host.params().unwrap().history(), ab.changes.as_slice());
}

#[test]
fn refused_proposals_are_listed_with_their_reasons_and_change_nothing() {
    let (events, labels, t0) = session();
    let ab = momentum_ab(
        events,
        labels,
        &BacktestConfig::default(),
        &[
            at(t0, 1, prop("trail_permille", 90)), // too big a step
            at(t0, 2, prop("trail_permille", 5)),  // below the minimum
            at(t0, 3, prop("trail_permille", 40)), // fine
            at(t0, 4, prop("trail_permille", 50)), // inside the cooldown
        ],
    )
    .unwrap();
    assert_eq!(ab.changes.len(), 1);
    let why: Vec<Reject> = ab.refused.iter().map(|r| r.why).collect();
    assert!(matches!(why[0], Reject::StepTooLarge { .. }));
    assert!(matches!(why[1], Reject::OutOfBounds { .. }));
    assert!(matches!(why[2], Reject::Cooldown { .. }));
    assert_eq!(ab.refused[0].proposal.value, 90);
    assert_eq!(
        ab.tape
            .iter()
            .filter(|e| matches!(e, Event::ParamChange(_)))
            .count(),
        1
    );
}

#[test]
fn proposals_are_made_in_time_order_whatever_order_they_are_listed_in() {
    let (events, labels, t0) = session();
    let ab = momentum_ab(
        events,
        labels,
        &BacktestConfig::default(),
        &[
            at(t0, 62, prop("min_higher_lows", 3)),
            at(t0, 1, prop("min_higher_lows", 2)),
        ],
    )
    .unwrap();
    assert_eq!(
        ab.refused.len(),
        0,
        "2 then 3 works; the listed order (3 then 2) would not"
    );
    assert_eq!(ab.changes.iter().map(|a| a.new).collect::<Vec<_>>(), [2, 3]);
}

#[test]
fn both_equity_curves_are_observable_as_they_diverge_and_end_at_the_reports() {
    let (events, labels, t0) = session();
    let cfg = BacktestConfig::default();
    let n = labels.len();
    let store = ParamStore::new(tunable_specs(&cfg.params)).unwrap();
    let mut th =
        Host::new(MomentumLong::new(StrategyId(1), cfg.params, n).unwrap(), n).with_params(store);
    let mut sh = Host::new(MomentumLong::new(StrategyId(1), cfg.params, n).unwrap(), n);
    let (mut tb, mut sb) = (SimBroker::new(cfg.sim, n), SimBroker::new(cfg.sim, n));
    let (mut tg, mut sg) = (Gateway::new(cfg.limits, n), Gateway::new(cfg.limits, n));
    let mut snaps: Vec<Snapshot> = Vec::new();
    let ab = run_ab(
        Side {
            host: &mut th,
            broker: &mut tb,
            gateway: &mut tg,
        },
        Side {
            host: &mut sh,
            broker: &mut sb,
            gateway: &mut sg,
        },
        labels,
        events.iter().copied(),
        &stricter(t0),
        None,
        |s| snaps.push(*s),
    )
    .unwrap();
    assert_eq!(
        snaps.len(),
        events.len(),
        "one snapshot per market event, none for changes"
    );
    assert!(snaps.windows(2).all(|w| w[0].ts <= w[1].ts));
    let last = snaps.last().unwrap();
    assert_eq!(
        last.tuned,
        ab.tuned.report.total.net_pnl(),
        "borrow is zero here, so equity is net P&L"
    );
    assert_eq!(last.shadow, ab.shadow.report.total.net_pnl());
    assert!(
        snaps.iter().any(|s| s.tuned != s.shadow),
        "they did diverge"
    );
    assert!(
        snaps[..5].iter().all(|s| s.tuned == s.shadow),
        "and agree until something differs"
    );
}

#[test]
fn comparison_metrics_are_exported_as_run_result_metrics() {
    let (events, labels, t0) = session();
    let ab = momentum_ab(events, labels, &BacktestConfig::default(), &stricter(t0)).unwrap();
    let m = ab.metrics();
    let get = |n: &str| m.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
    assert_eq!(get("delta.pnl_net"), Some(ab.comparison.pnl_net as i64));
    assert_eq!(
        get("tuned.pnl_net"),
        Some(ab.tuned.report.total.net_pnl() as i64)
    );
    assert_eq!(
        get("shadow.trades"),
        Some(ab.shadow.report.total.trades as i64)
    );
    assert_eq!(
        (
            get("params.applied"),
            get("params.refused"),
            get("params.errors")
        ),
        (Some(2), Some(0), Some(0))
    );
    assert!(get("delta.group.healthy.pnl_net").is_some());
    let data = DataRange {
        source: "synth:test".into(),
        from: 0,
        to: 1,
    };
    let mut r = RunResult::new(Manifest::new("abc", "backtest", 1, data).unwrap(), 1, 0);
    for (k, v) in m {
        r = r
            .with_metric(&k, v)
            .unwrap_or_else(|e| panic!("{k}: {e:?}"));
    }
    assert_eq!(r.metric("delta.trades"), Some(ab.comparison.trades));
}

#[test]
fn a_tuned_side_without_a_store_refuses_every_proposal() {
    let (events, labels, t0) = session();
    let cfg = BacktestConfig::default();
    let n = labels.len();
    let mk = || Host::new(MomentumLong::new(StrategyId(1), cfg.params, n).unwrap(), n);
    let (mut a, mut b) = (mk(), mk());
    let (mut ab1, mut ab2) = (SimBroker::new(cfg.sim, n), SimBroker::new(cfg.sim, n));
    let (mut g1, mut g2) = (
        Gateway::new(default_limits(), n),
        Gateway::new(default_limits(), n),
    );
    let r = run_ab(
        Side {
            host: &mut a,
            broker: &mut ab1,
            gateway: &mut g1,
        },
        Side {
            host: &mut b,
            broker: &mut ab2,
            gateway: &mut g2,
        },
        labels,
        events,
        &[at(t0, 1, prop("min_higher_lows", 2))],
        None,
        |_| {},
    )
    .unwrap();
    assert_eq!((r.changes.len(), r.refused.len()), (0, 1));
    assert_eq!(r.tuned, r.shadow);
}

#[test]
fn the_same_inputs_give_the_same_comparison() {
    let (events, labels, t0) = session();
    let cfg = BacktestConfig::default();
    let a = momentum_ab(events.clone(), labels.clone(), &cfg, &stricter(t0)).unwrap();
    let b = momentum_ab(events, labels, &cfg, &stricter(t0)).unwrap();
    assert_eq!(a, b);
}

// ---- auto-revert ----

use crate::ab::{REASON_AUTO_REVERT, RevertConfig, momentum_ab_with};
use tf_params::PROPOSER_POLICY;

const D: u128 = 1_000_000_000;

/// Two healthy runners four minutes apart: room for a revert between them.
fn two_waves() -> (Vec<Event>, Vec<String>, Nanos) {
    let spec = |symbol: &str, base: i64, lead: u64| tf_synth::SymbolSpec {
        symbol: symbol.into(),
        base_px_cents: base,
        base_interval_ns: 300_000_000,
        quote_every: 2,
        scenario: tf_synth::Scenario::runner(tf_synth::PullbackKind::Healthy, lead * SEC),
        news: Vec::new(),
    };
    let cfg = tf_synth::SynthConfig {
        seed: 1,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: 700 * SEC,
        symbols: vec![spec("EARLY", 500, 20), spec("LATE", 600, 300)],
    };
    let events: Vec<Event> = tf_synth::SynthStream::new(&cfg).collect();
    let t0 = events[0].ts_recv();
    (events, vec!["early".into(), "late".into()], t0)
}

fn revert_cfg() -> RevertConfig {
    RevertConfig {
        max_drawdown: 100 * D,
        lockout: 120 * SEC,
    }
}

#[test]
fn a_tuned_side_that_falls_behind_the_shadow_is_returned_to_baseline_in_time_for_the_next_entry() {
    let (events, labels, t0) = two_waves();
    let cfg = BacktestConfig::default();
    let without = momentum_ab(events.clone(), labels.clone(), &cfg, &stricter(t0)).unwrap();
    let with = momentum_ab_with(
        events.clone(),
        labels,
        &cfg,
        &stricter(t0),
        Some(revert_cfg()),
    )
    .unwrap();
    // Without the policy the strict rule keeps it out of both runners; the shadow trades them.
    assert_eq!(without.tuned.report.total.trades, 0);
    assert!(without.shadow.report.total.trades >= 2);
    // With it, the policy trips once the shadow's first trade is far enough ahead.
    assert!(!with.reverts.is_empty(), "{:?}", with.comparison);
    let r = with.reverts[0];
    assert!(r.trip.drawdown >= 100 * D && r.parameters == 1, "{r:?}");
    assert!(
        with.reverts.iter().all(|r| r.parameters > 0),
        "a trip with nothing tuned is not a revert: {:?}",
        with.reverts
    );
    assert_eq!(with.reverts.len(), 1, "{:?}", with.reverts);
    assert!(r.at > t0 + 62 * SEC, "after the tuning that caused it");
    // The revert is on the tape, after the event that tripped it, from the policy, back to baseline.
    let pos = with
        .tape
        .iter()
        .position(|e| matches!(e, Event::ParamChange(c) if c.proposer == PROPOSER_POLICY))
        .unwrap();
    let Event::ParamChange(c) = with.tape[pos] else {
        unreachable!()
    };
    assert_eq!(
        (c.new_value, c.reason, c.hdr.ts_recv),
        (1, REASON_AUTO_REVERT, r.at)
    );
    assert_eq!(c.evidence, u64::try_from(r.trip.drawdown).unwrap());
    assert_eq!(
        with.tape[pos - 1].ts_recv(),
        r.at,
        "right after the market event that tripped it"
    );
    assert!(with.changes.last().unwrap().policy);
    // The second runner is entered on baseline parameters; the shadow side is unaffected by all this.
    assert!(
        with.tuned.report.by_label["late"].trades >= 1,
        "tuned traded the second runner after the revert"
    );
    assert_eq!(with.shadow, without.shadow);
    assert!(
        with.comparison.pnl_net > without.comparison.pnl_net,
        "reverting recovered something"
    );
    assert!(with.tuned.books_agree() && with.tuned.bookkeeping_errors == 0);
}

#[test]
fn a_limit_that_is_never_reached_changes_nothing() {
    let (events, labels, t0) = two_waves();
    let cfg = BacktestConfig::default();
    let plain = momentum_ab(events.clone(), labels.clone(), &cfg, &stricter(t0)).unwrap();
    let huge = RevertConfig {
        max_drawdown: 1_000_000 * D,
        lockout: 120 * SEC,
    };
    let with = momentum_ab_with(events, labels, &cfg, &stricter(t0), Some(huge)).unwrap();
    assert!(with.reverts.is_empty());
    assert_eq!(with.tuned, plain.tuned);
    assert_eq!(with.tape, plain.tape);
}

#[test]
fn tuning_proposed_during_the_lockout_is_refused_and_after_it_is_accepted() {
    let (events, labels, t0) = two_waves();
    let cfg = BacktestConfig::default();
    let probe = momentum_ab_with(
        events.clone(),
        labels.clone(),
        &cfg,
        &stricter(t0),
        Some(revert_cfg()),
    )
    .unwrap();
    let revert_at = probe.reverts[0].at;
    // A proposal a minute after the revert (lockout 120 s) and another three minutes after it.
    let after = |secs: u64| Scheduled {
        at: revert_at + secs * SEC,
        proposal: prop("trail_permille", 40),
    };
    let during = momentum_ab_with(
        events.clone(),
        labels.clone(),
        &cfg,
        &[stricter(t0), vec![after(60)]].concat(),
        Some(revert_cfg()),
    )
    .unwrap();
    assert!(
        matches!(
            during.refused.last().map(|r| r.why),
            Some(Reject::Locked { .. })
        ),
        "{:?}",
        during.refused
    );
    let later = momentum_ab_with(
        events,
        labels,
        &cfg,
        &[stricter(t0), vec![after(180)]].concat(),
        Some(revert_cfg()),
    )
    .unwrap();
    assert!(later.refused.is_empty(), "{:?}", later.refused);
    assert_eq!(
        later.changes.last().map(|a| (a.param, a.new)),
        Some((prop("trail_permille", 40).param, 40))
    );
}

#[test]
fn a_session_with_a_revert_replays_exactly() {
    let (events, labels, t0) = two_waves();
    let cfg = BacktestConfig::default();
    let ab = momentum_ab_with(
        events,
        labels.clone(),
        &cfg,
        &stricter(t0),
        Some(revert_cfg()),
    )
    .unwrap();
    assert!(!ab.reverts.is_empty());
    let n = labels.len();
    let store = ParamStore::new(tunable_specs(&cfg.params))
        .unwrap()
        .with_lockout(120 * SEC);
    let mut host =
        Host::new(MomentumLong::new(StrategyId(1), cfg.params, n).unwrap(), n).with_params(store);
    let mut broker = SimBroker::new(cfg.sim, n);
    let mut gateway = Gateway::new(cfg.limits, n);
    let replay = run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        ab.tape.iter().copied(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(replay, ab.tuned);
    assert_eq!(host.param_errors(), 0);
    assert_eq!(host.params().unwrap().history(), ab.changes.as_slice());
}

#[test]
fn reverts_are_exported_with_the_other_comparison_metrics() {
    let (events, labels, t0) = two_waves();
    let ab = momentum_ab_with(
        events,
        labels,
        &BacktestConfig::default(),
        &stricter(t0),
        Some(revert_cfg()),
    )
    .unwrap();
    let m = ab.metrics();
    let get = |n: &str| m.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
    assert_eq!(get("params.reverts"), Some(ab.reverts.len() as i64));
    assert_eq!(get("params.reverted"), Some(1));
    assert_eq!(get("params.applied"), Some(ab.changes.len() as i64));
}

#[test]
fn a_zero_limit_is_refused() {
    let (events, labels, _) = two_waves();
    let bad = RevertConfig {
        max_drawdown: 0,
        lockout: SEC,
    };
    assert!(momentum_ab_with(events, labels, &BacktestConfig::default(), &[], Some(bad)).is_err());
}
