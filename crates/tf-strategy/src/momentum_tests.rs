use tf_core::{Event, NANOS_PER_SEC, Nanos};
use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

use crate::intent::{Intent, IntentId, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::momentum::{MomentumLong, MomentumParams};
use crate::report::ReportBuilder;
use crate::sim::{SimBroker, SimConfig, run_backtest_observed};
use crate::strategy::Host;

const SEC: Nanos = NANOS_PER_SEC;

fn spec(symbol: &str, base: i64, scenario: Scenario) -> SymbolSpec {
    SymbolSpec {
        symbol: symbol.into(),
        base_px_cents: base,
        base_interval_ns: 300_000_000,
        quote_every: 2,
        scenario,
        news: Vec::new(),
    }
}

fn stream(seed: u64, symbols: Vec<SymbolSpec>) -> Vec<Event> {
    let cfg = SynthConfig {
        seed,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: 420 * SEC,
        symbols,
    };
    SynthStream::new(&cfg).collect()
}

fn one(kind: PullbackKind, seed: u64) -> Vec<Event> {
    stream(
        seed,
        vec![spec("RUN", 500, Scenario::runner(kind, 70 * SEC))],
    )
}

struct Outcome {
    intents: Vec<Intent>,
    stats: crate::MomentumStats,
    watched: usize,
}

fn run(events: &[Event], n: usize, p: MomentumParams) -> Outcome {
    let strat = MomentumLong::new(StrategyId(1), p, n).unwrap();
    let mut host = strat.host(n);
    let mut broker = SimBroker::new(
        SimConfig {
            latency_ns: 50_000_000,
            borrow_bps_per_year: 0,
        },
        n,
    );
    let intents = run_backtest_observed(&mut host, &mut broker, events.iter().copied(), |_, _| {});
    Outcome {
        intents,
        stats: host.strategy().stats(),
        watched: host.strategy().watched(),
    }
}

fn opens(o: &Outcome) -> Vec<&Intent> {
    o.intents
        .iter()
        .filter(|i| i.purpose == Purpose::Open)
        .collect()
}

fn closes(o: &Outcome) -> Vec<&Intent> {
    o.intents
        .iter()
        .filter(|i| i.purpose == Purpose::Close)
        .collect()
}

fn with(f: impl FnOnce(&mut MomentumParams)) -> MomentumParams {
    let mut p = MomentumParams::default();
    f(&mut p);
    p
}

fn healthy(seed: u64) -> Vec<Event> {
    one(PullbackKind::Healthy, seed)
}

fn enters(p: MomentumParams) -> bool {
    !opens(&run(&healthy(1), 1, p)).is_empty()
}

// ---- parameters ----

#[test]
fn defaults_are_valid_and_each_bad_parameter_is_refused() {
    assert_eq!(MomentumParams::default().validate(), Ok(()));
    let bad: Vec<(&str, MomentumParams)> = vec![
        ("spike_secs", with(|p| p.spike_secs = 0)),
        ("spike_secs", with(|p| p.spike_secs = 61)),
        ("spike_permille", with(|p| p.spike_permille = 0)),
        ("price", with(|p| p.min_price = tf_core::Px::ZERO)),
        ("price", with(|p| p.max_price = tf_core::Px::from_cents(1))),
        ("max_watched", with(|p| p.max_watched = 0)),
        ("max_positions", with(|p| p.max_positions = 0)),
        ("min_impulse", with(|p| p.min_impulse_permille = 0)),
        (
            "pullback_secs",
            with(|p| p.min_pullback_secs = p.max_pullback_secs + 1),
        ),
        (
            "depth",
            with(|p| p.min_depth_permille = p.max_depth_permille + 1),
        ),
        ("notional", with(|p| p.entry_notional = 0)),
        ("max_qty", with(|p| p.max_qty = 0)),
        ("collar", with(|p| p.collar_permille = 1000)),
        ("stop buffer", with(|p| p.stop_buffer_permille = 1000)),
        ("trail", with(|p| p.trail_permille = 0)),
        ("trail", with(|p| p.trail_permille = 1000)),
        ("hold", with(|p| p.max_hold_secs = 0)),
    ];
    for (name, p) in bad {
        assert!(p.validate().is_err(), "{name} should be refused");
        assert!(MomentumLong::new(StrategyId(1), p, 1).is_err(), "{name}");
    }
}

// ---- the acceptance criterion ----

#[test]
fn it_enters_the_healthy_pullback_and_never_the_dangerous_one() {
    // Healthy runners whose pullback never reads healthy (here seed 10: a 1.3% dip against a
    // 15-dollar impulse is under the 3% minimum depth). Measured on seeds 0..60: 10, 28 and 53,
    // the same three before and after the strategy moved onto the shared promoter.
    const NEVER_HEALTHY: [u64; 1] = [10];
    for seed in (0..20).filter(|s| !NEVER_HEALTHY.contains(s)) {
        let h = run(&healthy(seed), 1, MomentumParams::default());
        assert_eq!(opens(&h).len(), 1, "healthy, seed {seed}: {:?}", h.stats);
        assert_eq!(h.stats.rejected_dangerous, 0, "seed {seed}");
        let d = run(
            &one(PullbackKind::Dangerous, seed),
            1,
            MomentumParams::default(),
        );
        assert!(
            opens(&d).is_empty(),
            "dangerous, seed {seed}: {:?}",
            d.stats
        );
        assert!(
            d.stats.rejected_dangerous >= 1,
            "it looked and said no, seed {seed}"
        );
    }
}

#[test]
fn across_many_seeds_it_nearly_always_enters_healthy_and_never_dangerous() {
    let (mut entered, mut dangerous_entries) = (0, 0);
    for seed in 0..60 {
        entered +=
            usize::from(!opens(&run(&healthy(seed), 1, MomentumParams::default())).is_empty());
        dangerous_entries += opens(&run(
            &one(PullbackKind::Dangerous, seed),
            1,
            MomentumParams::default(),
        ))
        .len();
    }
    // Measured: 58 of 60. The two misses never produced a healthy reading; the rate is
    // a property of this generator and these defaults, not a claim about real markets.
    assert!(entered >= 55, "entered {entered}/60");
    assert_eq!(dangerous_entries, 0);
}

#[test]
fn entering_early_in_the_pullback_loses_and_waiting_for_the_bounce_wins_on_these_scenarios() {
    // The reason min_higher_lows defaults to 1, kept as a test so the default is not
    // loosened without noticing what it costs.
    let net = |hl: u32| {
        let mut total = 0i128;
        for seed in 0..10 {
            let p = with(|p| p.min_higher_lows = hl);
            let strat = MomentumLong::new(StrategyId(1), p, 1).unwrap();
            let mut host = strat.host(1);
            let mut broker = SimBroker::new(
                SimConfig {
                    latency_ns: 50_000_000,
                    borrow_bps_per_year: 0,
                },
                1,
            );
            let mut rb = ReportBuilder::new(vec!["h".into()]).unwrap();
            run_backtest_observed(&mut host, &mut broker, healthy(seed), |ev, fills| {
                for f in fills {
                    rb.on_fill(f);
                }
                rb.on_event(ev);
            });
            total += rb.finish(&[0]).total.net_pnl();
        }
        total
    };
    assert!(net(0) < 0, "early entries are stopped out");
    assert!(
        net(1) > 0,
        "waiting for a higher low rides the continuation"
    );
}

#[test]
fn a_quiet_symbol_is_never_even_watched() {
    let o = run(
        &stream(1, vec![spec("Q", 1000, Scenario::quiet())]),
        1,
        MomentumParams::default(),
    );
    assert_eq!((o.stats.promoted, o.watched, o.intents.len()), (0, 0, 0));
}

#[test]
fn the_entry_is_a_protected_ioc_collar_after_the_pullback_has_aged() {
    let events = healthy(1);
    let o = run(&events, 1, MomentumParams::default());
    let e = opens(&o)[0];
    assert_eq!((e.side, e.tif, e.reason), (Side::Buy, Tif::Ioc, 1));
    assert_eq!(e.validate(), Ok(()));
    let Pricing::Collar {
        reference,
        collar_permille,
    } = e.pricing
    else {
        panic!("collar expected")
    };
    assert_eq!(collar_permille, 20);
    // The reference is an ask that was on the tape, and the order is sized from the budget.
    assert!(
        events.iter().any(
            |ev| matches!(ev, Event::Quote(q) if q.ask_px == reference && q.hdr.ts_recv <= e.ts)
        )
    );
    assert_eq!(
        u128::from(e.qty),
        1_000 * 1_000_000_000 / reference.raw() as u128
    );
    // The stop is 2% under the pullback low, which is below the entry and above the lead-in price.
    let stop = e.protect.unwrap().stop_trigger;
    assert!(
        stop < reference && stop.raw() > 500 * 10_000_000,
        "stop {stop:?}"
    );
    // The high came at least min_pullback_secs (give or take second granularity) before.
    let high = events
        .iter()
        .filter_map(|ev| match ev {
            Event::Trade(t) if t.hdr.ts_recv <= e.ts => Some((t.px, t.hdr.ts_recv)),
            _ => None,
        })
        .max_by_key(|(p, ts)| (*p, *ts))
        .unwrap();
    assert!(
        e.ts - high.1 >= 9 * SEC,
        "entered {} s after the high",
        (e.ts - high.1) / SEC
    );
    assert!(e.ts - high.1 <= 61 * SEC);
}

#[test]
fn a_filled_entry_is_ridden_to_an_exit_that_closes_exactly_what_was_bought() {
    let events = healthy(1);
    let n = 1;
    let strat = MomentumLong::new(StrategyId(1), MomentumParams::default(), n).unwrap();
    let mut host = strat.host(n);
    let mut broker = SimBroker::new(
        SimConfig {
            latency_ns: 50_000_000,
            borrow_bps_per_year: 0,
        },
        n,
    );
    let mut rb = ReportBuilder::new(vec!["healthy".into()]).unwrap();
    let intents = run_backtest_observed(&mut host, &mut broker, events, |ev, fills| {
        for f in fills {
            rb.on_fill(f);
        }
        rb.on_event(ev);
    });
    let (open, close): (Vec<&Intent>, Vec<&Intent>) =
        intents.iter().partition(|i| i.purpose == Purpose::Open);
    assert_eq!((open.len(), close.len()), (1, 1));
    let fills = broker.fills();
    assert_eq!(fills.len(), 2);
    assert_eq!(fills[0].qty, open[0].qty, "the sim filled the whole entry");
    assert_eq!(
        (close[0].qty, close[0].side, close[0].protect),
        (fills[0].qty, Side::Sell, None)
    );
    assert!(close[0].ts > open[0].ts && matches!(close[0].reason, 2 | 3));
    assert_eq!(close[0].validate(), Ok(()));
    assert_eq!(broker.position(0), 0, "flat at the end");
    let r = rb.finish(&[broker.borrow_fee(0)]);
    assert_eq!((r.total.trades, r.total.open_pnl), (1, 0));
    assert_eq!(host.strategy().positions(), 0);
}

#[test]
fn the_same_stream_gives_the_same_decisions() {
    let a = run(&healthy(5), 1, MomentumParams::default());
    let b = run(&healthy(5), 1, MomentumParams::default());
    assert_eq!(a.intents, b.intents);
    assert_eq!(a.stats, b.stats);
}

// ---- every threshold is a parameter, and each one matters ----

#[test]
fn each_scan_and_classification_threshold_changes_the_outcome() {
    assert!(enters(MomentumParams::default()));
    assert!(!enters(with(|p| p.spike_permille = 100_000)), "spike size");
    assert!(
        !enters(with(|p| p.spike_min_volume = u64::MAX)),
        "spike volume"
    );
    assert!(
        !enters(with(|p| {
            p.min_price = tf_core::Px::from_cents(5_000);
            p.max_price = tf_core::Px::from_cents(6_000);
        })),
        "price floor"
    );
    assert!(
        !enters(with(|p| p.max_price = tf_core::Px::from_cents(201))),
        "price cap"
    );
    assert!(!enters(with(|p| p.max_spread_permille = 0)), "spread");
    assert!(
        !enters(with(|p| p.min_impulse_permille = 100_000)),
        "impulse size"
    );
    assert!(
        !enters(with(|p| {
            p.min_depth_permille = 600;
            p.max_depth_permille = 900;
        })),
        "depth floor"
    );
    assert!(!enters(with(|p| p.max_depth_permille = 50)), "depth cap");
    assert!(
        !enters(with(|p| p.max_volume_ratio_permille = 5)),
        "volume ratio"
    );
    assert!(
        !enters(with(|p| p.max_retrace_now_permille = 5)),
        "retrace now"
    );
    assert!(!enters(with(|p| p.min_higher_lows = 50)), "higher lows");
    assert!(
        !enters(with(|p| p.min_bid_support_permille = 1000)),
        "bid support"
    );
    assert!(
        !enters(with(|p| p.entry_notional = 1)),
        "budget buys nothing"
    );
    assert!(
        !enters(with(|p| p.min_pullback_secs = 59)),
        "pullback must age"
    );
}

#[test]
fn a_pullback_that_never_turns_healthy_is_dropped_when_the_window_closes() {
    // Depth never reaches 60%, so it is never "healthy" enough to enter.
    let never = |f: fn(&mut MomentumParams)| run(&healthy(1), 1, with(f)).stats;
    let dropped = never(|p| {
        p.min_depth_permille = 600;
        p.max_depth_permille = 900;
    });
    assert_eq!((dropped.entries, dropped.rejected_too_old), (0, 1));
    let waiting = never(|p| {
        p.min_depth_permille = 600;
        p.max_depth_permille = 900;
        p.max_pullback_secs = 100_000;
    });
    assert_eq!(
        (waiting.entries, waiting.rejected_too_old),
        (0, 0),
        "still watching"
    );
}

#[test]
fn execution_parameters_shape_the_order() {
    let base = run(&healthy(1), 1, MomentumParams::default());
    let e0 = opens(&base)[0];
    let capped = run(&healthy(1), 1, with(|p| p.max_qty = 7));
    assert_eq!(opens(&capped)[0].qty, 7);
    let wide = run(&healthy(1), 1, with(|p| p.collar_permille = 55));
    assert!(matches!(
        opens(&wide)[0].pricing,
        Pricing::Collar {
            collar_permille: 55,
            ..
        }
    ));
    assert!(
        matches!(
            closes(&wide)[0].pricing,
            Pricing::Collar {
                collar_permille: 55,
                ..
            }
        ),
        "the exit uses the same collar"
    );
    let loose = run(&healthy(1), 1, with(|p| p.stop_buffer_permille = 200));
    assert!(opens(&loose)[0].protect.unwrap().stop_trigger < e0.protect.unwrap().stop_trigger);
}

#[test]
fn the_max_hold_ends_a_position_the_trailing_stop_does_not() {
    // A trail so wide it cannot fire: only the clock can end this.
    let o = run(
        &healthy(1),
        1,
        with(|p| {
            p.trail_permille = 999;
            p.max_hold_secs = 5;
        }),
    );
    let (e, c) = (opens(&o)[0], closes(&o)[0]);
    assert_eq!(c.reason, 3);
    let held = c.ts - e.ts;
    assert!(
        (5 * SEC..=8 * SEC).contains(&held),
        "held {} ms",
        held / 1_000_000
    );
    // And a tight trail leaves earlier, for the other reason.
    let t = run(&healthy(1), 1, with(|p| p.trail_permille = 1));
    assert_eq!(closes(&t)[0].reason, 2);
    assert!(closes(&t)[0].ts <= c.ts);
}

#[test]
fn watched_and_open_positions_are_limited() {
    let two = stream(
        2,
        vec![
            spec("A", 500, Scenario::runner(PullbackKind::Healthy, 70 * SEC)),
            spec("B", 600, Scenario::runner(PullbackKind::Healthy, 70 * SEC)),
        ],
    );
    let both = run(&two, 2, MomentumParams::default());
    assert_eq!(opens(&both).len(), 2);
    let watch1 = run(&two, 2, with(|p| p.max_watched = 1));
    assert_eq!(
        watch1.stats.promotions_refused, 1,
        "the second runner is refused once, not on every trade: {:?}",
        watch1.stats
    );
    assert_eq!(opens(&watch1).len(), 1);
    let pos1 = run(&two, 2, with(|p| p.max_positions = 1));
    let (os, cs) = (opens(&pos1), closes(&pos1));
    assert!(!os.is_empty() && os.len() <= 2);
    if os.len() == 2 {
        assert!(
            os[1].ts >= cs[0].ts,
            "the second waited for the first to be closed"
        );
    }
}

#[test]
fn a_symbol_re_arms_only_after_its_cooldown() {
    let events = stream(4, vec![spec("M", 500, Scenario::multi_spike(3, 70 * SEC))]);
    let long = run(&events, 1, with(|p| p.cooldown_secs = 100_000));
    let short = run(&events, 1, with(|p| p.cooldown_secs = 1));
    assert_eq!(long.stats.promoted, 1, "one look, then it stays out");
    assert!(
        short.stats.promoted > long.stats.promoted,
        "short {:?}",
        short.stats
    );
}

// ---- what the strategy does with partial and failed orders ----

fn drive_to_entry(p: MomentumParams) -> (Host<MomentumLong>, Vec<Event>, Intent, usize) {
    let events = healthy(1);
    let strat = MomentumLong::new(StrategyId(1), p, 1).unwrap();
    let mut host = strat.host(1);
    for (k, ev) in events.iter().enumerate() {
        host.on_event(ev);
        if let Some(i) = host.drain_intents().into_iter().next() {
            return (host, events, i, k + 1);
        }
    }
    panic!("no entry");
}

fn update(i: &Intent, state: OrderState, filled: u32) -> OrderUpdate {
    OrderUpdate {
        intent: i.id,
        order: None,
        state,
        filled_qty: filled,
        avg_px: (filled > 0).then(|| i.pricing.reference_price()),
        reject: None,
        ts: i.ts,
    }
}

#[test]
fn a_partly_filled_entry_holds_what_it_got_and_a_partly_filled_exit_retries() {
    let (mut host, events, entry, k) = drive_to_entry(with(|p| p.trail_permille = 1));
    host.on_order_update(&update(&entry, OrderState::Expired, 30)); // 30 of the order, then the IOC expired
    let mut exits: Vec<Intent> = Vec::new();
    let mut next = k;
    while exits.is_empty() {
        host.on_event(&events[next]);
        next += 1;
        exits.extend(host.drain_intents());
    }
    assert_eq!((exits[0].purpose, exits[0].qty), (Purpose::Close, 30));
    host.on_order_update(&update(&exits[0], OrderState::Expired, 10)); // only 10 of 30 sold
    let mut again: Vec<Intent> = Vec::new();
    while again.is_empty() && next < events.len() {
        host.on_event(&events[next]);
        next += 1;
        again.extend(host.drain_intents());
    }
    assert_eq!(
        (again[0].purpose, again[0].qty),
        (Purpose::Close, 20),
        "the rest, tried again"
    );
    host.on_order_update(&update(&again[0], OrderState::Filled, 20));
    assert_eq!(host.strategy().positions(), 0);
}

fn trade_at(ts: Nanos, px: tf_core::Px) -> Event {
    Event::Trade(tf_core::Trade {
        hdr: tf_core::Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument: 0,
            provider: tf_core::ProviderId::Synthetic,
        },
        px,
        size: 1,
        flags: tf_core::TradeFlags::NONE,
    })
}

