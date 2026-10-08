//! The research runner (E19-S13): strategy definitions over stored days, one record per round trip.
//!
//! **One pass.** Each day is read once through one host that holds every definition, as the live host does: the same
//! engine, the same gateway and ledger, the simulated broker filling against the recorded quote after the cost model's
//! latency. What the strategies did is turned into round trips ([`super::trips`]) and the day's file is written. Nothing
//! is run per strategy: twenty definitions cost one read of the day's data.
//!
//! **A day is one unit.** Days are independent: a fresh host, an empty ledger and the day's own reference snapshot (the
//! caller gives it per day: what was known before that session, see E19-S04). Positions are not carried overnight; a
//! trip open when the day's events end is closed at the last trade and flagged. A day's events are one file or several
//! (a store's day), read in a first pass for the names of the instruments and again for the run, exactly as the host's
//! replay does, with the same session boundaries from the calendar ([`HostConfig::day`]) and the same removal of events
//! the gateway sent twice. A run of a day therefore decides what [`crate::replay_files`] would on the log it wrote.
//!
//! **Results are a directory**: `research.cfg` (the configuration: what each definition is and the cost model, whole,
//! with its fingerprint) and a file per day, `<date>.trips`, written whole and moved into place, so a day is there
//! complete or not at all. A day file says which configuration and which data it was made from and ends with a checksum.
//! - *Refusals*: a results directory without its configuration, with one that does not read, with a day made under
//!   another configuration or damaged, or asked for under another configuration than it was started with, is refused;
//!   so is a cost model that has no rate for a day.
//! - *Resume*: a run skips the days that are there, whole, for the same data and configuration, and runs the rest, so a run
//!   that stopped (a missing file, a crash) goes on from the first day it did not finish. Two runs of the same days and
//!   definitions leave identical files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_capture::CaptureReplay;
use tf_core::{Event, Nanos};
use tf_provider::{Poll, Provider};
use tf_stats::StatsError;
use tf_strategy::Trace;
use tf_strategy::trace::{parse_all, render_all};
use tf_universe::Snapshot;

use super::cost::{CostError, CostModel, is_date};
use super::keep::{self, Evidence, EvidenceWindow, gather_evidence_with};
use super::trips::{Assembler, COLUMNS, Trip, Who};
use crate::def::{StrategyDef, fnv};
use crate::equiv::{Log, Rec};
use crate::host::{FillNote, HostConfig, HostError, Reference};
use crate::replay::{learn_symbols, replay_host};

pub const CONFIG_FILE: &str = "research.cfg";
const CONFIG_HEADER: &str = "research config v1";
const DAY_HEADER: &str = "research trips v1";
const EXT: &str = ".trips";
const LOG_EXT: &str = ".log";
const TRACE_EXT: &str = ".trace";
const EVIDENCE_EXT: &str = ".evidence.zst";

#[derive(Debug)]
pub enum ResearchError {
    Io(String),
    Cost(CostError),
    /// The configuration is missing, does not read, or is not the one the results were made under.
    Config(String),
    /// A day's file in the results is damaged or was made under another configuration.
    Results(String),
    /// A day could not be run: its date, and why.
    Day {
        date: String,
        why: String,
    },
    Host(HostError),
    /// Statistics were refused: a variant not in the trial registry, days that do not fit, a damaged registry.
    Stats(StatsError),
}

