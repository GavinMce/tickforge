use tf_core::{Event, ParamScope, ProviderId};
use tf_synth::SplitMix64;

use super::*;

const SEC: Nanos = 1_000_000_000;

fn spec(
    name: &'static str,
    baseline: i64,
    min: i64,
    max: i64,
    max_step: u64,
    cooldown_s: u64,
    scope: Scope,
) -> ParamSpec {
    ParamSpec {
        name,
        baseline,
        min,
        max,
        max_step,
        cooldown: cooldown_s * SEC,
        scope,
    }
}

/// trail: 10..=100, step 10, 60 s cooldown, per instrument. depth: 0..=500, step 50, 0 cooldown, global.
/// locked: cannot change.
fn store() -> ParamStore {
    ParamStore::new(vec![
        spec("trail", 30, 10, 100, 10, 60, Scope::PerInstrument),
        spec("depth", 350, 0, 500, 50, 0, Scope::Global),
        spec("locked", 5, 0, 10, 0, 0, Scope::Global),
    ])
    .unwrap()
}

const TRAIL: ParamId = 0;
const DEPTH: ParamId = 1;
const LOCKED: ParamId = 2;

fn prop(param: ParamId, target: Target, value: i64) -> Proposal {
    Proposal {
        param,
        target,
        value,
        proposer: 7,
        reason: 3,
        evidence: 0xabc,
    }
}

fn g(param: ParamId, value: i64) -> Proposal {
    prop(param, Target::Global, value)
}

// ---- declaration ----

#[test]
fn a_bad_declaration_is_refused() {
    let bad = |s: Vec<ParamSpec>| ParamStore::new(s).err();
    assert_eq!(
        bad(vec![spec("a", 5, 10, 0, 1, 0, Scope::Global)]),
        Some(SpecError::BadBounds("a"))
    );
    assert_eq!(
        bad(vec![spec("a", 11, 0, 10, 1, 0, Scope::Global)]),
        Some(SpecError::BaselineOutOfBounds("a"))
    );
    assert_eq!(
        bad(vec![spec("a", -1, 0, 10, 1, 0, Scope::Global)]),
        Some(SpecError::BaselineOutOfBounds("a"))
    );
    assert_eq!(
        bad(vec![
            spec("a", 1, 0, 10, 1, 0, Scope::Global),
            spec("a", 2, 0, 10, 1, 0, Scope::Global)
        ]),
        Some(SpecError::DuplicateName("a"))
    );
    assert!(ParamStore::new(vec![spec("edge", 0, 0, 0, 1, 0, Scope::Global)]).is_ok());
    let s = store();
    assert_eq!((s.id_of("depth"), s.id_of("nope")), (Some(DEPTH), None));
    assert_eq!((s.value(TRAIL), s.value(DEPTH), s.revision()), (30, 350, 0));
}

// ---- the rules ----

#[test]
fn bounds_are_inclusive_and_steps_are_measured_from_the_current_value() {
    let mut s = store();
    s.propose(&g(DEPTH, 400), 0).unwrap(); // +50 = max_step exactly
    assert_eq!(
        s.propose(&g(DEPTH, 451), 0),
        Err(Reject::StepTooLarge {
            step: 51,
            max_step: 50
        })
    );
    s.propose(&g(DEPTH, 450), 0).unwrap();
    s.propose(&g(DEPTH, 500), 0).unwrap(); // the max itself
    assert_eq!(
        s.propose(&g(DEPTH, 501), 0),
        Err(Reject::OutOfBounds { min: 0, max: 500 })
    );
    for v in [450, 400, 350, 300, 250, 200, 150, 100, 50, 0] {
        s.propose(&g(DEPTH, v), 0).unwrap();
    }
    assert_eq!(
        s.propose(&g(DEPTH, -1), 0),
        Err(Reject::OutOfBounds { min: 0, max: 500 })
    );
    assert_eq!(
        s.value(DEPTH),
        0,
        "walked down to the min, one step at a time"
    );
}

#[test]
fn the_other_refusals() {
    let mut s = store();
    assert_eq!(s.propose(&g(DEPTH, 350), 0), Err(Reject::NoChange));
    assert_eq!(s.propose(&g(LOCKED, 6), 0), Err(Reject::Frozen));
    assert_eq!(s.propose(&g(9, 1), 0), Err(Reject::UnknownParam(9)));
    assert_eq!(
        s.propose(&prop(DEPTH, Target::Instrument(4), 400), 0),
        Err(Reject::ScopeNotAllowed)
    );
    // A refusal changes nothing, not even the sequence number of the next event.
    assert_eq!((s.revision(), s.value(DEPTH)), (0, 350));
    assert_eq!(s.check(&g(DEPTH, 400), 0).unwrap().hdr.seq, 0);
}