#[test]
fn the_trailing_stop_follows_the_high_since_entry_not_the_entry_price() {
    let (mut host, events, entry, k) = drive_to_entry(with(|p| p.max_hold_secs = 100_000));
    let ask = entry.pricing.reference_price();
    host.on_order_update(&update(&entry, OrderState::Filled, entry.qty));
    let mut t = events[k - 1].ts_recv() + SEC;
    let px = |permille: i64| tf_core::Px::from_raw(ask.raw() / 1000 * permille);
    // Up 10%: no exit. Back to +8% (a 1.8% drop from the high, inside the 3% trail): none.
    for permille in [1050, 1100, 1080] {
        host.on_event(&trade_at(t, px(permille)));
        t += SEC;
        assert!(host.drain_intents().is_empty(), "no exit at {permille}");
    }
    // +5% is 4.5% under the +10% high: out. It is still above the entry, so only a trail
    // measured from the high can have fired.
    host.on_event(&trade_at(t, px(1050)));
    let out = host.drain_intents();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].purpose, out[0].reason, out[0].qty),
        (Purpose::Close, 2, entry.qty)
    );
}

#[test]
fn an_entry_that_does_not_fill_frees_the_slot_and_the_symbol_cools_down() {
    let (mut host, _, entry, _) = drive_to_entry(MomentumParams::default());
    assert_eq!(host.strategy().positions(), 1);
    host.on_order_update(&update(&entry, OrderState::Expired, 0));
    assert_eq!(host.strategy().positions(), 0);
    assert_eq!(host.strategy().stats().entries_failed, 1);
    assert_eq!(host.strategy().watched(), 0);
    // Rejected by the gateway: the same.
    let (mut host, _, entry, _) = drive_to_entry(MomentumParams::default());
    host.on_order_update(&OrderUpdate::rejected(
        entry.id,
        crate::RejectReason::KillSwitch,
        entry.ts,
    ));
    assert_eq!(host.strategy().positions(), 0);
}

