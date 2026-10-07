//! Replaying a day through the host and comparing it with what the live run decided.

use std::path::Path;

use tf_capture::CaptureReplay;
use tf_core::{Event, Nanos, SymbolTable};
use tf_ledger::MemStore;
use tf_provider::{Poll, Provider};
use tf_strategy::sim::SimBroker;
use tf_universe::Snapshot;

use crate::def::{Route, StrategyDef};
use crate::equiv::{Log, Rec, Verdict, compare};
use crate::host::{AdmitError, Host, HostConfig, HostError, Reference, StrategyStats};

#[derive(Debug)]
pub enum ReplayError {
    Host(HostError),
    /// The live log adds a strategy that was not given.
    UnknownStrategy(u16),
    /// The strategy given has another fingerprint than the one that ran live: another parameter, universe
    /// or priority, so the replay would not be of the same strategy.
    StrategyChanged {
        id: u16,
        live: u64,
        given: u64,
    },
    Admit(u16, AdmitError),
    /// An action in the live log that this host does not know.
    BadAction(String),
    Capture(String),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Host(e) => write!(f, "{e}"),
            ReplayError::UnknownStrategy(id) => {
                write!(f, "the live log adds strategy {id}, which was not given")
            }
            ReplayError::StrategyChanged { id, live, given } => {
                write!(
                    f,
                    "strategy {id} ran live as {live:016x} and was given as {given:016x}: not the same strategy"
                )
            }
            ReplayError::Admit(id, e) => write!(f, "strategy {id} cannot be set up: {e:?}"),
            ReplayError::BadAction(a) => write!(f, "an action this host does not know: `{a}`"),
            ReplayError::Capture(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ReplayError {}

impl From<HostError> for ReplayError {
    fn from(e: HostError) -> Self {
        ReplayError::Host(e)
    }
}

#[derive(Debug)]
pub struct Replayed {
    pub log: Log,
    pub stats: Vec<(u16, StrategyStats)>,
    pub events: u64,
    pub ledger_records: usize,
}

fn actions(live: &Log) -> Vec<(u64, Nanos, String)> {
    live.recs
        .iter()
        .filter_map(|r| match r {
            Rec::Action { idx, ts, what } => Some((*idx, *ts, what.clone())),
            _ => None,
        })
        .collect()
}

fn apply(
    host: &mut Host<MemStore>,
    defs: &[StrategyDef],
    ts: Nanos,
    what: &str,
) -> Result<(), ReplayError> {
    let w: Vec<&str> = what.split(' ').collect();
    match w.as_slice() {
        ["add", id, fp] => {
            let id: u16 = id
                .parse()
                .map_err(|_| ReplayError::BadAction(what.to_owned()))?;
            let live =
                u64::from_str_radix(fp, 16).map_err(|_| ReplayError::BadAction(what.to_owned()))?;
            let def = defs
                .iter()
                .find(|d| d.id == id)
                .ok_or(ReplayError::UnknownStrategy(id))?;
            if def.fingerprint() != live {
                return Err(ReplayError::StrategyChanged {
                    id,
                    live,
                    given: def.fingerprint(),
                });
            }
            host.install(def).map_err(|e| ReplayError::Admit(id, e))
        }
        ["kill_strategy", id] => {
            let id: u16 = id
                .parse()
                .map_err(|_| ReplayError::BadAction(what.to_owned()))?;
            host.kill_strategy(id, ts)?;
            Ok(())
        }
        ["kill_switch"] => Ok(host.kill_switch(ts)?),
        ["end_of_day"] => Ok(host.end_of_day(ts)?),
        _ => Err(ReplayError::BadAction(what.to_owned())),
    }
}

/// Run `source` (which appends the next events to the buffer and says whether there were any) through a
/// fresh host built like the live one, doing what the live log says was done by hand, at the event it was
/// done at. Strategies on the paper route are replayed against the simulated broker.
pub fn replay(
    live: &Log,
    cfg: &HostConfig,
    reference: &Reference,
    defs: &[StrategyDef],
    mut source: impl FnMut(&mut Vec<Event>) -> bool,
) -> Result<Replayed, ReplayError> {
    let mut host = Host::new(
        cfg.clone(),
        reference.clone(),
        MemStore::from_records(vec![]),
    )?
    .with_paper(Box::new(SimBroker::new(cfg.sim, cfg.id_space)))
    .record();
    let acts = actions(live);
    let mut next = 0;
    let mut batch = Vec::new();
    // The live day ended at its `end_of_day`: nothing after it was seen, though a capture may hold it.
    let mut ended = false;
    macro_rules! due {
        () => {
            while next < acts.len() && acts[next].0 <= host.events() {
                let (_, ts, what) = &acts[next];
                apply(&mut host, defs, *ts, what)?;
                ended |= what == "end_of_day";
                next += 1;
            }
        };
    }
    'day: loop {
        batch.clear();
        if !source(&mut batch) {
            break;
        }
        for ev in &batch {
            due!();
            if ended {
                break 'day;
            }
            host.on_event(ev)?;
        }
    }
    due!();
    let _ = ended;
    let stats = defs
        .iter()
        .filter_map(|d| host.stats_of(d.id).map(|s| (d.id, s)))
        .collect();
    Ok(Replayed {
        log: host.log().cloned().expect("recording"),
        stats,
        events: host.events(),
        ledger_records: host.journal().records() as usize,
    })
}

/// Replay a vector of events.
pub fn replay_events(
    live: &Log,
    cfg: &HostConfig,
    reference: &Reference,
    defs: &[StrategyDef],
    events: &[Event],
) -> Result<Replayed, ReplayError> {
    let mut at = 0;
    replay(live, cfg, reference, defs, |out| {
        if at >= events.len() {
            return false;
        }
        let end = (at + 4096).min(events.len());
        out.extend_from_slice(&events[at..end]);
        at = end;
        true
    })
}

/// What a nightly check says.
#[derive(Debug)]
pub struct Report {
    pub verdict: Verdict,
    pub events: u64,
    /// Strategies that sent orders to a paper broker live; their fills cannot be reproduced from a capture.
    pub paper_strategies: Vec<u16>,
    /// What the live ingest queue gave up (conflated quotes, dropped trades). The capture holds what the
    /// feed sent, the engine saw less, so a replay of the capture can differ when this is not zero.
    pub live_drops: u64,
}

impl Report {
    pub fn text(&self) -> String {
        let mut s = self.verdict.report();
        s.push_str(&format!("{} events replayed\n", self.events));
        if !self.paper_strategies.is_empty() {
            s.push_str(&format!("strategies {:?} traded on a paper broker live: their fills are simulated in the replay and are not comparable\n", self.paper_strategies));
        }
        if self.live_drops > 0 && !self.verdict.is_equal() {
            s.push_str(&format!(
                "the live ingest queue gave up {} events: the engine saw less than the capture holds, which can explain a difference (replay the engine-input tape instead)\n",
                self.live_drops
            ));
        }
        s
    }
}

/// Compare a replay with the live log into a report.
pub fn report(
    live: &Log,
    replayed: &Replayed,
    names: &SymbolTable,
    defs: &[StrategyDef],
    live_drops: u64,
) -> Report {
    let paper = live
        .recs
        .iter()
        .filter_map(|r| match r {
            Rec::Action { what, .. } => what
                .strip_prefix("add ")
                .and_then(|w| w.split(' ').next())
                .and_then(|id| id.parse::<u16>().ok()),
            _ => None,
        })
        .filter(|id| defs.iter().any(|d| d.id == *id && d.route == Route::Paper))
        .collect();
    Report {
        verdict: compare(live, &replayed.log, names),
        events: replayed.events,
        paper_strategies: paper,
        live_drops,
    }
}

/// The table of names for dense instrument ids `0..`, as the gateway named them (`None` where an id was
/// seen in a record and not named: it is called `#id`). Two ids that carry one symbol are still two
/// instruments, numbered as they were, the second called `SYMBOL~1`.
pub fn symbol_table(names: &[Option<String>]) -> SymbolTable {
    let mut table = SymbolTable::new();
    for (id, n) in names.iter().enumerate() {
        let base = n.clone().unwrap_or_else(|| format!("#{id}"));
        let mut name = base.clone();
        let mut k = 1;
        while table.get(&name).is_some() {
            name = format!("{base}~{k}");
            k += 1;
        }
        let got = table.intern(&name);
        debug_assert_eq!(got as usize, id);
    }
    table
}

/// Replay a raw capture of a day (a directory written by `tf-capture`) through the host and compare.
/// Two passes: the first reads the capture to learn which instrument ids the day had and what they were
/// called (the same ids the live run assigned, since they are numbered in the order first seen); the
/// second streams the events through the host, so a full day never sits in memory.
pub fn replay_capture(
    dir: &Path,
    live: &Log,
    cfg: &HostConfig,
    snapshot: Snapshot,
    defs: &[StrategyDef],
    live_drops: u64,
) -> Result<Report, ReplayError> {
    let cap = |e: tf_capture::Error| ReplayError::Capture(e.to_string());
    // Checked and listed by the capture's own manifest, then replayed as files.
    let files: Vec<std::path::PathBuf> = {
        let open = CaptureReplay::open(dir).map_err(cap)?;
        drop(open);
        tf_capture::list(dir)
            .map_err(cap)?
            .iter()
            .map(|e| dir.join(&e.file))
            .collect()
    };
    replay_files(&files, live, cfg, snapshot, defs, live_drops)
}

/// [`replay_capture`] over zstd-compressed DBN files in the order given: the segments of a capture, or the days of a
/// history store (E19-S07). The same two passes, so the same instrument numbering.
pub fn replay_files(
    files: &[std::path::PathBuf],
    live: &Log,
    cfg: &HostConfig,
    snapshot: Snapshot,
    defs: &[StrategyDef],
    live_drops: u64,
) -> Result<Report, ReplayError> {
    let mut first = CaptureReplay::from_files(files.to_vec());
    let mut sink = Vec::new();
    loop {
        sink.clear();
        match first.poll(&mut sink, 65_536) {
            Poll::Events(_) => {}
            _ => break,
        }
    }
    let map = first.instruments();
    let names: Vec<Option<String>> = (0..map.len() as u32)
        .map(|id| {
            Some(map.symbol(id).map_or_else(
                || format!("#{}", map.raw_of(id).unwrap_or(0)),
                str::to_owned,
            ))
        })
        .collect();
    let symbols = symbol_table(&names);
    let reference = Reference { symbols, snapshot };
    let mut cfg = cfg.clone();
    cfg.id_space = live.id_space;
    let mut second = CaptureReplay::from_files(files.to_vec());
    // The live run dropped what the gateway sent twice after a reconnect; the capture holds it, so the
    // replay drops it the same way.
    let mut dedupe = tf_core::Dedupe::new();
    let mut raw = Vec::new();
    let replayed = replay(live, &cfg, &reference, defs, |out| {
        raw.clear();
        if !matches!(second.poll(&mut raw, 4096), Poll::Events(_)) {
            return false;
        }
        out.extend(raw.iter().filter(|e| dedupe.admit(e)));
        true
    })?;
    Ok(report(
        live,
        &replayed,
        &reference.symbols,
        defs,
        live_drops,
    ))
}