#[test]
fn the_cooldown_is_per_parameter_and_target_and_inclusive_of_its_end() {
    let mut s = store();
    s.propose(&g(TRAIL, 40), 100 * SEC).unwrap();
    assert_eq!(
        s.propose(&g(TRAIL, 50), 159 * SEC),
        Err(Reject::Cooldown { until: 160 * SEC })
    );
    assert_eq!(
        s.propose(&g(TRAIL, 50), 160 * SEC - 1),
        Err(Reject::Cooldown { until: 160 * SEC })
    );
    // Another parameter is unaffected; so is the same parameter on an instrument.
    s.propose(&g(DEPTH, 400), 101 * SEC).unwrap();
    s.propose(&prop(TRAIL, Target::Instrument(3), 50), 101 * SEC)
        .unwrap();
    assert_eq!(
        s.propose(&prop(TRAIL, Target::Instrument(3), 60), 102 * SEC),
        Err(Reject::Cooldown { until: 161 * SEC })
    );
    s.propose(&prop(TRAIL, Target::Instrument(4), 50), 102 * SEC)
        .unwrap();
    s.propose(&g(TRAIL, 50), 160 * SEC).unwrap(); // exactly at the end
}

#[test]
fn per_instrument_values_override_and_persist_across_global_changes() {
    let mut s = store();
    s.propose(&prop(TRAIL, Target::Instrument(5), 20), 0)
        .unwrap();
    assert_eq!(
        (s.value(TRAIL), s.value_for(TRAIL, 5), s.value_for(TRAIL, 6)),
        (30, 20, 30)
    );
    s.propose(&g(TRAIL, 40), 0).unwrap();
    assert_eq!(
        (s.value_for(TRAIL, 5), s.value_for(TRAIL, 6)),
        (20, 40),
        "the global change did not clear the override"
    );
    // The step is measured from what the instrument has now, not from the global.
    assert_eq!(
        s.propose(&prop(TRAIL, Target::Instrument(5), 40), 100 * SEC),
        Err(Reject::StepTooLarge {
            step: 20,
            max_step: 10
        })
    );
    // A fresh instrument starts from the global.
    s.propose(&prop(TRAIL, Target::Instrument(7), 50), 100 * SEC)
        .unwrap();
}

#[test]
fn extreme_bounds_do_not_overflow_the_step_arithmetic() {
    let mut s = ParamStore::new(vec![spec(
        "wide",
        0,
        i64::MIN,
        i64::MAX,
        u64::MAX,
        0,
        Scope::Global,
    )])
    .unwrap();
    s.propose(&g(0, i64::MAX), 0).unwrap();
    s.propose(&g(0, i64::MIN), 0).unwrap();
    let mut t = ParamStore::new(vec![spec(
        "wide",
        i64::MAX,
        i64::MIN,
        i64::MAX,
        10,
        0,
        Scope::Global,
    )])
    .unwrap();
    assert!(matches!(
        t.propose(&g(0, i64::MIN), 0),
        Err(Reject::StepTooLarge { max_step: 10, .. })
    ));
}

// ---- events ----

#[test]
fn a_check_is_pure_and_describes_the_change_as_an_event() {
    let mut s = store();
    let ev = s
        .check(&prop(TRAIL, Target::Instrument(9), 40), 5 * SEC)
        .unwrap();
    assert_eq!(
        (s.revision(), s.value_for(TRAIL, 9)),
        (0, 30),
        "nothing happened yet"
    );
    assert_eq!(
        (
            ev.hdr.ts_event,
            ev.hdr.ts_recv,
            ev.hdr.seq,
            ev.hdr.instrument,
            ev.hdr.provider
        ),
        (5 * SEC, 5 * SEC, 0, 9, ProviderId::Internal)
    );
    assert_eq!(
        (
            ev.param,
            ev.scope,
            ev.new_value,
            ev.proposer,
            ev.reason,
            ev.evidence
        ),
        (TRAIL, ParamScope::Instrument, 40, 7, 3, 0xabc)
    );
    s.apply(&ev).unwrap();
    assert_eq!(s.value_for(TRAIL, 9), 40);
    let g = s.check(&g(DEPTH, 400), 6 * SEC).unwrap();
    assert_eq!(
        (g.scope, g.hdr.instrument, g.hdr.seq),
        (ParamScope::Global, 0, 1),
        "sequence numbers follow the applied events"
    );
}

