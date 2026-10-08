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

use tf_budget::{Change, Tree, diff};
use tf_catalog::{Catalog, Kind, Run, Source as RunSource, when};
use tf_core::Nanos;
use tf_ledger::{Journal, ReadOnlyStore};
use tf_strategy::Purpose;
use tf_strategy::intent::Side;

pub mod budgets;
pub mod http;
pub mod proposals;

/// Why a stored run was not opened in the explorer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplorerError {
    /// No stored run by that name.
    NotFound(String),
    /// The run exists but cannot be shown (not a kind that can be replayed, rules missing, or the
    /// code no longer reproduces it); the text says which.
    Refused(String),
}

/// Opens a stored run in the trade explorer: given a run's hash (or a prefix), the complete page.
/// The service does not know how to replay a run; the program that hosts it does.
#[derive(Clone)]
pub struct Explorer(std::sync::Arc<OpenRun>);

type OpenRun = dyn Fn(&str) -> Result<String, ExplorerError> + Send + Sync;

impl Explorer {
    pub fn new(
        f: impl Fn(&str) -> Result<String, ExplorerError> + Send + Sync + 'static,
    ) -> Explorer {
        Explorer(std::sync::Arc::new(f))
    }

    pub fn open(&self, run: &str) -> Result<String, ExplorerError> {
        (self.0)(run)
    }
}

impl std::fmt::Debug for Explorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Explorer")
    }
}

/// Why a research view was not given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResearchError {
    /// No such scenario, day, strategy or trade.
    NotFound(String),
    /// It exists but cannot be shown (a damaged or inconsistent file); the text says which.
    Refused(String),
}

/// What the backtest view reads: the research scenarios of the program that hosts the service (E19-S34, E19-S35). The service
/// does not know how a results directory is read; the program does, as it does for the explorer. Every method only reads.
pub trait ResearchView: Send + Sync {
    /// The strategies of every scenario as runs, for the catalog.
    fn runs(&self) -> Result<Vec<Run>, String>;
    /// Every scenario with its days, strategies, costs and budgets, as JSON.
    fn scenarios(&self) -> Result<String, String>;
    /// One strategy's trades on a day, as JSON.
    fn trades(&self, scenario: &str, day: &str, strategy: u16) -> Result<String, ResearchError>;
    /// The page that replays trade `n` (counting a strategy's trades of the day from 0): the whole page, its data embedded.
    fn trade_page(
        &self,
        scenario: &str,
        day: &str,
        strategy: u16,
        n: usize,
    ) -> Result<String, ResearchError>;
}

/// The backtest view's source, if this process has research results to show.
#[derive(Clone)]
pub struct Research(std::sync::Arc<dyn ResearchView>);

impl Research {
    pub fn new(v: impl ResearchView + 'static) -> Research {
        Research(std::sync::Arc::new(v))
    }

    pub fn view(&self) -> &dyn ResearchView {
        self.0.as_ref()
    }
}

impl std::fmt::Debug for Research {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Research")
    }
}

