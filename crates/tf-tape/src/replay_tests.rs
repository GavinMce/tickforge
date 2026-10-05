use std::io::Cursor;
use std::sync::Arc;

use tf_core::{Event, NANOS_PER_SEC, Nanos, ProviderId, SimClock};
use tf_provider::{Channels, Poll, Provider, ProviderError, Subscription};
use tf_replay::{EventSink, HashSink, RunConfig, RunError, VecSink, run};
use tf_synth::{SynthConfig, SynthStream};

use super::replay::{Pacer, Speed, TapeProvider, WallPacer};
use super::{TapeReader, TapeWriter};

const W0: Nanos = 5_000_000_000;

fn session() -> Vec<Event> {
    SynthStream::new(&SynthConfig::universe(9, 20, 120 * NANOS_PER_SEC, 400)).collect()
}

fn tape(events: &[Event], block_events: u32) -> Vec<u8> {
    let mut w = TapeWriter::with_block_events(Vec::new(), block_events).unwrap();
    for e in events {
        w.write(e).unwrap();
    }
    w.finish().unwrap()
}

type Provided = TapeProvider<Cursor<Vec<u8>>>;

fn provider(bytes: Vec<u8>, speed: Speed, clock: &Arc<SimClock>) -> Provided {
    let reader = TapeReader::open(Cursor::new(bytes)).unwrap();
    TapeProvider::new(
        reader,
        ProviderId::Synthetic,
        speed,
        Box::new(clock.clone()),
    )
}

fn replay(p: &mut Provided, sub: &Subscription) -> Vec<Event> {
    let mut sink = VecSink::default();
    run(p, sub, &SimClock::new(0), &mut sink, &RunConfig::default()).unwrap();
    sink.0
}

/// Records the pacer's time at the moment each event is delivered.
struct Rec {
    clock: Arc<SimClock>,
    seen: Vec<(Event, Nanos)>,
}

impl EventSink for Rec {
    fn on_event(&mut self, ev: &Event) {
        self.seen.push((*ev, Pacer::now(&self.clock)));
    }
}

fn all() -> Subscription {
    Subscription::all(Channels::ALL)
}

#[test]
fn max_speed_hands_over_the_whole_tape_without_touching_the_clock() {
    let evs = session();
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(tape(&evs, 100), Speed::Max, &clock);
    assert_eq!(replay(&mut p, &all()), evs);
    assert_eq!(Pacer::now(&clock), W0, "no pacing, no waiting");
}

#[test]
fn paced_replay_releases_each_event_exactly_when_due() {
    let evs = session();
    let t0 = evs[0].ts_recv();
    for permille in [500u32, 1000, 10_000, 1_000_000] {
        let clock = Arc::new(SimClock::new(W0));
        let mut p = provider(tape(&evs, 100), Speed::Paced { permille }, &clock);
        let mut rec = Rec {
            clock: clock.clone(),
            seen: Vec::new(),
        };
        let report = run(
            &mut p,
            &all(),
            &SimClock::new(0),
            &mut rec,
            &RunConfig::default(),
        )
        .unwrap();
        assert_eq!(report.events, evs.len() as u64);
        for (ev, at) in &rec.seen {
            let due = W0 + (u128::from(ev.ts_recv() - t0) * 1000 / u128::from(permille)) as Nanos;
            assert_eq!(*at, due, "at {permille} permille, event {}", ev.seq());
        }
        let last = evs.last().unwrap().ts_recv();
        assert_eq!(
            Pacer::now(&clock),
            W0 + (u128::from(last - t0) * 1000 / u128::from(permille)) as Nanos
        );
    }
}

#[test]
fn the_event_stream_is_the_same_at_every_speed() {
    let evs = session();
    let mut hashes = Vec::new();
    for speed in [
        Speed::Max,
        Speed::REALTIME,
        Speed::Paced { permille: 10_000 },
    ] {
        let clock = Arc::new(SimClock::new(W0));
        let mut p = provider(tape(&evs, 64), speed, &clock);
        let mut sink = HashSink::new();
        run(
            &mut p,
            &all(),
            &SimClock::new(0),
            &mut sink,
            &RunConfig::default(),
        )
        .unwrap();
        hashes.push((sink.events, sink.finish()));
    }
    assert!(hashes.windows(2).all(|w| w[0] == w[1]), "{hashes:?}");

    let mut direct = HashSink::new();
    evs.iter().for_each(|e| direct.on_event(e));
    assert_eq!(
        hashes[0],
        (direct.events, direct.finish()),
        "tape replay differs from the source stream"
    );
}