impl std::fmt::Display for ResearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResearchError::Io(m) | ResearchError::Config(m) | ResearchError::Results(m) => {
                write!(f, "{m}")
            }
            ResearchError::Cost(e) => write!(f, "{e}"),
            ResearchError::Day { date, why } => write!(f, "{date}: {why}"),
            ResearchError::Host(e) => write!(f, "{e}"),
            ResearchError::Stats(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ResearchError {}

impl From<StatsError> for ResearchError {
    fn from(e: StatsError) -> Self {
        ResearchError::Stats(e)
    }
}

impl From<CostError> for ResearchError {
    fn from(e: CostError) -> Self {
        ResearchError::Cost(e)
    }
}

fn io(p: &Path, e: std::io::Error) -> ResearchError {
    ResearchError::Io(format!("{}: {e}", p.display()))
}

/// What a day needs to be run: the files of its events in order and the reference snapshot as it was before the session.
pub struct DayInput {
    pub files: Vec<PathBuf>,
    pub snapshot: Snapshot,
}

/// Where the days come from.
pub trait DaySource {
    /// The dates to run, `YYYY-MM-DD`, in order.
    fn dates(&self) -> Vec<String>;
    /// Identifies the day's data (for a store: the checksums of its files), so that a day made from other data is made
    /// again. Cheap: it is asked for every day, run or not.
    fn data_id(&self, date: &str) -> Result<String, String>;
    /// One day's input, read only when the day is to be run.
    fn load(&mut self, date: &str) -> Result<DayInput, String>;
}

/// One day as run.
pub struct DayOutcome {
    pub trips: Vec<Trip>,
    pub events: u64,
    /// The host's decision log, which [`crate::replay_files`] replays to the same decisions.
    pub log: Log,
    pub outcome_hash: u64,
    pub anomalies: Vec<String>,
    pub ledger_refusals: u64,
    /// Intents the gateway refused (a limit, a budget) and orders the broker would not take, over every strategy: a
    /// strategy whose orders all end here makes no trips, and this is how that is seen.
    pub rejected: u64,
    pub refused: u64,
    /// Every execution with what its order was for, in the order they happened: what the trips were made from.
    pub notes: Vec<FillNote>,
    /// Why the strategies acted, as they recorded it, by strategy number (E19-S32).
    pub traces: Vec<(u16, Trace)>,
    /// The names of the day's instruments, as the run numbered them.
    pub symbols: tf_core::SymbolTable,
}

/// What a run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    pub ran: Vec<String>,
    pub skipped: Vec<String>,
    pub events: u64,
    pub trips: u64,
    /// Trips that nothing was kept around (only when evidence was asked for): day, symbol, strategy and entry time.
    pub no_evidence: Vec<String>,
}

/// What a run keeps beyond what every run keeps (the trips, the decision log and the traces of each day).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Keep the market around every trade (a second, sequential read of each day's files after it is run).
    pub evidence: Option<EvidenceWindow>,
}

fn unescape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match (c, it.clone().next()) {
            ('\\', Some('\\')) => {
                o.push('\\');
                it.next();
            }
            ('\\', Some('n')) => {
                o.push('\n');
                it.next();
            }
            ('\\', Some('t')) => {
                o.push('\t');
                it.next();
            }
            _ => o.push(c),
        }
    }
    o
}

/// One definition as the stored configuration lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefLine {
    pub id: u16,
    /// The fingerprint of the definition: the variant of its trips.
    pub fingerprint: u64,
    pub name: String,
    pub params: String,
    /// The universe spec, as text.
    pub universe: String,
}

/// The budget tree a run was made under, as the configuration stores it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetView {
    /// What the tree divides, raw units.
    pub balance: u128,
    pub tree: tf_budget::Tree,
    /// Which strategy number is which strategy of the tree.
    pub ids: BTreeMap<u16, String>,
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// What a run is made of: the definitions, the host's limits and engine settings, and the cost model.
pub struct Setup<'a> {
    pub host: &'a HostConfig,
    pub cost: &'a CostModel,
    pub defs: &'a [StrategyDef],
}

