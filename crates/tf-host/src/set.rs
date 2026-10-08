//! The strategy set: the variants a run is made of, in one text file that research and the live day both read (E19-S31).
//!
//! ```text
//! strategy set v1
//! balance 100000
//! loss 300 600
//! limits order=50000 position=100000 gross=5000000 daily_loss=10000 orders=10000 window_secs=1
//! strategy 1 a t04 universe=universes/liquid.txt share=5000 names=3 dollars=2000
//! strategy 2 b t04 universe=universes/liquid.txt share=5000 names=10 priority=2
//! ```
//!
//! A **strategy** line is a number (the one in its intents), a name (its id in the budget tree and in results: lowercase
//! letters and digits, as the tree requires), a template from [`templates`] and `key=value` words: the parameters of
//! the template (any not given are the template's defaults), and `universe=FILE` (required, a [`tf_universe::Spec`] in a file
//! beside the set), `priority=N` (Tier 1 evictions, default 1) and `share=BP` (its share of the group in basis points; the
//! shares left over are split evenly). One group `g` holds every strategy, with the loss limits of the `loss` line (soft and hard,
//! in basis points of a strategy's own budget; default 300 and 600). Nothing here reads the market or runs anything: the same file
//! gives a research run and a live day the same host limits, budgets and definitions.

use std::collections::BTreeSet;
use std::path::Path;

use tf_budget::{Group, LossLimits, Strategy as BudgetStrategy, Tree};
use tf_core::Nanos;
use tf_engine::{PromoterConfig, ScannerConfig};
use tf_risk::{Budgets, Limits};
use tf_strategy::closing_reversal::ClosingReversalParams;
use tf_strategy::random_entries::RandomEntriesParams;
use tf_universe::Spec;

use crate::def::{Route, StrategyDef};
use crate::host::HostConfig;
use crate::library::{closing_reversal, random_entries};

const HEADER: &str = "strategy set v1";
const DOLLAR: u128 = 1_000_000_000;
const SEC: Nanos = 1_000_000_000;

/// Why a strategy set was refused, with the line it is about where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetError(pub String);

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SetError {}

fn err<T>(line: usize, why: impl std::fmt::Display) -> Result<T, SetError> {
    Err(SetError(format!("line {line}: {why}")))
}

/// The templates a strategy line can name, with their parameters (and defaults) as text.
pub fn templates() -> Vec<(&'static str, String)> {
    vec![
        ("t04", ClosingReversalParams::default().render()),
        ("t14", RandomEntriesParams::default().render()),
    ]
}

/// The template's parameters with `given` laid over its defaults. A key the template does not have is refused.
fn params_text(template: &str, given: &[(String, String)]) -> Result<String, String> {
    let defaults = templates()
        .into_iter()
        .find(|(n, _)| *n == template)
        .map(|(_, d)| d)
        .ok_or_else(|| {
            format!(
                "`{template}` is not a template (known: {})",
                templates()
                    .iter()
                    .map(|t| t.0)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let mut pairs: Vec<(String, String)> = defaults
        .split_whitespace()
        .filter_map(|w| w.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    for (k, v) in given {
        match pairs.iter_mut().find(|p| &p.0 == k) {
            Some(p) => p.1 = v.clone(),
            None => return Err(format!("`{k}` is not a parameter of {template}")),
        }
    }
    Ok(pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" "))
}

/// One strategy line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetStrategy {
    pub id: u16,
    pub name: String,
    pub template: String,
    /// The template's parameters, every one, as `key=value` words.
    pub params: String,
    /// Where the universe is, as written (relative to the set's directory).
    pub universe: String,
    pub priority: u8,
    /// Its share of the group in basis points, if the line gave one.
    pub share: Option<u32>,
}

/// A parsed strategy set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StrategySet {
    /// What the budget tree divides, whole dollars.
    pub balance: u64,
    pub loss_soft: u32,
    pub loss_hard: u32,
    pub limits: SetLimits,
    pub strategies: Vec<SetStrategy>,
}

/// The gateway's hard limits, in dollars and counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetLimits {
    pub order: u64,
    pub position: u32,
    pub gross: u64,
    pub daily_loss: u64,
    pub orders: u32,
    pub window_secs: u32,
}