#[test]
fn a_subscription_filters_and_pacing_starts_at_the_first_wanted_event() {
    let evs = session();
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(tape(&evs, 100), Speed::REALTIME, &clock);
    let sub = Subscription::list(Channels::TRADES, vec![0, 3]);
    let want: Vec<Event> = evs.iter().filter(|e| sub.matches(e)).copied().collect();
    assert!(want.len() > 50 && want.len() < evs.len());

    let mut rec = Rec {
        clock: clock.clone(),
        seen: Vec::new(),
    };
    run(
        &mut p,
        &sub,
        &SimClock::new(0),
        &mut rec,
        &RunConfig::default(),
    )
    .unwrap();
    assert_eq!(rec.seen.iter().map(|(e, _)| *e).collect::<Vec<_>>(), want);
    let t0 = want[0].ts_recv();
    assert_eq!(
        rec.seen[0].1, W0,
        "the clock anchors on the first event delivered"
    );
    for (ev, at) in &rec.seen {
        assert_eq!(*at, W0 + (ev.ts_recv() - t0));
    }
}

#[test]
fn starting_at_skips_to_a_time_and_anchors_there() {
    let evs = session();
    let start = evs[evs.len() / 2].ts_recv();
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(tape(&evs, 100), Speed::REALTIME, &clock).starting_at(start);
    let want: Vec<Event> = evs
        .iter()
        .filter(|e| e.ts_recv() >= start)
        .copied()
        .collect();
    let mut rec = Rec {
        clock: clock.clone(),
        seen: Vec::new(),
    };
    run(
        &mut p,
        &all(),
        &SimClock::new(0),
        &mut rec,
        &RunConfig::default(),
    )
    .unwrap();
    assert_eq!(rec.seen.iter().map(|(e, _)| *e).collect::<Vec<_>>(), want);
    assert_eq!(rec.seen[0].1, W0);
}

#[test]
fn reconnect_replays_from_the_given_time_including_the_boundary() {
    let evs = session();
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(tape(&evs, 100), Speed::Max, &clock);
    p.connect().unwrap();
    p.subscribe(&all()).unwrap();
    let mut got = Vec::new();
    assert_eq!(p.poll(&mut got, 250), Poll::Events(250));
    let resume = got.last().unwrap().ts_recv();

    p.reconnect(Some(resume)).unwrap();
    let mut again = Vec::new();
    while let Poll::Events(_) = p.poll(&mut again, 1000) {}
    let want: Vec<Event> = evs
        .iter()
        .filter(|e| e.ts_recv() >= resume)
        .copied()
        .collect();
    assert_eq!(again, want);
    assert_eq!(
        again[0],
        *got.last().unwrap(),
        "the boundary event comes again"
    );

    // And with no resume point it starts over.
    p.reconnect(None).unwrap();
    let mut from_start = Vec::new();
    assert_eq!(p.poll(&mut from_start, 5), Poll::Events(5));
    assert_eq!(from_start, evs[..5]);
}

#[test]
fn the_provider_contract_before_and_after_a_session() {
    let evs = session();
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(tape(&evs, 100), Speed::Max, &clock);
    let mut out = Vec::new();
    assert_eq!(
        p.poll(&mut out, 10),
        Poll::Disconnected,
        "not connected yet"
    );
    assert_eq!(p.subscribe(&all()), Err(ProviderError::NotConnected));
    p.connect().unwrap();
    p.subscribe(&all()).unwrap();
    assert_eq!(p.poll(&mut out, 0), Poll::Idle);
    p.disconnect();
    assert_eq!(p.poll(&mut out, 10), Poll::Disconnected);

    let caps = p.capabilities();
    let span = (evs.last().unwrap().ts_recv() - evs[0].ts_recv()) / NANOS_PER_SEC;
    assert_eq!(caps.provider, ProviderId::Synthetic);
    assert_eq!(caps.replay_window_secs, Some(span));
    assert!(caps.wildcard && caps.max_symbols_per_session.is_none());
}

#[test]
fn an_empty_tape_ends_at_once() {
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(tape(&[], 10), Speed::REALTIME, &clock);
    assert!(replay(&mut p, &all()).is_empty());
}

#[test]
fn a_damaged_tape_fails_the_run_instead_of_ending_early() {
    let evs = session();
    let mut bytes = tape(&evs, 50);
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    let clock = Arc::new(SimClock::new(W0));
    let mut p = provider(bytes, Speed::Max, &clock);
    let mut sink = VecSink::default();
    let result = run(
        &mut p,
        &all(),
        &SimClock::new(0),
        &mut sink,
        &RunConfig::default(),
    );
    match result {
        Err(RunError::Provider(ProviderError::Source(m))) => assert!(m.contains("corrupt"), "{m}"),
        other => panic!("expected a source failure, got {other:?}"),
    }
    assert!(
        sink.0.len() < evs.len(),
        "events before the damage were still delivered"
    );
    assert_eq!(sink.0[..], evs[..sink.0.len()], "and they are correct");
}

#[test]
fn the_wall_pacer_really_waits() {
    let target = Pacer::now(&WallPacer) + 30_000_000;
    WallPacer.wait_until(target);
    assert!(Pacer::now(&WallPacer) >= target);
    WallPacer.wait_until(0); // already past: returns at once
}
