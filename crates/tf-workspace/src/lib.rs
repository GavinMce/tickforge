//! The workspace service: what the workspace app shows, read from the ledger and the run store.
//!
//! Everything here reads. The ledger is opened without its lock and without repair (it may be
//! open in a running engine), the run store is only listed, and no route does anything but answer:
//! there is no code path from a request to an order, a budget change or a file write.
//!
//! - [`overview`]: the balance, groups and strategies with budget, use, day P&L and state.
//! - [`runs`]: every run, newest first.
//! - [`http`]: the routes, sign-in, and a small server.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::fmt::Write as _;
use std::path::PathBuf;

use tf_catalog::{Catalog, Kind, Run, Source as RunSource, when};
use tf_ledger::{Journal, ReadOnlyStore};

pub mod http;

/// Where the service reads from.
#[derive(Clone, Debug)]
pub struct Source {
    /// The order ledger of the paper or live account, and which of the two it is.
    pub ledger: Option<(PathBuf, Kind)>,
    /// The run store of backtests.
    pub store: Option<PathBuf>,
}

/// Dollars to the cent from raw 1e-9 dollars, as text so no precision is lost in JSON.
pub fn dollars(raw: i128) -> String {
    let cents = (raw.abs() + 5_000_000) / 10_000_000;
    format!(
        "{}{}.{:02}",
        if raw < 0 && cents > 0 { "-" } else { "" },
        cents / 100,
        cents % 100
    )
}

fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn opt(s: Option<String>) -> String {
    s.map_or("null".to_owned(), |s| js(&s))
}

/// One run as JSON. Times are text because event time in nanoseconds does not fit a JSON number.
pub fn run_json(r: &Run) -> String {
    let (kind_src, id) = match &r.source {
        RunSource::Stored { hash } => ("stored", hash.clone()),
        RunSource::Ledger { ledger, session } => ("ledger", format!("{ledger}#{session}")),
    };
    format!(
        "{{\"strategy\":{},\"kind\":{},\"started_ns\":{},\"started\":{},\"net_pnl\":{},\"trades\":{},\"rules\":{},\"budget\":{},\"source\":{},\"id\":{},\"explorable\":{}}}",
        js(&r.strategy),
        js(r.kind.name()),
        js(&r.started.to_string()),
        js(&when(r.started)),
        opt(r.net_pnl.map(dollars)),
        r.trades.map_or("null".to_owned(), |t| t.to_string()),
        opt(r.rules.clone()),
        opt(r.budget.map(|b| dollars(b as i128))),
        js(kind_src),
        js(&id),
        r.explorable()
    )
}

