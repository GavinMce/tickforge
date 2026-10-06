//! The run catalog: every run of each strategy, newest first, with backtest, paper and live kept
//! apart.
//!
//! Two sources feed it, and neither is written to:
//! - the run store (`tf backtest --store`): each stored result is one backtest run, and only these
//!   can be opened in the explorer, because only they keep what is needed to replay them;
//! - an order ledger: each trading day in it (the records between `newday` marks) is one session of
//!   each strategy that did anything that day. The ledger does not say whether it came from paper
//!   or live trading, so the operator says.
//!
//! A session's profit is the change in the strategy's realised profit over the session, read by
//! replaying the ledger, so the sessions of a strategy add up to the ledger's own total.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::{BTreeMap, BTreeSet};

use tf_core::{InstrumentId, Nanos};
use tf_ledger::{Input, Journal, JournalError, LedgerStore, Record};
use tf_manifest::RunResult;
use tf_strategy::Purpose;
use tf_strategy::intent::Side;
use tf_strategy::lifecycle::Decision;

/// What kind of run it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Backtest,
    Paper,
    Live,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Backtest => "backtest",
            Kind::Paper => "paper",
            Kind::Live => "live",
        }
    }

    /// `paper` or `live`, the kinds a ledger can be.
    pub fn parse_session(s: &str) -> Option<Kind> {
        match s {
            "paper" => Some(Kind::Paper),
            "live" => Some(Kind::Live),
            _ => None,
        }
    }
}

/// Where a run came from, and so whether it can be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A stored backtest: `tf explore HASH` opens it.
    Stored { hash: String },
    /// Day number `session` (from 1) of the ledger named `ledger`.
    Ledger { ledger: String, session: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub strategy: String,
    pub kind: Kind,
    /// Event time the run began (a backtest's first data, a session's first record).
    pub started: Nanos,
    /// Net profit in 1e-9 dollars, if it is known.
    pub net_pnl: Option<i128>,
    pub trades: Option<u64>,
    /// `built-in`, a rule set's fingerprint, or nothing for a strategy without rules.
    pub rules: Option<String>,
    /// The strategy's budget at the time, raw units, for sessions that had one.
    pub budget: Option<u128>,
    pub source: Source,
}

impl Run {
    /// Whether the explorer can open it.
    pub fn explorable(&self) -> bool {
        matches!(self.source, Source::Stored { .. })
    }
}

/// All the runs known, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    runs: Vec<Run>,
}

impl Catalog {
    pub fn new(mut runs: Vec<Run>) -> Catalog {
        // Newest first; ties fall back to the strategy, kind and source so the order is stable.
        runs.sort_by(|a, b| {
            b.started
                .cmp(&a.started)
                .then_with(|| a.strategy.cmp(&b.strategy))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| key(&b.source).cmp(&key(&a.source)))
        });
        Catalog { runs }
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// The strategies with at least one run, in name order.
    pub fn strategies(&self) -> Vec<&str> {
        let set: BTreeSet<&str> = self.runs.iter().map(|r| r.strategy.as_str()).collect();
        set.into_iter().collect()
    }

    /// A strategy's runs, newest first.
    pub fn of<'a>(&'a self, strategy: &'a str) -> impl Iterator<Item = &'a Run> {
        self.runs.iter().filter(move |r| r.strategy == strategy)
    }

    /// A strategy's newest run, which is what opening the strategy shows.
    pub fn latest<'a>(&'a self, strategy: &'a str) -> Option<&'a Run> {
        self.of(strategy).next()
    }
}

fn key(s: &Source) -> (String, u32) {
    match s {
        Source::Stored { hash } => (hash.clone(), 0),
        Source::Ledger { ledger, session } => (ledger.clone(), *session),
    }
}

/// `YYYY-MM-DD HH:MM` (UTC) for event time in nanoseconds since the epoch.
pub fn when(ns: u64) -> String {
    let secs = ns / 1_000_000_000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a calendar date (Howard Hinnant's civil_from_days).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

/// The backtest run a stored result is.
pub fn run_of(r: &RunResult) -> Run {
    let m = r.manifest();
    let c = m.config();
    let strategy = c.get("strategy").cloned().unwrap_or_else(|| "-".to_owned());
    let rules = match (c.get("rules"), strategy.as_str()) {
        (Some(id), _) => Some(id.clone()),
        (None, "momentum") => Some("built-in".to_owned()),
        _ => None,
    };
    Run {
        net_pnl: r.metric("pnl_net").map(i128::from),
        trades: r.metric("trades").and_then(|t| u64::try_from(t).ok()),
        rules,
        budget: None,
        started: m.data().from,
        source: Source::Stored {
            hash: r.key().hex(),
        },
        kind: Kind::Backtest,
        strategy,
    }
}

/// The catalog of stored backtests.
pub fn from_results(results: &[RunResult]) -> Catalog {
    Catalog::new(results.iter().map(run_of).collect())
}

/// One fill in a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillLine {
    pub ts: Nanos,
    pub instrument: InstrumentId,
    pub side: Side,
    pub purpose: Purpose,
    pub qty: u32,
    /// Raw price (1e-9 dollars).
    pub px: i64,
    /// Realised profit this fill made (zero for a fill that opens), raw units.
    pub pnl: i128,
}

/// What a ledger session did beyond its summary: every fill in order, and why the gateway refused
/// what it refused (by reason, with how many times).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detail {
    pub fills: Vec<FillLine>,
    pub refused: Vec<(String, u64)>,
}

