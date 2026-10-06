//! The universe spec: its text form, canonical rendering and fingerprint.
//!
//! ```text
//! universe v1
//! param min_adv = 5000000
//! param min_price = 2.00
//! static price >= @min_price; price <= 50; adv_dollar >= @min_adv; etf = no; exchange in NASDAQ NYSE
//! dynamic top 30 by gap_permille desc keep 45 every 5 where trades >= 20
//! ```
//!
//! `static` conditions are all required of a symbol, judged from the reference snapshot before the
//! session. `dynamic` picks, among those, the top N by a live measurement, re-ranked every so many
//! seconds, with a symbol staying until it falls below rank `keep` so the set does not flicker.
//! A threshold is a literal or `@name` of a `param`; every param must be used, and always as the same
//! kind of value.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::feature::{Kind, LiveFeature, StaticFeature, parse_value, render_value};
use crate::reference::{fnv, valid_name};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cmp {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
    Ne,
}

const CMPS: [(Cmp, &str); 6] = [
    (Cmp::Ge, ">="),
    (Cmp::Le, "<="),
    (Cmp::Gt, ">"),
    (Cmp::Lt, "<"),
    (Cmp::Eq, "="),
    (Cmp::Ne, "!="),
];

impl Cmp {
    pub fn text(self) -> &'static str {
        CMPS.iter().find(|c| c.0 == self).map_or("?", |c| c.1)
    }

    fn parse(s: &str) -> Option<Cmp> {
        CMPS.iter().find(|c| c.1 == s).map(|c| c.0)
    }

    pub fn holds(self, a: i64, b: i64) -> bool {
        match self {
            Cmp::Ge => a >= b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Lt => a < b,
            Cmp::Eq => a == b,
            Cmp::Ne => a != b,
        }
    }
}