#[test]
fn updates_that_are_not_final_or_not_ours_change_nothing() {
    let (mut host, _, entry, _) = drive_to_entry(MomentumParams::default());
    host.on_order_update(&update(&entry, OrderState::Accepted, 0));
    host.on_order_update(&update(&entry, OrderState::PartiallyFilled, 10));
    assert_eq!(host.strategy().positions(), 1);
    let other = IntentId {
        strategy: StrategyId(9),
        seq: 99,
    };
    host.on_order_update(&OrderUpdate {
        intent: other,
        order: None,
        state: OrderState::Filled,
        filled_qty: 5,
        avg_px: None,
        reject: None,
        ts: 0,
    });
    assert_eq!(host.strategy().positions(), 1);
}

#[test]
fn every_parameter_is_recorded_once_and_a_change_shows() {
    let base = MomentumParams::default().pairs();
    let mut names: Vec<_> = base.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), base.len(), "names are unique");
    assert_eq!(base.len(), 24);
    let changed = with(|p| p.min_higher_lows = 0).pairs();
    let diff: Vec<_> = base
        .iter()
        .zip(&changed)
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.0)
        .collect();
    assert_eq!(diff, ["min_higher_lows"]);
}

// ---- entry rules as data ----

use crate::rules::{MOMENTUM_RULES, RuleSet, StageKind};

