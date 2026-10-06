use tf_core::{
    Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Quote, TierAction, TierChange, Trade,
    TradeFlags,
};
use tf_engine::{Promoter, PromoterConfig, ScannerConfig, Tier0, TierError, tier_reason};
use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

const SEC: Nanos = NANOS_PER_SEC;
const T0: Nanos = 1_767_571_200 * SEC;

fn hdr(inst: u32, ts: Nanos) -> Header {
    Header {
        ts_event: ts,
        ts_recv: ts,
        seq: ts,
        instrument: inst,
        provider: ProviderId::Synthetic,
    }
}

fn trade(inst: u32, ts: Nanos, cents: i64, size: u32) -> Event {
    Event::Trade(Trade {
        hdr: hdr(inst, ts),
        px: Px::from_cents(cents),
        size,
        flags: TradeFlags::NONE,
    })
}

fn quote(inst: u32, ts: Nanos, bid: i64, ask: i64) -> Event {
    Event::Quote(Quote {
        hdr: hdr(inst, ts),
        bid_px: Px::from_cents(bid),
        ask_px: Px::from_cents(ask),
        bid_sz: 100,
        ask_sz: 100,
    })
}

/// A hand-built symbol: for each phase, `secs` seconds of one trade a second of `size`
/// shares at `cents` (with a two-cent quote around it).
struct Phase {
    secs: u64,
    size: u32,
    cents: i64,
}

fn ph(secs: u64, size: u32, cents: i64) -> Phase {
    Phase { secs, size, cents }
}

fn symbol(inst: u32, start_sec: u64, phases: &[Phase]) -> Vec<Event> {
    let mut v = Vec::new();
    let mut s = start_sec;
    for p in phases {
        for _ in 0..p.secs {
            let t = T0 + s * SEC;
            v.push(quote(inst, t, p.cents - 1, p.cents + 1));
            v.push(trade(inst, t + SEC / 2, p.cents, p.size));
            s += 1;
        }
    }
    v
}

fn merge(mut a: Vec<Event>, b: Vec<Event>) -> Vec<Event> {
    a.extend(b);
    a.sort_by_key(|e| e.ts_recv());
    a
}

fn scanner() -> ScannerConfig {
    ScannerConfig {
        min_volume: 1_000,
        ..ScannerConfig::default()
    }
}

struct Run {
    changes: Vec<TierChange>,
    /// The stream with each decision placed right after the event that caused it.
    tape: Vec<Event>,
    promoter: Promoter,
    max_live: usize,
}

fn run(ev: &[Event], n: usize, cfg: PromoterConfig, sc: ScannerConfig) -> Run {
    run_with(ev, n, cfg, sc, |_, _| {})
}

fn run_with(
    ev: &[Event],
    n: usize,
    cfg: PromoterConfig,
    sc: ScannerConfig,
    mut hook: impl FnMut(&mut Promoter, &Event),
) -> Run {
    let mut t0 = Tier0::new(n);
    let mut p = Promoter::new(cfg, sc, n).unwrap();
    let (mut changes, mut tape, mut max_live) = (Vec::new(), Vec::new(), 0);
    for e in ev {
        hook(&mut p, e);
        t0.on_event(e);
        let mut out = Vec::new();
        p.on_event(&t0, e, &mut out);
        // The tape holds a decision just before the event that caused it.
        for c in out {
            tape.push(Event::TierChange(c));
            changes.push(c);
        }
        tape.push(*e);
        max_live = max_live.max(p.promoted().len());
    }
    Run {
        changes,
        tape,
        promoter: p,
        max_live,
    }
}

fn secs_of(c: &TierChange) -> u64 {
    (c.hdr.ts_recv - T0) / SEC
}

// ---- the universe ----

#[test]
fn only_runners_are_promoted_and_every_runner_with_a_baseline_is() {
    for seed in 0..3 {
        let cfg = SynthConfig::universe(seed, 400, 900 * SEC, 50);
        let ev: Vec<Event> = SynthStream::new(&cfg).collect();
        let r = run(
            &ev,
            400,
            PromoterConfig::default(),
            ScannerConfig::default(),
        );
        let promoted: std::collections::BTreeSet<usize> = r
            .changes
            .iter()
            .filter(|c| c.action == TierAction::Promote)
            .map(|c| c.hdr.instrument as usize)
            .collect();
        for (i, s) in cfg.symbols.iter().enumerate() {
            let runner = s.scenario.name != "quiet";
            let lead = s.scenario.catalyst / SEC;
            if !runner {
                assert!(
                    !promoted.contains(&i),
                    "seed {seed}: quiet symbol {i} promoted"
                );
            } else if lead >= 70 && lead + 30 < 900 {
                assert!(
                    promoted.contains(&i),
                    "seed {seed}: runner {i} (catalyst {lead} s) not promoted"
                );
            }
        }
        assert!(r.max_live <= 50 && r.promoter.refused_full() == 0);
    }
}

