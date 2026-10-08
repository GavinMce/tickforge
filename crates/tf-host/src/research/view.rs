//! What the backtest view shows (E19-S34, E19-S35): JSON read from results directories, only read.
//!
//! A **root** holds scenarios, each a results directory of [`super::run`]. Nothing here writes, runs a strategy or reads the store
//! of market data: everything comes from the directory (the trips, the decision log, the traces and the evidence kept with them), so
//! a directory copied to another machine shows the same. A scenario is named in a URL, so a name is one plain directory name and
//! nothing else (no separators, no dots at the front): a request cannot reach outside the root.

use std::path::{Path, PathBuf};

use tf_calendar::{Calendar, Date};
use tf_catalog::Run;
use tf_core::Nanos;

use super::cost::is_date;
use super::run::{CONFIG_FILE, DefLine, ResearchError, Results};
pub use super::trade::trade_page;

#[cfg(test)]
pub(crate) fn dollars_for_tests(raw: i128) -> String {
    dollars(raw)
}
use super::trips::{OPEN_AT_END, Trip};
use crate::equiv::{Answer, Rec};

/// Why something was not shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    /// No such scenario, day, strategy or trade.
    NotFound(String),
    /// It is there and cannot be shown: a damaged or inconsistent file, and which.
    Refused(String),
}

impl From<ResearchError> for ViewError {
    fn from(e: ResearchError) -> Self {
        ViewError::Refused(e.to_string())
    }
}

/// A JSON string.
pub(crate) fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            // `<` and `/` kept out of a string that may sit inside a page.
            '<' => o.push_str("\\u003c"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Dollars to the cent from raw 1e-9 dollars, as text so no precision is lost.
pub(crate) fn dollars(raw: i128) -> String {
    let cents = (raw.abs() + 5_000_000) / 10_000_000;
    format!(
        "{}{}.{:02}",
        if raw < 0 && cents > 0 { "-" } else { "" },
        cents / 100,
        cents % 100
    )
}

/// A price from raw 1e-9 dollars to four places.
pub(crate) fn price(raw: i64) -> String {
    let r = i128::from(raw);
    let scaled = (r.abs() + 50_000) / 100_000;
    format!(
        "{}{}.{:04}",
        if r < 0 && scaled > 0 { "-" } else { "" },
        scaled / 10_000,
        scaled % 10_000
    )
}

/// Hundredths of a basis point as basis points to two places: -1029 is -10.29.
/// Thousandths as a decimal with three places: -2 is "-0.002".
pub(crate) fn milli(m: i64) -> String {
    let a = m.unsigned_abs();
    format!(
        "{}{}.{:03}",
        if m < 0 { "-" } else { "" },
        a / 1000,
        a % 1000
    )
}

pub(crate) fn bp(x100: i64) -> String {
    let a = x100.unsigned_abs();
    format!(
        "{}{}.{:02}",
        if x100 < 0 { "-" } else { "" },
        a / 100,
        a % 100
    )
}

/// New York time of day with milliseconds, for event time `ts`.
pub(crate) fn et(ts: Nanos) -> String {
    match Calendar::us_equities().local(ts) {
        Ok((_, s)) => format!(
            "{:02}:{:02}:{:02}.{:03}",
            s / 3600,
            s % 3600 / 60,
            s % 60,
            ts % 1_000_000_000 / 1_000_000
        ),
        Err(_) => "?".to_owned(),
    }
}

/// What an exit reason code means.
pub(crate) fn reason_text(code: u16) -> String {
    match code {
        0xE501 => "stop".to_owned(),
        0xE502 => "target".to_owned(),
        0xE503 => "time exit".to_owned(),
        OPEN_AT_END => "still open at the end of the day".to_owned(),
        c => format!("strategy code {c}"),
    }
}

/// Whether `s` can name a scenario: one plain directory name.
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub(super) fn open(root: &Path, scenario: &str) -> Result<Results, ViewError> {
    if !valid_name(scenario) || !root.join(scenario).join(CONFIG_FILE).is_file() {
        return Err(ViewError::NotFound(format!("no scenario `{scenario}`")));
    }
    Ok(Results::open(&root.join(scenario))?)
}

/// The scenarios under `root`: the directories that have a configuration, in name order.
pub fn scenario_names(root: &Path) -> Result<Vec<String>, String> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let name = e.file_name().to_string_lossy().into_owned();
        if valid_name(&name) && e.path().join(CONFIG_FILE).is_file() {
            v.push(name);
        }
    }
    v.sort();
    Ok(v)
}

fn net_of(trips: &[Trip], fp: u64) -> (u64, i128, i128) {
    let mine = trips.iter().filter(|t| t.variant == fp);
    let (mut n, mut net, mut bps) = (0u64, 0i128, 0i128);
    for t in mine {
        n += 1;
        net += i128::from(t.net);
        bps += i128::from(t.net_bps_x100);
    }
    (n, net, bps)
}