type Evidence = (Vec<crate::EntryTrace>, Vec<crate::Decline>);

fn run_rules(events: &[Event], rules: &str, p: MomentumParams) -> (Outcome, Evidence) {
    let strat = MomentumLong::new(StrategyId(1), p, 1)
        .unwrap()
        .with_rules(RuleSet::parse(rules).unwrap());
    let mut host = strat.host(1);
    let mut broker = SimBroker::new(
        SimConfig {
            latency_ns: 50_000_000,
            borrow_bps_per_year: 0,
        },
        1,
    );
    let intents = run_backtest_observed(&mut host, &mut broker, events.iter().copied(), |_, _| {});
    let out = Outcome {
        intents,
        stats: host.strategy().stats(),
        watched: host.strategy().watched(),
    };
    let ev = (
        host.strategy().entry_traces().to_vec(),
        host.strategy().declines().to_vec(),
    );
    (out, ev)
}

#[test]
fn the_built_in_rules_given_explicitly_decide_exactly_as_the_default_does() {
    for seed in 0..12 {
        for kind in [PullbackKind::Healthy, PullbackKind::Dangerous] {
            let ev = one(kind, seed);
            let a = run(&ev, 1, MomentumParams::default());
            let (b, _) = run_rules(&ev, MOMENTUM_RULES, MomentumParams::default());
            assert_eq!(a.intents, b.intents, "{kind:?} seed {seed}");
            assert_eq!(a.stats, b.stats);
        }
    }
}