#[test]
fn the_tape_events_are_well_formed() {
    let cfg = SynthConfig::universe(1, 200, 900 * SEC, 50);
    let ev: Vec<Event> = SynthStream::new(&cfg).collect();
    let r = run(
        &ev,
        200,
        PromoterConfig::default(),
        ScannerConfig::default(),
    );
    assert!(r.changes.len() >= 4);
    for (k, c) in r.changes.iter().enumerate() {
        assert_eq!(c.hdr.seq, k as u64, "consecutive sequence numbers");
        assert_eq!(c.hdr.provider, ProviderId::Internal);
        assert_eq!(c.hdr.ts_event, c.hdr.ts_recv);
        match c.action {
            TierAction::Promote => assert!(
                c.reason == tier_reason::SCANNER_HIT && c.score >= 8_000,
                "{c:?}"
            ),
            TierAction::Demote => assert_eq!(c.reason, tier_reason::COOLED_OFF),
        }
    }
    // Each decision sits directly before the market event that caused it, with the same time.
    for (i, e) in r.tape.iter().enumerate() {
        if let Event::TierChange(c) = e {
            let next = r.tape[i..]
                .iter()
                .find(|x| !matches!(x, Event::TierChange(_)))
                .unwrap();
            assert_eq!(next.ts_recv(), c.hdr.ts_recv);
        }
    }
}

// ---- bounded ----

#[test]
fn tier_one_never_exceeds_its_bound_and_refusals_are_counted() {
    let spec = |i: usize| SymbolSpec {
        symbol: format!("R{i}"),
        base_px_cents: 500,
        base_interval_ns: 300_000_000,
        quote_every: 2,
        scenario: Scenario::runner(PullbackKind::Healthy, 120 * SEC),
        news: Vec::new(),
    };
    let cfg = SynthConfig {
        seed: 4,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: 400 * SEC,
        symbols: (0..12).map(spec).collect(),
    };
    let ev: Vec<Event> = SynthStream::new(&cfg).collect();
    let r = run(
        &ev,
        12,
        PromoterConfig {
            max_tier1: 3,
            ..PromoterConfig::default()
        },
        ScannerConfig::default(),
    );
    assert_eq!(r.max_live, 3, "twelve runners want in, three get in");
    assert!(r.promoter.refused_full() > 0);
    assert_eq!(
        r.changes
            .iter()
            .filter(|c| c.action == TierAction::Promote)
            .count(),
        3
    );
}

// ---- demotion and hysteresis ----

fn burst_then_quiet() -> Vec<Event> {
    // 120 s steady, a 10 s burst (20x volume, +8%), then 400 s steady at the higher price.
    symbol(
        0,
        0,
        &[
            ph(120, 1_000, 500),
            ph(10, 20_000, 540),
            ph(400, 1_000, 540),
        ],
    )
}

#[test]
fn a_run_is_promoted_at_once_and_demoted_only_after_it_has_been_cold_for_the_cooldown() {
    let r = run(&burst_then_quiet(), 1, PromoterConfig::default(), scanner());
    let (p, d): (Vec<_>, Vec<_>) = r
        .changes
        .iter()
        .partition(|c| c.action == TierAction::Promote);
    assert_eq!((p.len(), d.len()), (1, 1));
    assert!(
        (120..=122).contains(&secs_of(p[0])),
        "promoted at {} s",
        secs_of(p[0])
    );
    // The burst ends at 130 s; the 10 s window stays elevated until about 140 s; then 120 s cooldown.
    let held = secs_of(d[0]) - secs_of(p[0]);
    assert!((125..=145).contains(&held), "held for {held} s");
    assert!(r.promoter.symbol(0).is_none() && !r.promoter.is_promoted(0));
}

