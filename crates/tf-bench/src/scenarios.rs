//! The benchmark scenarios and the measurement loop.
//!
//! Each scenario is measured in two separate passes, because timing every event
//! costs about as much as processing one:
//!
//! 1. an **untimed** pass, wall-clocked as a whole, gives events per second;
//! 2. a **timed** pass records the time inside the sink for every event into a
//!    histogram, giving p50, p99, p99.9 and max.
//!
//! The cost of the timer itself is measured separately (an empty sink) and
//! reported, so latencies can be read net of it.

use std::hint::black_box;
use std::io::Cursor;
use std::time::Instant;

use tf_core::{Event, Fnv1a64, NANOS_PER_SEC, ProviderId, SimClock};
use tf_engine::Tier0;
use tf_provider::{
    Capabilities, Channels, Poll, Provider, ProviderError, Subscription, WireFormat,
};
use tf_replay::{EventSink, RunConfig, RunReport, run};
use tf_synth::{Account, SynthConfig, SynthOptions, SynthProvider, SynthStream};
use tf_tape::replay::{Speed, TapeProvider, WallPacer};
use tf_tape::{TapeReader, TapeWriter};

use crate::hist::Histogram;
use crate::report::Row;

/// What to run: the synthetic universe that every scenario consumes.
#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub symbols: usize,
    pub secs: u64,
    pub seed: u64,
}

/// Where and on what the numbers were taken.
#[derive(Clone, Debug)]
pub struct Env {
    pub commit: String,
    pub arch: String,
    pub os: String,
    pub profile: String,
    pub cpus: u64,
}

impl Env {
    pub fn detect(commit: String) -> Env {
        Env {
            commit,
            arch: std::env::consts::ARCH.to_owned(),
            os: std::env::consts::OS.to_owned(),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
        }
    }
}

/// Serves a slice of events through the `Provider` trait, so the sink and run
/// loop are measured without the cost of generating or decoding them.
struct SliceProvider<'a> {
    events: &'a [Event],
    pos: usize,
    caps: Capabilities,
}

impl<'a> SliceProvider<'a> {
    fn new(events: &'a [Event]) -> Self {
        SliceProvider {
            events,
            pos: 0,
            caps: Capabilities {
                provider: ProviderId::Synthetic,
                max_connections: u32::MAX,
                max_symbols_per_session: None,
                wildcard: true,
                replay_window_secs: None,
                wire: WireFormat::Binary,
            },
        }
    }
}

impl Provider for SliceProvider<'_> {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }
    fn connect(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }
    fn subscribe(&mut self, _: &Subscription) -> Result<(), ProviderError> {
        Ok(())
    }
    fn reconnect(&mut self, _: Option<u64>) -> Result<(), ProviderError> {
        Ok(())
    }
    fn disconnect(&mut self) {}
    fn poll(&mut self, out: &mut Vec<Event>, max: usize) -> Poll {
        if self.pos >= self.events.len() {
            return Poll::End;
        }
        let end = (self.pos + max).min(self.events.len());
        out.extend_from_slice(&self.events[self.pos..end]);
        let n = end - self.pos;
        self.pos = end;
        Poll::Events(n)
    }
}

/// Does nothing, so the run loop's own cost shows.
struct NullSink;

impl EventSink for NullSink {
    fn on_event(&mut self, ev: &Event) {
        black_box(ev);
    }
}

/// The engine's Tier 0 state as a sink: the realistic per-event work so far.
struct Tier0Sink(Tier0);

impl EventSink for Tier0Sink {
    fn on_event(&mut self, ev: &Event) {
        self.0.on_event(ev);
    }
}

fn digest(t: &Tier0) -> u64 {
    let mut h = Fnv1a64::new();
    for id in 0..t.len() {
        let s = t.symbol(id as u32).expect("in range");
        h.write(&s.volume.to_le_bytes());
        h.write(&s.trades.to_le_bytes());
        h.write(&s.last_px.map_or(0, |p| p.raw()).to_le_bytes());
        h.write(&(s.notional as u64).to_le_bytes());
    }
    h.finish()
}

/// Times each `on_event` call into a histogram.
struct Timed<S> {
    inner: S,
    hist: Histogram,
}

impl<S: EventSink> EventSink for Timed<S> {
    #[inline]
    fn on_event(&mut self, ev: &Event) {
        let start = Instant::now();
        self.inner.on_event(ev);
        self.hist.record(start.elapsed().as_nanos() as u64);
    }
}

fn drive<P: Provider, S: EventSink>(p: &mut P, s: &mut S) -> Result<RunReport, String> {
    let sub = Subscription::all(Channels::ALL);
    run(p, &sub, &SimClock::new(0), s, &RunConfig::default())
        .map_err(|e| format!("run failed: {e}"))
}