/// Where the service reads from.
#[derive(Clone, Debug, Default)]
pub struct Source {
    /// How to open a stored backtest in the explorer, if this process can.
    pub explorer: Option<Explorer>,
    /// The order ledger of the paper or live account, and which of the two it is.
    pub ledger: Option<(PathBuf, Kind)>,
    /// The run store of backtests.
    pub store: Option<PathBuf>,
    /// The research scenarios of the backtest view.
    pub research: Option<Research>,
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

/// What names a run in this service's API: the stored result's hash, or `session-N` for day N of
/// the ledger (the ledger's path is not given out).
pub fn run_id(r: &Run) -> String {
    match &r.source {
        RunSource::Stored { hash } => hash.clone(),
        RunSource::Ledger { session, .. } => format!("session-{session}"),
        RunSource::Research { scenario, variant } => format!("research:{scenario}:{variant}"),
    }
}

/// One run as JSON. Times are text because event time in nanoseconds does not fit a JSON number.
pub fn run_json(r: &Run) -> String {
    let kind_src = match &r.source {
        RunSource::Stored { .. } => "stored",
        RunSource::Ledger { .. } => "ledger",
        RunSource::Research { .. } => "research",
    };
    let id = run_id(r);
    format!(
        "{{\"strategy\":{},\"kind\":{},\"started_ns\":{},\"started\":{},\"net_pnl\":{},\"trades\":{},\"rules\":{},\"budget\":{},\"source\":{},\"id\":{},\"explorable\":{},\"researchable\":{}}}",
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
        r.explorable(),
        r.researchable()
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
    if let Some(r) = &src.research {
        runs.extend(r.view().runs()?);
    }
    Ok(Catalog::new(runs))
}

/// Raw 1e-9 dollars as dollars to four places (prices need more than cents).
fn price(raw: i64) -> String {
    let r = i128::from(raw);
    let scaled = (r.abs() + 50_000) / 100_000;
    format!(
        "{}{}.{:04}",
        if r < 0 && scaled > 0 { "-" } else { "" },
        scaled / 10_000,
        scaled % 10_000
    )
}

fn clock(ns: Nanos) -> String {
    let s = ns / 1_000_000_000 % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3_600, s % 3_600 / 60, s % 60)
}

/// One run with what it did: its fills (as trades), the running realised profit after each, and the
/// reasons the gateway refused anything. A stored backtest has none of these here (it is opened in
/// the explorer). `None` if there is no such run.
pub fn run_detail(src: &Source, strategy: &str, id: &str) -> Result<Option<String>, String> {
    let cat = catalog(src)?;
    let Some(run) = cat.of(strategy).find(|r| run_id(r) == id) else {
        return Ok(None);
    };
    if let RunSource::Research { scenario, .. } = &run.source {
        return Ok(Some(format!(
            "{{\"run\":{},\"trades\":null,\"curve\":null,\"refused\":[],\"note\":{}}}",
            run_json(run),
            js(&format!(
                "A research backtest of the scenario {scenario}. Its days and trades are under Backtests."
            ))
        )));
    }
    let RunSource::Ledger { session, .. } = &run.source else {
        return Ok(Some(format!(
            "{{\"run\":{},\"trades\":null,\"curve\":null,\"refused\":[],\"note\":{}}}",
            run_json(run),
            js(
                "A stored backtest. Its trades, the scanner hit and why the strategy decided as it did are in the trade explorer."
            )
        )));
    };
    let (dir, kind) = src.ledger.as_ref().ok_or("no ledger")?;
    let name = dir.display().to_string();
    let all = tf_catalog::sessions_detailed(ReadOnlyStore::open(dir), &name, *kind)
        .map_err(|e| e.to_string())?;
    let detail = all
        .into_iter()
        .find(|(r, _)| {
            r.strategy == strategy
                && matches!(&r.source, RunSource::Ledger { session: s, .. } if s == session)
        })
        .map(|(_, d)| d)
        .ok_or("the run disappeared from the ledger")?;
    let mut running = 0i128;
    let mut curve = Vec::new();
    let mut trades = Vec::new();
    for f in &detail.fills {
        running += f.pnl;
        curve.push(js(&dollars(running)));
        trades.push(format!(
            "{{\"time\":{},\"instrument\":{},\"side\":{},\"purpose\":{},\"qty\":{},\"price\":{},\"pnl\":{}}}",
            js(&clock(f.ts)),
            f.instrument,
            js(match f.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
                Side::SellShort => "short",
            }),
            js(match f.purpose {
                Purpose::Open => "open",
                Purpose::Close => "close",
            }),
            f.qty,
            js(&price(f.px)),
            js(&dollars(f.pnl)),
        ));
    }
    let refused: Vec<String> = detail
        .refused
        .iter()
        .map(|(why, n)| format!("{{\"reason\":{},\"count\":{n}}}", js(why)))
        .collect();
    Ok(Some(format!(
        "{{\"run\":{},\"trades\":[{}],\"curve\":[{}],\"refused\":[{}],\"note\":null}}",
        run_json(run),
        trades.join(","),
        curve.join(","),
        refused.join(",")
    )))
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

fn pct(bp: u32) -> String {
    let (w, f) = (bp / 100, bp % 100);
    match f {
        0 => format!("{w}%"),
        f if f % 10 == 0 => format!("{w}.{}%", f / 10),
        f => format!("{w}.{f:02}%"),
    }
}

/// `$12,345.67` from raw 1e-9 dollars.
pub(crate) fn money(raw: i128) -> String {
    let d = dollars(raw);
    let (sign, d) = d.strip_prefix('-').map_or(("", d.as_str()), |r| ("-", r));
    let (whole, cents) = d.split_once('.').unwrap_or((d, "00"));
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}${grouped}.{cents}")
}

/// One difference between the budgets `a` in force and a tree `b` to be, in words, with what a
/// change in a share means in dollars of `balance`.
pub(crate) fn change_text(c: &Change, a: &Tree, b: &Tree, balance: u128) -> String {
    let dollars = |t: &Tree, id: &str, group: bool| {
        let v = if group {
            t.group_budget(balance, id)
        } else {
            t.strategy_budget(balance, id)
        };
        money(v.unwrap_or(0) as i128)
    };
    match c {
        Change::GroupAdded(id) => format!("add group {id}"),
        Change::GroupRemoved(id) => format!("remove group {id}"),
        Change::GroupShare { id, from, to } => format!(
            "{id}: {} → {} of the balance ({} → {})",
            pct(*from),
            pct(*to),
            dollars(a, id, true),
            dollars(b, id, true)
        ),
        Change::Loss { id, from, to } => format!(
            "{id} loss limits: stop opening {} → {}, flatten {} → {}",
            pct(from.soft),
            pct(to.soft),
            pct(from.hard),
            pct(to.hard)
        ),
        Change::StrategyAdded { group, id } => format!("add {id} to {group}"),
        Change::StrategyRemoved { group, id } => format!("remove {id} from {group}"),
        Change::StrategyShare {
            group,
            id,
            from,
            to,
        } => format!(
            "{id}: {} → {} of {group} ({} → {})",
            pct(*from),
            pct(*to),
            dollars(a, id, false),
            dollars(b, id, false)
        ),
    }
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
    let changes: Vec<String> = j
        .scheduled()
        .map(|t| {
            diff(b.tree(), t)
                .iter()
                .map(|c| js(&change_text(c, b.tree(), t, b.balance())))
                .collect()
        })
        .unwrap_or_default();
    let waiting = tf_ledger::inbox::pending(dir).map_or(0, |(w, _)| w.len());
    let _ = write!(
        head,
        ",\"scheduled\":[{}],\"requests_waiting\":{waiting}",
        changes.join(",")
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