/// Every run the service knows, newest first.
pub fn catalog(src: &Source) -> Result<Catalog, String> {
    let mut runs = Vec::new();
    if let Some(dir) = &src.store {
        let (found, _) = tf_manifest::DirStore::new(dir)
            .list()
            .map_err(|e| e.to_string())?;
        runs.extend(tf_catalog::from_results(&found).runs().iter().cloned());
    }
    if let Some((dir, kind)) = &src.ledger {
        let name = dir.display().to_string();
        runs.extend(
            tf_catalog::sessions(ReadOnlyStore::open(dir), &name, *kind)
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(Catalog::new(runs))
}

/// The runs as JSON, for one strategy or all.
pub fn runs(src: &Source, strategy: Option<&str>) -> Result<String, String> {
    let cat = catalog(src)?;
    let list: Vec<String> = cat
        .runs()
        .iter()
        .filter(|r| strategy.is_none_or(|s| r.strategy == s))
        .map(run_json)
        .collect();
    Ok(format!("{{\"runs\":[{}]}}", list.join(",")))
}

/// The balance, groups and strategies as JSON: budget, use, day P&L, loss limits, state and runs.
pub fn overview(src: &Source) -> Result<String, String> {
    let cat = catalog(src)?;
    let Some((dir, kind)) = &src.ledger else {
        return Ok("{\"account\":null,\"groups\":[],\"strategies\":[]}".to_owned());
    };
    let (j, rec) = Journal::open_recorded(ReadOnlyStore::open(dir)).map_err(|e| e.to_string())?;
    let gw = j.gateway();
    let snap = j.snapshot();
    let base = |n: u16| {
        snap.strategy_day_base
            .iter()
            .find(|(s, _)| *s == n)
            .map_or(0, |(_, b)| *b)
    };
    let day_pnl = |n: u16| gw.strategy_pnl(n) - base(n);
    let mut head = format!(
        "\"account\":{{\"kind\":{},\"records\":{},\"killed\":{},\"scheduled_change\":{}",
        js(kind.name()),
        rec.records,
        snap.killed,
        j.scheduled().is_some()
    );
    let Some(b) = gw.budgets() else {
        head.push_str(",\"budgets\":false}");
        return Ok(format!("{{{head},\"groups\":[],\"strategies\":[]}}"));
    };
    let _ = write!(
        head,
        ",\"budgets\":true,\"balance\":{}",
        js(&dollars(b.balance() as i128))
    );
    let mut total_used = 0u128;
    let mut total_pnl = 0i128;
    let mut groups = Vec::new();
    let mut strategies = Vec::new();
    for g in b.tree().groups() {
        let members: Vec<(u16, &String)> = b
            .ids()
            .iter()
            .filter(|(_, id)| g.strategies.iter().any(|s| &s.id == *id))
            .map(|(n, id)| (*n, id))
            .collect();
        let used = gw.group_charge(&g.id);
        let pnl: i128 = members.iter().map(|(n, _)| day_pnl(*n)).sum();
        total_used += used;
        total_pnl += pnl;
        groups.push(format!(
            "{{\"id\":{},\"share_bp\":{},\"budget\":{},\"used\":{},\"day_pnl\":{},\"loss_soft_bp\":{},\"loss_hard_bp\":{},\"strategies\":{}}}",
            js(&g.id),
            g.share,
            js(&dollars(b.tree().group_budget(b.balance(), &g.id).unwrap_or(0) as i128)),
            js(&dollars(used as i128)),
            js(&dollars(pnl)),
            g.loss.soft,
            g.loss.hard,
            g.strategies.len()
        ));
        for s in &g.strategies {
            let Some((n, _)) = members.iter().find(|(_, id)| **id == s.id) else {
                continue;
            };
            let n = *n;
            let state = if snap.killed {
                "killed"
            } else if snap.hard_latched.contains(&n) {
                "flatten"
            } else if snap.soft_latched.contains(&n) {
                "no new entries"
            } else {
                "active"
            };
            let (soft, hard) = gw.strategy_limits(n).unwrap_or((0, 0));
            let mine: Vec<&Run> = cat.of(&s.id).collect();
            strategies.push(format!(
                "{{\"name\":{},\"number\":{},\"group\":{},\"share_bp\":{},\"budget\":{},\"used\":{},\"day_pnl\":{},\"loss_soft\":{},\"loss_hard\":{},\"state\":{},\"runs\":{},\"latest_run\":{}}}",
                js(&s.id),
                n,
                js(&g.id),
                s.share,
                js(&dollars(b.strategy_budget(n).unwrap_or(0) as i128)),
                js(&dollars(gw.strategy_charge(n) as i128)),
                js(&dollars(day_pnl(n))),
                js(&dollars(soft as i128)),
                js(&dollars(hard as i128)),
                js(state),
                mine.len(),
                mine.first().map_or("null".to_owned(), |r| run_json(r))
            ));
        }
    }
    let _ = write!(
        head,
        ",\"used\":{},\"day_pnl\":{}}}",
        js(&dollars(total_used as i128)),
        js(&dollars(total_pnl))
    );
    Ok(format!(
        "{{{head},\"groups\":[{}],\"strategies\":[{}]}}",
        groups.join(","),
        strategies.join(",")
    ))
}

#[cfg(test)]
mod tests;
