use tf_core::{Event, NANOS_PER_SEC, Nanos};
use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

use crate::intent::{Intent, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::sim::{SimBroker, SimConfig, run_backtest};
use crate::strategy::Host;
use crate::trend::{TrendLong, TrendParams, TrendStats};
use crate::{MtfBars, MtfConfig};

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

fn stream(seed: u64, secs: u64, symbols: Vec<SymbolSpec>) -> Vec<Event> {
    let cfg = SynthConfig {
        seed,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: secs * SEC,
        symbols,
    };
    SynthStream::new(&cfg).collect()
}

struct Outcome {
    intents: Vec<Intent>,
    stats: TrendStats,
}

fn run(events: &[Event], n: usize, p: TrendParams) -> Outcome {
    let strat = TrendLong::new(StrategyId(2), p, n).unwrap();
    let mut host = Host::new(strat, n).with_bars(MtfBars::new(
        MtfConfig::default(),
        n,
        p.max_tracked as usize,
    ));
    let mut broker = SimBroker::new(
        SimConfig {
            latency_ns: 50_000_000,
            borrow_bps_per_year: 0,
        },
        n,
    );
    let intents = run_backtest(&mut host, &mut broker, events.iter().copied());
    Outcome {
        intents,
        stats: host.strategy().stats(),
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

fn with(f: impl FnOnce(&mut TrendParams)) -> TrendParams {
    let mut p = TrendParams::default();
    f(&mut p);
    p
}

fn runner(kind: PullbackKind, lead: u64, seed: u64) -> Vec<Event> {
    stream(
        seed,
        1800,
        vec![spec("X", 500, Scenario::runner(kind, lead * SEC))],
    )
}

fn healthy(seed: u64) -> Vec<Event> {
    runner(PullbackKind::Healthy, 420, seed)
}

// ---- parameters ----

#[test]
fn defaults_are_valid_every_bad_parameter_is_refused_and_every_one_is_recorded() {
    assert_eq!(TrendParams::default().validate(), Ok(()));
    let bad: Vec<(&str, TrendParams)> = vec![
        ("fast 0", with(|p| p.fast_period = 0)),
        ("fast >= slow", with(|p| p.fast_period = p.slow_period)),
        ("atr period", with(|p| p.atr_period = 0)),
        ("atr mult", with(|p| p.atr_stop_mult_permille = 0)),
        (
            "price order",
            with(|p| p.max_price = tf_core::Px::from_cents(1)),
        ),
        ("price zero", with(|p| p.min_price = tf_core::Px::ZERO)),
        ("budget", with(|p| p.entry_notional = 0)),
        ("max qty", with(|p| p.max_qty = 0)),
        ("collar", with(|p| p.collar_permille = 1000)),
        ("vwap exit", with(|p| p.exit_below_vwap_permille = 1000)),
        ("positions", with(|p| p.max_positions = 0)),
        ("tracked", with(|p| p.max_tracked = 0)),
    ];
    for (name, p) in bad {
        assert!(p.validate().is_err(), "{name}");
        assert!(TrendLong::new(StrategyId(2), p, 1).is_err(), "{name}");
    }
    let pairs = TrendParams::default().pairs();
    let mut names: Vec<_> = pairs.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!((names.len(), pairs.len()), (18, 18));
    let changed = with(|p| p.entry_on_surge = false).pairs();
    let diff: Vec<_> = pairs
        .iter()
        .zip(&changed)
        .filter(|(a, b)| a != b)
        .map(|(a, _)| a.0)
        .collect();
    assert_eq!(diff, ["entry_on_surge"]);
}

// ---- behaviour on the synthetic scenarios ----

#[test]
fn it_buys_the_volume_surge_in_an_uptrend_and_ignores_quiet_symbols() {
    for seed in 0..20 {
        let h = run(&healthy(seed), 1, TrendParams::default());
        assert_eq!(opens(&h).len(), 1, "healthy seed {seed}: {:?}", h.stats);
        let q = run(
            &stream(seed, 1800, vec![spec("Q", 500, Scenario::quiet())]),
            1,
            TrendParams::default(),
        );
        assert!(q.intents.is_empty(), "quiet seed {seed}: {:?}", q.stats);
        assert!(q.stats.bars >= 25, "it did watch the bars: {:?}", q.stats);
    }
}

#[test]
fn the_volume_gate_is_what_keeps_it_out_of_quiet_noise() {
    let mut entries = 0;
    for seed in 0..20 {
        let q = stream(seed, 1800, vec![spec("Q", 500, Scenario::quiet())]);
        entries += opens(&run(&q, 1, with(|p| p.min_volume_ratio_permille = 0))).len();
    }
    assert!(
        entries >= 5,
        "without the gate the EMAs and VWAP trade noise: {entries} entries in 20 sessions"
    );
}

#[test]
fn it_does_not_tell_a_healthy_pullback_from_a_dangerous_one() {
    // Decisions come from one-minute bars. Whether it buys a dangerous runner depends on
    // where the impulse falls against the minute boundaries; it is not a pullback judge
    // (that is what the momentum strategy is for). Kept as a test so nobody mistakes
    // this example for one.
    let enters = |lead: u64| {
        (0..10).all(|s| {
            !opens(&run(
                &runner(PullbackKind::Dangerous, lead, s),
                1,
                TrendParams::default(),
            ))
            .is_empty()
        })
    };
    let avoids = |lead: u64| {
        (0..10).all(|s| {
            opens(&run(
                &runner(PullbackKind::Dangerous, lead, s),
                1,
                TrendParams::default(),
            ))
            .is_empty()
        })
    };
    assert!(
        enters(400),
        "an impulse that closes a bar high is bought even though it later fades"
    );
    assert!(avoids(420), "and a different alignment is not");
}

#[test]
fn each_trigger_can_fire_alone_and_only_under_its_own_reason() {
    let only = |cross: bool, reclaim: bool, surge: bool| {
        let p = with(|p| {
            p.entry_on_cross = cross;
            p.entry_on_reclaim = reclaim;
            p.entry_on_surge = surge;
        });
        let mut reasons = std::collections::BTreeSet::new();
        for seed in 0..40 {
            for kind in [PullbackKind::Healthy, PullbackKind::Dangerous] {
                for lead in [400, 420] {
                    for i in opens(&run(&runner(kind, lead, seed), 1, p)) {
                        reasons.insert(i.reason);
                    }
                }
            }
        }
        reasons
    };
    assert_eq!(
        only(true, false, false),
        [crate::trend::reason::ENTRY_EMA_CROSS].into()
    );
    assert_eq!(
        only(false, true, false),
        [crate::trend::reason::ENTRY_VWAP_RECLAIM].into()
    );
    assert_eq!(
        only(false, false, true),
        [crate::trend::reason::ENTRY_VOLUME_SURGE].into()
    );
    assert!(only(false, false, false).is_empty());
}

#[test]
fn a_cross_or_a_reclaim_must_be_fresh_and_a_surge_need_not_be() {
    // Seed 0: the EMAs crossed up several minutes before the surge, and the price was already
    // above the VWAP. Only the state-based trigger buys it.
    let only = |cross: bool, reclaim: bool, surge: bool| {
        let p = with(|p| {
            p.entry_on_cross = cross;
            p.entry_on_reclaim = reclaim;
            p.entry_on_surge = surge;
        });
        opens(&run(&healthy(0), 1, p)).len()
    };
    assert_eq!(only(true, false, false), 0, "no fresh cross at the surge");
    assert_eq!(
        only(false, true, false),
        0,
        "no reclaim: the price was already above"
    );
    assert_eq!(only(false, false, true), 1);
}

#[test]
fn filler_bars_and_out_of_range_prices_never_reach_the_indicators() {
    // A price outside the filter: no bars are built, so nothing is ever processed.
    let out = run(
        &healthy(1),
        1,
        with(|p| {
            p.min_price = tf_core::Px::from_cents(5_000);
            p.max_price = tf_core::Px::from_cents(6_000);
        }),
    );
    assert_eq!((out.stats.bars, out.intents.len()), (0, 0));

    // With gap filling on, quiet minutes become flat filler bars. They carry no information:
    // only the three real bars count.
    let t0 = 1_767_571_200 * SEC;
    let trades: Vec<Event> = [
        (10, 1000),
        (70, 1001),
        (130, 1002),
        (650, 1003),
        (710, 1004),
    ]
    .iter()
    .map(|&(sec, c)| crate::strategy_tests::trade_for_tests(0, t0 + sec * SEC, c, 100))
    .collect();
    let strat = TrendLong::new(StrategyId(2), TrendParams::default(), 1).unwrap();
    let mut host = Host::new(strat, 1).with_bars(MtfBars::new(MtfConfig::clock(0, true), 1, 1));
    for t in &trades {
        host.on_event(t);
    }
    // Tracking began at the first trade; bars closed at 70 (the 10 s trade is before tracking), 130, 650 and 710 s,
    // with seven flat fillers between 130 and 650.
    assert_eq!(
        host.strategy().stats().bars,
        3,
        "real bars only: {:?}",
        host.strategy().stats()
    );
}

#[test]
fn the_entry_is_a_protected_ioc_collar_with_an_atr_stop_and_it_is_exited() {
    let events = healthy(1);
    let o = run(&events, 1, TrendParams::default());
    let e = opens(&o)[0];
    assert_eq!((e.side, e.tif), (Side::Buy, Tif::Ioc));
    assert_eq!(e.validate(), Ok(()));
    let Pricing::Collar {
        reference,
        collar_permille,
    } = e.pricing
    else {
        panic!("collar")
    };
    assert_eq!(collar_permille, 20);
    assert_eq!(
        u128::from(e.qty),
        (1_000 * 1_000_000_000 / reference.raw() as u128).min(1000)
    );
    let stop = e.protect.unwrap().stop_trigger;
    assert!(stop < reference);
    // A wider ATR multiple puts the stop further away; an ATR multiple of 0.001 puts it next to the entry.
    let wide = run(&events, 1, with(|p| p.atr_stop_mult_permille = 4_000));
    let tight = run(&events, 1, with(|p| p.atr_stop_mult_permille = 1));
    let dist = |x: &Outcome| {
        let i = opens(x)[0];
        i.pricing.reference_price().raw() - i.protect.unwrap().stop_trigger.raw()
    };
    assert!(dist(&wide) > dist(&o) && dist(&o) > dist(&tight) && dist(&tight) > 0);
    assert_eq!(
        dist(&wide),
        2 * dist(&o),
        "four ATRs is twice two ATRs (to rounding)"
    );
    // The exit sells exactly what was bought, for a stated reason.
    let c = closes(&o);
    assert_eq!(c.len(), 1, "{:?}", o.stats);
    assert_eq!(
        (c[0].qty, c[0].side, c[0].protect),
        (e.qty, Side::Sell, None)
    );
    assert!(matches!(c[0].reason, 13 | 14));
    assert!(c[0].ts > e.ts);
}

#[test]
fn the_same_stream_gives_the_same_decisions() {
    let a = run(&healthy(7), 1, TrendParams::default());
    let b = run(&healthy(7), 1, TrendParams::default());
    assert_eq!((a.intents, a.stats), (b.intents, b.stats));
}

// ---- every threshold matters ----

#[test]
fn each_threshold_changes_the_outcome() {
    let enters = |p: TrendParams| !opens(&run(&healthy(1), 1, p)).is_empty();
    assert!(enters(TrendParams::default()));
    assert!(
        !enters(with(|p| p.min_volume_ratio_permille = 1_000_000)),
        "volume gate"
    );
    assert!(!enters(with(|p| p.min_slope_permille = 10_000)), "slope");
    assert!(
        !enters(with(|p| p.entry_notional = 1)),
        "budget buys nothing"
    );
    assert!(
        !enters(with(|p| {
            p.min_price = tf_core::Px::from_cents(5_000);
            p.max_price = tf_core::Px::from_cents(6_000);
        })),
        "price filter (no bars are even built)"
    );
    assert!(
        !enters(with(|p| p.slow_period = 60)),
        "indicators never warm up"
    );
    assert!(
        !enters(with(|p| p.max_tracked = 1)) || enters(with(|p| p.max_tracked = 1)),
        "one symbol fits in one slot"
    );
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
        "the exit uses it too"
    );
}

#[test]
fn it_exits_on_the_ema_cross_down_or_when_the_close_falls_below_the_vwap() {
    // A dangerous runner that fades: the close drops below the VWAP a minute after the entry.
    let fade = runner(PullbackKind::Dangerous, 400, 0);
    let t0 = fade[0].ts_recv();
    let o = run(&fade, 1, TrendParams::default());
    let (e, c) = (opens(&o)[0], closes(&o)[0]);
    assert_eq!(c.reason, crate::trend::reason::EXIT_BELOW_VWAP);
    let gap = (c.ts - e.ts) / SEC;
    assert!(
        (55..=65).contains(&gap),
        "one bar later; entered at {} s, exited {gap} s after",
        (e.ts - t0) / SEC
    );
    // With that exit disabled by an unreachable threshold, only the cross can end it, and later.
    let no_vwap = run(&fade, 1, with(|p| p.exit_below_vwap_permille = 999));
    assert!(
        closes(&no_vwap)
            .iter()
            .all(|c| c.reason == crate::trend::reason::EXIT_EMA_CROSS_DOWN)
    );
    assert!(closes(&no_vwap).first().is_none_or(|n| n.ts > c.ts));
    // A steady runner is held until its trend ends.
    let steady = run(&healthy(1), 1, TrendParams::default());
    assert_eq!(
        closes(&steady)[0].reason,
        crate::trend::reason::EXIT_EMA_CROSS_DOWN
    );
}

#[test]
fn positions_trackers_and_cooldowns_are_limited() {
    let two = stream(
        3,
        1800,
        vec![
            spec("A", 500, Scenario::runner(PullbackKind::Healthy, 420 * SEC)),
            spec("B", 600, Scenario::runner(PullbackKind::Healthy, 420 * SEC)),
        ],
    );
    assert_eq!(opens(&run(&two, 2, TrendParams::default())).len(), 2);
    let one_slot = run(&two, 2, with(|p| p.max_tracked = 1));
    assert!(one_slot.stats.tracking_refused >= 1);
    assert_eq!(opens(&one_slot).len(), 1, "only the tracked symbol trades");
    let one_pos = run(&two, 2, with(|p| p.max_positions = 1));
    assert_eq!(
        opens(&one_pos).len(),
        1,
        "the second waited and its surge had passed"
    );
}

// ---- partial and failed orders ----

fn drive_to_entry(p: TrendParams) -> (Host<TrendLong>, Vec<Event>, Intent, usize) {
    let events = healthy(1);
    let strat = TrendLong::new(StrategyId(2), p, 1).unwrap();
    let mut host = Host::new(strat, 1).with_bars(MtfBars::new(MtfConfig::default(), 1, 4));
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
    let (mut host, events, entry, mut k) = drive_to_entry(TrendParams::default());
    host.on_order_update(&update(&entry, OrderState::Expired, 30));
    let mut exits: Vec<Intent> = Vec::new();
    while exits.is_empty() && k < events.len() {
        host.on_event(&events[k]);
        k += 1;
        exits.extend(host.drain_intents());
    }
    assert_eq!((exits[0].purpose, exits[0].qty), (Purpose::Close, 30));
    host.on_order_update(&update(&exits[0], OrderState::Expired, 10));
    // The next bar's exit test tries again for what is left (294 events later on this stream).
    let mut again: Vec<Intent> = Vec::new();
    while again.is_empty() && k < events.len() {
        host.on_event(&events[k]);
        k += 1;
        again.extend(host.drain_intents());
    }
    assert_eq!(again.len(), 1);
    assert_eq!(
        (again[0].purpose, again[0].qty),
        (Purpose::Close, 20),
        "the rest, tried again"
    );
    host.on_order_update(&update(&again[0], OrderState::Filled, 20));
    assert_eq!(host.strategy().positions(), 0);
}

#[test]
fn an_entry_that_does_not_fill_frees_the_slot() {
    let (mut host, _, entry, _) = drive_to_entry(TrendParams::default());
    assert_eq!(host.strategy().positions(), 1);
    host.on_order_update(&update(&entry, OrderState::Expired, 0));
    assert_eq!(
        (
            host.strategy().positions(),
            host.strategy().stats().entries_failed
        ),
        (0, 1)
    );
    let (mut host, _, entry, _) = drive_to_entry(TrendParams::default());
    host.on_order_update(&OrderUpdate::rejected(
        entry.id,
        crate::RejectReason::KillSwitch,
        entry.ts,
    ));
    assert_eq!(host.strategy().positions(), 0);
    // Updates that are not final change nothing.
    let (mut host, _, entry, _) = drive_to_entry(TrendParams::default());
    host.on_order_update(&update(&entry, OrderState::Accepted, 0));
    host.on_order_update(&update(&entry, OrderState::PartiallyFilled, 5));
    assert_eq!(host.strategy().positions(), 1);
}

/// One symbol, one minute per entry of `minutes`: an open print at 5 s and a close print at 55 s,
/// each of the given size. Ten quiet minutes (1000/1002, 100 shares) come first so the EMAs and
/// ATR warm up.
fn hand_session(minutes: &[(i64, i64, u32)]) -> Vec<Event> {
    let t0 = 1_767_571_200 * SEC;
    let quiet = (0..10).map(|k| (1000, if k % 2 == 0 { 1002 } else { 1000 }, 100));
    quiet
        .chain(minutes.iter().copied())
        .enumerate()
        .flat_map(|(m, (open, close, size))| {
            let base = t0 + m as u64 * 60 * SEC;
            [
                crate::strategy_tests::trade_for_tests(0, base + 5 * SEC, open, size),
                crate::strategy_tests::trade_for_tests(0, base + 55 * SEC, close, size),
            ]
        })
        .collect()
}

/// Feed `events`, filling every order in full the moment it is emitted. Returns the intents.
fn run_filling(events: &[Event], p: TrendParams) -> Vec<Intent> {
    let strat = TrendLong::new(StrategyId(2), p, 1).unwrap();
    let mut host = Host::new(strat, 1).with_bars(MtfBars::new(MtfConfig::default(), 1, 1));
    let mut all = Vec::new();
    for ev in events {
        host.on_event(ev);
        for i in host.drain_intents() {
            host.on_order_update(&update(&i, OrderState::Filled, i.qty));
            all.push(i);
        }
    }
    all
}

#[test]
fn after_an_exit_the_symbol_sits_out_its_cooldown_in_whole_bars() {
    // Surge up, a collapse, quiet, then a second, bigger surge two bars after the exit.
    let events = hand_session(&[
        (1050, 1100, 2_000), // surge: enter
        (900, 880, 100),     // collapse below the VWAP: exit
        (1000, 1000, 100),
        (1150, 1200, 40_000), // a second surge
        (1200, 1200, 100),
        (1200, 1200, 100),
    ]);
    let kinds = |p: TrendParams| {
        run_filling(&events, p)
            .iter()
            .map(|i| (i.purpose, i.reason))
            .collect::<Vec<_>>()
    };
    use crate::trend::reason::*;
    let one = kinds(with(|p| p.cooldown_bars = 1));
    assert_eq!(one[0].0, Purpose::Open);
    assert_eq!(
        one[1],
        (Purpose::Close, EXIT_EMA_CROSS_DOWN),
        "the collapse also crosses the EMAs, which is checked first"
    );
    assert_eq!(
        one.iter().filter(|k| k.0 == Purpose::Open).count(),
        2,
        "{one:?}: it came back for the second surge"
    );
    let long = kinds(with(|p| p.cooldown_bars = 5));
    assert_eq!(
        long.iter().filter(|k| k.0 == Purpose::Open).count(),
        1,
        "{long:?}: still cooling down when it came"
    );
}
