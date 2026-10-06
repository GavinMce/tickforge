//! The live day driver (E18-S12): the feed, the ingest queue, the host, the raw capture and the decision
//! log, wired together and run from the open to the close.
//!
//! One engine thread runs [`run`]: it takes what the feed thread has queued ([`tf_live::LiveProvider`]),
//! drops what the gateway sent twice, and gives each event to the host. Around that loop:
//!
//! - **Warm-up.** The gateway names the day's instruments as the session starts. The driver waits until
//!   it has been quiet (or a limit passes), builds the symbol table from the names, builds the host and
//!   admits the strategies, whose universes are resolved against those names. Events wait in the queue.
//! - **The raw capture** is written by the feed thread from the bytes as they are read, so what is kept
//!   is what the gateway sent, whatever the engine did with it. If it cannot be written the day stops:
//!   the kill switch, a flatten, and an honest report.
//! - **The decision log** is appended to a file as the day goes and closed at the end.
//! - **Reconnects.** A dropped or silent session is reopened from the last event's time with backoff; a
//!   gateway that stays down ends the day (and says what was left open).
//! - **Measurements** the host cannot make (it reads no clock): how long the engine took for each event,
//!   the queue's counters, the capture's size. They go into the daily report.
//! - **The end.** The close time (in event time), an operator's stop, a capture failure or a gateway
//!   that will not come back: the session is closed, what is queued is taken, the day is ended, the
//!   capture is finished and checked, and the report is written. If asked, the capture is replayed
//!   through a fresh host and compared with the day's decisions (E18-S06), and that is in the report.

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tf_capture::RawWriter;
use tf_core::{Dedupe, Nanos};
use tf_host::{
    AdmitError, Certificate, DailyReport, Host, HostConfig, HostError, Log, Reference, ReplayError,
    StrategyDef, SystemInputs, symbol_table,
};
use tf_ingest::Delivery;
use tf_ledger::LedgerStore;
use tf_live::{LiveProvider, RawSink, SharedSink, State};
use tf_provider::Provider;
use tf_universe::Snapshot;

#[derive(Clone, Debug)]
pub struct Warmup {
    /// Warm-up ends when no new instrument name has come for this long.
    pub quiet: Duration,
    /// Or when it has lasted this long.
    pub max: Duration,
}

#[derive(Clone, Debug)]
pub struct Reconnect {
    /// Reconnects in a day before giving up.
    pub max: u32,
    pub first_backoff: Duration,
    pub max_backoff: Duration,
}

pub struct DriverConfig {
    pub live: tf_live::Config,
    pub ingest: tf_ingest::Config,
    pub host: HostConfig,
    pub snapshot: Snapshot,
    /// Where the day's files go: `capture/`, `decisions.log`, `report.txt`.
    pub dir: PathBuf,
    pub segment_secs: u64,
    pub warmup: Warmup,
    pub reconnect: Reconnect,
    /// End the day when an event with this receive time arrives.
    pub close_ts: Option<Nanos>,
    pub label: String,
    /// Replay the capture through a fresh host after the day and put the comparison in the report.
    pub replay_check: bool,
}

#[derive(Debug)]
pub enum DriverError {
    Io(io::Error),
    Live(String),
    Host(HostError),
    Admit(u16, AdmitError),
    Capture(String),
    Replay(ReplayError),
    /// The ledger could not be written or no longer agrees with itself, mid-day.
    Ledger(HostError),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::Io(e) => write!(f, "{e}"),
            DriverError::Live(m) => write!(f, "{m}"),
            DriverError::Host(e) | DriverError::Ledger(e) => write!(f, "{e}"),
            DriverError::Admit(id, e) => write!(f, "strategy {id} was not admitted: {e:?}"),
            DriverError::Capture(m) => write!(f, "capture: {m}"),
            DriverError::Replay(e) => write!(f, "replay check: {e}"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<io::Error> for DriverError {
    fn from(e: io::Error) -> Self {
        DriverError::Io(e)
    }
}

/// How the day ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// An event at or after the close time arrived.
    Closed,
    /// An operator asked it to stop.
    Stopped,
    /// The gateway could not be reached again after this many reconnects.
    GaveUp(String),
    /// The raw capture could not be written.
    CaptureFailed(String),
}

pub struct DayResult<S: LedgerStore> {
    pub outcome: Outcome,
    pub host: Host<S>,
    pub log_path: PathBuf,
    pub capture_dir: PathBuf,
    pub reconnects: u32,
    pub repeats_dropped: u64,
    /// The report as written, with what happened to the day on top.
    pub text: String,
    pub replay_equal: Option<bool>,
}

/// The engine's time per event, in power-of-two buckets of nanoseconds.
struct Lag([u64; 65]);

impl Lag {
    fn record(&mut self, ns: u64) {
        self.0[(64 - ns.leading_zeros()) as usize] += 1;
    }