impl Setup<'_> {
    fn host_fingerprint(&self) -> u64 {
        let h = self.host;
        let text = format!(
            "{} {:?} {:?} {:?} {:?} {} {} {:?}",
            h.id_space,
            h.limits,
            h.budgets,
            h.promoter,
            h.scanner,
            h.min_certified_events,
            h.start_ts,
            h.bars
        );
        fnv(&[text.as_bytes()])
    }

    /// Why this setup cannot be run, if it cannot.
    fn check(&self) -> Result<(), ResearchError> {
        let mut ids = BTreeSet::new();
        for d in self.defs {
            if d.name.is_empty() || d.name.contains(['\t', '\n', '\r']) {
                return Err(ResearchError::Config(format!(
                    "strategy {} has a name that is empty or has a tab or a line break",
                    d.id
                )));
            }
            if !ids.insert(d.id) {
                return Err(ResearchError::Config(format!(
                    "strategy {} is given twice",
                    d.id
                )));
            }
        }
        if self.defs.is_empty() {
            return Err(ResearchError::Config("no strategy to run".into()));
        }
        Ok(())
    }

    /// The configuration as text: kept in the results directory and fingerprinted into every day.
    pub fn render(&self) -> String {
        let mut s = format!("{CONFIG_HEADER}\nhost {:016x}\n", self.host_fingerprint());
        // The budget tree the strategies were run under, so that a result can say what each was allowed (the host line holds only
        // its fingerprint).
        if let Some(b) = &self.host.budgets {
            s.push_str(&format!("budget\tbalance\t{}\n", b.balance()));
            s.push_str(&format!("budget\ttree\t{}\n", escape(&b.tree().render())));
            for (n, id) in b.ids() {
                s.push_str(&format!("budget\tid\t{n}\t{id}\n"));
            }
        }
        for d in self.defs {
            s.push_str(&format!(
                "def\t{}\t{:016x}\t{}\t{}\t{}\n",
                d.id,
                d.fingerprint(),
                d.name,
                escape(&d.params),
                escape(&d.universe.render())
            ));
        }
        s.push_str(&self.cost.render());
        s
    }

    pub fn fingerprint(&self) -> u64 {
        fnv(&[self.render().as_bytes()])
    }
}

/// The text of a configuration, read back: its fingerprint and cost model.
pub fn read_config(text: &str) -> Result<(u64, CostModel), ResearchError> {
    let bad = |m: &str| ResearchError::Config(format!("{CONFIG_FILE}: {m}"));
    if !text.starts_with(CONFIG_HEADER) {
        return Err(bad("not a research configuration"));
    }
    let at = text
        .find("cost model v1")
        .ok_or_else(|| bad("has no cost model"))?;
    let cost = CostModel::parse(&text[at..])?;
    if !text[..at].lines().any(|l| l.starts_with("def\t")) {
        return Err(bad("lists no strategy"));
    }
    Ok((fnv(&[text.as_bytes()]), cost))
}

/// The directory of results.
pub struct Results {
    dir: PathBuf,
    fingerprint: u64,
    cost: CostModel,
}

fn day_path(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("{date}{EXT}"))
}

impl Results {
    /// Open the directory for a run: made if it is new, refused if it holds results made under another configuration.
    pub fn start(dir: &Path, setup: &Setup<'_>) -> Result<Results, ResearchError> {
        setup.check()?;
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        let path = dir.join(CONFIG_FILE);
        let text = setup.render();
        match fs::read_to_string(&path) {
            Ok(have) if have == text => {}
            Ok(_) => {
                return Err(ResearchError::Config(format!(
                    "{} holds results made under another configuration (another strategy, parameter, limit or cost): use a new directory",
                    dir.display()
                )));
            }
            Err(_) => {
                // A directory with day files and no configuration is results without their configuration.
                if has_days(dir)? {
                    return Err(ResearchError::Config(format!(
                        "{} has results and no {CONFIG_FILE}: a result without its configuration is refused",
                        dir.display()
                    )));
                }
                write_whole(&path, &text)?;
            }
        }
        Ok(Results {
            dir: dir.to_owned(),
            fingerprint: setup.fingerprint(),
            cost: setup.cost.clone(),
        })
    }