#[test]
fn editing_the_rules_changes_the_decision_without_touching_the_parameters() {
    let ev = healthy(1);
    // A literal nobody reaches: never enters, and does not give up either.
    let never = MOMENTUM_RULES.replace("higher_lows >= @min_higher_lows", "higher_lows >= 99");
    let (o, _) = run_rules(&ev, &never, MomentumParams::default());
    assert!(opens(&o).is_empty());
    // Dropping the higher-lows and bid-support conditions is the same as setting those
    // parameters to zero: the rule edit and the parameter edit agree to the intent.
    let dropped = MOMENTUM_RULES
        .replace("; higher_lows >= @min_higher_lows", "")
        .replace("; bid_support >= @min_bid_support_permille", "");
    let (by_rules, _) = run_rules(&ev, &dropped, MomentumParams::default());
    let by_params = run(
        &ev,
        1,
        with(|p| {
            p.min_higher_lows = 0;
            p.min_bid_support_permille = 0;
        }),
    );
    assert_eq!(by_rules.intents.len(), by_params.intents.len());
    assert_eq!(opens(&by_rules).len(), 1);
    // bid support 0 still needs a bid reading, which the dropped condition did not, so
    // the two may differ in timing by a tick; both enter on the early, pre-bounce reading.
    let base = run(&ev, 1, MomentumParams::default());
    assert!(
        opens(&by_rules)[0].ts <= opens(&base)[0].ts,
        "no later than with the bounce required"
    );
    // An empty `dangerous` stage means nothing is dangerous.
    let none = MOMENTUM_RULES.replace(
        "dangerous any: depth > @max_depth_permille; volume_ratio > @max_volume_ratio_permille",
        "dangerous any:",
    );
    let (d, _) = run_rules(
        &one(PullbackKind::Dangerous, 1),
        &none,
        MomentumParams::default(),
    );
    assert_eq!(d.stats.rejected_dangerous, 0);
    let base_d = run(
        &one(PullbackKind::Dangerous, 1),
        1,
        MomentumParams::default(),
    );
    assert!(base_d.stats.rejected_dangerous >= 1);
}

