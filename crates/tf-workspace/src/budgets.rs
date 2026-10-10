//! Editing the budgets from the app.
//!
//! The app never changes the ledger. A person edits a draft; [`view`] says whether the draft is
//! allowed (by [`tf_budget::check_edit`], the one place the rules live), what each share may be set to
//! and why, and what the changes come to in dollars. [`request`] puts an allowed draft in the
//! ledger's inbox ([`tf_ledger::inbox`]), where the engine takes it up and records it as a scheduled
//! change for the next rebalance; [`withdraw`] asks to cancel what is scheduled. Both check the
//! draft again themselves, so a client cannot skip the preview.

use std::fmt::Write as _;

use tf_budget::{Range, Tree, Usage, Why, check_edit, diff};
use tf_ledger::{Journal, ReadOnlyStore, inbox};

use crate::{Source, change_text, dollars, js, money};

/// Why a request could not be made.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// There is nothing to edit (no ledger, or no budgets in force).
    NothingToEdit(String),
    /// The draft is not allowed, with the reason.
    NotAllowed(String),
    /// The draft changes nothing.
    NoChange,
    /// The ledger or its inbox could not be read or written.
    Failed(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NothingToEdit(m) | Refusal::NotAllowed(m) | Refusal::Failed(m) => {
                f.write_str(m)
            }
            Refusal::NoChange => f.write_str("the draft is the same as the budgets in force"),
        }
    }
}

struct Now {
    tree: Tree,
    balance: u128,
    usage: Usage,
}

fn now(src: &Source) -> Result<(std::path::PathBuf, Now), Refusal> {
    let Some((dir, _)) = &src.live() else {
        return Err(Refusal::NothingToEdit("no ledger is connected".to_owned()));
    };
    let (j, _) = Journal::open_recorded(ReadOnlyStore::open(dir))
        .map_err(|e| Refusal::Failed(e.to_string()))?;
    let gw = j.gateway();
    let Some(b) = gw.budgets() else {
        return Err(Refusal::NothingToEdit(
            "this account has no budgets in force".to_owned(),
        ));
    };
    let mut usage = Usage::new();
    for (n, id) in b.ids() {
        usage = usage.with(id, gw.strategy_charge(*n));
    }
    Ok((
        dir.clone(),
        Now {
            tree: b.tree().clone(),
            balance: b.balance(),
            usage,
        },
    ))
}

/// The draft as a tree the rules allow, or why not. `None` is the budgets as they are.
fn allowed(now: &Now, draft: Option<&str>) -> Result<Tree, String> {
    let Some(text) = draft else {
        return Ok(now.tree.clone());
    };
    let t = Tree::parse(text).map_err(|e| e.to_string())?;
    check_edit(&now.tree, &t, now.balance, &now.usage).map_err(|e| e.to_string())?;
    Ok(t)
}

fn why_text(w: &Why, floor: bool, parent: &str, balance_level: bool) -> String {
    match (w, floor) {
        (Why::Nothing, _) => "nothing is in use, so it can go to zero".to_owned(),
        (Why::Unassigned, _) => format!(
            "what is not yet assigned {}",
            if balance_level {
                "of the balance".to_owned()
            } else {
                format!("in {parent}")
            }
        ),
        (Why::InUse { who, used }, _) => format!("{who} has {} in use", money(*used as i128)),
        (Why::OverBudget, _) => "it is already over its budget, so it cannot be cut".to_owned(),
    }
}

fn range_json(r: &Range, parent: &str, balance_level: bool) -> String {
    format!(
        "{{\"min\":{},\"max\":{},\"min_why\":{},\"max_why\":{}}}",
        r.min.value,
        r.max.value,
        js(&why_text(&r.min.why, true, parent, balance_level)),
        js(&why_text(&r.max.why, false, parent, balance_level)),
    )
}