    /// Open existing results to read them. Refused without a readable configuration.
    pub fn open(dir: &Path) -> Result<Results, ResearchError> {
        let path = dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&path).map_err(|_| {
            ResearchError::Config(format!(
                "{} has no {CONFIG_FILE}: a result without its configuration is refused",
                dir.display()
            ))
        })?;
        let (fingerprint, cost) = read_config(&text)?;
        Ok(Results {
            dir: dir.to_owned(),
            fingerprint,
            cost,
        })
    }

    /// The cost model the results were made under.
    pub fn cost(&self) -> &CostModel {
        &self.cost
    }

    /// The strategies the run was made with, as the stored configuration lists them, in the order given. A strategy that made no
    /// trade is here too.
    pub fn definition_lines(&self) -> Result<Vec<DefLine>, ResearchError> {
        let path = self.dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let bad = |l: &str| {
            ResearchError::Config(format!(
                "{CONFIG_FILE}: a strategy line that does not read: `{l}`"
            ))
        };
        text.lines()
            .filter(|l| l.starts_with("def\t"))
            .map(|l| {
                let w: Vec<&str> = l.split('\t').collect();
                let [_, id, fp, name, params, universe] = w.as_slice() else {
                    return Err(bad(l));
                };
                Ok(DefLine {
                    id: id.parse().map_err(|_| bad(l))?,
                    fingerprint: u64::from_str_radix(fp, 16).map_err(|_| bad(l))?,
                    name: (*name).to_owned(),
                    params: unescape(params),
                    universe: unescape(universe),
                })
            })
            .collect()
    }

    /// The strategies the run was made with: the fingerprint and the name of each, in the order given.
    pub fn definitions(&self) -> Result<Vec<(u64, String)>, ResearchError> {
        Ok(self
            .definition_lines()?
            .into_iter()
            .map(|d| (d.fingerprint, d.name))
            .collect())
    }

    /// The budget tree the run was made under; `None` for a run without budgets.
    pub fn budgets(&self) -> Result<Option<BudgetView>, ResearchError> {
        let path = self.dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let bad = |m: &str| ResearchError::Config(format!("{CONFIG_FILE}: the budget lines {m}"));
        let (mut balance, mut tree, mut ids) = (None, None, BTreeMap::new());
        for l in text.lines().filter(|l| l.starts_with("budget\t")) {
            let w: Vec<&str> = l.split('\t').collect();
            match w.as_slice() {
                [_, "balance", v] => {
                    balance = Some(v.parse::<u128>().map_err(|_| bad("do not read"))?)
                }
                [_, "tree", t] => {
                    tree = Some(
                        tf_budget::Tree::parse(&unescape(t))
                            .map_err(|e| bad(&format!("do not read: {e:?}")))?,
                    )
                }
                [_, "id", n, id] => {
                    ids.insert(
                        n.parse::<u16>().map_err(|_| bad("do not read"))?,
                        (*id).to_owned(),
                    );
                }
                _ => return Err(bad("do not read")),
            }
        }
        match (balance, tree) {
            (None, None) => Ok(None),
            (Some(balance), Some(tree)) => Ok(Some(BudgetView { balance, tree, ids })),
            _ => Err(bad("are not whole")),
        }
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// The dates that have a day file, in order.
    pub fn dates(&self) -> Result<Vec<String>, ResearchError> {
        let mut v = Vec::new();
        for e in fs::read_dir(&self.dir).map_err(|e| io(&self.dir, e))? {
            let e = e.map_err(|e| io(&self.dir, e))?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(d) = name.strip_suffix(EXT) {
                if is_date(d) {
                    v.push(d.to_owned());
                }
            }
        }
        v.sort();
        Ok(v)
    }

    /// One day's file, read and checked against the configuration.
    pub fn day(&self, date: &str) -> Result<DayFile, ResearchError> {
        let path = day_path(&self.dir, date);
        let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let d = DayFile::parse(&text)
            .map_err(|m| ResearchError::Results(format!("{}: {m}", path.display())))?;
        if d.day != date {
            return Err(ResearchError::Results(format!(
                "{}: it is the file of {}",
                path.display(),
                d.day
            )));
        }
        if d.config != self.fingerprint {
            return Err(ResearchError::Results(format!(
                "{}: made under configuration {:016x}, these results are {:016x}",
                path.display(),
                d.config,
                self.fingerprint
            )));
        }
        Ok(d)
    }

    /// Every round trip, in date order and within a day in the order they ended.
    pub fn trips(&self) -> Result<Vec<Trip>, ResearchError> {
        let mut all = Vec::new();
        for d in self.dates()? {
            all.extend(self.day(&d)?.trips);
        }
        Ok(all)
    }

    fn write_day(&self, d: &DayFile) -> Result<(), ResearchError> {
        write_whole(&day_path(&self.dir, &d.day), &d.render())
    }

    fn companion(&self, date: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{date}{ext}"))
    }

    /// Keep a day's decision log and traces, and its evidence if there is any, beside its trips.
    fn write_companions(
        &self,
        date: &str,
        out: &DayOutcome,
        evidence: Option<&Evidence>,
    ) -> Result<(), ResearchError> {
        let (fp, outcome) = (self.fingerprint, out.outcome_hash);
        write_whole(
            &self.companion(date, LOG_EXT),
            &keep::wrap("research log", date, fp, outcome, &out.log.render()),
        )?;
        write_whole(
            &self.companion(date, TRACE_EXT),
            &keep::wrap(
                "research trace",
                date,
                fp,
                outcome,
                &render_all(&out.traces),
            ),
        )?;
        let path = self.companion(date, EVIDENCE_EXT);
        match evidence {
            Some(e) => {
                let part = path.with_extension("part");
                fs::write(&part, keep::evidence_file(date, fp, outcome, e))
                    .map_err(|e| io(&part, e))?;
                fs::rename(&part, &path).map_err(|e| io(&path, e))
            }
            // A day made again without evidence leaves none from an earlier run.
            None => {
                let _ = fs::remove_file(&path);
                Ok(())
            }
        }
    }

    fn read_companion(&self, date: &str, ext: &str, kind: &str) -> Result<String, ResearchError> {
        let outcome = self.day(date)?.outcome_hash;
        let path = self.companion(date, ext);
        let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        keep::unwrap(kind, &text, date, self.fingerprint, outcome)
            .map(str::to_owned)
            .map_err(|m| ResearchError::Results(format!("{}: {m}", path.display())))
    }

    /// The host's decision log of a day, checked to be that day's, under this configuration, with that outcome.
    pub fn log(&self, date: &str) -> Result<Log, ResearchError> {
        let body = self.read_companion(date, LOG_EXT, "research log")?;
        Log::parse(&body).map_err(|m| {
            ResearchError::Results(format!("{}: {m}", self.companion(date, LOG_EXT).display()))
        })
    }

    /// What the strategies recorded of why they acted on a day, by strategy number.
    pub fn traces(&self, date: &str) -> Result<Vec<(u16, Trace)>, ResearchError> {
        let body = self.read_companion(date, TRACE_EXT, "research trace")?;
        parse_all(&body).map_err(|m| {
            ResearchError::Results(format!(
                "{}: {m}",
                self.companion(date, TRACE_EXT).display()
            ))
        })
    }

    /// The market kept around the day's trades. An error if the day was run without evidence.
    pub fn evidence(&self, date: &str) -> Result<Evidence, ResearchError> {
        let outcome = self.day(date)?.outcome_hash;
        let path = self.companion(date, EVIDENCE_EXT);
        let bytes = fs::read(&path).map_err(|e| io(&path, e))?;
        keep::read_evidence(&bytes, date, self.fingerprint, outcome)
            .map_err(|m| ResearchError::Results(format!("{}: {m}", path.display())))
    }

    /// Whether a day's companions are there and whole (its evidence too, if `with_evidence`).
    fn companions_ok(&self, date: &str, with_evidence: bool) -> bool {
        self.log(date).is_ok()
            && self.traces(date).is_ok()
            && (!with_evidence || self.evidence(date).is_ok())
    }
}