#[test]
fn every_entry_and_every_decline_carries_the_rules_that_decided_and_how_each_condition_read() {
    let (o, (entries, _)) = run_rules(&healthy(1), MOMENTUM_RULES, MomentumParams::default());
    assert_eq!(opens(&o).len(), 1);
    let id = RuleSet::momentum().fingerprint();
    let t = &entries[0];
    assert_eq!(t.rules, id);
    assert_eq!(t.evaluations.len(), 10);
    let enter: Vec<_> = t
        .evaluations
        .iter()
        .filter(|e| e.stage == StageKind::Enter)
        .collect();
    assert_eq!(enter.len(), 5);
    assert!(
        enter.iter().all(|e| e.pass),
        "it entered because every entry condition held"
    );
    assert!(
        t.evaluations
            .iter()
            .filter(|e| e.stage == StageKind::Dangerous)
            .all(|e| !e.pass),
        "and nothing dangerous held"
    );
    // The value recorded for a condition is the feature the decision saw.
    let depth = t
        .evaluations
        .iter()
        .find(|e| {
            e.stage == StageKind::Enter && e.condition.feature == crate::rules::Feature::Depth
        })
        .unwrap();
    assert_eq!(depth.value, Some(t.features.depth_permille));

    let (_, (_, declines)) = run_rules(
        &one(PullbackKind::Dangerous, 1),
        MOMENTUM_RULES,
        MomentumParams::default(),
    );
    let dec = declines
        .iter()
        .find(|x| x.reason == crate::DeclineReason::Dangerous)
        .expect("a dangerous decline");
    assert_eq!(dec.rules, id);
    assert!(
        dec.evaluations
            .iter()
            .any(|e| e.stage == StageKind::Dangerous && e.pass),
        "the decline shows which dangerous condition held"
    );
}

