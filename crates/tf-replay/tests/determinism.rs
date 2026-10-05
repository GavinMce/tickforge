use tf_core::{NANOS_PER_SEC, SimClock};
use tf_provider::{Channels, Subscription};
use tf_replay::{DedupeSink, HashSink, RunConfig, RunError, StatsSink, Tee, VecSink, run};
use tf_synth::{Account, Faults, SynthConfig, SynthOptions, SynthProvider};

fn cfg() -> SynthConfig {
    SynthConfig::universe(2026, 40, 90 * NANOS_PER_SEC, 300)
}

fn clean_run() -> (tf_replay::RunReport, u64, Vec<tf_core::Event>) {
    let c = cfg();
    let mut p = SynthProvider::new(c.clone(), SynthOptions::default(), Account::new(1));
    let clock = SimClock::new(c.session_start);
    let mut sink = Tee(HashSink::new(), VecSink::default());
    let report = run(
        &mut p,
        &Subscription::all(Channels::ALL),
        &clock,
        &mut sink,
        &RunConfig::default(),
    )
    .unwrap();
    (report, sink.0.finish(), sink.1.0)
}

#[test]
fn identical_runs_produce_identical_hashes() {
    let (r1, h1, _) = clean_run();
    let (r2, h2, _) = clean_run();
    assert_eq!(r1, r2);
    assert_eq!(h1, h2);
    assert!(r1.events > 1_000, "only {} events", r1.events);
}

#[test]
fn sim_clock_ends_at_last_delivery() {
    let c = cfg();
    let mut p = SynthProvider::new(c.clone(), SynthOptions::default(), Account::new(1));
    let clock = SimClock::new(c.session_start);
    let mut stats = StatsSink::default();
    let report = run(
        &mut p,
        &Subscription::all(Channels::ALL),
        &clock,
        &mut stats,
        &RunConfig::default(),
    )
    .unwrap();
    use tf_core::Clock;
    assert_eq!(clock.now(), report.last_recv.unwrap());
    assert_eq!(stats.trades + stats.quotes + stats.status, report.events);
    assert!(stats.trades > 0 && stats.quotes > 0);
}

#[test]
fn run_survives_a_drop_with_replay_and_dedupe_and_matches_the_clean_run() {
    let (_, clean_hash, clean_events) = clean_run();

    let c = cfg();
    let faults = Faults {
        disconnect_after_events: Some(3_000),
        outage_events: 250,
        ..Default::default()
    };
    let opts = SynthOptions {
        replay: true,
        faults,
        ..Default::default()
    };
    let mut p = SynthProvider::new(c.clone(), opts, Account::new(1));
    let clock = SimClock::new(c.session_start);
    let mut sink = DedupeSink::new(Tee(HashSink::new(), VecSink::default()));
    let report = run(
        &mut p,
        &Subscription::all(Channels::ALL),
        &clock,
        &mut sink,
        &RunConfig::default(),
    )
    .unwrap();

    assert_eq!(report.reconnects, 1);
    assert_eq!(sink.dropped, 1, "exactly the boundary event is replayed");
    let Tee(hash, evs) = sink.into_inner();
    assert_eq!(evs.0, clean_events);
    assert_eq!(hash.finish(), clean_hash);
}

#[test]
fn run_without_replay_loses_exactly_the_outage() {
    let (clean_report, _, _) = clean_run();

    let c = cfg();
    let faults = Faults {
        disconnect_after_events: Some(3_000),
        outage_events: 250,
        ..Default::default()
    };
    let opts = SynthOptions {
        faults,
        ..Default::default()
    };
    let mut p = SynthProvider::new(c.clone(), opts, Account::new(1));
    let clock = SimClock::new(c.session_start);
    let mut stats = StatsSink::default();
    let report = run(
        &mut p,
        &Subscription::all(Channels::ALL),
        &clock,
        &mut stats,
        &RunConfig::default(),
    )
    .unwrap();

    assert_eq!(report.events, clean_report.events - 250);
}

#[test]
fn run_reports_connection_limit_instead_of_hanging() {
    let c = cfg();
    let account = Account::new(1);
    let mut holder = SynthProvider::new(c.clone(), SynthOptions::default(), account.clone());
    tf_provider::Provider::connect(&mut holder).unwrap();

    let mut p = SynthProvider::new(c.clone(), SynthOptions::default(), account);
    let clock = SimClock::new(c.session_start);
    let mut sink = HashSink::new();
    let err = run(
        &mut p,
        &Subscription::all(Channels::ALL),
        &clock,
        &mut sink,
        &RunConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(err, RunError::Provider(_)), "{err}");
}