fn mean_bp(n: u64, bps: i128) -> String {
    if n == 0 {
        return "null".to_owned();
    }
    let mean = (bps / i128::from(n)) as i64;
    js(&bp(mean))
}

/// The start of a scenario's data: the open of its first day, as event time (0 if the calendar does not say).
fn started(first_day: Option<&str>) -> Nanos {
    let Some(d) = first_day.filter(|d| is_date(d)) else {
        return 0;
    };
    let date = Date::new(
        d[..4].parse().unwrap_or(0),
        d[5..7].parse().unwrap_or(0),
        d[8..10].parse().unwrap_or(0),
    );
    date.and_then(|date| Calendar::us_equities().times(date).ok().flatten())
        .map_or(0, |t| t.open)
}

fn budget_of(results: &Results, id: u16) -> Option<u128> {
    let b = results.budgets().ok().flatten()?;
    b.tree.strategy_budget(b.balance, b.ids.get(&id)?)
}

fn budgets_json(results: &Results) -> String {
    let Ok(Some(b)) = results.budgets() else {
        return "null".to_owned();
    };
    let by_name: std::collections::BTreeMap<&str, u16> =
        b.ids.iter().map(|(n, s)| (s.as_str(), *n)).collect();
    let groups: Vec<String> = b
        .tree
        .groups()
        .iter()
        .map(|g| {
            let strategies: Vec<String> = g
                .strategies
                .iter()
                .map(|s| {
                    format!(
                        "{{\"id\":{},\"number\":{},\"share_bp\":{},\"budget\":{}}}",
                        js(&s.id),
                        by_name.get(s.id.as_str()).map_or("null".to_owned(), |n| n.to_string()),
                        s.share,
                        b.tree
                            .strategy_budget(b.balance, &s.id)
                            .map_or("null".to_owned(), |v| js(&dollars(v as i128)))
                    )
                })
                .collect();
            format!(
                "{{\"id\":{},\"share_bp\":{},\"budget\":{},\"soft_bp\":{},\"hard_bp\":{},\"strategies\":[{}]}}",
                js(&g.id),
                g.share,
                js(&dollars(b.tree.group_budget(b.balance, &g.id).unwrap_or(0) as i128)),
                g.loss.soft,
                g.loss.hard,
                strategies.join(",")
            )
        })
        .collect();
    format!(
        "{{\"balance\":{},\"unassigned_bp\":{},\"groups\":[{}]}}",
        js(&dollars(b.balance as i128)),
        b.tree.unassigned(),
        groups.join(",")
    )
}

fn strategy_json(l: &DefLine, trips: &[Trip], results: &Results) -> String {
    let (n, net, bps) = net_of(trips, l.fingerprint);
    format!(
        "{{\"id\":{},\"name\":{},\"variant\":{},\"params\":{},\"universe\":{},\"trades\":{n},\"net\":{},\"mean_bp\":{},\"budget\":{}}}",
        l.id,
        js(&l.name),
        js(&format!("{:016x}", l.fingerprint)),
        js(&l.params),
        js(&l.universe),
        js(&dollars(net)),
        mean_bp(n, bps),
        budget_of(results, l.id).map_or("null".to_owned(), |v| js(&dollars(v as i128)))
    )
}

fn scenario_json(root: &Path, name: &str) -> Result<String, ViewError> {
    let r = open(root, name)?;
    let days = r.dates()?;
    let lines = r.definition_lines()?;
    let trips = r.trips()?;
    let cost = r.cost();
    let strategies: Vec<String> = lines.iter().map(|l| strategy_json(l, &trips, &r)).collect();
    let (n, net, _) = trips.iter().fold((0u64, 0i128, 0i128), |a, t| {
        (a.0 + 1, a.1 + i128::from(t.net), 0)
    });
    Ok(format!(
        "{{\"name\":{},\"days\":[{}],\"trades\":{n},\"net\":{},\"cost\":{{\"latency_ms\":{},\"borrow_bps_per_year\":{},\"sec_through\":{},\"taf_through\":{}}},\"strategies\":[{}],\"budgets\":{}}}",
        js(name),
        days.iter().map(|d| js(d)).collect::<Vec<_>>().join(","),
        js(&dollars(net)),
        cost.latency_ns / 1_000_000,
        cost.borrow_bps_per_year,
        js(&cost.sec_through),
        js(&cost.taf_through),
        strategies.join(","),
        budgets_json(&r)
    ))
}

/// Every scenario under `root` as JSON: `{"scenarios":[...]}`. A scenario that cannot be read is listed with its error, so one
/// damaged directory does not hide the others.
pub fn scenarios_json(root: &Path) -> Result<String, String> {
    let mut items = Vec::new();
    for name in scenario_names(root)? {
        items.push(match scenario_json(root, &name) {
            Ok(j) => j,
            Err(ViewError::NotFound(m) | ViewError::Refused(m)) => {
                format!("{{\"name\":{},\"error\":{}}}", js(&name), js(&m))
            }
        });
    }
    Ok(format!("{{\"scenarios\":[{}]}}", items.join(",")))
}