struct Measured {
    events: u64,
    events_per_s: u64,
    hist: Histogram,
    digest: u64,
}

fn measure<P, S>(
    runs: usize,
    mut make: impl FnMut() -> Result<(P, S), String>,
    digest_of: impl Fn(&S) -> u64,
) -> Result<Measured, String>
where
    P: Provider,
    S: EventSink,
{
    let (mut rates, mut hist) = (Vec::new(), Histogram::new());
    let (mut events, mut dig) = (0, 0);
    for r in 0..runs.max(1) {
        let (mut p, mut s) = make()?;
        let start = Instant::now();
        let report = drive(&mut p, &mut s)?;
        let ns = start.elapsed().as_nanos().max(1);
        rates.push((u128::from(report.events) * 1_000_000_000 / ns) as u64);
        let d = digest_of(&s);
        if r == 0 {
            (events, dig) = (report.events, d);
        } else if (report.events, d) != (events, dig) {
            return Err("runs of one scenario disagree: the benchmark is not deterministic".into());
        }

        let (mut p, s) = make()?;
        let mut timed = Timed {
            inner: s,
            hist: Histogram::new(),
        };
        drive(&mut p, &mut timed)?;
        hist.merge(&timed.hist);
    }
    rates.sort_unstable();
    Ok(Measured {
        events,
        events_per_s: rates[rates.len() / 2],
        hist,
        digest: dig,
    })
}

/// Run every scenario `runs` times and return one row per scenario.
///
/// Errors if a scenario fails, or if scenarios that should have processed the
/// same events did not (a benchmark of the wrong thing is worse than none).
pub fn run_all(w: &Workload, runs: usize, env: &Env) -> Result<Vec<Row>, String> {
    let cfg = SynthConfig::universe(w.seed, w.symbols, w.secs * NANOS_PER_SEC, 20);
    let events: Vec<Event> = SynthStream::new(&cfg).collect();
    if events.is_empty() {
        return Err("the workload produced no events".into());
    }
    let mut writer = TapeWriter::new(Vec::new()).map_err(|e| e.to_string())?;
    for ev in &events {
        writer.write(ev).map_err(|e| e.to_string())?;
    }
    let tape = writer.finish().map_err(|e| e.to_string())?;

    let timer = measure(runs, || Ok((SliceProvider::new(&events), NullSink)), |_| 0)?;
    // The timer's own cost: a timed pass over an empty sink.
    let floor = {
        let mut p = SliceProvider::new(&events);
        let mut t = Timed {
            inner: NullSink,
            hist: Histogram::new(),
        };
        drive(&mut p, &mut t)?;
        t.hist.percentile(500)
    };

    let tier0 = || Tier0Sink(Tier0::new(w.symbols));
    let loop_t0 = measure(
        runs,
        || Ok((SliceProvider::new(&events), tier0())),
        |s| digest(&s.0),
    )?;
    let tape_t0 = measure(
        runs,
        || {
            let reader = TapeReader::open(Cursor::new(&tape[..])).map_err(|e| e.to_string())?;
            let p = TapeProvider::new(
                reader,
                ProviderId::Synthetic,
                Speed::Max,
                Box::new(WallPacer),
            );
            Ok((p, tier0()))
        },
        |s| digest(&s.0),
    )?;
    let synth_t0 = measure(
        runs,
        || {
            Ok((
                SynthProvider::new(cfg.clone(), SynthOptions::default(), Account::new(1)),
                tier0(),
            ))
        },
        |s| digest(&s.0),
    )?;

    let all = [&timer, &loop_t0, &tape_t0, &synth_t0];
    if all.iter().any(|m| m.events != timer.events) {
        return Err("scenarios processed different numbers of events".into());
    }
    if [&tape_t0, &synth_t0]
        .iter()
        .any(|m| m.digest != loop_t0.digest)
    {
        return Err("scenarios reached different Tier 0 state from the same events".into());
    }

    let row = |name: &str, m: &Measured| Row {
        commit: env.commit.clone(),
        scenario: name.to_owned(),
        arch: env.arch.clone(),
        os: env.os.clone(),
        profile: env.profile.clone(),
        cpus: env.cpus,
        symbols: w.symbols as u64,
        secs: w.secs,
        seed: w.seed,
        events: m.events,
        events_per_s: m.events_per_s,
        p50_ns: m.hist.percentile(500),
        p99_ns: m.hist.percentile(990),
        p999_ns: m.hist.percentile(999),
        max_ns: m.hist.max(),
        timer_p50_ns: floor,
    };
    Ok(vec![
        row("run-loop/null", &timer),
        row("run-loop/tier0", &loop_t0),
        row("tape/tier0", &tape_t0),
        row("synth/tier0", &synth_t0),
    ])
}