    fn quantile(&self, permille: u64) -> Nanos {
        let total: u64 = self.0.iter().sum();
        if total == 0 {
            return 0;
        }
        let want = (total * permille).div_ceil(1000).max(1);
        let mut seen = 0;
        for (b, n) in self.0.iter().enumerate() {
            seen += n;
            if seen >= want {
                return if b == 0 {
                    0
                } else if b >= 64 {
                    u64::MAX
                } else {
                    (1u64 << b) - 1
                };
            }
        }
        0
    }
}

/// The capture as a sink the feed thread writes to.
pub struct CaptureSink {
    writer: Option<RawWriter>,
    since_sync: u32,
}

impl CaptureSink {
    pub fn new(writer: RawWriter) -> CaptureSink {
        CaptureSink {
            writer: Some(writer),
            since_sync: 0,
        }
    }

    /// Close the capture: its last segment is finished and listed. `(records, segments)`.
    pub fn finish(&mut self) -> Result<(u64, u64), String> {
        self.writer
            .take()
            .ok_or_else(|| "the capture was already finished".to_owned())?
            .finish()
            .map_err(|e| e.to_string())
    }
}

impl RawSink for CaptureSink {
    fn record(&mut self, rec: &dbn::RecordRef<'_>) -> Result<(), String> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| "the capture was already finished".to_owned())?;
        w.write(rec).map_err(|e| e.to_string())?;
        self.since_sync += 1;
        if self.since_sync >= 65_536 {
            self.since_sync = 0;
            w.sync().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// The decision log, appended to as the day goes.
struct LogFile {
    file: File,
    written: usize,
    started: bool,
}

impl LogFile {
    fn open(path: &Path) -> io::Result<LogFile> {
        Ok(LogFile {
            file: OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)?,
            written: 0,
            started: false,
        })
    }

    /// Write what is new in `log` and make it durable.
    fn flush(&mut self, log: &Log) -> io::Result<()> {
        if !self.started {
            self.file.write_all(log.header().as_bytes())?;
            self.started = true;
        }
        let mut text = String::new();
        for r in &log.recs[self.written..] {
            let _ = writeln!(text, "{}", r.line());
        }
        self.written = log.recs.len();
        self.file.write_all(text.as_bytes())?;
        self.file.sync_data()
    }

    fn close(&mut self, log: &Log) -> io::Result<()> {
        self.flush(log)?;
        self.file.write_all(log.footer().as_bytes())?;
        self.file.sync_all()
    }
}

fn wait_for_names(p: &LiveProvider, w: &Warmup) {
    let start = Instant::now();
    let Some(shared) = p.shared() else { return };
    let mut last = shared.mappings.load(Relaxed);
    let mut last_change = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(5).min(w.quiet));
        let now = shared.mappings.load(Relaxed);
        if now != last {
            last = now;
            last_change = Instant::now();
        }
        let down = shared.state() != State::Streaming;
        if (now > 0 && last_change.elapsed() >= w.quiet) || start.elapsed() >= w.max || down {
            return;
        }
    }
}

/// Run a day. `strategies` come with their certificates (see `tf_host::certify`); `store` is the ledger;
/// `stop` ends the day when set. Returns when the day is over, with the host so it can be read.
pub fn run<S: LedgerStore>(
    cfg: DriverConfig,
    strategies: Vec<(StrategyDef, Certificate)>,
    store: S,
    stop: Arc<AtomicBool>,
) -> Result<DayResult<S>, DriverError> {
    let capture_dir = cfg.dir.join("capture");
    fs::create_dir_all(&capture_dir)?;
    let (writer, _) = RawWriter::open(tf_capture::Config {
        segment_secs: cfg.segment_secs,
        ..tf_capture::Config::new(&capture_dir, &cfg.live.dataset)
    })
    .map_err(|e| DriverError::Capture(e.to_string()))?;
    let sink = Arc::new(Mutex::new(CaptureSink::new(writer)));
    let shared_sink: SharedSink = sink.clone();
    let r = run_with_sink(
        cfg,
        strategies,
        store,
        stop,
        shared_sink,
        Some(sink.clone()),
        capture_dir,
    );
    // Whatever happened, the capture is finished (its last segment closed and listed).
    if let Ok(mut s) = sink.lock() {
        let _ = s.finish();
    }
    r
}

/// [`run`] with the capture sink given (the capture itself, or one that fails, for a test).
pub fn run_with_sink<S: LedgerStore>(
    cfg: DriverConfig,
    strategies: Vec<(StrategyDef, Certificate)>,
    store: S,
    stop: Arc<AtomicBool>,
    sink: SharedSink,
    capture: Option<Arc<Mutex<CaptureSink>>>,
    capture_dir: PathBuf,
) -> Result<DayResult<S>, DriverError> {
    fs::create_dir_all(&cfg.dir)?;
    let log_path = cfg.dir.join("decisions.log");
    let mut logfile = LogFile::open(&log_path)?;
    let mut provider = LiveProvider::new(cfg.live.clone(), cfg.ingest)
        .map_err(|e| DriverError::Live(format!("{e:?}")))?
        .with_sink(sink);

    // Warm-up: log in, let the gateway name the instruments, then build the day.
    provider
        .connect()
        .map_err(|e| DriverError::Live(e.to_string()))?;
    wait_for_names(&provider, &cfg.warmup);
    let names = provider.shared().map(|s| s.names()).unwrap_or_default();
    let reference = Reference {
        symbols: symbol_table(&names),
        snapshot: cfg.snapshot.clone(),
    };
    let mut host_cfg = cfg.host.clone();
    host_cfg.id_space = host_cfg.id_space.max(names.len());
    let mut host = Host::new(host_cfg.clone(), reference, store)
        .map_err(DriverError::Host)?
        .record();
    let (defs, certs): (Vec<StrategyDef>, Vec<Certificate>) = strategies.into_iter().unzip();
    for (def, cert) in defs.iter().zip(&certs) {
        host.add_strategy(def, cert)
            .map_err(|e| DriverError::Admit(def.id, e))?;
    }
    let mut notes_live: Vec<String> = Vec::new();

    let mut dedupe = Dedupe::new();
    let mut lag = Lag([0; 65]);
    let mut reconnects = 0u32;
    let mut backoff = cfg.reconnect.first_backoff;
    let mut last_flush = Instant::now();
    let mut last_ts: Nanos = 0;
    let outcome = loop {
        if stop.load(Relaxed) {
            break Outcome::Stopped;
        }
        // Look at the session before the queue, so what arrives in between is not mistaken for the end.
        let session = provider.shared().map(|s| (s.state(), s.error()));
        match provider.recv_timeout(Duration::from_millis(2)) {
            Some(Delivery::Event(e)) => {
                if !dedupe.admit(&e) {
                    continue;
                }
                if cfg.close_ts.is_some_and(|c| e.ts_recv() >= c) {
                    break Outcome::Closed;
                }
                last_ts = last_ts.max(e.ts_recv());
                let t = Instant::now();
                host.on_event(&e).map_err(DriverError::Ledger)?;
                lag.record(u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX));
            }
            Some(Delivery::Gap(g)) => host.on_gap(g.lost, g.count, g.first_ts, g.last_ts),
            None => {
                if last_flush.elapsed() >= Duration::from_secs(1) {
                    last_flush = Instant::now();
                    if let Some(l) = host.log() {
                        logfile.flush(l)?;
                    }
                }
                if matches!(session, Some((State::Streaming, _))) {
                    continue;
                }
                let (why, tapped) = match session {
                    Some((_, Some(err))) => (err.clone(), err.contains("tap:")),
                    Some((_, None)) => ("the gateway closed the session".to_owned(), false),
                    None => ("no session".to_owned(), false),
                };
                if tapped {
                    break Outcome::CaptureFailed(why);
                }
                // The session is over: reopen it from the last event, with backoff.
                if reconnects >= cfg.reconnect.max {
                    break Outcome::GaveUp(format!(
                        "{why}; {reconnects} reconnects were not enough"
                    ));
                }
                reconnects += 1;
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(cfg.reconnect.max_backoff);
                let resume = (provider.last_event_ts() > 0).then(|| provider.last_event_ts());
                match provider.reconnect(resume) {
                    Ok(()) => {
                        backoff = cfg.reconnect.first_backoff;
                        notes_live.push(format!(
                            "reconnect {reconnects}: {why}; resumed from {}",
                            resume.map_or("the start".to_owned(), |t| t.to_string())
                        ));
                    }
                    Err(e) => notes_live.push(format!("reconnect {reconnects} failed: {e}")),
                }
            }
        }
    };