fn has_days(dir: &Path) -> Result<bool, ResearchError> {
    for e in fs::read_dir(dir).map_err(|e| io(dir, e))? {
        let e = e.map_err(|e| io(dir, e))?;
        if e.file_name().to_string_lossy().ends_with(EXT) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Written whole beside the target and moved into place.
fn write_whole(path: &Path, text: &str) -> Result<(), ResearchError> {
    let part = path.with_extension("part");
    fs::write(&part, text).map_err(|e| io(&part, e))?;
    fs::rename(&part, path).map_err(|e| io(path, e))
}

/// A day's file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayFile {
    pub day: String,
    pub config: u64,
    pub data: String,
    pub events: u64,
    pub outcome_hash: u64,
    pub anomalies: u64,
    /// Intents the gateway refused and orders the broker refused.
    pub rejected: u64,
    pub refused: u64,
    pub trips: Vec<Trip>,
}

impl DayFile {
    /// The file for a day as run: under the configuration with this fingerprint, from the data with this identifier.
    pub(crate) fn of(day: &str, config: u64, data: String, out: DayOutcome) -> DayFile {
        DayFile {
            day: day.to_owned(),
            config,
            data,
            events: out.events,
            outcome_hash: out.outcome_hash,
            anomalies: out.anomalies.len() as u64 + out.ledger_refusals,
            rejected: out.rejected,
            refused: out.refused,
            trips: out.trips,
        }
    }

    pub fn render(&self) -> String {
        let mut s = format!(
            "{DAY_HEADER}\nday {}\nconfig {:016x}\ndata {}\nevents {}\noutcome {:016x}\nanomalies {}\nrejected {}\nrefused {}\ntrips {}\n{COLUMNS}\n",
            self.day,
            self.config,
            self.data,
            self.events,
            self.outcome_hash,
            self.anomalies,
            self.rejected,
            self.refused,
            self.trips.len()
        );
        for t in &self.trips {
            s.push_str(&t.line());
            s.push('\n');
        }
        let sum = fnv(&[s.as_bytes()]);
        s.push_str(&format!("end {sum:016x}\n"));
        s
    }

    pub fn parse(text: &str) -> Result<DayFile, String> {
        let at = text.rfind("end ").ok_or("cut short: no `end`")?;
        let (body, tail) = text.split_at(at);
        if !tail.ends_with('\n') || tail.matches('\n').count() != 1 {
            return Err("text after `end`, or cut short".into());
        }
        let want = u64::from_str_radix(tail[4..].trim_end(), 16).map_err(|_| "a bad checksum")?;
        if fnv(&[body.as_bytes()]) != want {
            return Err("damaged: the checksum differs".into());
        }
        let mut lines = body.lines();
        if lines.next() != Some(DAY_HEADER) {
            return Err(format!("not `{DAY_HEADER}`"));
        }
        let mut field = |name: &str| -> Result<String, String> {
            lines
                .next()
                .and_then(|l| l.strip_prefix(&format!("{name} ")))
                .map(str::to_owned)
                .ok_or(format!("expected `{name}`"))
        };
        let day = field("day")?;
        let config = u64::from_str_radix(&field("config")?, 16).map_err(|_| "config")?;
        let data = field("data")?;
        let events = field("events")?.parse().map_err(|_| "events")?;
        let outcome_hash = u64::from_str_radix(&field("outcome")?, 16).map_err(|_| "outcome")?;
        let anomalies = field("anomalies")?.parse().map_err(|_| "anomalies")?;
        let rejected = field("rejected")?.parse().map_err(|_| "rejected")?;
        let refused = field("refused")?.parse().map_err(|_| "refused")?;
        let n: usize = field("trips")?.parse().map_err(|_| "trips")?;
        if lines.next() != Some(COLUMNS) {
            return Err("the columns are not the ones this version reads".into());
        }
        let trips: Vec<Trip> = lines
            .map(Trip::parse)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("a record does not read: {e}"))?;
        if trips.len() != n {
            return Err(format!("{} records, {n} said", trips.len()));
        }
        Ok(DayFile {
            day,
            config,
            data,
            events,
            outcome_hash,
            anomalies,
            rejected,
            refused,
            trips,
        })
    }
}