/// A threshold: a number written in place, or a parameter by name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Operand {
    Lit(i64),
    Param(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Test {
    Cmp(Cmp, Operand),
    In(Vec<String>),
    NotIn(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StaticCond {
    pub feature: StaticFeature,
    pub test: Test,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LiveCond {
    pub feature: LiveFeature,
    pub cmp: Cmp,
    pub operand: Operand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dynamic {
    pub top: u32,
    pub by: LiveFeature,
    pub descending: bool,
    /// A member stays while its rank is at most this (at least `top`).
    pub keep: u32,
    /// Re-rank at most this often, in seconds of event time.
    pub every_secs: u32,
    pub filters: Vec<LiveCond>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub kind: Kind,
    pub raw: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub params: BTreeMap<String, Param>,
    pub statics: Vec<StaticCond>,
    pub dynamic: Option<Dynamic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError {
    pub line: usize,
    pub why: String,
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "universe spec line {}: {}", self.line, self.why)
    }
}

impl std::error::Error for SpecError {}

fn valid_param_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What each `param` was first used as, and that it is used.
#[derive(Default)]
struct Uses(BTreeMap<String, Kind>);

fn operand(text: &str, kind: Kind, uses: &mut Uses) -> Result<Operand, String> {
    if let Some(name) = text.strip_prefix('@') {
        if !valid_param_name(name) {
            return Err(format!("`{text}` is not a parameter name"));
        }
        if let Some(k) = uses.0.insert(name.to_owned(), kind) {
            if k != kind {
                return Err(format!("@{name} is used as both a {k:?} and a {kind:?}"));
            }
        }
        return Ok(Operand::Param(name.to_owned()));
    }
    parse_value(kind, text).map(Operand::Lit)
}

fn names(words: &[&str]) -> Result<Vec<String>, String> {
    if words.is_empty() {
        return Err("`in` needs at least one name".to_owned());
    }
    let mut v: Vec<String> = Vec::new();
    for w in words {
        if !valid_name(w) {
            return Err(format!(
                "`{w}` is not a name (upper case letters, digits, . _ -)"
            ));
        }
        v.push((*w).to_owned());
    }
    v.sort();
    v.dedup();
    Ok(v)
}

fn static_cond(text: &str, uses: &mut Uses) -> Result<StaticCond, String> {
    let w: Vec<&str> = text.split_whitespace().collect();
    let f = StaticFeature::parse(w.first().copied().unwrap_or("")).ok_or_else(|| format!("`{text}`: unknown feature (static: price adv_dollar adv_shares atr_permille exchange etf shortable easy_to_borrow tradable float short_interest)"))?;
    let kind = f.kind();
    let test = match (w.get(1).copied(), kind) {
        (Some("in"), Kind::Text) => Test::In(names(&w[2..])?),
        (Some("not"), Kind::Text) if w.get(2) == Some(&"in") => Test::NotIn(names(&w[3..])?),
        (Some(op), Kind::Text) if op == "in" || op == "not" => {
            return Err("write `exchange in A B` or `exchange not in A B`".to_owned());
        }
        (Some(op), _) => {
            let cmp = Cmp::parse(op)
                .ok_or_else(|| format!("`{op}` is not a comparison (>= <= > < = !=)"))?;
            if kind == Kind::Text {
                return Err(format!("{} is a name: use `in` or `not in`", f.name()));
            }
            if kind == Kind::Flag && !matches!(cmp, Cmp::Eq | Cmp::Ne) {
                return Err(format!("{} is yes or no: use = or !=", f.name()));
            }
            if w.len() != 3 {
                return Err(format!("`{text}`: expected `{} {op} value`", f.name()));
            }
            if kind == Kind::Flag && w[2].starts_with('@') {
                return Err("a yes/no condition takes yes or no, not a parameter".to_owned());
            }
            Test::Cmp(cmp, operand(w[2], kind, uses)?)
        }
        (None, _) => return Err(format!("`{text}`: expected a comparison")),
    };
    Ok(StaticCond { feature: f, test })
}

fn live_cond(text: &str, uses: &mut Uses) -> Result<LiveCond, String> {
    let w: Vec<&str> = text.split_whitespace().collect();
    if w.len() != 3 {
        return Err(format!("`{text}`: expected `feature comparison value`"));
    }
    let feature = LiveFeature::parse(w[0]).ok_or_else(|| format!("`{}` is not a live feature (gap_permille volume_ratio_permille dollar_volume range_permille trades)", w[0]))?;
    let cmp = Cmp::parse(w[1])
        .ok_or_else(|| format!("`{}` is not a comparison (>= <= > < = !=)", w[1]))?;
    Ok(LiveCond {
        feature,
        cmp,
        operand: operand(w[2], Kind::Int, uses)?,
    })
}

fn number(s: Option<&&str>, what: &str) -> Result<u32, String> {
    s.and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{what} must be a whole number above zero"))
}

fn dynamic(rest: &str, uses: &mut Uses) -> Result<Dynamic, String> {
    let (head, filters) = match rest.split_once(" where ") {
        Some((h, f)) => (h, Some(f)),
        None => (rest, None),
    };
    let w: Vec<&str> = head.split_whitespace().collect();
    // top N by FEATURE desc|asc keep M every S
    if w.len() != 9 || w[0] != "top" || w[2] != "by" || w[5] != "keep" || w[7] != "every" {
        return Err(
            "expected `dynamic top N by FEATURE desc|asc keep M every S [where ...]`".to_owned(),
        );
    }
    let top = number(w.get(1), "top")?;
    let by = LiveFeature::parse(w[3]).ok_or_else(|| format!("`{}` is not a live feature", w[3]))?;
    let descending = match w[4] {
        "desc" => true,
        "asc" => false,
        o => return Err(format!("`{o}` is not desc or asc")),
    };
    let keep = number(w.get(6), "keep")?;
    if keep < top {
        return Err(format!(
            "keep {keep} is below top {top}: a member would leave the moment it joined"
        ));
    }
    let every_secs = number(w.get(8), "every")?;
    let mut fs: Vec<LiveCond> = Vec::new();
    if let Some(f) = filters {
        for c in f.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            fs.push(live_cond(c, uses)?);
        }
    }
    fs.sort();
    fs.dedup();
    Ok(Dynamic {
        top,
        by,
        descending,
        keep,
        every_secs,
        filters: fs,
    })
}

impl Spec {
    pub fn parse(text: &str) -> Result<Spec, SpecError> {
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.trim()))
            .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'));
        match lines.next() {
            Some((_, "universe v1")) => {}
            Some((n, other)) => {
                return Err(SpecError {
                    line: n,
                    why: format!("expected `universe v1`, found `{other}`"),
                });
            }
            None => {
                return Err(SpecError {
                    line: 1,
                    why: "empty".to_owned(),
                });
            }
        }
        let mut declared: BTreeMap<String, (usize, String)> = BTreeMap::new();
        let mut uses = Uses::default();
        let mut statics: Vec<StaticCond> = Vec::new();
        let mut dyn_: Option<Dynamic> = None;
        let mut seen_static = false;
        for (n, line) in lines {
            let err = |why: String| SpecError { line: n, why };
            if let Some(p) = line.strip_prefix("param ") {
                let (name, value) = p
                    .split_once('=')
                    .ok_or_else(|| err("expected `param name = value`".to_owned()))?;
                let (name, value) = (name.trim(), value.trim());
                if !valid_param_name(name) {
                    return Err(err(format!(
                        "`{name}` is not a parameter name (lower case letters, digits, _)"
                    )));
                }
                if declared
                    .insert(name.to_owned(), (n, value.to_owned()))
                    .is_some()
                {
                    return Err(err(format!("param {name} declared twice")));
                }
            } else if let Some(c) = line.strip_prefix("static ") {
                if seen_static {
                    return Err(err(
                        "only one `static` line: separate conditions with `;`".to_owned()
                    ));
                }
                seen_static = true;
                for cond in c.split(';').map(str::trim).filter(|c| !c.is_empty()) {
                    statics.push(static_cond(cond, &mut uses).map_err(err)?);
                }
            } else if let Some(d) = line.strip_prefix("dynamic ") {
                if dyn_.is_some() {
                    return Err(err("only one `dynamic` line".to_owned()));
                }
                dyn_ = Some(dynamic(d, &mut uses).map_err(err)?);
            } else {
                return Err(err(format!("`{line}`: expected param, static or dynamic")));
            }
        }
        let mut params = BTreeMap::new();
        for (name, kind) in &uses.0 {
            let (n, value) = declared.get(name).ok_or_else(|| SpecError {
                line: 0,
                why: format!("@{name} is used but never declared"),
            })?;
            let raw = parse_value(*kind, value).map_err(|why| SpecError {
                line: *n,
                why: format!("param {name}: {why}"),
            })?;
            params.insert(name.clone(), Param { kind: *kind, raw });
        }
        if let Some((name, (n, _))) = declared.iter().find(|(k, _)| !uses.0.contains_key(*k)) {
            return Err(SpecError {
                line: *n,
                why: format!("param {name} is declared but never used"),
            });
        }
        statics.sort();
        statics.dedup();
        Ok(Spec {
            params,
            statics,
            dynamic: dyn_,
        })
    }

    /// The canonical text: parameters by name, conditions in a fixed order, numbers in canonical form.
    pub fn render(&self) -> String {
        let mut s = String::from("universe v1\n");
        for (k, p) in &self.params {
            let _ = writeln!(s, "param {k} = {}", render_value(p.kind, p.raw));
        }
        if !self.statics.is_empty() {
            let parts: Vec<String> = self.statics.iter().map(|c| self.static_text(c)).collect();
            let _ = writeln!(s, "static {}", parts.join("; "));
        }
        if let Some(d) = &self.dynamic {
            let _ = write!(
                s,
                "dynamic top {} by {} {} keep {} every {}",
                d.top,
                d.by.name(),
                if d.descending { "desc" } else { "asc" },
                d.keep,
                d.every_secs
            );
            if !d.filters.is_empty() {
                let parts: Vec<String> = d
                    .filters
                    .iter()
                    .map(|c| {
                        format!(
                            "{} {} {}",
                            c.feature.name(),
                            c.cmp.text(),
                            self.operand_text(&c.operand, Kind::Int)
                        )
                    })
                    .collect();
                let _ = write!(s, " where {}", parts.join("; "));
            }
            s.push('\n');
        }
        s
    }

    fn operand_text(&self, o: &Operand, kind: Kind) -> String {
        match o {
            Operand::Lit(v) => render_value(kind, *v),
            Operand::Param(p) => format!("@{p}"),
        }
    }

    fn static_text(&self, c: &StaticCond) -> String {
        let f = c.feature;
        match &c.test {
            Test::Cmp(cmp, o) => format!(
                "{} {} {}",
                f.name(),
                cmp.text(),
                self.operand_text(o, f.kind())
            ),
            Test::In(v) => format!("{} in {}", f.name(), v.join(" ")),
            Test::NotIn(v) => format!("{} not in {}", f.name(), v.join(" ")),
        }
    }

    /// FNV-1a 64 of the canonical text: the same universe always has the same fingerprint.
    pub fn fingerprint(&self) -> u64 {
        fnv(self.render().as_bytes())
    }

    /// The static features this spec needs a snapshot to have.
    pub fn needs(&self) -> Vec<StaticFeature> {
        let mut v: Vec<StaticFeature> = self.statics.iter().map(|c| c.feature).collect();
        // Live features that are measured against the reference.
        if let Some(d) = &self.dynamic {
            let live: Vec<LiveFeature> = std::iter::once(d.by)
                .chain(d.filters.iter().map(|f| f.feature))
                .collect();
            if live.contains(&LiveFeature::GapPermille) {
                v.push(StaticFeature::Price);
            }
            if live.contains(&LiveFeature::VolumeRatioPermille) {
                v.push(StaticFeature::AdvShares);
            }
        }
        v.sort();
        v.dedup();
        v
    }

    /// A threshold's value; `None` for a parameter that is not declared (a hand-built spec), which
    /// fails the condition.
    pub(crate) fn value(&self, o: &Operand) -> Option<i64> {
        match o {
            Operand::Lit(v) => Some(*v),
            Operand::Param(p) => self.params.get(p).map(|p| p.raw),
        }
    }
}