    // The end of the day: close the session, take what the queue still holds, end the host's day.
    provider.disconnect();
    let mut tail = 0u64;
    while let Some(d) = provider.recv() {
        match d {
            Delivery::Event(e) => {
                if dedupe.admit(&e) && cfg.close_ts.is_none_or(|c| e.ts_recv() < c) {
                    last_ts = last_ts.max(e.ts_recv());
                    host.on_event(&e).map_err(DriverError::Ledger)?;
                    tail += 1;
                }
            }
            Delivery::Gap(g) => host.on_gap(g.lost, g.count, g.first_ts, g.last_ts),
        }
    }
    let _ = tail;
    if matches!(outcome, Outcome::CaptureFailed(_)) {
        host.kill_switch(last_ts).map_err(DriverError::Ledger)?;
    }
    host.end_of_day(last_ts.max(host.now()))
        .map_err(DriverError::Ledger)?;
    let log = host.log().cloned().expect("the host records");
    logfile.close(&log)?;

    // The capture: finished, then checked.
    let mut facts = None;
    if let Some(c) = &capture {
        if let Ok(mut s) = c.lock() {
            if let Some(w) = s.writer.take() {
                let (records, segments) = w
                    .finish()
                    .map_err(|e| DriverError::Capture(e.to_string()))?;
                let bytes = tf_capture::list(&capture_dir)
                    .map(|l| l.iter().map(|e| e.bytes).sum())
                    .unwrap_or(0);
                facts = Some(tf_host::CaptureFacts {
                    segments,
                    records,
                    bytes,
                });
            }
        }
    }
    let mut notes = String::new();
    if facts.is_some() {
        match tf_capture::verify(&capture_dir) {
            Ok(r) if r.is_clean() => {}
            Ok(r) => {
                let _ = writeln!(
                    notes,
                    "THE CAPTURE DOES NOT VERIFY: {}",
                    r.problems.join("; ")
                );
            }
            Err(e) => {
                let _ = writeln!(notes, "THE CAPTURE COULD NOT BE CHECKED: {e}");
            }
        }
    }
    let mut replay_equal = None;
    let mut replay_report = None;
    if cfg.replay_check && facts.is_some() && !matches!(outcome, Outcome::CaptureFailed(_)) {
        let st = provider.stats();
        let r = tf_host::replay_capture(
            &capture_dir,
            &log,
            &host_cfg,
            cfg.snapshot.clone(),
            &defs,
            st.dropped_trades + st.dropped_quotes + st.dropped_control,
        )
        .map_err(DriverError::Replay)?;
        replay_equal = Some(r.verdict.is_equal());
        replay_report = Some(r);
    }
    let inputs = SystemInputs {
        ingest: Some(provider.stats()),
        engine_lag: Some((lag.quantile(990), lag.quantile(1000))),
        capture: facts,
    };
    let daily = DailyReport::build(&host, &cfg.label, inputs, replay_report.as_ref());
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{}",
        match &outcome {
            Outcome::Closed => "THE DAY CLOSED at its close time.".to_owned(),
            Outcome::Stopped => "THE DAY WAS STOPPED by an operator.".to_owned(),
            Outcome::GaveUp(why) => format!(
                "THE GATEWAY WAS LOST and did not come back: {why}. Nothing more was seen; what was open stayed open (below)."
            ),
            Outcome::CaptureFailed(why) => format!(
                "THE CAPTURE FAILED ({why}). The kill switch was thrown and the day ended: a day that cannot be kept is not traded."
            ),
        }
    );
    let _ = writeln!(
        text,
        "{reconnects} reconnects, {} repeated events dropped.",
        dedupe.dropped()
    );
    text.push_str(&notes);
    for n in &notes_live {
        let _ = writeln!(text, "{n}");
    }
    let open = host.journal().snapshot().positions;
    if open.iter().any(|p| p.2 != 0) {
        let _ = writeln!(text, "POSITIONS OPEN AT THE END:");
        for (s, i, q, _, _) in open.iter().filter(|p| p.2 != 0) {
            let name = host.reference().symbols.name(*i).unwrap_or("?");
            let _ = writeln!(text, "  strategy {s}: {q} shares of {name}");
        }
    }
    text.push('\n');
    text.push_str(&daily.render());
    fs::write(cfg.dir.join("report.txt"), &text)?;
    Ok(DayResult {
        outcome,
        host,
        log_path,
        capture_dir,
        reconnects,
        repeats_dropped: dedupe.dropped(),
        text,
        replay_equal,
    })
}