#[test]
fn a_symbol_that_stops_trading_altogether_is_still_demoted() {
    // A bursts and then goes silent; B trades throughout and keeps the clock moving.
    let a = symbol(0, 0, &[ph(120, 1_000, 500), ph(10, 20_000, 540)]);
    let b = symbol(1, 0, &[ph(500, 1_000, 700)]);
    let r = run(&merge(a, b), 2, PromoterConfig::default(), scanner());
    let d: Vec<_> = r
        .changes
        .iter()
        .filter(|c| c.action == TierAction::Demote)
        .collect();
    assert_eq!(d.len(), 1, "{:?}", r.changes);
    assert_eq!(d[0].hdr.instrument, 0);
    assert!(
        (250..=280).contains(&secs_of(d[0])),
        "demoted at {} s",
        secs_of(d[0])
    );
}

#[test]
fn a_symbol_between_the_two_bars_neither_flaps_nor_is_demoted() {
    // After the burst volume settles at ~1,700 shares a second: about 4.7 deviations, above the
    // demotion bar (3) and below the promotion bar (8). It stays in, with no further events.
    let ev = symbol(
        0,
        0,
        &[
            ph(120, 1_000, 500),
            ph(10, 20_000, 540),
            ph(400, 1_700, 540),
        ],
    );
    let r = run(&ev, 1, PromoterConfig::default(), scanner());
    assert_eq!(r.changes.len(), 1, "{:?}", r.changes);
    assert!(r.promoter.is_promoted(0));
    // With the demotion bar raised above where it sits, the same stream does demote.
    let strict = PromoterConfig {
        demote_z_milli: 7_000,
        ..PromoterConfig::default()
    };
    let r = run(&ev, 1, strict, scanner());
    assert_eq!(
        r.changes
            .iter()
            .filter(|c| c.action == TierAction::Demote)
            .count(),
        1
    );
}

#[test]
fn repeated_bursts_keep_one_promotion_with_hysteresis_and_flap_without_it() {
    // Six 10 s bursts a minute and a half apart.
    let mut phases = vec![ph(120, 1_000, 500)];
    for k in 1..=6 {
        // Each burst is a new high, so the price gate passes every time.
        phases.push(ph(10, 20_000, 500 + 40 * k));
        phases.push(ph(80, 1_000, 500 + 40 * k));
    }
    let ev = symbol(0, 0, &phases);
    let calm = run(&ev, 1, PromoterConfig::default(), scanner());
    assert_eq!(
        calm.changes.len(),
        1,
        "one promotion and no demotions: {:?}",
        calm.changes
    );
    let twitchy = PromoterConfig {
        cooldown_secs: 5,
        min_dwell_secs: 1,
        repromote_after_secs: 0,
        ..PromoterConfig::default()
    };
    let flappy = run(&ev, 1, twitchy, scanner());
    assert!(
        flappy.changes.len() >= 6,
        "without hysteresis it moves {} times",
        flappy.changes.len()
    );
}

#[test]
fn a_symbol_stays_for_at_least_the_dwell_and_cannot_return_for_the_repromote_wait() {
    // A burst at 120 s, a second (to a new high) at 200 s.
    let ev = symbol(
        0,
        0,
        &[
            ph(120, 1_000, 500),
            ph(10, 20_000, 540),
            ph(60, 1_000, 540),
            ph(10, 20_000, 580),
            ph(300, 1_000, 580),
        ],
    );
    let long_dwell = PromoterConfig {
        cooldown_secs: 5,
        min_dwell_secs: 100,
        repromote_after_secs: 0,
        ..PromoterConfig::default()
    };
    let r = run(&ev, 1, long_dwell, scanner());
    let first = &r.changes[0];
    let demote = r
        .changes
        .iter()
        .find(|c| c.action == TierAction::Demote)
        .unwrap();
    assert!(
        secs_of(demote) - secs_of(first) >= 100,
        "dwell: demoted after {} s",
        secs_of(demote) - secs_of(first)
    );

    // With no dwell it would have gone at about 145 s (five seconds after the first burst cooled).
    let short = PromoterConfig {
        min_dwell_secs: 1,
        ..long_dwell
    };
    let r = run(&ev, 1, short, scanner());
    let early = r
        .changes
        .iter()
        .find(|c| c.action == TierAction::Demote)
        .unwrap();
    assert!(secs_of(early) < 150, "demoted at {}", secs_of(early));

    // Demoted at about 145 s; a 100 s wait runs to about 245 s, which swallows the second burst.
    let waiting = PromoterConfig {
        repromote_after_secs: 100,
        ..short
    };
    let r = run(&ev, 1, waiting, scanner());
    assert_eq!(
        r.changes
            .iter()
            .filter(|c| c.action == TierAction::Promote)
            .count(),
        1,
        "{:?}",
        r.changes
    );
    let immediate = PromoterConfig {
        repromote_after_secs: 0,
        ..short
    };
    let r = run(&ev, 1, immediate, scanner());
    assert_eq!(
        r.changes
            .iter()
            .filter(|c| c.action == TierAction::Promote)
            .count(),
        2
    );
}

