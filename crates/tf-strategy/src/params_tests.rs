use tf_core::{Event, NANOS_PER_SEC, Nanos};
use tf_params::{ParamStore, Proposal, Reject, Target};
use tf_synth::{PullbackKind, Scenario, SplitMix64, SymbolSpec, SynthConfig, SynthStream};

use crate::intent::{Intent, Purpose, StrategyId};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::momentum::{MomentumLong, MomentumParams, TUNABLES, tunable_specs};
use crate::strategy::Host;

const SEC: Nanos = NANOS_PER_SEC;

fn healthy(seed: u64) -> Vec<Event> {
    let cfg = SynthConfig {
        seed,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: 470 * SEC,
        symbols: vec![SymbolSpec {
            symbol: "RUN".into(),
            base_px_cents: 500,
            base_interval_ns: 300_000_000,
            quote_every: 2,
            scenario: Scenario::runner(PullbackKind::Healthy, 70 * SEC),
            news: Vec::new(),
        }],
    };
    SynthStream::new(&cfg).collect()
}

fn store() -> ParamStore {
    ParamStore::new(tunable_specs(&MomentumParams::default())).unwrap()
}

fn host(p: MomentumParams) -> Host<MomentumLong> {
    MomentumLong::new(StrategyId(1), p, 1)
        .unwrap()
        .host(1)
        .with_params(store())
}

fn proposal(name: &str, value: i64) -> Proposal {
    Proposal {
        param: store().id_of(name).unwrap(),
        target: Target::Global,
        value,
        proposer: 1,
        reason: 1,
        evidence: 0,
    }
}

// ---- the declarations ----

#[test]
fn the_tunables_are_real_parameters_inside_bounds_that_cannot_make_an_invalid_set() {
    let base = MomentumParams::default();
    let names: Vec<&str> = base.pairs().iter().map(|(n, _)| *n).collect();
    for t in &TUNABLES {
        assert!(
            names.contains(&t.name) || names.contains(&format!("{}_raw", t.name).as_str()),
            "{} is not a parameter",
            t.name
        );
        assert!(
            (t.min..=t.max).contains(&(t.get)(&base)),
            "{}: the default {} is outside {}..={}",
            t.name,
            (t.get)(&base),
            t.min,
            t.max
        );
        assert!(
            t.max_step > 0 && t.max_step <= t.max - t.min,
            "{} has a step",
            t.name
        );
    }
    let s = store();
    assert_eq!(s.specs().len(), TUNABLES.len());
    // Every combination of extremes is a valid parameter set (so overrides cannot conflict).
    let mut rng = SplitMix64::new(5);
    for _ in 0..500 {
        let mut p = base;
        for t in &TUNABLES {
            (t.set)(
                &mut p,
                if rng.next_u64() % 2 == 0 {
                    t.min
                } else {
                    t.max
                },
            );
        }
        assert_eq!(p.validate(), Ok(()), "{p:?}");
    }
    // A baseline outside its bounds is a declaration error, not a surprise later.
    let bad = MomentumParams {
        trail_permille: 500,
        ..base
    };
    assert!(ParamStore::new(tunable_specs(&bad)).is_err());
}

#[test]
fn what_is_not_tunable_is_not_in_the_store() {
    let s = store();
    for fixed in [
        "min_price_raw",
        "max_price_raw",
        "max_positions",
        "max_watched",
        "cooldown_secs",
        "spike_secs",
        "max_spread_permille",
    ] {
        assert_eq!(s.id_of(fixed), None, "{fixed}");
    }
}

// ---- changes reach the strategy, and only through events ----

fn run_events(h: &mut Host<MomentumLong>, events: &[Event]) -> Vec<Intent> {
    let mut all = Vec::new();
    for ev in events {
        h.on_event(ev);
        for i in h.drain_intents() {
            h.on_order_update(&OrderUpdate {
                intent: i.id,
                order: None,
                state: OrderState::Filled,
                filled_qty: i.qty,
                avg_px: Some(i.pricing.reference_price()),
                reject: None,
                ts: i.ts,
            });
            all.push(i);
        }
    }
    all
}

