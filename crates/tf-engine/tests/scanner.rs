use tf_core::{Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_engine::{Hit, Scanner, ScannerConfig, ScannerError, Tier0};
use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

const SEC: Nanos = NANOS_PER_SEC;

fn spec(symbol: &str, base: i64, interval_ms: u64, scenario: Scenario) -> SymbolSpec {
    SymbolSpec {
        symbol: symbol.into(),
        base_px_cents: base,
        base_interval_ns: interval_ms * 1_000_000,
        quote_every: 2,
        scenario,
        news: Vec::new(),
    }
}

fn events(seed: u64, secs: u64, symbols: Vec<SymbolSpec>) -> Vec<Event> {
    let cfg = SynthConfig {
        seed,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: secs * SEC,
        symbols,
    };
    SynthStream::new(&cfg).collect()
}

fn scan(events: &[Event], n: usize, cfg: ScannerConfig) -> Vec<Hit> {
    let mut t0 = Tier0::new(n);
    let mut sc = Scanner::new(cfg, n).unwrap();
    let mut hits = Vec::new();
    for ev in events {
        t0.on_event(ev);
        sc.on_event(&t0, ev, &mut hits);
    }
    hits
}

fn runner(kind: PullbackKind) -> impl Fn(u64) -> Scenario {
    move |lead| Scenario::runner(kind, lead * SEC)
}

fn detect_all(name: &str, mk: &dyn Fn(u64) -> Scenario, secs: u64) {
    for lead in [60u64, 90, 120, 200] {
        for seed in 0..10 {
            let ev = events(seed, secs, vec![spec("X", 500, 300, mk(lead))]);
            let catalyst = ev[0].ts_recv() + lead * SEC;
            let hits = scan(&ev, 1, ScannerConfig::default());
            let first = hits
                .first()
                .unwrap_or_else(|| panic!("{name}: lead {lead} seed {seed}: missed"));
            assert!(
                first.ts >= catalyst,
                "{name}: a hit {} s before the catalyst",
                (catalyst - first.ts) / SEC
            );
            assert!(
                first.ts <= catalyst + 3 * SEC,
                "{name}: lead {lead} seed {seed}: first hit {} s after the catalyst",
                (first.ts - catalyst) / SEC
            );
        }
    }
}

// ---- no false negatives ----

#[test]
fn every_runner_halt_and_squeeze_scenario_is_detected_within_seconds_of_its_catalyst() {
    detect_all("healthy runner", &runner(PullbackKind::Healthy), 500);
    detect_all("dangerous runner", &runner(PullbackKind::Dangerous), 500);
    detect_all("halt up", &|l| Scenario::halt_up(l * SEC), 500);
    detect_all("squeeze", &|l| Scenario::squeeze(l * SEC), 500);
}

#[test]
fn each_spike_of_a_multi_spike_name_is_detected() {
    for seed in 0..10 {
        let ev = events(
            seed,
            700,
            vec![spec("M", 500, 300, Scenario::multi_spike(3, 90 * SEC))],
        );
        let hits = scan(&ev, 1, ScannerConfig::default());
        // Spikes start 90 s, 225 s and 360 s in (20 s spike, 25 s pullback, 90 s cooldown each).
        let clusters = hits.iter().fold(Vec::<Nanos>::new(), |mut c, h| {
            if c.last().is_none_or(|&l| h.ts - l > 60 * SEC) {
                c.push(h.ts);
            }
            c
        });
        assert_eq!(
            clusters.len(),
            3,
            "seed {seed}: {} clusters",
            clusters.len()
        );
    }
}

#[test]
fn a_universe_of_mostly_quiet_names_gives_hits_only_on_the_runners_and_none_are_missed() {
    for seed in 0..4 {
        let cfg = SynthConfig::universe(seed, 400, 900 * SEC, 50);
        let runners: Vec<usize> = cfg
            .symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| s.scenario.name != "quiet")
            .map(|(i, _)| i)
            .collect();
        assert!(
            runners.len() >= 10,
            "seed {seed}: {} runners",
            runners.len()
        );
        let ev: Vec<Event> = SynthStream::new(&cfg).collect();
        let hits = scan(&ev, 400, ScannerConfig::default());
        let hit_ids: std::collections::BTreeSet<usize> =
            hits.iter().map(|h| h.instrument as usize).collect();
        for h in &hit_ids {
            assert!(
                runners.contains(h),
                "seed {seed}: a hit on quiet symbol {h}"
            );
        }
        for r in &runners {
            // Runners whose impulse falls in the session and has had a minute of baseline; the
            // universe's lead-ins are 30 to 300 s, with a 60 s warm-up.
            let lead = cfg.symbols[*r].scenario.catalyst / SEC;
            if lead >= 70 && lead + 30 < 900 {
                assert!(
                    hit_ids.contains(r),
                    "seed {seed}: missed runner {r} (catalyst at {lead} s)"
                );
            }
        }
    }
}

// ---- the false-positive rate ----

