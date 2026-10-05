use tf_core::{Event, Fnv1a64, NANOS_PER_SEC, Nanos};
use tf_provider::{Channels, Poll, Provider, ProviderError, Subscription};

use crate::{
    Account, DEFAULT_SESSION_START, Faults, PullbackKind, Scenario, SymbolSpec, SynthConfig,
    SynthOptions, SynthProvider, SynthStream,
};

fn hash_events(evs: impl IntoIterator<Item = Event>) -> (u64, u64) {
    let mut h = Fnv1a64::new();
    let mut n = 0;
    let mut buf = Vec::new();
    for ev in evs {
        buf.clear();
        ev.encode(&mut buf);
        h.write(&buf);
        n += 1;
    }
    (n, h.finish())
}

fn drain(p: &mut SynthProvider) -> Vec<Event> {
    let mut out = Vec::new();
    loop {
        match p.poll(&mut out, 4096) {
            Poll::End => return out,
            Poll::Events(_) => {}
            other => panic!("unexpected poll result {other:?}"),
        }
    }
}

fn single(scenario: Scenario, base_px_cents: i64, secs: u64) -> SynthConfig {
    SynthConfig {
        seed: 1,
        session_start: DEFAULT_SESSION_START,
        duration: secs * NANOS_PER_SEC,
        symbols: vec![SymbolSpec {
            symbol: "RUN".into(),
            base_px_cents,
            base_interval_ns: NANOS_PER_SEC,
            quote_every: 2,
            scenario,
        }],
    }
}

/// Golden values: any change to the generator, scenarios or encoding changes
/// these, and must be a deliberate decision (regenerate and say why in the PR).
#[test]
fn golden_stream_hash() {
    let cfg = SynthConfig::universe(42, 20, 120 * NANOS_PER_SEC, 500);
    let (n, h) = hash_events(SynthStream::new(&cfg));
    assert_eq!(
        (n, h),
        (GOLDEN_COUNT, GOLDEN_HASH),
        "got count={n} hash={h:#018x}"
    );
}
const GOLDEN_COUNT: u64 = 3166;
const GOLDEN_HASH: u64 = 0xa19b_6423_e169_b302;

#[test]
fn same_seed_same_stream_different_seed_differs() {
    let a = hash_events(SynthStream::new(&SynthConfig::universe(
        7,
        10,
        60 * NANOS_PER_SEC,
        300,
    )));
    let b = hash_events(SynthStream::new(&SynthConfig::universe(
        7,
        10,
        60 * NANOS_PER_SEC,
        300,
    )));
    let c = hash_events(SynthStream::new(&SynthConfig::universe(
        8,
        10,
        60 * NANOS_PER_SEC,
        300,
    )));
    assert_eq!(a, b);
    assert_ne!(a.1, c.1);
}

#[test]
fn stream_is_arrival_ordered_with_dense_seq() {
    let cfg = SynthConfig::universe(3, 50, 120 * NANOS_PER_SEC, 400);
    let end = cfg.session_start + cfg.duration;
    let evs: Vec<Event> = SynthStream::new(&cfg).collect();
    assert!(evs.len() > 1_000);
    let mut last_event_ts = vec![0u64; 50];
    for (i, w) in evs.windows(2).enumerate() {
        assert!(
            w[0].ts_recv() <= w[1].ts_recv(),
            "ts_recv went backwards at {i}"
        );
    }
    for (i, ev) in evs.iter().enumerate() {
        assert_eq!(ev.seq(), i as u64);
        assert!(ev.ts_event() < end);
        assert!(ev.ts_recv() > ev.ts_event());
        let slot = &mut last_event_ts[ev.instrument() as usize];
        assert!(
            ev.ts_event() >= *slot,
            "ts_event regressed within one symbol"
        );
        *slot = ev.ts_event();
    }
}