#[test]
fn several_hits_can_be_required_before_promoting() {
    // A burst keeps qualifying every second while it sits in the 10 s window, so asking for
    // three hits promotes two seconds later, and asking for three within one second never can.
    let ev = burst_then_quiet();
    let first = |cfg: PromoterConfig| run(&ev, 1, cfg, scanner()).changes.first().map(secs_of);
    let one = first(PromoterConfig::default()).unwrap();
    let three = first(PromoterConfig {
        confirm_hits: 3,
        confirm_window_secs: 5,
        ..PromoterConfig::default()
    })
    .unwrap();
    assert_eq!(three, one + 2);
    assert_eq!(
        first(PromoterConfig {
            confirm_hits: 3,
            confirm_window_secs: 1,
            ..PromoterConfig::default()
        }),
        None
    );
}

#[test]
fn a_pinned_symbol_is_never_demoted_until_it_is_released() {
    let ev = burst_then_quiet();
    let r = run_with(&ev, 1, PromoterConfig::default(), scanner(), |p, e| {
        if e.ts_recv() >= T0 + 125 * SEC {
            p.pin(0, 0);
        }
    });
    assert!(
        r.promoter.is_promoted(0) && r.promoter.is_pinned(0),
        "{:?}",
        r.changes
    );
    // A promoted symbol is being recorded: its trades and quotes are in Tier 1.
    let t1 = r.promoter.symbol(0).unwrap();
    assert!(
        t1.ticks_len() > 200 && t1.quotes_len() > 0,
        "{} ticks",
        t1.ticks_len()
    );
    assert_eq!(r.changes.len(), 1);
    // Released, it goes at the next sweep.
    let released = run_with(&ev, 1, PromoterConfig::default(), scanner(), |p, e| {
        let t = e.ts_recv();
        if (T0 + 125 * SEC..T0 + 300 * SEC).contains(&t) {
            p.pin(0, 0);
        } else if t >= T0 + 300 * SEC {
            p.unpin(0, 0);
        }
    });
    let d = released
        .changes
        .iter()
        .find(|c| c.action == TierAction::Demote)
        .unwrap();
    assert!(
        (300..=302).contains(&secs_of(d)),
        "demoted at {}",
        secs_of(d)
    );
}

// ---- replay ----

#[test]
fn a_follower_fed_the_tape_has_the_same_tier_one_at_every_step() {
    let cfg = SynthConfig::universe(2, 300, 900 * SEC, 60);
    let ev: Vec<Event> = SynthStream::new(&cfg).collect();
    // The live run. For each market event: the tape entries it produced (the event and then
    // the decisions it caused) and who is promoted, and what they look like, afterwards.
    let mut t0 = Tier0::new(300);
    let mut live = Promoter::new(PromoterConfig::default(), ScannerConfig::default(), 300).unwrap();
    type Features = Vec<Option<tf_engine::PullbackFeatures>>;
    let mut groups: Vec<(Vec<Event>, Vec<u32>, Option<Features>)> = Vec::new();
    for (i, e) in ev.iter().enumerate() {
        t0.on_event(e);
        let mut out = Vec::new();
        live.on_event(&t0, e, &mut out);
        // Decisions go on the tape just before the event that caused them.
        let mut entries: Vec<Event> = out.into_iter().map(Event::TierChange).collect();
        entries.push(*e);
        let members = live.promoted().to_vec();
        let features = (i % 50 == 0).then(|| {
            members
                .iter()
                .map(|&id| live.symbol(id).unwrap().features())
                .collect()
        });
        groups.push((entries, members, features));
    }
    let changes: usize = groups.iter().map(|g| g.0.len() - 1).sum();
    assert!(
        changes > 6 && groups.iter().any(|g| g.1.len() >= 2),
        "{changes} changes"
    );
    assert!(
        groups.iter().all(|g| g.1.windows(2).all(|w| w[0] < w[1])),
        "members are always listed in id order"
    );

    let mut t1 = Tier0::new(300);
    let mut follower = Promoter::follower(50, 300);
    let mut sink = Vec::new();
    let mut compared_features = 0;
    for (i, (entries, members, features)) in groups.iter().enumerate() {
        for e in entries {
            // Through the wire format, as a tape.
            let mut b = Vec::new();
            e.encode(&mut b);
            let e = Event::decode(&b).unwrap().0;
            t1.on_event(&e);
            follower.on_event(&t1, &e, &mut sink);
        }
        assert_eq!(
            follower.promoted(),
            members.as_slice(),
            "membership differs after event {i}"
        );
        if let Some(want) = features {
            let got: Features = follower
                .promoted()
                .iter()
                .map(|&id| follower.symbol(id).unwrap().features())
                .collect();
            assert_eq!(&got, want, "features differ after event {i}");
            compared_features += got.len();
        }
    }
    assert!(sink.is_empty(), "a follower decides nothing");
    assert!(
        compared_features > 10,
        "{compared_features} feature sets compared"
    );
    // Market events alone give a follower nothing.
    let mut bare = Promoter::follower(50, 300);
    let mut t2 = Tier0::new(300);
    for e in ev.iter().take(20_000) {
        t2.on_event(e);
        bare.on_event(&t2, e, &mut sink);
    }
    assert!(bare.promoted().is_empty());
}