impl StrategySet {
    pub fn parse(text: &str) -> Result<StrategySet, SetError> {
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.trim()))
            .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'));
        match lines.next() {
            Some((_, HEADER)) => {}
            _ => return Err(SetError(format!("the first line must be `{HEADER}`"))),
        }
        let mut balance: Option<u64> = None;
        let (mut soft, mut hard) = (300u32, 600u32);
        let mut limits: Option<SetLimits> = None;
        let mut strategies: Vec<SetStrategy> = Vec::new();
        for (n, line) in lines {
            let w: Vec<&str> = line.split_whitespace().collect();
            match w[0] {
                "balance" => {
                    if w.len() != 2 || balance.is_some() {
                        return err(n, "`balance DOLLARS`, once");
                    }
                    balance = Some(w[1].parse::<u64>().ok().filter(|b| *b > 0).map_or_else(
                        || err(n, "the balance is a whole number of dollars above zero"),
                        Ok,
                    )?);
                }
                "loss" => {
                    let (Some(s), Some(h)) = (
                        w.get(1).and_then(|x| x.parse::<u32>().ok()),
                        w.get(2).and_then(|x| x.parse::<u32>().ok()),
                    ) else {
                        return err(n, "`loss SOFT HARD` in basis points");
                    };
                    if w.len() != 3 || s == 0 || s >= h || h > 10_000 {
                        return err(
                            n,
                            "the loss limits need 0 < soft < hard <= 10000 basis points",
                        );
                    }
                    (soft, hard) = (s, h);
                }
                "limits" => {
                    if limits.is_some() {
                        return err(n, "`limits` is given once");
                    }
                    limits = Some(parse_limits(n, &w[1..], balance)?);
                }
                "strategy" => strategies.push(parse_strategy(n, &w[1..])?),
                other => return err(n, format!("`{other}` is not a line of a strategy set")),
            }
        }
        let balance = balance.ok_or_else(|| SetError("`balance DOLLARS` is missing".to_owned()))?;
        let limits = match limits {
            Some(l) => l,
            None => parse_limits(0, &[], Some(balance))?,
        };
        if strategies.is_empty() {
            return Err(SetError("the set has no strategy".to_owned()));
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for s in &strategies {
            if !ids.insert(s.id) {
                return Err(SetError(format!("strategy number {} is used twice", s.id)));
            }
            if !names.insert(s.name.clone()) {
                return Err(SetError(format!(
                    "strategy name `{}` is used twice",
                    s.name
                )));
            }
        }
        let set = StrategySet {
            balance,
            loss_soft: soft,
            loss_hard: hard,
            limits,
            strategies,
        };
        set.tree()?;
        Ok(set)
    }

    /// The shares in basis points: those given, and the rest of the 10,000 split evenly among the others (the first get the
    /// remainder). Refused if the given ones leave nothing for the others or add past the whole.
    pub fn shares(&self) -> Result<Vec<u32>, SetError> {
        let given: u32 = self.strategies.iter().filter_map(|s| s.share).sum();
        let open = self.strategies.iter().filter(|s| s.share.is_none()).count() as u32;
        if given > 10_000 {
            return Err(SetError(format!(
                "the shares given add to {given} basis points, over 10000"
            )));
        }
        if open > 0 && given >= 10_000 {
            return Err(SetError(
                "the shares given leave nothing for the strategies without one".to_owned(),
            ));
        }
        let left = 10_000 - given;
        let (each, mut extra) = (
            left.checked_div(open).unwrap_or(0),
            left.checked_rem(open).unwrap_or(0),
        );
        Ok(self
            .strategies
            .iter()
            .map(|s| {
                s.share.unwrap_or_else(|| {
                    let e = u32::from(extra > 0);
                    extra = extra.saturating_sub(1);
                    each + e
                })
            })
            .collect())
    }

    /// The budget tree: one group `g` of every strategy.
    pub fn tree(&self) -> Result<Tree, SetError> {
        let shares = self.shares()?;
        Tree::new(vec![Group {
            id: "g".to_owned(),
            share: 10_000,
            loss: LossLimits {
                soft: self.loss_soft,
                hard: self.loss_hard,
            },
            strategies: self
                .strategies
                .iter()
                .zip(shares)
                .map(|(s, share)| BudgetStrategy {
                    id: s.name.clone(),
                    share,
                })
                .collect(),
        }])
        .map_err(|e| SetError(format!("the budget tree is not valid: {e:?}")))
    }

    /// The host's configuration: the gateway's limits, the budgets, and the engine's defaults. `id_space` is how many
    /// instrument numbers it must cover (the day's symbols).
    pub fn host_config(&self, id_space: usize) -> Result<HostConfig, SetError> {
        let l = &self.limits;
        let limits = Limits::new(
            u128::from(l.order) * DOLLAR,
            l.position,
            u128::from(l.gross) * DOLLAR,
            u128::from(l.daily_loss) * DOLLAR,
            l.orders,
            Nanos::from(l.window_secs) * SEC,
        )
        .map_err(|e| SetError(format!("the limits are not valid: {e:?}")))?;
        let budgets = Budgets::new(
            self.tree()?,
            u128::from(self.balance) * DOLLAR,
            self.strategies.iter().map(|s| (s.id, s.name.clone())),
        )
        .map_err(|e| SetError(format!("the budgets are not valid: {e:?}")))?;
        Ok(HostConfig {
            id_space,
            limits,
            budgets: Some(budgets),
            promoter: PromoterConfig::default(),
            scanner: ScannerConfig::default(),
            sim: crate::research::CostModel::published().sim(),
            min_certified_events: 1_000,
            start_ts: 0,
            bars: None,
            day: None,
        })
    }

    /// The definitions, each with its universe read from `dir` (where the set's file is).
    pub fn definitions(&self, dir: &Path) -> Result<Vec<StrategyDef>, SetError> {
        self.strategies
            .iter()
            .map(|s| {
                let path = dir.join(&s.universe);
                let text = std::fs::read_to_string(&path).map_err(|e| {
                    SetError(format!("strategy {}: {}: {e}", s.name, path.display()))
                })?;
                let universe = Spec::parse(&text).map_err(|e| {
                    SetError(format!("strategy {}: {}: {e:?}", s.name, path.display()))
                })?;
                let mut def = match s.template.as_str() {
                    "t04" => closing_reversal(
                        s.id,
                        &s.name,
                        universe,
                        ClosingReversalParams::parse(&s.params).map_err(|e| SetError(e.0))?,
                    )
                    .map_err(|e| SetError(e.0))?,
                    "t14" => random_entries(
                        s.id,
                        &s.name,
                        universe,
                        RandomEntriesParams::parse(&s.params).map_err(|e| SetError(e.0))?,
                    )
                    .map_err(|e| SetError(e.0))?,
                    other => return Err(SetError(format!("`{other}` is not a template"))),
                };
                def.priority = s.priority;
                def.route = Route::Sim;
                Ok(def)
            })
            .collect()
    }

    /// Read a set from a file; its universes are read from the same directory.
    pub fn load(path: &Path) -> Result<(StrategySet, Vec<StrategyDef>), SetError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SetError(format!("{}: {e}", path.display())))?;
        let set = StrategySet::parse(&text)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let defs = set.definitions(dir)?;
        Ok((set, defs))
    }
}

