//! Submitting, approving and declining proposals against a real ledger.
//!
//! The ledger is read without its lock (a running engine may hold it) and never written: whatever is
//! to take effect goes into the ledger's inbox ([`tf_ledger::inbox`]), to be recorded as a scheduled
//! change for the next rebalance, exactly as a person's edit in the app would be.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use tf_budget::{Tree, Usage, check_edit, diff};
use tf_catalog::Kind;
use tf_core::Nanos;
use tf_ledger::{Journal, ReadOnlyStore, inbox};

use crate::store::{self, Call, Decision, Entry, Proposal, State, Status, StoreError};
use crate::{Facts, Policy, Verdict, decide};

#[derive(Debug, PartialEq, Eq)]
pub enum FlowError {
    /// The ledger cannot be read, or has no budgets in force.
    Ledger(String),
    /// The text is not a budget tree.
    Unreadable(String),
    /// No such proposal.
    NotFound(u64),
    /// The proposal is not waiting for a person.
    NotWaiting(u64, State),
    /// What was asked cannot be done, with the reason.
    Refused(String),
    Store(String),
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowError::Ledger(m)
            | FlowError::Unreadable(m)
            | FlowError::Refused(m)
            | FlowError::Store(m) => f.write_str(m),
            FlowError::NotFound(id) => write!(f, "no proposal {id}"),
            FlowError::NotWaiting(id, s) => {
                write!(f, "proposal {id} is not waiting for a person (it is {s:?})")
            }
        }
    }
}

impl From<StoreError> for FlowError {
    fn from(e: StoreError) -> FlowError {
        FlowError::Store(e.to_string())
    }
}

/// The ledger's budgets as they are, with what the policy needs to know about them.
struct Now {
    tree: Tree,
    balance: u128,
    usage: Usage,
    latched: BTreeSet<String>,
}

fn now(ledger: &Path) -> Result<Now, FlowError> {
    let (j, _) = Journal::open_recorded(ReadOnlyStore::open(ledger))
        .map_err(|e| FlowError::Ledger(e.to_string()))?;
    let gw = j.gateway();
    let b = gw
        .budgets()
        .ok_or_else(|| FlowError::Ledger("this account has no budgets in force".to_owned()))?;
    let snap = j.snapshot();
    let mut usage = Usage::new();
    let mut latched = BTreeSet::new();
    for (n, id) in b.ids() {
        usage = usage.with(id, gw.strategy_charge(*n));
        if snap.soft_latched.contains(n) || snap.hard_latched.contains(n) {
            latched.insert(id.clone());
        }
    }
    Ok(Now {
        tree: b.tree().clone(),
        balance: b.balance(),
        usage,
        latched,
    })
}

/// Strategies in drawdown: stopped by a loss limit, or down over their last three sessions.
pub fn drawdown(ledger: &Path) -> Result<BTreeSet<String>, FlowError> {
    let mut out = now(ledger)?.latched;
    let sessions = tf_catalog::sessions(ReadOnlyStore::open(ledger), "ledger", Kind::Paper)
        .map_err(|e| FlowError::Ledger(e.to_string()))?;
    let mut recent: BTreeMap<&str, Vec<(Nanos, i128)>> = BTreeMap::new();
    for r in &sessions {
        recent
            .entry(&r.strategy)
            .or_default()
            .push((r.started, r.net_pnl.unwrap_or(0)));
    }
    for (name, mut runs) in recent {
        runs.sort_by_key(|r| std::cmp::Reverse(r.0));
        if runs.iter().take(3).map(|r| r.1).sum::<i128>() < 0 {
            out.insert(name.to_owned());
        }
    }
    Ok(out)
}

/// When each node was last changed by a proposal that took effect (applied, or approved).
fn last_changes(entries: &[Entry]) -> BTreeMap<String, Nanos> {
    let mut out: BTreeMap<String, Nanos> = BTreeMap::new();
    for e in entries {
        let when = match e.state() {
            State::Scheduled => e.proposal.at,
            State::Approved => e.decision.as_ref().map_or(e.proposal.at, |d| d.at),
            _ => continue,
        };
        for n in &e.proposal.nodes {
            let slot = out.entry(n.clone()).or_insert(0);
            *slot = (*slot).max(when);
        }
    }
    out
}

fn facts(ledger: &Path, at: Nanos) -> Result<Facts, FlowError> {
    let (entries, _) = store::list(ledger)?;
    Ok(Facts {
        last_change: last_changes(&entries),
        drawdown: drawdown(ledger)?,
        now: at,
    })
}

/// What happened to a submitted proposal.
#[derive(Debug, PartialEq, Eq)]
pub struct Submitted {
    pub id: u64,
    pub status: Status,
    pub why: Vec<String>,
}