#[test]
fn quiet_symbols_never_hit_in_two_hundred_symbol_hours_and_the_gates_are_what_prevent_it() {
    let (mut hits, mut relaxed) = (0, 0);
    for seed in 0..4 {
        let cfg = SynthConfig::universe(seed, 200, 900 * SEC, 0);
        let ev: Vec<Event> = SynthStream::new(&cfg).collect();
        hits += scan(&ev, 200, ScannerConfig::default()).len();
        // The same data with the volume gates nearly removed does hit: the measurement can see false positives.
        let loose = ScannerConfig {
            min_z_milli: 1_000,
            min_volume: 0,
            min_change_permille: 5,
            ..ScannerConfig::default()
        };
        relaxed += scan(&ev, 200, loose).len();
    }
    assert_eq!(
        hits, 0,
        "measured: 0 hits in 4 x 200 symbols x 900 s = 200 symbol-hours"
    );
    assert!(
        relaxed > 20,
        "with the gates relaxed it fires {relaxed} times, so a zero above means something"
    );
}

// ---- the mechanics, on hand-built streams ----

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

const T0: Nanos = 1_767_571_200 * SEC;

/// `calm` seconds of one 1,000-share trade a second near $5.00 with a tight quote, then a
/// burst: `burst_size` shares a second at a price `burst_cents` above, for 10 s.
fn steady_then_burst(calm: u64, burst_size: u32, burst_cents: i64) -> Vec<Event> {
    let mut v = Vec::new();
    for s in 0..calm {
        v.push(quote(0, T0 + s * SEC, 499, 501));
        v.push(trade(0, T0 + s * SEC + SEC / 2, 500, 1_000));
    }
    for s in 0..10 {
        let t = T0 + (calm + s) * SEC;
        v.push(quote(0, t, 499 + burst_cents, 501 + burst_cents));
        v.push(trade(0, t + SEC / 2, 500 + burst_cents, burst_size));
    }
    v
}

fn hits(ev: &[Event], cfg: ScannerConfig) -> Vec<Hit> {
    scan(ev, 1, cfg)
}

#[test]
fn a_big_burst_with_a_price_spike_hits_and_a_small_wobble_does_not() {
    let cfg = ScannerConfig {
        min_volume: 1_000,
        ..ScannerConfig::default()
    };
    assert!(
        !hits(&steady_then_burst(120, 20_000, 40), cfg).is_empty(),
        "20x the volume and +8%"
    );
    // 30% more volume is about 2 standard deviations once the 15% floor under the deviation applies.
    assert!(hits(&steady_then_burst(120, 1_300, 40), cfg).is_empty());
    // Volume without a price rise, and a price rise without volume, each fail one gate.
    assert!(hits(&steady_then_burst(120, 20_000, 0), cfg).is_empty());
    assert!(hits(&steady_then_burst(120, 1_000, 40), cfg).is_empty());
    let h = &hits(&steady_then_burst(120, 20_000, 40), cfg)[0];
    assert!(h.z_milli >= 8_000 && h.change_permille >= 5 && h.volume >= 20_000);
}

#[test]
fn a_symbol_is_not_scored_before_its_baseline_has_enough_samples() {
    let cfg = ScannerConfig {
        min_volume: 1_000,
        ..ScannerConfig::default()
    };
    // 30 s of calm is three 10 s samples; six are needed.
    assert!(hits(&steady_then_burst(30, 20_000, 40), cfg).is_empty());
    let quick = ScannerConfig {
        baseline_samples: 2,
        ..cfg
    };
    assert!(!hits(&steady_then_burst(30, 20_000, 40), quick).is_empty());
}

#[test]
fn a_run_does_not_teach_the_baseline_that_runs_are_normal() {
    let cfg = ScannerConfig {
        min_volume: 1_000,
        ..ScannerConfig::default()
    };
    // A burst, calm for two minutes, a second identical burst: both must hit, with similar scores.
    let mut ev = steady_then_burst(120, 20_000, 40);
    for s in 0..120 {
        ev.push(quote(0, T0 + (130 + s) * SEC, 539, 541));
        ev.push(trade(0, T0 + (130 + s) * SEC + SEC / 2, 540, 1_000));
    }
    for s in 0..10 {
        let t = T0 + (250 + s) * SEC;
        ev.push(quote(0, t, 579, 581));
        ev.push(trade(0, t + SEC / 2, 580, 20_000));
    }
    let h = hits(&ev, cfg);
    let first_burst: Vec<_> = h.iter().filter(|h| h.ts < T0 + 200 * SEC).collect();
    let second_burst: Vec<_> = h.iter().filter(|h| h.ts >= T0 + 240 * SEC).collect();
    assert!(
        !first_burst.is_empty() && !second_burst.is_empty(),
        "{} + {}",
        first_burst.len(),
        second_burst.len()
    );
    let (a, b) = (first_burst[0].z_milli, second_burst[0].z_milli);
    assert!(
        b * 2 > a,
        "the second burst scores {b}, the first {a}: the baseline was not inflated"
    );
}