// ---- on the shared promoter ----

use tf_core::TierAction;
use tf_engine::Promoter;

/// The events with each tier decision the promoter makes put on the tape just before the
/// event that caused it, and the strategy's decisions from running them live.
fn live_with_tape(events: &[Event]) -> (Vec<Event>, Host<MomentumLong>, Vec<Intent>) {
    let mut host = MomentumLong::new(StrategyId(1), MomentumParams::default(), 1)
        .unwrap()
        .host(1);
    let (mut tape, mut intents) = (Vec::new(), Vec::new());
    for ev in events {
        host.on_event(ev);
        tape.extend(host.drain_tier_events().into_iter().map(Event::TierChange));
        tape.push(*ev);
        intents.extend(host.drain_intents());
    }
    (tape, host, intents)
}

#[test]
fn without_a_promoter_the_strategy_watches_nothing_and_says_so() {
    let mut host = Host::new(
        MomentumLong::new(StrategyId(1), MomentumParams::default(), 1).unwrap(),
        1,
    );
    for ev in &healthy(1) {
        host.on_event(ev);
    }
    let st = host.strategy().stats();
    assert!(host.drain_intents().is_empty());
    assert_eq!((st.promoted, st.entries), (0, 0));
    assert!(
        st.no_promoter > 100,
        "every trade it could not act on is counted: {st:?}"
    );
    // With one, the same stream is traded and nothing is counted.
    let (_, live, intents) = live_with_tape(&healthy(1));
    assert_eq!(live.strategy().stats().no_promoter, 0);
    assert_eq!(intents.len(), 1);
}

