//! Static selection: the symbols of a snapshot that pass a spec, as a `Selection` that is stored with
//! the run so a replay uses the very list the live run used.

use std::fmt::Write as _;

use crate::feature::{Kind, StaticFeature, render_value};
use crate::reference::{RefRow, Snapshot, fnv, valid_name};
use crate::spec::{Spec, StaticCond, Test};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectError {
    /// The snapshot has no such column at all, so the condition could not be judged for anyone.
    MissingColumn(StaticFeature),
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectError::MissingColumn(c) => write!(
                f,
                "the reference snapshot has no `{}` column, which the universe uses; add the data or take the condition out",
                c.name()
            ),
        }
    }
}

impl std::error::Error for SelectError {}

fn holds(spec: &Spec, c: &StaticCond, row: &RefRow) -> bool {
    let f = c.feature;
    match (&c.test, f.kind()) {
        (Test::Cmp(cmp, o), Kind::Price | Kind::Int) => match (row.num(f), spec.value(o)) {
            (Some(v), Some(t)) => cmp.holds(v, t),
            _ => false,
        },
        (Test::Cmp(cmp, o), Kind::Flag) => match (row.flag(f), spec.value(o)) {
            (Some(v), Some(t)) => cmp.holds(i64::from(v), t),
            _ => false,
        },
        (Test::In(names), _) => row.text(f).is_some_and(|v| names.iter().any(|n| n == v)),
        (Test::NotIn(names), _) => row.text(f).is_some_and(|v| !names.iter().any(|n| n == v)),
        _ => false,
    }
}

/// Whether one row passes every static condition. Unknown values fail.
pub fn passes(spec: &Spec, row: &RefRow) -> bool {
    spec.statics.iter().all(|c| holds(spec, c, row))
}

/// The members, fixed for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub as_of: String,
    pub spec_fp: u64,
    pub snapshot_fp: u64,
    /// The thresholds the spec resolved to, `name=value`, by name.
    pub params: Vec<(String, String)>,
    /// Sorted.
    pub symbols: Vec<String>,
}

/// Select from a snapshot. Refuses when the snapshot lacks a column the spec needs.
pub fn select(spec: &Spec, snap: &Snapshot) -> Result<Selection, SelectError> {
    if let Some(m) = spec.needs().into_iter().find(|f| !snap.columns.contains(f)) {
        return Err(SelectError::MissingColumn(m));
    }
    Ok(Selection {
        as_of: snap.as_of.clone(),
        spec_fp: spec.fingerprint(),
        snapshot_fp: snap.fingerprint(),
        params: spec
            .params
            .iter()
            .map(|(k, p)| (k.clone(), render_value(p.kind, p.raw)))
            .collect(),
        symbols: snap
            .rows
            .iter()
            .filter(|r| passes(spec, r))
            .map(|r| r.symbol.clone())
            .collect(),
    })
}

impl Selection {
    pub fn render(&self) -> String {
        let mut s = format!(
            "members v1\nas_of {}\nspec {:016x}\nsnapshot {:016x}\n",
            self.as_of, self.spec_fp, self.snapshot_fp
        );
        for (k, v) in &self.params {
            let _ = writeln!(s, "param {k} = {v}");
        }
        let _ = writeln!(s, "count {}", self.symbols.len());
        for sym in &self.symbols {
            s.push_str(sym);
            s.push('\n');
        }
        s
    }

    pub fn fingerprint(&self) -> u64 {
        fnv(self.render().as_bytes())
    }