#[test]
fn a_symbol_hits_at_most_once_a_second() {
    let cfg = ScannerConfig {
        min_volume: 1_000,
        ..ScannerConfig::default()
    };
    let mut ev = Vec::new();
    for s in 0..120 {
        ev.push(quote(0, T0 + s * SEC, 499, 501));
        ev.push(trade(0, T0 + s * SEC + SEC / 2, 500, 1_000));
    }
    // Many trades in each second of the burst.
    for s in 0..10u64 {
        for k in 0..20u64 {
            let t = T0 + (120 + s) * SEC + k * (SEC / 25);
            ev.push(quote(0, t, 539, 541));
            ev.push(trade(0, t + 1_000, 540, 1_500));
        }
    }
    let h = hits(&ev, cfg);
    assert!(h.len() >= 3);
    let secs: Vec<u64> = h.iter().map(|h| h.ts / SEC).collect();
    let mut dedup = secs.clone();
    dedup.dedup();
    assert_eq!(secs, dedup, "no second appears twice");
}

#[test]
fn the_universe_filters_price_spread_and_float() {
    let base = ScannerConfig {
        min_volume: 1_000,
        ..ScannerConfig::default()
    };
    let ev = steady_then_burst(120, 20_000, 40);
    assert!(!hits(&ev, base).is_empty());
    // Price: the burst trades at $5.40.
    assert!(
        hits(
            &ev,
            ScannerConfig {
                max_price: Px::from_cents(530),
                ..base
            }
        )
        .is_empty()
    );
    assert!(
        hits(
            &ev,
            ScannerConfig {
                min_price: Px::from_cents(600),
                max_price: Px::from_cents(900),
                ..base
            }
        )
        .is_empty()
    );
    // Spread: 2 cents on $5.41 is about 3.7 permille.
    assert!(
        !hits(
            &ev,
            ScannerConfig {
                max_spread_permille: 4,
                ..base
            }
        )
        .is_empty()
    );
    assert!(
        hits(
            &ev,
            ScannerConfig {
                max_spread_permille: 3,
                ..base
            }
        )
        .is_empty()
    );
    // No quote at all: no spread to judge, no hit.
    let no_quotes: Vec<Event> = ev
        .iter()
        .copied()
        .filter(|e| matches!(e, Event::Trade(_)))
        .collect();
    assert!(hits(&no_quotes, base).is_empty());
}

#[test]
fn float_is_a_filter_when_it_is_known_and_unknown_floats_follow_require_float() {
    let cfg = ScannerConfig {
        min_volume: 1_000,
        max_float: Some(10_000_000),
        ..ScannerConfig::default()
    };
    let ev = steady_then_burst(120, 20_000, 40);
    let run = |cfg: ScannerConfig, float: Option<u64>| {
        let mut t0 = Tier0::new(1);
        let mut sc = Scanner::new(cfg, 1).unwrap();
        if let Some(f) = float {
            sc.set_float(0, f);
        }
        let mut out = Vec::new();
        for e in &ev {
            t0.on_event(e);
            sc.on_event(&t0, e, &mut out);
        }
        out.len()
    };
    assert!(
        run(cfg, Some(10_000_000)) > 0,
        "a float at the limit passes"
    );
    assert_eq!(run(cfg, Some(10_000_001)), 0, "a float over it fails");
    assert!(run(cfg, None) > 0, "unknown passes by default");
    assert_eq!(
        run(
            ScannerConfig {
                require_float: true,
                ..cfg
            },
            None
        ),
        0,
        "and fails when required"
    );
    assert!(
        run(
            ScannerConfig {
                max_float: None,
                ..cfg
            },
            Some(u64::MAX)
        ) > 0,
        "no limit, no filter"
    );
}

#[test]
fn bad_configurations_are_refused() {
    let ok = ScannerConfig::default();
    assert_eq!(ok.validate(), Ok(()));
    for (bad, what) in [
        (
            ScannerConfig {
                spike_secs: 0,
                ..ok
            },
            "spike_secs",
        ),
        (
            ScannerConfig {
                spike_secs: 61,
                ..ok
            },
            "spike_secs",
        ),
        (
            ScannerConfig {
                min_z_milli: 0,
                ..ok
            },
            "min_z",
        ),
        (
            ScannerConfig {
                min_change_permille: 0,
                ..ok
            },
            "min_change",
        ),
        (
            ScannerConfig {
                baseline_samples: 0,
                ..ok
            },
            "baseline",
        ),
        (
            ScannerConfig {
                baseline_alpha_permille: 0,
                ..ok
            },
            "alpha",
        ),
        (
            ScannerConfig {
                min_price: Px::ZERO,
                ..ok
            },
            "price",
        ),
        (
            ScannerConfig {
                max_price: Px::from_cents(1),
                ..ok
            },
            "price order",
        ),
    ] {
        assert!(
            matches!(Scanner::new(bad, 1), Err(ScannerError(_))),
            "{what}"
        );
    }
}

#[test]
fn the_same_stream_gives_the_same_hits() {
    let cfg = SynthConfig::universe(3, 100, 600 * SEC, 80);
    let ev: Vec<Event> = SynthStream::new(&cfg).collect();
    let a = scan(&ev, 100, ScannerConfig::default());
    assert!(!a.is_empty());
    assert_eq!(a, scan(&ev, 100, ScannerConfig::default()));
}