/// Put the change events for `proposals` (each at the given time) into `events`, checking
/// each against `h`'s store as it would be when its turn comes. Returns the combined stream.
fn with_changes(events: &[Event], proposals: &[(Nanos, Proposal)]) -> (Vec<Event>, Vec<Reject>) {
    let mut h = host(MomentumParams::default());
    let (mut out, mut refused) = (Vec::new(), Vec::new());
    let mut pending = proposals.iter().peekable();
    for ev in events {
        while let Some((t, p)) = pending.next_if(|(t, _)| *t <= ev.ts_recv()) {
            match h.params().unwrap().check(p, *t) {
                Ok(change) => {
                    let e = Event::ParamChange(change);
                    h.on_event(&e); // applies it, as the live path does
                    out.push(e);
                }
                Err(r) => refused.push(r),
            }
        }
        h.on_event(ev);
        h.drain_intents();
        out.push(*ev);
    }
    (out, refused)
}

#[test]
fn a_change_before_the_decision_changes_it() {
    let events = healthy(1);
    let t0 = events[0].ts_recv();
    let base = run_events(&mut host(MomentumParams::default()), &events);
    assert_eq!(
        base.iter().filter(|i| i.purpose == Purpose::Open).count(),
        1
    );
    // Require three higher lows (from one), in two permitted steps of one a minute apart.
    let (stream, refused) = with_changes(
        &events,
        &[
            (t0 + SEC, proposal("min_higher_lows", 2)),
            (t0 + 62 * SEC, proposal("min_higher_lows", 3)),
        ],
    );
    assert!(refused.is_empty(), "{refused:?}");
    assert_eq!(
        stream
            .iter()
            .filter(|e| matches!(e, Event::ParamChange(_)))
            .count(),
        2
    );
    let mut h = host(MomentumParams::default());
    let tuned = run_events(&mut h, &stream);
    assert_eq!(h.param_errors(), 0);
    assert_eq!(h.params().unwrap().revision(), 2);
    assert_ne!(tuned, base, "the stricter entry rule changed what it did");
}

#[test]
fn an_unbounded_or_too_fast_proposal_never_becomes_an_event() {
    let events = healthy(1);
    let t0 = events[0].ts_recv();
    let (stream, refused) = with_changes(
        &events,
        &[
            (t0 + SEC, proposal("trail_permille", 90)), // a step of 60: too large
            (t0 + 2 * SEC, proposal("trail_permille", 5)), // below the minimum
            (t0 + 3 * SEC, proposal("trail_permille", 40)), // fine
            (t0 + 4 * SEC, proposal("trail_permille", 50)), // inside the cooldown
        ],
    );
    assert!(matches!(refused[0], Reject::StepTooLarge { .. }));
    assert!(matches!(refused[1], Reject::OutOfBounds { .. }));
    assert!(matches!(refused[2], Reject::Cooldown { .. }));
    assert_eq!(
        stream
            .iter()
            .filter(|e| matches!(e, Event::ParamChange(_)))
            .count(),
        1
    );
}

#[test]
fn a_session_replays_with_exactly_the_parameters_it_ran_with() {
    let events = healthy(1);
    let t0 = events[0].ts_recv();
    let (stream, _) = with_changes(
        &events,
        &[
            (t0 + 5 * SEC, proposal("min_higher_lows", 2)),
            (t0 + 70 * SEC, proposal("trail_permille", 20)),
            (t0 + 90 * SEC, proposal("min_higher_lows", 1)),
        ],
    );
    // The recorded stream goes through the wire format, as a tape does.
    let tape: Vec<Event> = stream
        .iter()
        .map(|e| {
            let mut b = Vec::new();
            e.encode(&mut b);
            Event::decode(&b).unwrap().0
        })
        .collect();
    let (mut a, mut b) = (
        host(MomentumParams::default()),
        host(MomentumParams::default()),
    );
    let (ia, ib) = (run_events(&mut a, &stream), run_events(&mut b, &tape));
    assert_eq!(ia, ib);
    assert_eq!(
        (a.strategy().stats(), a.param_errors()),
        (b.strategy().stats(), 0)
    );
    assert_eq!(a.params().unwrap().history(), b.params().unwrap().history());
    assert_eq!(a.params().unwrap().revision(), 3);
    assert_eq!(a.strategy().parameter_conflicts(), 0);
}