/// The strategies of every scenario under `root` as runs for the catalog.
pub fn catalog_runs(root: &Path) -> Result<Vec<Run>, String> {
    let mut runs = Vec::new();
    for name in scenario_names(root)? {
        let Ok(r) = open(root, &name) else { continue };
        let (Ok(lines), Ok(trips), Ok(days)) = (r.definition_lines(), r.trips(), r.dates()) else {
            continue;
        };
        let at = started(days.first().map(String::as_str));
        for l in &lines {
            let (n, net, _) = net_of(&trips, l.fingerprint);
            runs.push(tf_catalog::research_run(
                &name,
                &format!("{:016x}", l.fingerprint),
                &l.name,
                at,
                net,
                n,
                budget_of(&r, l.id),
            ));
        }
    }
    Ok(runs)
}

/// The directory of a scenario, if `scenario` names one: for the hosting program to say where results are.
pub fn scenario_dir(root: &Path, scenario: &str) -> Option<PathBuf> {
    (valid_name(scenario) && root.join(scenario).join(CONFIG_FILE).is_file())
        .then(|| root.join(scenario))
}

pub(super) fn day_of(r: &Results, day: &str) -> Result<(), ViewError> {
    if !is_date(day) || !r.dates()?.iter().any(|d| d == day) {
        return Err(ViewError::NotFound(format!(
            "no day `{day}` in this scenario"
        )));
    }
    Ok(())
}

pub(super) fn strategy_of(r: &Results, id: u16) -> Result<DefLine, ViewError> {
    r.definition_lines()?
        .into_iter()
        .find(|l| l.id == id)
        .ok_or_else(|| ViewError::NotFound(format!("no strategy {id} in this scenario")))
}

/// What a strategy did on a day, as JSON: what it tried and was refused, and each trade.
pub fn trades_json(
    root: &Path,
    scenario: &str,
    day: &str,
    strategy: u16,
) -> Result<String, ViewError> {
    let r = open(root, scenario)?;
    day_of(&r, day)?;
    let line = strategy_of(&r, strategy)?;
    let file = r.day(day)?;
    let mine: Vec<&Trip> = file
        .trips
        .iter()
        .filter(|t| t.strategy == strategy)
        .collect();
    // What the host counted of this strategy, from the traces; and why the gateway refused what it did, from the log.
    let traces = r.traces(day)?;
    let stats = traces
        .iter()
        .find(|(s, t)| *s == strategy && t.kind == "stats")
        .map(|(_, t)| t);
    let count = |k: &str| stats.and_then(|t| t.value(k)).unwrap_or("0").to_owned();
    let mut reasons: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for rec in &r.log(day)?.recs {
        if let Rec::Decision {
            strategy: s,
            answer: Answer::Rejected(why),
            ..
        } = rec
        {
            if *s == strategy {
                *reasons.entry(why.clone()).or_default() += 1;
            }
        }
    }
    let rows: Vec<String> = mine
        .iter()
        .enumerate()
        .map(|(n, t)| {
            format!(
                "{{\"n\":{n},\"symbol\":{},\"side\":{},\"qty\":{},\"entry\":{},\"entry_px\":{},\"exit\":{},\"exit_px\":{},\"net\":{},\"bp\":{},\"r\":{},\"exit_reason\":{},\"open_at_end\":{}}}",
                js(&t.symbol),
                js(if t.long { "long" } else { "short" }),
                t.qty,
                js(&et(t.entry_ts)),
                js(&price(t.entry_px)),
                js(&et(t.exit_ts)),
                js(&price(t.exit_px)),
                js(&dollars(i128::from(t.net))),
                js(&bp(t.net_bps_x100)),
                t.r_milli.map_or("null".to_owned(), |m| js(&milli(m))),
                js(&reason_text(t.exit_reason)),
                t.open_at_end
            )
        })
        .collect();
    let why: Vec<String> = reasons
        .iter()
        .map(|(k, v)| format!("{{\"reason\":{},\"count\":{v}}}", js(k)))
        .collect();
    Ok(format!(
        "{{\"scenario\":{},\"day\":{},\"strategy\":{{\"id\":{},\"name\":{},\"variant\":{}}},\"accepted\":{},\"rejected\":{},\"refused\":{},\"rejections\":[{}],\"trades\":[{}]}}",
        js(scenario),
        js(day),
        line.id,
        js(&line.name),
        js(&format!("{:016x}", line.fingerprint)),
        count("accepted"),
        count("rejected"),
        count("refused"),
        why.join(","),
        rows.join(",")
    ))
}