/// (peak price in the impulse, trough price in the pullback window, base) in cents,
/// plus the best price seen after the pullback.
fn runner_shape(kind: PullbackKind) -> (i64, i64, i64, i64) {
    let lead_in: Nanos = 20 * NANOS_PER_SEC;
    let base = 500;
    let cfg = single(Scenario::runner(kind, lead_in), base, 300);
    let t0 = cfg.session_start;
    let s = NANOS_PER_SEC;
    let (mut peak, mut trough, mut after_peak) = (0, i64::MAX, 0);
    for ev in SynthStream::new(&cfg) {
        if let Event::Trade(t) = ev {
            let dt = t.hdr.ts_event - t0;
            let c = t.px.to_cents();
            if (lead_in..lead_in + 30 * s).contains(&dt) {
                peak = peak.max(c);
            } else if (lead_in + 30 * s..lead_in + 70 * s).contains(&dt) {
                trough = trough.min(c);
            } else if dt >= lead_in + 70 * s {
                after_peak = after_peak.max(c);
            }
        }
    }
    (peak, trough, base, after_peak)
}

#[test]
fn healthy_runner_pulls_back_shallowly_then_makes_new_highs() {
    let (peak, trough, base, after) = runner_shape(PullbackKind::Healthy);
    let depth = (peak - trough) as f64 / (peak - base) as f64;
    assert!(peak as f64 > 1.3 * base as f64, "no impulse: peak {peak}");
    assert!(
        (0.05..0.5).contains(&depth),
        "pullback depth {depth:.2} (peak {peak}, trough {trough})"
    );
    assert!(
        after > peak,
        "no continuation: after {after} <= peak {peak}"
    );
}

#[test]
fn dangerous_runner_retraces_deeply_and_never_recovers() {
    let (peak, trough, base, after) = runner_shape(PullbackKind::Dangerous);
    let depth = (peak - trough) as f64 / (peak - base) as f64;
    assert!(peak as f64 > 1.3 * base as f64, "no impulse: peak {peak}");
    assert!(
        depth > 0.6,
        "pullback depth {depth:.2} (peak {peak}, trough {trough})"
    );
    assert!(after < peak, "dangerous scenario recovered to new highs");
}

#[test]
fn runner_volume_spikes_during_impulse() {
    let lead_in = 20 * NANOS_PER_SEC;
    let cfg = single(Scenario::runner(PullbackKind::Healthy, lead_in), 500, 120);
    let t0 = cfg.session_start;
    let (mut quiet, mut hot) = (0u64, 0u64);
    for ev in SynthStream::new(&cfg) {
        if let Event::Trade(t) = ev {
            let dt = t.hdr.ts_event - t0;
            if dt < lead_in {
                quiet += u64::from(t.size);
            } else if dt < lead_in + 30 * NANOS_PER_SEC {
                hot += u64::from(t.size);
            }
        }
    }
    // Per second of wall time: impulse volume must dwarf the lead-in's.
    let quiet_per_s = quiet as f64 / 20.0;
    let hot_per_s = hot as f64 / 30.0;
    assert!(
        hot_per_s > 20.0 * quiet_per_s,
        "quiet {quiet_per_s:.0}/s vs impulse {hot_per_s:.0}/s"
    );
}

#[test]
fn account_enforces_connection_limit_across_providers() {
    let cfg = SynthConfig::universe(1, 3, 10 * NANOS_PER_SEC, 0);
    let account = Account::new(1);
    let mut a = SynthProvider::new(cfg.clone(), SynthOptions::default(), account.clone());
    let mut b = SynthProvider::new(cfg, SynthOptions::default(), account.clone());
    a.connect().unwrap();
    assert_eq!(
        b.connect(),
        Err(ProviderError::ConnectionLimitExceeded { limit: 1 })
    );
    a.disconnect();
    assert_eq!(account.active(), 0);
    b.connect().unwrap();
}

#[test]
fn symbol_limit_is_enforced_like_alpaca_basic() {
    let cfg = SynthConfig::universe(1, 100, 10 * NANOS_PER_SEC, 0);
    let opts = SynthOptions {
        max_symbols: Some(30),
        ..Default::default()
    };
    let mut p = SynthProvider::new(cfg, opts, Account::new(1));
    assert_eq!(
        p.subscribe(&Subscription::all(Channels::ALL)),
        Err(ProviderError::NotConnected)
    );
    p.connect().unwrap();
    let too_many = Subscription::list(Channels::ALL, (0..31).collect());
    assert_eq!(
        p.subscribe(&too_many),
        Err(ProviderError::SymbolLimitExceeded {
            limit: 30,
            requested: 31
        })
    );
    p.subscribe(&Subscription::list(Channels::ALL, (0..30).collect()))
        .unwrap();
}