/// One strategy's activity within one session while it is being read.
#[derive(Default)]
struct Acc {
    fills: Vec<FillLine>,
    refused: BTreeMap<String, u64>,
    /// Realised profit when the session began.
    base: i128,
    /// Orders that opened a position and got at least one fill.
    opened: BTreeSet<u64>,
    budget: Option<u128>,
}

/// The sessions in a ledger, one per strategy that was active in each trading day. `name` identifies
/// the ledger in each run's source. A strategy is named by the budget tree if there is one, else
/// `s<number>`.
pub fn sessions<S: LedgerStore>(
    store: S,
    name: &str,
    kind: Kind,
) -> Result<Vec<Run>, JournalError> {
    Ok(sessions_detailed(store, name, kind)?
        .into_iter()
        .map(|(r, _)| r)
        .collect())
}

/// [`sessions`], each with what it did.
pub fn sessions_detailed<S: LedgerStore>(
    store: S,
    name: &str,
    kind: Kind,
) -> Result<Vec<(Run, Detail)>, JournalError> {
    let mut out: Vec<(Run, Detail)> = Vec::new();
    let mut day = 1u32;
    let mut first: Option<Nanos> = None;
    let mut acc: BTreeMap<u16, Acc> = BTreeMap::new();
    let mut names: BTreeMap<u16, String> = BTreeMap::new();
    // Each strategy's realised profit after the previous record: only fills move it, and the observer
    // runs after a record is applied, so this is the profit a strategy had when a session found it.
    let mut last: BTreeMap<u16, i128> = BTreeMap::new();

    let finish = |j: &Journal<S>,
                  day: u32,
                  first: Option<Nanos>,
                  acc: &mut BTreeMap<u16, Acc>,
                  names: &BTreeMap<u16, String>,
                  out: &mut Vec<(Run, Detail)>| {
        for (n, a) in std::mem::take(acc) {
            let detail = Detail {
                fills: a.fills,
                refused: a.refused.into_iter().collect(),
            };
            let run = Run {
                strategy: names.get(&n).cloned().unwrap_or_else(|| format!("s{n}")),
                kind,
                started: first.unwrap_or(0),
                net_pnl: Some(j.gateway().strategy_realized(n) - a.base),
                trades: Some(a.opened.len() as u64),
                rules: None,
                budget: a.budget,
                source: Source::Ledger {
                    ledger: name.to_owned(),
                    session: day,
                },
            };
            out.push((run, detail));
        }
    };

    let (journal, _) = Journal::open_recorded_observed(store, &mut |j, rec| {
        let Record::Event { input, outcome } = rec else {
            return;
        };
        if let Some(b) = j.gateway().budgets() {
            for (n, id) in b.ids() {
                names.insert(*n, id.clone());
            }
        }
        match input {
            Input::NewDay { .. } => {
                // The day's records are all in: close it. (The profit before this record is the
                // day's, since a new day only moves baselines.)
                finish(j, day, first.take(), &mut acc, &names, &mut out);
                day += 1;
            }
            Input::Decide { intent, now } => {
                first.get_or_insert(*now);
                let n = intent.id.strategy.0;
                let a = acc.entry(n).or_insert_with(|| Acc {
                    base: last.get(&n).copied().unwrap_or(0),
                    ..Acc::default()
                });
                a.budget = j.gateway().budgets().and_then(|b| b.strategy_budget(n));
                if let Some(Decision::Rejected(why)) = outcome {
                    *a.refused
                        .entry(tf_risk::reason_name(why).to_owned())
                        .or_insert(0) += 1;
                }
            }
            Input::Fill { order, ts, qty, px } => {
                first.get_or_insert(*ts);
                if let Some(o) = j.order(*order) {
                    let n = o.intent.id.strategy.0;
                    let a = acc.entry(n).or_insert_with(|| Acc {
                        base: last.get(&n).copied().unwrap_or(0),
                        ..Acc::default()
                    });
                    if o.intent.purpose == Purpose::Open {
                        a.opened.insert(order.0);
                    }
                    let now = j.gateway().strategy_realized(n);
                    a.fills.push(FillLine {
                        ts: *ts,
                        instrument: o.intent.instrument,
                        side: o.intent.side,
                        purpose: o.intent.purpose,
                        qty: *qty,
                        px: *px,
                        pnl: now - last.get(&n).copied().unwrap_or(0),
                    });
                    last.insert(n, now);
                }
            }
            _ => {}
        }
    })?;
    finish(&journal, day, first.take(), &mut acc, &names, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests;