#[test]
fn apply_rechecks_so_two_proposals_checked_together_cannot_both_land() {
    let mut s = store();
    // Both depth changes are fine against 350 (a step of 50 each way).
    let a = s.check(&g(DEPTH, 400), 0).unwrap();
    let b = s.check(&g(DEPTH, 300), 0).unwrap();
    let c = s.check(&g(TRAIL, 40), 0).unwrap();
    // The same parameter again, straight after: a valid step, but inside the cooldown.
    let mut d = c;
    d.new_value = 30;
    d.hdr.seq += 1;
    s.apply(&a).unwrap();
    assert_eq!(
        s.apply(&b),
        Err(Reject::StepTooLarge {
            step: 100,
            max_step: 50
        }),
        "from 400 it is two steps"
    );
    s.apply(&c).unwrap();
    assert!(
        matches!(s.apply(&d), Err(Reject::Cooldown { .. })),
        "the second trail change falls inside the cooldown"
    );
    let e = s.check(&g(DEPTH, 450), 0).unwrap();
    let f = s.check(&g(DEPTH, 450), 0).unwrap();
    s.apply(&e).unwrap();
    assert_eq!(s.apply(&f), Err(Reject::NoChange));
    assert_eq!(s.revision(), 3);
}

#[test]
fn a_tampered_or_foreign_event_is_refused_and_changes_nothing() {
    let mut s = store();
    let mut ev = s.check(&g(DEPTH, 400), 0).unwrap();
    ev.new_value = 999;
    assert_eq!(s.apply(&ev), Err(Reject::OutOfBounds { min: 0, max: 500 }));
    ev.new_value = 400;
    ev.param = 42;
    assert_eq!(s.apply(&ev), Err(Reject::UnknownParam(42)));
    assert_eq!((s.revision(), s.value(DEPTH)), (0, 350));
    // Non-change events pass through apply_event untouched.
    let trade = Event::Trade(tf_core::Trade {
        hdr: ev.hdr,
        px: tf_core::Px::from_cents(1),
        size: 1,
        flags: tf_core::TradeFlags::NONE,
    });
    assert_eq!(s.apply_event(&trade), None);
    let good = s.check(&g(DEPTH, 400), 0).unwrap();
    assert_eq!(s.apply_event(&Event::ParamChange(good)), Some(Ok(())));
}

// ---- history and entry-time values ----

#[test]
fn value_at_reports_what_a_position_opened_at_that_revision_was_governed_by() {
    let mut s = store();
    let r0 = s.revision();
    s.propose(&g(TRAIL, 40), 0).unwrap();
    let r1 = s.revision();
    s.propose(&prop(TRAIL, Target::Instrument(2), 35), 0)
        .unwrap();
    let r2 = s.revision();
    s.propose(&g(TRAIL, 50), 100 * SEC).unwrap();
    let r3 = s.revision();
    s.propose(&g(DEPTH, 300), 0).unwrap();
    let at = |rev: u64, i: u32| s.value_at(TRAIL, i, rev);
    assert_eq!(
        [at(r0, 2), at(r1, 2), at(r2, 2), at(r3, 2)],
        [30, 40, 35, 35]
    );
    assert_eq!(
        [at(r0, 1), at(r1, 1), at(r2, 1), at(r3, 1)],
        [30, 40, 40, 50]
    );
    assert_eq!(
        s.value_at(DEPTH, 1, r3),
        350,
        "a later change to another parameter does not show"
    );
    assert_eq!(s.value_at(DEPTH, 1, s.revision()), 300);
    assert_eq!(
        s.value_at(TRAIL, 1, 999),
        s.value_for(TRAIL, 1),
        "past the end is now"
    );
    let h = s.history();
    assert_eq!(h.len(), 4);
    assert_eq!((h[0].old, h[0].new, h[0].revision), (30, 40, 1));
    assert_eq!(
        (h[1].old, h[1].target),
        (40, Target::Instrument(2)),
        "an instrument's first change starts from the global"
    );
    assert_eq!((h[2].old, h[2].new, h[3].param), (40, 50, DEPTH));
}

// ---- replay ----

