//! Budget proposals from agents.
//!
//! An agent may propose a new budget tree, with a reason and the evidence for it. What happens is
//! asymmetric, because the two directions are not equally dangerous:
//!
//! - a change that only **reduces** risk (a share cut, loss limits tightened), small enough and not
//!   soon after the last change to the same node, **applies on its own**: it is scheduled for the next
//!   rebalance with no person involved, and recorded;
//! - anything that **increases** risk (a share raised, loss limits loosened) **needs a person**;
//! - an increase for a strategy (or a group holding one) that is **in drawdown** is **refused**, even
//!   for a person to approve: wait until it is not;
//! - a proposal the rules do not allow at all (children over the parent, a strategy cut below what it
//!   has in use, a different set of groups) is refused.
//!
//! Nothing here moves money or touches the ledger: "applies on its own" means "is put in the
//! ledger's inbox as a scheduled change" (ADR 0035), the same path a person's edit takes.
//!
//! - [`decide`]: the policy, a pure function of the trees and a few facts.
//! - [`store`]: the proposals and decisions kept as files beside the ledger.
//! - [`flow`]: submitting, approving and declining against a real ledger.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::{BTreeMap, BTreeSet};

use tf_budget::{Bp, Change, Tree, Usage, check_edit, diff};
use tf_core::Nanos;

pub mod flow;
pub mod store;

/// How much an agent may do on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The most any one share may be cut by in one proposal, in basis points of its parent.
    pub step: Bp,
    /// How long after a change to a node (by anyone) before an agent may cut it again.
    pub cooldown: Nanos,
}

impl Default for Policy {
    /// Ten points, once a day.
    fn default() -> Policy {
        Policy {
            step: 1_000,
            cooldown: 86_400 * 1_000_000_000,
        }
    }
}

/// What the policy needs to know besides the two trees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// When each node (group or strategy id) was last changed by an applied or approved proposal.
    pub last_change: BTreeMap<String, Nanos>,
    /// Strategies in drawdown.
    pub drawdown: BTreeSet<String>,
    pub now: Nanos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Reduces risk within bounds: schedule it without a person.
    Apply,
    /// Needs a person to approve; says why, one reason per line.
    NeedsPerson(Vec<String>),
    /// Not to be done, by an agent or a person.
    Refused(String),
}

/// Whether a change adds risk, removes it, and which node it is about.
fn classify(c: &Change) -> (Option<bool>, String, Bp) {
    // (Some(true) = increases risk, Some(false) = reduces it, None = not a risk change), node, size
    match c {
        Change::GroupShare { id, from, to } => (Some(to > from), id.clone(), from.abs_diff(*to)),
        Change::StrategyShare { id, from, to, .. } => {
            (Some(to > from), id.clone(), from.abs_diff(*to))
        }
        Change::Loss { id, from, to } => {
            let looser = to.soft > from.soft || to.hard > from.hard;
            let size = from.soft.abs_diff(to.soft).max(from.hard.abs_diff(to.hard));
            (Some(looser), id.clone(), size)
        }
        Change::GroupAdded(id)
        | Change::GroupRemoved(id)
        | Change::StrategyAdded { id, .. }
        | Change::StrategyRemoved { id, .. } => (None, id.clone(), 0),
    }
}

/// The strategies a node stands for: itself, or the strategies of the group it names.
fn strategies_of<'a>(t: &'a Tree, node: &'a str) -> Vec<&'a str> {
    match t.group(node) {
        Some(g) => g.strategies.iter().map(|s| s.id.as_str()).collect(),
        None => vec![node],
    }
}

/// What to do with a proposal to change `current` into `wanted`.
pub fn decide(
    policy: &Policy,
    current: &Tree,
    wanted: &Tree,
    balance: u128,
    usage: &Usage,
    facts: &Facts,
) -> Verdict {
    if let Err(e) = check_edit(current, wanted, balance, usage) {
        return Verdict::Refused(e.to_string());
    }
    let changes = diff(current, wanted);
    if changes.is_empty() {
        return Verdict::Refused("it changes nothing".to_owned());
    }
    let classified: Vec<_> = changes.iter().map(classify).collect();
    // An increase for anything in drawdown is refused outright.
    for (kind, node, _) in &classified {
        if *kind == Some(true) {
            if let Some(s) = strategies_of(wanted, node)
                .into_iter()
                .find(|s| facts.drawdown.contains(*s))
            {
                return Verdict::Refused(format!(
                    "{s} is in drawdown: no increase for {node} until it recovers"
                ));
            }
        }
    }
    let mut why = Vec::new();
    for (kind, node, size) in &classified {
        match kind {
            Some(true) => why.push(format!("{node}: an increase needs a person")),
            Some(false) => {
                if *size > policy.step {
                    why.push(format!(
                        "{node}: cut by {size} basis points, more than the {} an agent may cut at once",
                        policy.step
                    ));
                }
                if let Some(&last) = facts.last_change.get(node) {
                    if facts.now.saturating_sub(last) < policy.cooldown {
                        why.push(format!(
                            "{node}: changed too recently for an agent to cut it again"
                        ));
                    }
                }
            }
            None => {}
        }
    }
    if why.is_empty() {
        Verdict::Apply
    } else {
        Verdict::NeedsPerson(why)
    }
}

#[cfg(test)]
mod tests;
