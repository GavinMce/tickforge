//! The agents' budget proposals, as the app shows them, and a person's answer to them.
//!
//! Reading lists what is in `proposals/` beside the ledger. Approving or declining goes through
//! [`tf_proposals::flow`]: an approval is checked again against the budgets and drawdown as they are
//! now and is queued in the ledger's inbox like any other edit; nothing here writes the ledger.

use std::fmt::Write as _;

use tf_budget::diff;
use tf_ledger::{Journal, ReadOnlyStore};
use tf_proposals::flow::{self, FlowError};
use tf_proposals::store::{self, Call, State};

use crate::{Source, change_text, js};

/// Why an answer could not be given.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// There is no ledger to answer against.
    NoLedger,
    NotFound(String),
    /// Already decided, or not something a person decides.
    NotWaiting(String),
    /// The rules or the drawdown no longer allow it.
    NotAllowed(String),
    Failed(String),
}

fn wall_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn state_name(s: State) -> &'static str {
    match s {
        State::Scheduled => "scheduled",
        State::Waiting => "waiting",
        State::Approved => "approved",
        State::Declined => "declined",
        State::Refused => "refused",
    }
}

/// The proposals, newest first, each with what it changes in dollars of the budgets in force.
pub fn view(src: &Source) -> Result<String, Refusal> {
    let Some((dir, _)) = &src.ledger else {
        return Err(Refusal::NoLedger);
    };
    let (entries, unreadable) = store::list(dir).map_err(|e| Refusal::Failed(e.to_string()))?;
    let in_force = Journal::open_recorded(ReadOnlyStore::open(dir))
        .ok()
        .and_then(|(j, _)| {
            j.gateway()
                .budgets()
                .map(|b| (b.tree().clone(), b.balance()))
        });
    let mut items = Vec::new();
    for e in entries.iter().rev() {
        let p = &e.proposal;
        let changes: Vec<String> = match &in_force {
            Some((tree, balance)) => diff(tree, &p.tree)
                .iter()
                .map(|c| js(&change_text(c, tree, &p.tree, *balance)))
                .collect(),
            None => vec![],
        };
        let decision = e.decision.as_ref().map_or("null".to_owned(), |d| {
            format!(
                "{{\"by\":{},\"call\":{},\"note\":{},\"at\":{}}}",
                js(&d.by),
                js(if d.call == Call::Approved {
                    "approved"
                } else {
                    "declined"
                }),
                js(&d.note),
                js(&tf_catalog::when(d.at))
            )
        });
        let mut o = String::new();
        let _ = write!(
            o,
            "{{\"id\":{},\"by\":{},\"at\":{},\"state\":{},\"reason\":{},\"evidence\":{},\"why\":[{}],\"changes\":[{}],\"decision\":{}}}",
            p.id,
            js(&p.by),
            js(&tf_catalog::when(p.at)),
            js(state_name(e.state())),
            js(&p.reason),
            js(&p.evidence),
            p.why.iter().map(|w| js(w)).collect::<Vec<_>>().join(","),
            changes.join(","),
            decision
        );
        items.push(o);
    }
    Ok(format!(
        "{{\"proposals\":[{}],\"unreadable\":{}}}",
        items.join(","),
        unreadable.len()
    ))
}

fn refusal(e: FlowError) -> Refusal {
    match e {
        FlowError::NotFound(id) => Refusal::NotFound(format!("no proposal {id}")),
        e @ FlowError::NotWaiting(..) => Refusal::NotWaiting(e.to_string()),
        FlowError::Refused(m) => Refusal::NotAllowed(m),
        FlowError::Ledger(m) | FlowError::Unreadable(m) | FlowError::Store(m) => Refusal::Failed(m),
    }
}

/// A person approves a proposal that is waiting. Returns the inbox request it became.
pub fn approve(src: &Source, id: u64, by: &str, note: &str) -> Result<String, Refusal> {
    let Some((dir, _)) = &src.ledger else {
        return Err(Refusal::NoLedger);
    };
    flow::approve(dir, id, by, note, wall_clock()).map_err(refusal)
}

/// A person declines a proposal that is waiting.
pub fn decline(src: &Source, id: u64, by: &str, note: &str) -> Result<(), Refusal> {
    let Some((dir, _)) = &src.ledger else {
        return Err(Refusal::NoLedger);
    };
    flow::decline(dir, id, by, note, wall_clock()).map_err(refusal)
}