fn parse_limits(n: usize, words: &[&str], balance: Option<u64>) -> Result<SetLimits, SetError> {
    let mut l = SetLimits {
        order: 50_000,
        position: 100_000,
        gross: 5_000_000,
        // A tenth of the balance, unless the line says: a loss that size in a day is a stopped day.
        daily_loss: balance.map_or(10_000, |b| (b / 10).max(1)),
        orders: 10_000,
        window_secs: 1,
    };
    let mut seen = BTreeSet::new();
    for w in words {
        let Some((k, v)) = w.split_once('=') else {
            return err(n, format!("`{w}` is not key=value"));
        };
        if !seen.insert(k.to_owned()) {
            return err(n, format!("{k} is given twice"));
        }
        match k {
            "order" => l.order = positive(n, k, v)?,
            "position" => l.position = positive(n, k, v)?,
            "gross" => l.gross = positive(n, k, v)?,
            "daily_loss" => l.daily_loss = positive(n, k, v)?,
            "orders" => l.orders = positive(n, k, v)?,
            "window_secs" => l.window_secs = positive(n, k, v)?,
            other => {
                return err(
                    n,
                    format!(
                        "`{other}` is not a limit (order, position, gross, daily_loss, orders, window_secs)"
                    ),
                );
            }
        }
    }
    if l.gross < l.order {
        return err(n, "the gross limit is below the single-order limit");
    }
    Ok(l)
}