/// The host configuration a day is run with: the sessions of that day from the start, and the cost model's latency
/// and borrow rate in the simulated broker. A replay of the day is given the same.
pub(crate) fn day_config(host: &HostConfig, cost: &CostModel, times: SessionTimes) -> HostConfig {
    HostConfig {
        day: Some(times),
        sim: cost.sim(),
        ..host.clone()
    }
}

/// The instruments the snapshot says are easy to borrow: a short in one pays no borrow fee.
pub(crate) fn easy_to_borrow(reference: &Reference) -> BTreeSet<u32> {
    reference
        .snapshot
        .rows
        .iter()
        .filter(|r| r.easy_to_borrow == Some(true))
        .filter_map(|r| reference.symbols.get(&r.symbol))
        .collect()
}

/// The kind of the host's trace of the instruments a day traded, by number and symbol (strategy 0 in a day's traces).
pub const INSTRUMENTS: &str = "instruments";

/// What a strategy's orders met that was not the gateway's limits: the broker's refusals and the rate limit.
pub(crate) fn refusals(st: &crate::host::StrategyStats) -> u64 {
    st.refused_by_broker + st.rate_limited
}

/// One day through one host: the definitions, the events of `input`, the round trips.
pub fn run_day(
    date: &str,
    input: &DayInput,
    setup: &Setup<'_>,
) -> Result<DayOutcome, ResearchError> {
    let fail = |why: String| ResearchError::Day {
        date: date.to_owned(),
        why,
    };
    setup.check()?;
    if !is_date(date) {
        return Err(fail("not a date (YYYY-MM-DD)".into()));
    }
    // Refused before the day is run, not after.
    setup.cost.sec_rate(date)?;
    setup.cost.taf_rate(date)?;
    let d = Date::new(
        date[..4].parse().map_err(|_| fail("year".into()))?,
        date[5..7].parse().map_err(|_| fail("month".into()))?,
        date[8..10].parse().map_err(|_| fail("day".into()))?,
    )
    .ok_or_else(|| fail("not a calendar date".into()))?;
    let times = Calendar::us_equities()
        .times(d)
        .map_err(|e| fail(format!("the calendar does not cover it: {e:?}")))?
        .ok_or_else(|| fail("the market was closed that day".into()))?;
    if input.files.is_empty() {
        return Err(fail("no files".into()));
    }
    let symbols = learn_symbols(&input.files);
    let reference = Reference {
        symbols,
        snapshot: input.snapshot.clone(),
    };
    let cfg = day_config(setup.host, setup.cost, times);
    let mut host = replay_host(&cfg, &reference)
        .map_err(ResearchError::Host)?
        .with_fill_log()
        .with_traces();
    for def in setup.defs {
        host.install(def)
            .map_err(|e| fail(format!("strategy {} cannot be set up: {e:?}", def.id)))?;
    }
    let mut source = CaptureReplay::from_files(input.files.clone());
    let mut dedupe = tf_core::Dedupe::new();
    let mut raw: Vec<Event> = Vec::new();
    let mut last: Nanos = 0;
    let mut notes: Vec<FillNote> = Vec::new();
    loop {
        raw.clear();
        match source.poll(&mut raw, 4096) {
            Poll::Events(_) => {}
            Poll::Idle => continue,
            _ => break,
        }
        for ev in raw.iter().filter(|e| dedupe.admit(e)) {
            last = last.max(ev.ts_recv());
            host.on_event(ev).map_err(ResearchError::Host)?;
        }
        notes.extend(host.take_fill_notes());
    }
    if let Some(why) = source.failure() {
        return Err(fail(format!("the data cannot be read: {why}")));
    }
    if host.events() == 0 {
        return Err(fail("the day has no events".into()));
    }
    host.end_of_day(last).map_err(ResearchError::Host)?;
    notes.extend(host.take_fill_notes());
    let mut traces = host.take_traces();
    // What each strategy was refused, from the host's own counts: not a strategy's trace but kept beside them, so that the
    // view can say what a strategy tried and was not allowed. A strategy that panicked has none.
    for d in setup.defs {
        if let Some(st) = host.stats_of(d.id) {
            traces.push((
                d.id,
                Trace::new(last, "stats")
                    .with("accepted", st.accepted)
                    .with("rejected", st.rejected_by_gateway)
                    .with("refused", refusals(&st)),
            ));
        }
    }
    let who: BTreeMap<u16, Who> = setup
        .defs
        .iter()
        .map(|d| {
            (
                d.id,
                Who {
                    name: d.name.clone(),
                    variant: d.fingerprint(),
                },
            )
        })
        .collect();
    let names = &reference.symbols;
    let symbol = |i: u32| names.name(i).map_or_else(|| format!("#{i}"), str::to_owned);
    let easy = easy_to_borrow(&reference);
    let is_easy = |i: u32| easy.contains(&i);
    let mut asm = Assembler::new(date, setup.cost, &who, &symbol, &is_easy);
    for n in &notes {
        asm.fill(n);
    }
    let trips = asm.end(last, |i| host.journal().gateway().mark_of(i))?;
    // The log names instruments by number; the view names them by symbol. Kept for the instruments the day traded, as
    // the host's own (strategy 0) trace, so that a trip can be followed through the log without the day's files.
    let mut traded = std::collections::BTreeSet::new();
    for rec in &host.log().expect("recording").recs {
        match rec {
            Rec::Decision { instrument, .. } | Rec::Fill { instrument, .. } => {
                traded.insert(*instrument);
            }
            _ => {}
        }
    }
    let mut book = Trace::new(last, INSTRUMENTS).with_columns(&["instrument", "symbol"]);
    for i in traded {
        book.push_row(vec![i.to_string(), symbol(i)]);
    }
    traces.push((0, book));
    Ok(DayOutcome {
        trips,
        events: host.events(),
        log: host.log().cloned().expect("recording"),
        outcome_hash: host.outcome_hash(),
        anomalies: host.anomalies().to_vec(),
        ledger_refusals: host.ledger_refusals(),
        rejected: setup
            .defs
            .iter()
            .filter_map(|d| host.stats_of(d.id))
            .map(|s| s.rejected_by_gateway)
            .sum(),
        refused: setup
            .defs
            .iter()
            .filter_map(|d| host.stats_of(d.id))
            .map(|s| refusals(&s))
            .sum(),
        notes,
        traces,
        symbols: reference.symbols.clone(),
    })
}