#[test]
fn a_deciding_promoter_ignores_tier_changes_in_its_input() {
    let mut t0 = Tier0::new(4);
    let mut p = Promoter::new(PromoterConfig::default(), scanner(), 4).unwrap();
    let change = TierChange {
        hdr: Header {
            ts_event: T0,
            ts_recv: T0,
            seq: 0,
            instrument: 3,
            provider: ProviderId::Internal,
        },
        action: TierAction::Promote,
        reason: 1,
        score: 0,
    };
    let e = Event::TierChange(change);
    t0.on_event(&e);
    let mut out = Vec::new();
    p.on_event(&t0, &e, &mut out);
    assert!(
        p.promoted().is_empty() && out.is_empty(),
        "its decisions are its own"
    );
}

#[test]
fn applying_changes_is_checked_and_a_failure_changes_nothing() {
    let mk = |action, inst, seq| TierChange {
        hdr: Header {
            ts_event: T0,
            ts_recv: T0,
            seq,
            instrument: inst,
            provider: ProviderId::Internal,
        },
        action,
        reason: 1,
        score: 0,
    };
    let mut p = Promoter::follower(1, 4);
    assert_eq!(
        p.apply(&mk(TierAction::Demote, 2, 0)),
        Err(TierError::NotPromoted)
    );
    p.apply(&mk(TierAction::Promote, 2, 1)).unwrap();
    assert!(
        matches!(
            p.apply(&mk(TierAction::Promote, 3, 2)),
            Err(TierError::Promote(_))
        ),
        "full"
    );
    assert!(
        matches!(
            p.apply(&mk(TierAction::Promote, 2, 3)),
            Err(TierError::Promote(_))
        ),
        "already promoted"
    );
    assert_eq!(p.promoted(), [2]);
    p.apply(&mk(TierAction::Demote, 2, 4)).unwrap();
    assert!(p.promoted().is_empty());
}

#[test]
fn bad_configurations_are_refused() {
    let ok = PromoterConfig::default();
    let sc = ScannerConfig::default();
    for bad in [
        PromoterConfig { max_tier1: 0, ..ok },
        PromoterConfig {
            cooldown_secs: 0,
            ..ok
        },
        PromoterConfig {
            confirm_hits: 0,
            ..ok
        },
        PromoterConfig {
            confirm_window_secs: 0,
            ..ok
        },
        PromoterConfig {
            demote_z_milli: 0,
            ..ok
        },
        PromoterConfig {
            demote_z_milli: sc.min_z_milli,
            ..ok
        },
    ] {
        assert!(Promoter::new(bad, sc, 1).is_err(), "{bad:?}");
    }
    assert!(
        Promoter::new(
            ok,
            ScannerConfig {
                spike_secs: 0,
                ..sc
            },
            1
        )
        .is_err()
    );
    assert!(Promoter::new(ok, sc, 1).is_ok());
}

#[test]
fn the_same_stream_gives_the_same_decisions() {
    let cfg = SynthConfig::universe(5, 200, 600 * SEC, 80);
    let ev: Vec<Event> = SynthStream::new(&cfg).collect();
    let a = run(
        &ev,
        200,
        PromoterConfig::default(),
        ScannerConfig::default(),
    );
    let b = run(
        &ev,
        200,
        PromoterConfig::default(),
        ScannerConfig::default(),
    );
    assert!(!a.changes.is_empty());
    assert_eq!(a.changes, b.changes);
}