/// A whole number above zero.
fn positive<T: std::str::FromStr + Default + PartialEq>(
    n: usize,
    k: &str,
    v: &str,
) -> Result<T, SetError> {
    match v.parse::<T>() {
        Ok(x) if x != T::default() => Ok(x),
        _ => err(n, format!("`{v}` is not a whole number above zero for {k}")),
    }
}

fn parse_strategy(n: usize, w: &[&str]) -> Result<SetStrategy, SetError> {
    if w.len() < 3 {
        return err(n, "`strategy NUMBER NAME TEMPLATE [key=value ...]`");
    }
    let id: u16 = w[0].parse().ok().filter(|i| *i > 0).map_or_else(
        || err(n, format!("`{}` is not a strategy number above zero", w[0])),
        Ok,
    )?;
    let name = w[1];
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return err(
            n,
            format!(
                "`{name}` is not a strategy name (lowercase letters, digits, - and _, at most 32)"
            ),
        );
    }
    let template = w[2];
    let (mut universe, mut priority, mut share) = (None, 1u8, None);
    let mut given: Vec<(String, String)> = Vec::new();
    for word in &w[3..] {
        let Some((k, v)) = word.split_once('=') else {
            return err(n, format!("`{word}` is not key=value"));
        };
        match k {
            "universe" => universe = Some(v.to_owned()),
            "priority" => {
                priority = v.parse().map_err(|_| {
                    SetError(format!("line {n}: `{v}` is not a priority (0 to 255)"))
                })?;
            }
            "share" => {
                share = Some(
                    v.parse::<u32>()
                        .ok()
                        .filter(|s| (1..=10_000).contains(s))
                        .map_or_else(
                            || err(n, format!("`{v}` is not a share (1 to 10000 basis points)")),
                            Ok,
                        )?,
                );
            }
            _ => {
                if given.iter().any(|g| g.0 == k) {
                    return err(n, format!("{k} is given twice"));
                }
                given.push((k.to_owned(), v.to_owned()));
            }
        }
    }
    let params = params_text(template, &given).map_err(|e| SetError(format!("line {n}: {e}")))?;
    // The values must be ones the template accepts, found now and not when the day starts.
    let check = match template {
        "t04" => ClosingReversalParams::parse(&params).map(|_| ()),
        _ => RandomEntriesParams::parse(&params).map(|_| ()),
    };
    check.map_err(|e| SetError(format!("line {n}: {}", e.0)))?;
    let universe =
        universe.ok_or_else(|| SetError(format!("line {n}: `universe=FILE` is required")))?;
    Ok(SetStrategy {
        id,
        name: name.to_owned(),
        template: template.to_owned(),
        params,
        universe,
        priority,
        share,
    })
}