fn random_session(seed: u64) -> (ParamStore, Vec<ParamChange>, u64) {
    let mut rng = SplitMix64::new(seed);
    let mut s = store();
    let mut events = Vec::new();
    let mut now = 0;
    let mut refused = 0;
    for _ in 0..400 {
        now += rng.next_u64() % (40 * SEC);
        let param = (rng.next_u64() % 3) as ParamId;
        let target = if rng.next_u64() % 3 == 0 {
            Target::Instrument((rng.next_u64() % 4) as u32)
        } else {
            Target::Global
        };
        let cur = match target {
            Target::Global => s.value(param),
            Target::Instrument(i) => s.value_for(param, i),
        };
        let value = cur + (rng.next_u64() % 130) as i64 - 65;
        let p = Proposal {
            param,
            target,
            value,
            proposer: (rng.next_u64() % 3) as u16,
            reason: (rng.next_u64() % 9) as u16,
            evidence: rng.next_u64(),
        };
        match s.propose(&p, now) {
            Ok(ev) => events.push(ev),
            Err(_) => refused += 1,
        }
    }
    (s, events, refused)
}

#[test]
fn replaying_the_events_reproduces_the_store_exactly_and_every_change_obeys_its_rules() {
    for seed in 0..30 {
        let (live, events, refused) = random_session(seed);
        assert!(
            events.len() > 20 && refused > 20,
            "seed {seed}: {} applied, {refused} refused",
            events.len()
        );
        // Through the wire format, as a tape replay would.
        let wire: Vec<Event> = events
            .iter()
            .map(|e| {
                let mut b = Vec::new();
                Event::ParamChange(*e).encode(&mut b);
                Event::decode(&b).unwrap().0
            })
            .collect();
        let mut replay = store();
        for ev in &wire {
            assert_eq!(replay.apply_event(ev), Some(Ok(())), "seed {seed}");
        }
        assert_eq!(replay.history(), live.history(), "seed {seed}");
        for p in 0..3 {
            assert_eq!(replay.value(p), live.value(p));
            for i in 0..4 {
                assert_eq!(replay.value_for(p, i), live.value_for(p, i));
                for rev in [0, 5, live.revision() / 2, live.revision()] {
                    assert_eq!(replay.value_at(p, i, rev), live.value_at(p, i, rev));
                }
            }
        }
        // Independently of the store's own checks: every applied change is within bounds,
        // within its step, and outside the cooldown of the previous one at its target.
        let specs = store();
        let mut last: BTreeMap<(ParamId, Target), (Nanos, i64)> = BTreeMap::new();
        for a in live.history() {
            let sp = specs.specs()[usize::from(a.param)];
            assert!(a.new >= sp.min && a.new <= sp.max, "seed {seed}: {a:?}");
            assert!(
                a.old.abs_diff(a.new) <= sp.max_step && a.old != a.new,
                "seed {seed}: {a:?}"
            );
            if let Some(&(t, v)) = last.get(&(a.param, a.target)) {
                assert!(a.ts >= t + sp.cooldown, "seed {seed}: {a:?}");
                assert_eq!(a.old, v, "the old value is what the last change left");
            }
            if matches!(a.target, Target::Instrument(_)) {
                assert_eq!(sp.scope, Scope::PerInstrument);
            }
            assert_ne!(a.param, LOCKED);
            last.insert((a.param, a.target), (a.ts, a.new));
        }
        // Sequence numbers are consecutive.
        let seqs: Vec<u64> = live.history().iter().map(|a| a.seq).collect();
        assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
    }
}

// ---- the safety policy: revert to baseline ----

fn tuned_store() -> ParamStore {
    let mut s = store().with_lockout(300 * SEC);
    s.propose(&g(TRAIL, 40), 0).unwrap();
    s.propose(&g(DEPTH, 400), 0).unwrap();
    s.propose(&prop(TRAIL, Target::Instrument(3), 50), 0)
        .unwrap();
    s
}

