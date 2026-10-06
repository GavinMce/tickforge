//! The inbox: a person's requested budget changes, waiting for the engine.
//!
//! The ledger has one writer, the engine, and holds a lock while it runs, so nothing else may append
//! to it. A request therefore goes into a directory beside the ledger as one small file, written
//! whole and never changed. The engine (or `tf ledger apply-inbox` when none is running) takes the
//! requests in order, checks each again against the budgets and what is in use at that moment, and
//! records it in the ledger as a scheduled change, which takes effect at the next rebalance.
//!
//! A request is a file `NNNNNNNNNN.req`:
//!
//! ```text
//! tfreq 1
//! by <who asked>
//! schedule            (or: withdraw)
//! budgets v1          (the tree, for `schedule`)
//! group ...
//! ```
//!
//! One that cannot be applied is renamed `.rej` with the reason appended and is not tried again.

use std::fs;
use std::path::{Path, PathBuf};

use tf_budget::{Tree, Usage, check_edit};
use tf_core::Nanos;

use crate::journal::{Journal, JournalError};
use crate::store::LedgerStore;

/// A request that has not been applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The file's name inside the inbox; also the order.
    pub name: String,
    pub by: String,
    /// The tree to schedule, or `None` to withdraw what is scheduled.
    pub tree: Option<Tree>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxError {
    Io(String),
}

impl std::fmt::Display for InboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let InboxError::Io(m) = self;
        write!(f, "inbox: {m}")
    }
}

fn io(p: &Path, e: std::io::Error) -> InboxError {
    InboxError::Io(format!("{}: {e}", p.display()))
}

/// Where a ledger's inbox is.
pub fn dir(ledger: &Path) -> PathBuf {
    ledger.join("inbox")
}

fn text_of(by: &str, tree: Option<&Tree>) -> String {
    let by: String = by
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect::<String>()
        .trim()
        .to_owned();
    let mut s = format!("tfreq 1\nby {by}\n");
    match tree {
        Some(t) => {
            s.push_str("schedule\n");
            s.push_str(&t.render());
        }
        None => s.push_str("withdraw\n"),
    }
    s
}

fn parse(text: &str) -> Result<(String, Option<Tree>), String> {
    let mut lines = text.splitn(4, '\n');
    if lines.next() != Some("tfreq 1") {
        return Err("not a request (no `tfreq 1` first line)".to_owned());
    }
    let by = lines
        .next()
        .and_then(|l| l.strip_prefix("by "))
        .ok_or("no `by` line")?
        .to_owned();
    match lines.next() {
        Some("withdraw") => Ok((by, None)),
        Some("schedule") => {
            let tree = Tree::parse(lines.next().unwrap_or("")).map_err(|e| e.to_string())?;
            Ok((by, Some(tree)))
        }
        other => Err(format!(
            "expected `schedule` or `withdraw`, found {other:?}"
        )),
    }
}

fn number(name: &str) -> Option<u64> {
    let (n, ext) = name.split_once('.')?;
    if !matches!(ext, "req" | "rej") || n.len() != 10 {
        return None;
    }
    n.parse().ok()
}