/// An agent's proposal. `tree_text` is the budgets text form; `at` is the time it is made.
pub fn submit(
    ledger: &Path,
    policy: &Policy,
    by: &str,
    reason: &str,
    evidence: &str,
    tree_text: &str,
    at: Nanos,
) -> Result<Submitted, FlowError> {
    let wanted = Tree::parse(tree_text).map_err(|e| FlowError::Unreadable(e.to_string()))?;
    let now = now(ledger)?;
    let verdict = decide(
        policy,
        &now.tree,
        &wanted,
        now.balance,
        &now.usage,
        &facts(ledger, at)?,
    );
    let nodes: Vec<String> = changed_nodes(&now.tree, &wanted);
    let (status, mut why) = match &verdict {
        Verdict::Apply => (
            Status::Auto,
            vec!["reduces risk within bounds: scheduled without a person".to_owned()],
        ),
        Verdict::NeedsPerson(w) => (Status::Pending, w.clone()),
        Verdict::Refused(w) => (Status::Refused, vec![w.clone()]),
    };
    if status == Status::Auto {
        let name = inbox::submit(
            ledger,
            &format!("{by} (within bounds, no approval needed)"),
            Some(&wanted),
        )
        .map_err(|e| FlowError::Store(e.to_string()))?;
        why.push(format!("queued as {name}"));
    }
    let id = store::save_proposal(
        ledger,
        &Proposal {
            id: 0,
            by: by.to_owned(),
            at,
            status,
            policy: *policy,
            nodes,
            reason: reason.to_owned(),
            evidence: evidence.to_owned(),
            why: why.clone(),
            tree: wanted,
        },
    )?;
    Ok(Submitted { id, status, why })
}

fn changed_nodes(a: &Tree, b: &Tree) -> Vec<String> {
    use tf_budget::Change::*;
    let mut v: Vec<String> = diff(a, b)
        .into_iter()
        .map(|c| match c {
            GroupAdded(id)
            | GroupRemoved(id)
            | GroupShare { id, .. }
            | Loss { id, .. }
            | StrategyAdded { id, .. }
            | StrategyRemoved { id, .. }
            | StrategyShare { id, .. } => id,
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

fn waiting(ledger: &Path, id: u64) -> Result<Entry, FlowError> {
    let (entries, _) = store::list(ledger)?;
    let e = entries
        .into_iter()
        .find(|e| e.proposal.id == id)
        .ok_or(FlowError::NotFound(id))?;
    match e.state() {
        State::Waiting => Ok(e),
        s => Err(FlowError::NotWaiting(id, s)),
    }
}

/// A person approves a waiting proposal: it is checked again against the budgets and drawdown as
/// they are now, queued as a scheduled change, and the decision kept. A proposal whose increase has
/// since run into a drawdown cannot be approved.
pub fn approve(
    ledger: &Path,
    id: u64,
    by: &str,
    note: &str,
    at: Nanos,
) -> Result<String, FlowError> {
    let e = waiting(ledger, id)?;
    let now = now(ledger)?;
    // The agent's cooldown and step do not bind a person; the rules and drawdown still do.
    let open = Policy {
        step: u32::MAX,
        cooldown: 0,
    };
    match decide(
        &open,
        &now.tree,
        &e.proposal.tree,
        now.balance,
        &now.usage,
        &facts(ledger, at)?,
    ) {
        Verdict::Refused(why) => return Err(FlowError::Refused(why)),
        Verdict::Apply | Verdict::NeedsPerson(_) => {}
    }
    check_edit(&now.tree, &e.proposal.tree, now.balance, &now.usage)
        .map_err(|e| FlowError::Refused(e.to_string()))?;
    let dec = Decision {
        id,
        by: by.to_owned(),
        at,
        call: Call::Approved,
        note: note.to_owned(),
    };
    store::save_decision(ledger, &dec)?;
    let who = format!("{by}, approving a proposal by {}", e.proposal.by);
    match inbox::submit(ledger, &who, Some(&e.proposal.tree)) {
        Ok(name) => Ok(name),
        Err(err) => {
            let _ = store::forget_decision(ledger, id);
            Err(FlowError::Store(err.to_string()))
        }
    }
}

/// A person declines a waiting proposal.
pub fn decline(ledger: &Path, id: u64, by: &str, note: &str, at: Nanos) -> Result<(), FlowError> {
    waiting(ledger, id)?;
    store::save_decision(
        ledger,
        &Decision {
            id,
            by: by.to_owned(),
            at,
            call: Call::Declined,
            note: note.to_owned(),
        },
    )?;
    Ok(())
}