#[test]
fn a_revert_returns_everything_to_baseline_ignoring_steps_and_cooldowns() {
    let mut s = tuned_store();
    // Walk depth a long way from its baseline (350), one allowed step at a time.
    s.propose(&g(DEPTH, 450), 10 * SEC).unwrap();
    s.propose(&g(DEPTH, 500), 20 * SEC).unwrap();
    assert_eq!(
        (s.value(TRAIL), s.value(DEPTH), s.value_for(TRAIL, 3)),
        (40, 500, 50)
    );
    let events = s.revert_events(30 * SEC, 9, 0xfeed);
    // Per parameter: its global, then its overrides; each is the policy's, back to baseline.
    assert_eq!(
        events
            .iter()
            .map(|e| (e.param, e.scope, e.hdr.instrument, e.new_value))
            .collect::<Vec<_>>(),
        [
            (TRAIL, ParamScope::Global, 0, 30),
            (TRAIL, ParamScope::Instrument, 3, 30),
            (DEPTH, ParamScope::Global, 0, 350)
        ]
    );
    assert!(
        events
            .iter()
            .all(|e| e.proposer == PROPOSER_POLICY && e.reason == 9 && e.evidence == 0xfeed)
    );
    assert_eq!(
        events.iter().map(|e| e.hdr.seq).collect::<Vec<_>>(),
        [5, 6, 7],
        "numbered after the 5 applied changes"
    );
    assert_eq!(s.value(DEPTH), 500, "nothing changed yet");
    for e in &events {
        s.apply(e).unwrap(); // 500 -> 350 is a step of 150, and trail was changed moments ago: allowed
    }
    assert_eq!(
        (s.value(TRAIL), s.value(DEPTH), s.value_for(TRAIL, 3)),
        (30, 350, 30)
    );
    assert!(s.history().iter().rev().take(3).all(|a| a.policy));
    assert!(
        s.revert_events(40 * SEC, 9, 0).is_empty(),
        "already at baseline: nothing to revert"
    );
}

#[test]
fn a_revert_clears_overrides_so_the_instrument_follows_the_global_again() {
    let mut s = tuned_store();
    for e in s.revert_events(30 * SEC, 1, 0) {
        s.apply(&e).unwrap();
    }
    // After the lockout, a global tune reaches instrument 3 (its override is gone).
    s.propose(&g(TRAIL, 40), 1_000 * SEC).unwrap();
    assert_eq!(s.value_for(TRAIL, 3), 40);
    // And the history replays the same way.
    let h = s.history().len() as u64;
    assert_eq!(
        (s.value_at(TRAIL, 3, h - 1), s.value_at(TRAIL, 3, h)),
        (30, 40)
    );
    assert_eq!(
        s.value_at(TRAIL, 3, 3),
        50,
        "before the revert it had its override"
    );
    assert_eq!(s.value_at(TRAIL, 3, 6), 30, "after it, the baseline");
}

#[test]
fn tuning_is_locked_out_after_a_revert_and_the_cooldown_restarts() {
    let mut s = tuned_store();
    for e in s.revert_events(100 * SEC, 1, 0) {
        s.apply(&e).unwrap();
    }
    assert_eq!(s.locked_until(), 400 * SEC);
    assert_eq!(
        s.propose(&g(DEPTH, 400), 399 * SEC),
        Err(Reject::Locked { until: 400 * SEC })
    );
    assert_eq!(
        s.propose(&prop(TRAIL, Target::Instrument(1), 40), 200 * SEC),
        Err(Reject::Locked { until: 400 * SEC })
    );
    s.propose(&g(DEPTH, 400), 400 * SEC).unwrap(); // the lockout ends exactly at its end
    // Without a lockout configured, a revert locks nothing.
    let mut free = store();
    free.propose(&g(DEPTH, 400), 0).unwrap();
    for e in free.revert_events(1, 1, 0) {
        free.apply(&e).unwrap();
    }
    assert_eq!(free.locked_until(), 0);
    free.propose(&g(DEPTH, 400), 1).unwrap();
}

#[test]
fn the_policys_proposer_id_is_reserved_and_its_events_may_only_return_to_baseline() {
    let mut s = tuned_store();
    let mut p = g(DEPTH, 450);
    p.proposer = PROPOSER_POLICY;
    assert_eq!(
        s.check(&p, 1_000 * SEC),
        Err(Reject::ReservedProposer),
        "an agent cannot claim it"
    );
    // A tape event that uses it for anything but the baseline is refused and changes nothing.
    let mut e = s.revert_events(5 * SEC, 1, 0)[2]; // depth, global
    e.new_value = 300;
    assert_eq!(s.apply(&e), Err(Reject::NotBaseline));
    e.new_value = 5; // the baseline of the frozen parameter, which is already there
    e.param = LOCKED; // frozen and already at baseline
    assert_eq!(s.apply(&e), Err(Reject::NoChange));
    assert_eq!(s.value(DEPTH), 400);
    let before = s.history().len();
    let good = s.revert_events(5 * SEC, 1, 0)[2];
    s.apply(&good).unwrap();
    assert_eq!(s.history().len(), before + 1);
    // The policy cannot use an override scope the parameter does not allow.
    let mut bad = good;
    bad.scope = ParamScope::Instrument;
    bad.hdr.instrument = 1;
    bad.param = DEPTH;
    assert_eq!(s.apply(&bad), Err(Reject::ScopeNotAllowed));
}