/// Put a request in the inbox and return its name. `tree` `None` asks to withdraw what is scheduled.
/// The file appears whole or not at all, under a name no earlier request has had.
pub fn submit(ledger: &Path, by: &str, tree: Option<&Tree>) -> Result<String, InboxError> {
    let d = dir(ledger);
    fs::create_dir_all(&d).map_err(|e| io(&d, e))?;
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = d.join(format!("{}-{call}.tmp", std::process::id()));
    fs::write(&tmp, text_of(by, tree)).map_err(|e| io(&tmp, e))?;
    let mut next = fs::read_dir(&d)
        .map_err(|e| io(&d, e))?
        .flatten()
        .filter_map(|f| number(&f.file_name().to_string_lossy()))
        .max()
        .map_or(1, |n| n + 1);
    loop {
        let name = format!("{next:010}.req");
        match fs::hard_link(&tmp, d.join(&name)) {
            Ok(()) => {
                let _ = fs::remove_file(&tmp);
                return Ok(name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => next += 1,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(io(&d, e));
            }
        }
    }
}

/// The requests waiting and the unreadable files: see [`pending`].
pub type Waiting = (Vec<Request>, Vec<(String, String)>);

/// The requests waiting, oldest first, and the files in the inbox that could not be read as one
/// (named, with why). A missing inbox is empty.
pub fn pending(ledger: &Path) -> Result<Waiting, InboxError> {
    let d = dir(ledger);
    let entries = match fs::read_dir(&d) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], vec![])),
        Err(e) => return Err(io(&d, e)),
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".req"))
        .collect();
    names.sort();
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for name in names {
        let read = fs::read_to_string(d.join(&name)).map_err(|e| e.to_string());
        match read.and_then(|t| parse(&t)) {
            Ok((by, tree)) => ok.push(Request { name, by, tree }),
            Err(why) => bad.push((name, why)),
        }
    }
    Ok((ok, bad))
}

/// Finish with a request: applied (it is removed) or refused (it is kept as `.rej` with the reason).
pub fn settle(ledger: &Path, name: &str, outcome: Result<(), String>) -> Result<(), InboxError> {
    let d = dir(ledger);
    let from = d.join(name);
    match outcome {
        Ok(()) => fs::remove_file(&from).map_err(|e| io(&from, e)),
        Err(why) => {
            let rej = d.join(name.replace(".req", ".rej"));
            let mut text = fs::read_to_string(&from).unwrap_or_default();
            text.push_str(&format!("\nrejected: {}\n", why.replace('\n', " ")));
            fs::write(&rej, text).map_err(|e| io(&rej, e))?;
            fs::remove_file(&from).map_err(|e| io(&from, e))
        }
    }
}

/// What applying the inbox did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Requests recorded in the ledger.
    pub scheduled: Vec<String>,
    /// Requests refused, with why. Unreadable files are listed here too and left where they are.
    pub refused: Vec<(String, String)>,
}

/// Apply every waiting request to `journal`, in order, at event time `ts`. Each is checked again
/// against the budgets in force and what each strategy has in use now (so a request that was fine
/// when it was made but is not now is refused, not forced through). A ledger that cannot be written
/// stops the run with the error, leaving the rest waiting.
pub fn apply<S: LedgerStore>(
    journal: &mut Journal<S>,
    ledger: &Path,
    ts: Nanos,
) -> Result<Applied, ApplyError> {
    let (waiting, unreadable) = pending(ledger).map_err(ApplyError::Inbox)?;
    let mut out = Applied {
        refused: unreadable,
        ..Applied::default()
    };
    for r in waiting {
        let verdict = match &r.tree {
            None => Ok(()),
            Some(want) => match journal.gateway().budgets() {
                None => Err("no budgets are in force to change".to_owned()),
                Some(b) => {
                    let mut usage = Usage::new();
                    for (n, id) in b.ids() {
                        usage = usage.with(id, journal.gateway().strategy_charge(*n));
                    }
                    check_edit(b.tree(), want, b.balance(), &usage).map_err(|e| e.to_string())
                }
            },
        };
        match verdict {
            Ok(()) => {
                journal
                    .schedule_budgets(r.tree.clone(), ts)
                    .map_err(ApplyError::Journal)?;
                settle(ledger, &r.name, Ok(())).map_err(ApplyError::Inbox)?;
                out.scheduled.push(r.name);
            }
            Err(why) => {
                settle(ledger, &r.name, Err(why.clone())).map_err(ApplyError::Inbox)?;
                out.refused.push((r.name, why));
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
pub enum ApplyError {
    Inbox(InboxError),
    Journal(JournalError),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Inbox(e) => e.fmt(f),
            ApplyError::Journal(e) => e.fmt(f),
        }
    }
}