    pub fn parse(text: &str) -> Result<Selection, String> {
        let mut l = text.lines();
        let mut next = |what: &str| l.next().ok_or_else(|| format!("members: missing {what}"));
        if next("header")? != "members v1" {
            return Err("members: expected `members v1`".to_owned());
        }
        let field = |line: &str, key: &str| {
            line.strip_prefix(key)
                .map(str::to_owned)
                .ok_or_else(|| format!("members: expected `{}`", key.trim()))
        };
        let as_of = field(next("as_of")?, "as_of ")?;
        let hex = |s: String| {
            u64::from_str_radix(&s, 16).map_err(|_| format!("members: `{s}` is not a fingerprint"))
        };
        let spec_fp = hex(field(next("spec")?, "spec ")?)?;
        let snapshot_fp = hex(field(next("snapshot")?, "snapshot ")?)?;
        let mut params = Vec::new();
        let count = loop {
            let line = next("count")?;
            if let Some(p) = line.strip_prefix("param ") {
                let (k, v) = p.split_once(" = ").ok_or("members: bad param line")?;
                params.push((k.to_owned(), v.to_owned()));
            } else {
                break field(line, "count ")?
                    .parse::<usize>()
                    .map_err(|_| "members: bad count".to_owned())?;
            }
        };
        let symbols: Vec<String> = l.map(str::to_owned).collect();
        if symbols.len() != count {
            return Err(format!(
                "members: count says {count}, {} listed",
                symbols.len()
            ));
        }
        if let Some(bad) = symbols.iter().find(|s| !valid_name(s)) {
            return Err(format!("members: `{bad}` is not a symbol"));
        }
        if symbols.windows(2).any(|w| w[0] >= w[1]) {
            return Err("members: symbols must be sorted and unique".to_owned());
        }
        Ok(Selection {
            as_of,
            spec_fp,
            snapshot_fp,
            params,
            symbols,
        })
    }
}

/// What changed between two specs, in words, and what that does to a snapshot's members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub changes: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Members were added: the new universe watches symbols the old one did not. Anything that
    /// sizes against the universe (capacity, risk) has to be looked at again.
    pub widens: bool,
}

fn lines_of(s: &Spec) -> Vec<String> {
    s.render().lines().skip(1).map(str::to_owned).collect()
}

/// Compare two specs; with a snapshot, also the symbols each admits.
pub fn diff(old: &Spec, new: &Spec, snap: Option<&Snapshot>) -> Result<Diff, SelectError> {
    let mut changes = Vec::new();
    // Parameters by name, then each static condition and the dynamic line.
    for (k, p) in &new.params {
        match old.params.get(k) {
            None => changes.push(format!("+ param {k} = {}", render_value(p.kind, p.raw))),
            Some(o) if o != p => changes.push(format!(
                "~ param {k}: {} -> {}",
                render_value(o.kind, o.raw),
                render_value(p.kind, p.raw)
            )),
            _ => {}
        }
    }
    for k in old.params.keys().filter(|k| !new.params.contains_key(*k)) {
        changes.push(format!("- param {k}"));
    }
    let (a, b) = (lines_of(old), lines_of(new));
    let conds = |lines: &[String], kind: &str| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| l.strip_prefix(kind))
            .flat_map(|rest| {
                if kind == "static " {
                    rest.split("; ").map(str::to_owned).collect::<Vec<_>>()
                } else {
                    vec![rest.to_owned()]
                }
            })
            .collect()
    };
    for kind in ["static ", "dynamic "] {
        let (ca, cb) = (conds(&a, kind), conds(&b, kind));
        for c in cb.iter().filter(|c| !ca.contains(c)) {
            changes.push(format!("+ {}{c}", kind));
        }
        for c in ca.iter().filter(|c| !cb.contains(c)) {
            changes.push(format!("- {}{c}", kind));
        }
    }
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    if let Some(snap) = snap {
        let (o, n) = (select(old, snap)?, select(new, snap)?);
        added = n
            .symbols
            .iter()
            .filter(|s| !o.symbols.contains(s))
            .cloned()
            .collect();
        removed = o
            .symbols
            .iter()
            .filter(|s| !n.symbols.contains(s))
            .cloned()
            .collect();
    }
    let widens = !added.is_empty();
    Ok(Diff {
        changes,
        added,
        removed,
        widens,
    })
}