#[test]
fn a_replay_from_the_tape_decides_exactly_as_the_live_run_did() {
    for seed in [1, 2, 3, 7] {
        let events = healthy(seed);
        let (tape, live, live_intents) = live_with_tape(&events);
        assert!(
            tape.iter().any(|e| matches!(e, Event::TierChange(_))),
            "the promoter wrote its decisions on the tape"
        );
        let mut follower = Host::new(
            MomentumLong::new(StrategyId(1), MomentumParams::default(), 1).unwrap(),
            1,
        )
        .with_promoter(Promoter::follower(50, 1));
        let mut replay_intents = Vec::new();
        for e in &tape {
            follower.on_event(e);
            replay_intents.extend(follower.drain_intents());
        }
        assert_eq!(replay_intents, live_intents, "seed {seed}");
        assert_eq!(
            follower.strategy().entry_traces(),
            live.strategy().entry_traces(),
            "the same evidence, seed {seed}"
        );
        assert_eq!(follower.strategy().declines(), live.strategy().declines());
        assert_eq!(follower.strategy().stats(), live.strategy().stats());
    }
}

#[test]
fn an_engaged_symbol_is_pinned_in_tier_one_until_the_strategy_lets_go() {
    let (mut host, _events, entry, _) = drive_to_entry(MomentumParams::default());
    assert!(
        host.promoter().unwrap().is_pinned(0),
        "pinned from the moment it was taken up"
    );
    assert_eq!(host.strategy().watched(), 1);
    // The entry does not fill: the strategy gives the symbol up and releases it.
    host.on_order_update(&update(&entry, OrderState::Expired, 0));
    assert!(!host.promoter().unwrap().is_pinned(0));
    assert_eq!(host.strategy().watched(), 0);
    assert!(
        host.promoter().unwrap().is_promoted(0),
        "and the promoter, not the strategy, decides when it goes"
    );
}

#[test]
fn a_symbol_demoted_while_it_is_being_watched_is_dropped_cleanly() {
    let events = healthy(1);
    let (tape, _, _) = live_with_tape(&events);
    let promote_at = tape
        .iter()
        .position(|e| matches!(e, Event::TierChange(c) if c.action == TierAction::Promote))
        .unwrap();
    let Event::TierChange(promote) = tape[promote_at] else {
        unreachable!()
    };
    // Take the symbol out of Tier 1 twenty events into its watch, before any entry (and drop the
    // demotion the live run made later).
    let mut cut = tape.clone();
    cut.retain(|e| !matches!(e, Event::TierChange(c) if c.action == TierAction::Demote));
    let demote = tf_core::TierChange {
        action: TierAction::Demote,
        ..promote
    };
    cut.insert(promote_at + 20, Event::TierChange(demote));
    let mut follower = Host::new(
        MomentumLong::new(StrategyId(1), MomentumParams::default(), 1).unwrap(),
        1,
    )
    .with_promoter(Promoter::follower(50, 1));
    for e in &cut {
        follower.on_event(e);
    }
    let st = follower.strategy().stats();
    assert_eq!(
        st.promoted, 1,
        "it was taken up and being watched when it went: {st:?}"
    );
    assert_eq!(st.entries, 0, "nothing left to judge: {st:?}");
    assert_eq!(follower.strategy().watched(), 0, "the slot is free again");
    assert!(!follower.promoter().unwrap().is_pinned(0));
}