#[test]
fn a_tape_that_disagrees_with_the_store_is_counted_not_obeyed() {
    let events = healthy(1);
    let t0 = events[0].ts_recv();
    let (mut stream, _) = with_changes(&events, &[(t0 + SEC, proposal("trail_permille", 40))]);
    // Corrupt the recorded change so it breaks the step rule.
    for e in &mut stream {
        if let Event::ParamChange(c) = e {
            c.new_value = 100;
        }
    }
    let mut h = host(MomentumParams::default());
    run_events(&mut h, &stream);
    assert_eq!((h.param_errors(), h.params().unwrap().revision()), (1, 0));
    // A host without a store ignores change events.
    let mut plain = MomentumLong::new(StrategyId(1), MomentumParams::default(), 1)
        .unwrap()
        .host(1);
    run_events(&mut plain, &stream);
    assert_eq!(plain.param_errors(), 0);
}

// ---- changes apply to new entries only ----

fn drive_to_entry(
    mut h: Host<MomentumLong>,
    events: &[Event],
) -> (Host<MomentumLong>, Intent, usize) {
    for (k, ev) in events.iter().enumerate() {
        h.on_event(ev);
        if let Some(i) = h.drain_intents().into_iter().next() {
            return (h, i, k + 1);
        }
    }
    panic!("no entry");
}