/// Run every day of `source` that `dir` does not have whole, in order, and write each as it is done. A day that cannot be
/// run stops the run with the days before it written: run again to go on from it.
pub fn run(
    setup: &Setup<'_>,
    source: &mut dyn DaySource,
    dir: &Path,
) -> Result<RunReport, ResearchError> {
    run_with(setup, source, dir, &RunOptions::default())
}

/// [`run`] with options. A day is there when its trips, its decision log and its traces are (and its evidence, if asked for):
/// a day made without them is made again.
pub fn run_with(
    setup: &Setup<'_>,
    source: &mut dyn DaySource,
    dir: &Path,
    opts: &RunOptions,
) -> Result<RunReport, ResearchError> {
    let results = Results::start(dir, setup)?;
    let mut report = RunReport::default();
    for date in source.dates() {
        let day_err = |why: String| ResearchError::Day {
            date: date.clone(),
            why,
        };
        let data_id = source.data_id(&date).map_err(day_err)?;
        if let Ok(have) = results.day(&date) {
            if have.data == data_id && results.companions_ok(&date, opts.evidence.is_some()) {
                report.skipped.push(date);
                continue;
            }
        }
        let input = source.load(&date).map_err(day_err)?;
        let out = run_day(&date, &input, setup)?;
        report.events += out.events;
        report.trips += out.trips.len() as u64;
        let evidence = match opts.evidence {
            Some(w) => {
                let e = gather_evidence_with(&input.files, &out.symbols, &out.trips, w)
                    .map_err(day_err)?;
                report.no_evidence.extend(e.missing(&date, &out.trips, w));
                Some(e)
            }
            None => None,
        };
        // The companions first and the trips last: the trips file is what says a day is there.
        results.write_companions(&date, &out, evidence.as_ref())?;
        results.write_day(&DayFile::of(&date, results.fingerprint, data_id, out))?;
        report.ran.push(date);
    }
    Ok(report)
}