#[test]
fn subscription_filters_symbols_and_channels() {
    let cfg = SynthConfig::universe(5, 10, 60 * NANOS_PER_SEC, 0);
    let mut p = SynthProvider::new(cfg, SynthOptions::default(), Account::new(1));
    p.connect().unwrap();
    p.subscribe(&Subscription::list(Channels::TRADES, vec![2, 4]))
        .unwrap();
    let evs = drain(&mut p);
    assert!(!evs.is_empty());
    assert!(
        evs.iter()
            .all(|e| matches!(e, Event::Trade(_)) && [2, 4].contains(&e.instrument()))
    );
}

fn connected(cfg: &SynthConfig, opts: SynthOptions) -> SynthProvider {
    let mut p = SynthProvider::new(cfg.clone(), opts, Account::new(1));
    p.connect().unwrap();
    p.subscribe(&Subscription::all(Channels::ALL)).unwrap();
    p
}

#[test]
fn duplicates_and_reordering_are_injected_but_lose_nothing() {
    let cfg = SynthConfig::universe(9, 20, 60 * NANOS_PER_SEC, 200);
    let clean = drain(&mut connected(&cfg, SynthOptions::default()));
    let faults = Faults {
        dup_permille: 50,
        reorder_permille: 50,
        ..Default::default()
    };
    let noisy = drain(&mut connected(
        &cfg,
        SynthOptions {
            faults,
            ..Default::default()
        },
    ));

    assert!(noisy.len() > clean.len(), "no duplicates injected");
    assert!(
        noisy.windows(2).any(|w| w[0].seq() > w[1].seq()),
        "no reordering injected"
    );

    let mut seqs: Vec<u64> = noisy.iter().map(Event::seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), clean.len(), "events were lost");
}

#[test]
fn disconnect_without_replay_loses_the_outage() {
    let cfg = SynthConfig::universe(11, 20, 60 * NANOS_PER_SEC, 200);
    let clean = drain(&mut connected(&cfg, SynthOptions::default()));
    let faults = Faults {
        disconnect_after_events: Some(500),
        outage_events: 100,
        ..Default::default()
    };
    let mut p = connected(
        &cfg,
        SynthOptions {
            faults,
            ..Default::default()
        },
    );

    let mut got = Vec::new();
    assert_eq!(p.poll(&mut got, 10_000), Poll::Events(500));
    assert_eq!(p.poll(&mut got, 10_000), Poll::Disconnected);
    p.reconnect(Some(got.last().unwrap().ts_recv())).unwrap();
    got.extend(drain(&mut p));

    assert_eq!(got.len(), clean.len() - 100);
    assert_eq!(got[500].seq(), 600, "outage should skip exactly 100 events");
}

#[test]
fn disconnect_with_replay_recovers_everything_with_one_boundary_duplicate() {
    let cfg = SynthConfig::universe(11, 20, 60 * NANOS_PER_SEC, 200);
    let clean = drain(&mut connected(&cfg, SynthOptions::default()));
    let faults = Faults {
        disconnect_after_events: Some(500),
        outage_events: 100,
        ..Default::default()
    };
    let mut p = connected(
        &cfg,
        SynthOptions {
            faults,
            replay: true,
            ..Default::default()
        },
    );

    let mut got = Vec::new();
    assert_eq!(p.poll(&mut got, 10_000), Poll::Events(500));
    assert_eq!(p.poll(&mut got, 10_000), Poll::Disconnected);
    p.reconnect(Some(got.last().unwrap().ts_recv())).unwrap();
    got.extend(drain(&mut p));

    // Replay is inclusive of the boundary: the last pre-drop event comes again.
    assert_eq!(got.len(), clean.len() + 1);
    assert_eq!(got[499], got[500]);
    got.remove(500);
    assert_eq!(got, clean);
}