fn fill(h: &mut Host<MomentumLong>, i: &Intent) {
    h.on_order_update(&OrderUpdate {
        intent: i.id,
        order: None,
        state: OrderState::Filled,
        filled_qty: i.qty,
        avg_px: Some(i.pricing.reference_price()),
        reject: None,
        ts: i.ts,
    });
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

/// After a filled entry at `ask`: the price rises 10%, then falls 2% from that high. Does it exit?
fn exits_on_a_two_percent_dip(h: &mut Host<MomentumLong>, entry: &Intent, mut t: Nanos) -> bool {
    let ask = entry.pricing.reference_price().raw();
    for permille in [1_050, 1_100, 1_080] {
        t += SEC;
        h.on_event(&trade_at(t, tf_core::Px::from_raw(ask / 1000 * permille)));
        if !h.drain_intents().is_empty() {
            return true;
        }
    }
    false
}

#[test]
fn a_change_after_the_entry_does_not_touch_the_open_position_but_the_next_entry_uses_it() {
    let events = healthy(1);
    // Position A: entered under trail 30 (3%); the agent tightens it to 10 (1%) afterwards.
    let (mut a, entry, k) = drive_to_entry(host(MomentumParams::default()), &events);
    fill(&mut a, &entry);
    let t = events[k - 1].ts_recv();
    let change = a
        .params()
        .unwrap()
        .check(&proposal("trail_permille", 20), t + SEC)
        .unwrap();
    a.on_event(&Event::ParamChange(change));
    let change = a
        .params()
        .unwrap()
        .check(&proposal("trail_permille", 10), t + 62 * SEC)
        .unwrap();
    a.on_event(&Event::ParamChange(change));
    assert_eq!(
        a.params()
            .unwrap()
            .value(a.params().unwrap().id_of("trail_permille").unwrap()),
        10
    );
    assert!(
        !exits_on_a_two_percent_dip(&mut a, &entry, t + 70 * SEC),
        "the open position keeps the 3% trail it entered with"
    );

    // Position B: the same change happens before the entry, so the entry is governed by 1%.
    let t0 = events[0].ts_recv();
    let (stream, refused) = with_changes(
        &events[..events.len()],
        &[
            (t0 + SEC, proposal("trail_permille", 20)),
            (t0 + 62 * SEC, proposal("trail_permille", 10)),
        ],
    );
    assert!(refused.is_empty());
    let (mut b, entry_b, kb) = drive_to_entry(host(MomentumParams::default()), &stream);
    fill(&mut b, &entry_b);
    assert_eq!(
        b.params().unwrap().revision(),
        2,
        "both changes were in before the entry"
    );
    assert!(
        exits_on_a_two_percent_dip(&mut b, &entry_b, stream[kb - 1].ts_recv()),
        "a new entry trails at 1%"
    );
}

#[test]
fn the_hold_and_the_exit_collar_are_frozen_when_the_entry_is_decided() {
    let events = healthy(1);
    let (mut h, entry, k) = drive_to_entry(host(MomentumParams::default()), &events);
    let t = events[k - 1].ts_recv();
    // While the entry order is still in flight, the agent allows longer holds (120 s -> 180 s)
    // and a wider collar (20 -> 25 permille). The position was decided under the old ones.
    for (n, (name, v)) in [("max_hold_secs", 180), ("collar_permille", 25)]
        .into_iter()
        .enumerate()
    {
        let change = h
            .params()
            .unwrap()
            .check(&proposal(name, v), t + (n as u64 + 1) * SEC)
            .unwrap();
        h.on_event(&Event::ParamChange(change));
    }
    fill(&mut h, &entry);
    h.advance_to(t + 123 * SEC); // the fill arrives at t + 2 s, so 120 s later is t + 122 s
    let out = h.drain_intents();
    assert_eq!(
        out.len(),
        1,
        "the position still leaves after the 120 s it was entered with"
    );
    assert_eq!(
        (out[0].purpose, out[0].reason),
        (Purpose::Close, crate::momentum::reason::EXIT_MAX_HOLD)
    );
    assert!(
        matches!(
            out[0].pricing,
            crate::Pricing::Collar {
                collar_permille: 20,
                ..
            }
        ),
        "and sells with the collar it was entered with: {:?}",
        out[0].pricing
    );
}

#[test]
fn a_per_instrument_value_applies_to_that_instrument_only() {
    // Instrument 0 is a healthy runner; instrument 1 never trades. The stop buffer is
    // 2% under the pullback low by default.
    let events = healthy(1);
    let stop_with = |target: Option<Target>| {
        let mut h = MomentumLong::new(StrategyId(1), MomentumParams::default(), 2)
            .unwrap()
            .host(2)
            .with_params(store());
        if let Some(target) = target {
            let mut p = proposal("stop_buffer_permille", 35); // 2% -> 3.5%
            p.target = target;
            let change = h.params().unwrap().check(&p, 0).unwrap();
            h.on_event(&Event::ParamChange(change));
        }
        for ev in &events {
            h.on_event(ev);
            if let Some(i) = h.drain_intents().into_iter().next() {
                return i.protect.unwrap().stop_trigger.raw();
            }
        }
        panic!("no entry");
    };
    let base = stop_with(None);
    assert!(
        stop_with(Some(Target::Instrument(0))) < base,
        "instrument 0 got the wider buffer"
    );
    assert_eq!(
        stop_with(Some(Target::Instrument(1))),
        base,
        "another instrument's override changes nothing here"
    );
    assert!(stop_with(Some(Target::Global)) < base);
}

// ---- rules and the store ----

#[test]
fn a_rule_threshold_that_names_a_parameter_follows_the_store_and_a_literal_does_not() {
    use crate::rules::{MOMENTUM_RULES, RuleSet};
    let events = healthy(1);
    let t0 = events[0].ts_recv();
    let (stream, refused) = with_changes(
        &events,
        &[
            (t0 + SEC, proposal("min_higher_lows", 2)),
            (t0 + 62 * SEC, proposal("min_higher_lows", 3)),
        ],
    );
    assert!(refused.is_empty());
    let with_rules = |text: &str| {
        let strat = MomentumLong::new(StrategyId(1), MomentumParams::default(), 1)
            .unwrap()
            .with_rules(RuleSet::parse(text).unwrap());
        let mut h = strat.host(1).with_params(store());
        let out = run_events(&mut h, &stream);
        (out, h.param_errors())
    };
    let base = run_events(&mut host(MomentumParams::default()), &events);
    // Naming the parameter: the same stricter entry as the built-in, to the intent.
    let (followed, e1) = with_rules(MOMENTUM_RULES);
    assert_eq!(e1, 0);
    assert_ne!(followed, base, "the change reached the rule");
    assert_eq!(
        followed,
        run_events(&mut host(MomentumParams::default()), &stream)
    );
    // Writing the same number as a literal: the store's change is ignored.
    let literal = MOMENTUM_RULES.replace("higher_lows >= @min_higher_lows", "higher_lows >= 1");
    let (fixed, e2) = with_rules(&literal);
    assert_eq!(e2, 0);
    assert_eq!(fixed, base, "a literal threshold is not tuned");
}