#[test]
fn reverts_replay_exactly_like_every_other_change() {
    for seed in 0..20 {
        let mut rng = SplitMix64::new(seed);
        let mut live = store().with_lockout(120 * SEC);
        let mut events = Vec::new();
        let mut now = 0;
        for _ in 0..300 {
            now += rng.next_u64() % (50 * SEC);
            if rng.next_u64() % 25 == 0 {
                for e in live.revert_events(now, 1, 0) {
                    live.apply(&e).unwrap();
                    events.push(e);
                }
                continue;
            }
            let param = (rng.next_u64() % 2) as ParamId;
            let target = if param == TRAIL && rng.next_u64() % 3 == 0 {
                Target::Instrument((rng.next_u64() % 3) as u32)
            } else {
                Target::Global
            };
            let cur = match target {
                Target::Global => live.value(param),
                Target::Instrument(i) => live.value_for(param, i),
            };
            let p = Proposal {
                param,
                target,
                value: cur + (rng.next_u64() % 80) as i64 - 40,
                proposer: 2,
                reason: 1,
                evidence: 0,
            };
            if let Ok(e) = live.propose(&p, now) {
                events.push(e);
            }
        }
        assert!(
            live.history().iter().any(|a| a.policy),
            "seed {seed} included a revert"
        );
        let mut replay = store().with_lockout(120 * SEC);
        for e in &events {
            let mut b = Vec::new();
            Event::ParamChange(*e).encode(&mut b);
            replay
                .apply_event(&Event::decode(&b).unwrap().0)
                .unwrap()
                .unwrap();
        }
        assert_eq!(replay.history(), live.history(), "seed {seed}");
        assert_eq!(replay.locked_until(), live.locked_until());
        for p in 0..2 {
            for i in 0..3 {
                for rev in [0, live.revision() / 2, live.revision()] {
                    assert_eq!(replay.value_at(p, i, rev), live.value_at(p, i, rev));
                }
                assert_eq!(replay.value_for(p, i), live.value_for(p, i));
            }
        }
        // No tuning change ever landed inside a lockout.
        let mut locked = 0;
        for a in live.history() {
            if a.policy {
                locked = a.ts + 120 * SEC;
            } else {
                assert!(
                    a.ts >= locked,
                    "seed {seed}: {a:?} inside a lockout ending {locked}"
                );
            }
        }
    }
}

// ---- the policy itself ----

#[test]
fn the_policy_trips_on_a_drawdown_in_the_relative_curve_and_rearms() {
    assert_eq!(RevertPolicy::new(0), Err(PolicyError::ZeroDrawdown));
    let mut p = RevertPolicy::new(100).unwrap();
    // Together, then the tuned side pulls ahead by 50 and falls back: a drawdown from the peak.
    assert_eq!(p.observe(0, 0), None);
    assert_eq!(p.observe(150, 100), None); // relative +50
    assert_eq!(p.observe(100, 100), None); // relative 0: down 50 from the peak
    assert_eq!(p.observe(100, 149), None); // relative -49: down 99
    let t = p.observe(100, 150).unwrap(); // relative -50: down exactly 100
    assert_eq!((t.relative, t.peak, t.drawdown), (-50, 50, 100));
    // Re-armed from here: a fall of 99 more is not enough, 100 is.
    assert_eq!(p.observe(100, 249), None);
    assert!(p.observe(100, 250).is_some());
}

#[test]
fn a_shadow_that_is_also_losing_is_not_a_reason_to_revert() {
    let mut p = RevertPolicy::new(100).unwrap();
    // Both fall together by far more than the limit: the relative curve is flat.
    for k in 0..50 {
        assert_eq!(p.observe(-k * 50, -k * 50), None);
    }
    // And the tuned side falling while the shadow falls less does count.
    assert!(p.observe(-3_000, -2_800).is_some());
}

#[test]
fn the_peak_starts_at_zero_not_at_the_first_observation() {
    let mut p = RevertPolicy::new(100).unwrap();
    // The first thing seen is already 100 behind: that is a drawdown from the starting zero.
    assert!(p.observe(0, 100).is_some());
    let mut q = RevertPolicy::new(100).unwrap();
    assert_eq!(q.observe(500, 400), None);
    assert_eq!(q.peak(), 100);
}