/// The budgets in force with `draft` laid over them. `valid` says whether the draft is allowed; if
/// it is not, `error` says why and the ranges are those of the budgets as they are. Ranges are always
/// worked out for the tree shown, so a person moving one share sees what the others may now be.
pub fn view(src: &Source, draft: Option<&str>) -> Result<String, Refusal> {
    let (dir, now) = now(src)?;
    let (shown, error) = match allowed(&now, draft) {
        Ok(t) => (t, None),
        Err(e) => (now.tree.clone(), Some(e)),
    };
    let mut groups = Vec::new();
    for g in shown.groups() {
        let mut strategies = Vec::new();
        for s in &g.strategies {
            let range = shown
                .strategy_range(&s.id, now.balance, &now.usage)
                .map_err(|e| Refusal::Failed(e.to_string()))?;
            strategies.push(format!(
                "{{\"id\":{},\"share_bp\":{},\"budget\":{},\"used\":{},\"range\":{}}}",
                js(&s.id),
                s.share,
                js(&money(
                    shown.strategy_budget(now.balance, &s.id).unwrap_or(0) as i128
                )),
                js(&money(now.usage.strategy(&s.id) as i128)),
                range_json(&range, &g.id, false)
            ));
        }
        let range = shown
            .group_range(&g.id, now.balance, &now.usage)
            .map_err(|e| Refusal::Failed(e.to_string()))?;
        groups.push(format!(
            "{{\"id\":{},\"share_bp\":{},\"budget\":{},\"used\":{},\"loss_soft_bp\":{},\"loss_hard_bp\":{},\"range\":{},\"strategies\":[{}]}}",
            js(&g.id),
            g.share,
            js(&money(shown.group_budget(now.balance, &g.id).unwrap_or(0) as i128)),
            js(&money(now.usage.group(g) as i128)),
            g.loss.soft,
            g.loss.hard,
            range_json(&range, "", true),
            strategies.join(",")
        ));
    }
    let changes: Vec<String> = diff(&now.tree, &shown)
        .iter()
        .map(|c| js(&change_text(c, &now.tree, &shown, now.balance)))
        .collect();
    let (waiting, unreadable) = inbox::pending(&dir).map_err(|e| Refusal::Failed(e.to_string()))?;
    let pending: Vec<String> = waiting
        .iter()
        .map(|r| {
            let what = match &r.tree {
                None => vec![js("withdraw the scheduled change")],
                Some(t) => diff(&now.tree, t)
                    .iter()
                    .map(|c| js(&change_text(c, &now.tree, t, now.balance)))
                    .collect(),
            };
            format!(
                "{{\"name\":{},\"by\":{},\"changes\":[{}]}}",
                js(&r.name),
                js(&r.by),
                what.join(",")
            )
        })
        .collect();
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"balance\":{},\"valid\":{},\"error\":{},\"unassigned_bp\":{},\"text\":{},\"groups\":[{}],\"changes\":[{}],\"pending\":[{}],\"unreadable\":{}}}",
        js(&dollars(now.balance as i128)),
        error.is_none(),
        error.as_deref().map_or("null".to_owned(), js),
        shown.unassigned(),
        js(&shown.render()),
        groups.join(","),
        changes.join(","),
        pending.join(","),
        unreadable.len()
    );
    Ok(out)
}

/// Ask for `draft` to be scheduled for the next rebalance. Returns the request's name and what it
/// changes. Not allowed, or no different from now, is refused.
pub fn request(src: &Source, draft: &str, by: &str) -> Result<(String, Vec<String>), Refusal> {
    let (dir, now) = now(src)?;
    let t = allowed(&now, Some(draft)).map_err(Refusal::NotAllowed)?;
    let changes: Vec<String> = diff(&now.tree, &t)
        .iter()
        .map(|c| change_text(c, &now.tree, &t, now.balance))
        .collect();
    if changes.is_empty() {
        return Err(Refusal::NoChange);
    }
    let name = inbox::submit(&dir, by, Some(&t)).map_err(|e| Refusal::Failed(e.to_string()))?;
    Ok((name, changes))
}

/// Ask for what is scheduled to be cancelled.
pub fn withdraw(src: &Source, by: &str) -> Result<String, Refusal> {
    let (dir, _) = now(src)?;
    inbox::submit(&dir, by, None).map_err(|e| Refusal::Failed(e.to_string()))
}
