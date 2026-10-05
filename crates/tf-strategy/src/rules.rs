//! Entry rules as data.
//!
//! A [`RuleSet`] is the decision part of [`crate::MomentumLong`] written down as four
//! stages of conditions instead of code. Each condition compares one pullback feature with a
//! threshold, and a threshold is either a literal or the name of a tunable parameter (written
//! `@max_depth_permille`), which then follows the parameter store: bounds, audit and
//! auto-revert apply to it exactly as before.
//!
//! The stages, in the order the strategy asks them once a second for a watched symbol:
//!
//! 1. `too_old`: if true, give up (the pullback dragged on).
//! 2. `armed`: if false, keep watching (not yet an impulse, or too soon after the high).
//! 3. `dangerous`: if true, give up.
//! 4. `enter`: if true, buy.
//!
//! The text form is line based and canonical when produced by [`RuleSet::render`]:
//!
//! ```text
//! rules v1
//! too_old any: secs_since_high > @max_pullback_secs
//! armed all: impulse >= @min_impulse_permille; secs_since_high >= @min_pullback_secs
//! dangerous any: depth > @max_depth_permille; volume_ratio > @max_volume_ratio_permille
//! enter all: depth >= @min_depth_permille; retrace_now <= @max_retrace_now_permille
//! ```
//!
//! A feature with no value yet (no quotes, no volume ratio) makes its condition false, for
//! every comparison. Evaluating a rule set never allocates; [`RuleSet::evaluate`] builds the
//! per-condition record kept as evidence.
//!
//! What stays code: what the features are, and which stages exist. What is data: which
//! features matter, how they are compared, and the thresholds.

use std::fmt::Write as _;

use tf_engine::PullbackFeatures;

use crate::momentum::{MomentumParams, TUNABLES};

/// A measurement a condition may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Swing low to swing high, permille of the low.
    Impulse,
    /// Seconds from the swing low to the swing high.
    ImpulseSecs,
    SecsSinceHigh,
    /// Deepest give-back so far, permille of the impulse.
    Depth,
    /// Give-back at the last price, permille of the impulse.
    RetraceNow,
    /// Volume rate since the high against the impulse's, permille.
    VolumeRatio,
    HigherLows,
    /// Trade rate over the last 5 s against the impulse's, permille.
    TapeRatio,
    /// Bid size as permille of bid + ask size.
    BidSupport,
}

const FEATURES: [(Feature, &str); 9] = [
    (Feature::Impulse, "impulse"),
    (Feature::ImpulseSecs, "impulse_secs"),
    (Feature::SecsSinceHigh, "secs_since_high"),
    (Feature::Depth, "depth"),
    (Feature::RetraceNow, "retrace_now"),
    (Feature::VolumeRatio, "volume_ratio"),
    (Feature::HigherLows, "higher_lows"),
    (Feature::TapeRatio, "tape_ratio"),
    (Feature::BidSupport, "bid_support"),
];

impl Feature {
    pub fn name(self) -> &'static str {
        FEATURES
            .iter()
            .find(|(f, _)| *f == self)
            .map_or("?", |x| x.1)
    }

    /// The feature's value, or `None` when there is no data for it yet.
    pub fn value(self, f: &PullbackFeatures, impulse_permille: i64) -> Option<i64> {
        let big = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        match self {
            Feature::Impulse => Some(impulse_permille),
            Feature::ImpulseSecs => Some(i64::from(f.impulse_secs)),
            Feature::SecsSinceHigh => Some(i64::from(f.secs_since_high)),
            Feature::Depth => Some(f.depth_permille),
            Feature::RetraceNow => Some(f.retrace_now_permille),
            Feature::VolumeRatio => f.volume_ratio_permille.map(big),
            Feature::HigherLows => Some(i64::from(f.higher_lows)),
            Feature::TapeRatio => f.tape_ratio_permille.map(big),
            Feature::BidSupport => f.bid_support_permille.map(i64::from),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Ge,
    Gt,
    Le,
    Lt,
}

impl Cmp {
    pub fn symbol(self) -> &'static str {
        match self {
            Cmp::Ge => ">=",
            Cmp::Gt => ">",
            Cmp::Le => "<=",
            Cmp::Lt => "<",
        }
    }

    fn test(self, value: i64, limit: i64) -> bool {
        match self {
            Cmp::Ge => value >= limit,
            Cmp::Gt => value > limit,
            Cmp::Le => value <= limit,
            Cmp::Lt => value < limit,
        }
    }
}

/// A literal, or an index into the strategy's tunables (by name in the text form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threshold {
    Value(i64),
    Param(usize),
}

impl Threshold {
    /// The tunable this threshold follows, if it is not a literal.
    pub fn param_name(self) -> Option<&'static str> {
        match self {
            Threshold::Value(_) => None,
            Threshold::Param(i) => Some(TUNABLES[i].name),
        }
    }

    pub fn resolve(self, p: &MomentumParams) -> i64 {
        match self {
            Threshold::Value(v) => v,
            Threshold::Param(i) => (TUNABLES[i].get)(p),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Condition {
    pub feature: Feature,
    pub cmp: Cmp,
    pub threshold: Threshold,
}

impl Condition {
    pub fn holds(&self, f: &PullbackFeatures, impulse: i64, p: &MomentumParams) -> bool {
        self.feature
            .value(f, impulse)
            .is_some_and(|v| self.cmp.test(v, self.threshold.resolve(p)))
    }
}

/// How the conditions of a stage combine. An empty `all` is true and an empty `any` is false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    All,
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    pub mode: Mode,
    pub conditions: Vec<Condition>,
}

impl Stage {
    pub fn holds(&self, f: &PullbackFeatures, impulse: i64, p: &MomentumParams) -> bool {
        match self.mode {
            Mode::All => self.conditions.iter().all(|c| c.holds(f, impulse, p)),
            Mode::Any => self.conditions.iter().any(|c| c.holds(f, impulse, p)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageKind {
    TooOld,
    Armed,
    Dangerous,
    Enter,
}

const STAGES: [(StageKind, &str); 4] = [
    (StageKind::TooOld, "too_old"),
    (StageKind::Armed, "armed"),
    (StageKind::Dangerous, "dangerous"),
    (StageKind::Enter, "enter"),
];

impl StageKind {
    pub fn name(self) -> &'static str {
        STAGES.iter().find(|(k, _)| *k == self).map_or("?", |x| x.1)
    }
}

/// One condition as it evaluated at a decision: the evidence kept with the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evaluation {
    pub stage: StageKind,
    pub mode: Mode,
    pub condition: Condition,
    /// The threshold after resolving a parameter reference.
    pub limit: i64,
    pub value: Option<i64>,
    pub pass: bool,
}

/// Why a rule set could not be read or is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError(pub String);

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuleError {}

fn err<T>(line: usize, msg: impl std::fmt::Display) -> Result<T, RuleError> {
    Err(RuleError(format!("rules line {line}: {msg}")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSet {
    pub too_old: Stage,
    pub armed: Stage,
    pub dangerous: Stage,
    pub enter: Stage,
}

/// The rules MomentumLong ships with. Parsed (and so checked) by [`RuleSet::momentum`].
pub const MOMENTUM_RULES: &str = "\
rules v1
too_old any: secs_since_high > @max_pullback_secs
armed all: impulse >= @min_impulse_permille; secs_since_high >= @min_pullback_secs
dangerous any: depth > @max_depth_permille; volume_ratio > @max_volume_ratio_permille
enter all: depth >= @min_depth_permille; retrace_now <= @max_retrace_now_permille; volume_ratio >= 0; higher_lows >= @min_higher_lows; bid_support >= @min_bid_support_permille
";

impl RuleSet {
    /// The default momentum rules.
    pub fn momentum() -> RuleSet {
        RuleSet::parse(MOMENTUM_RULES).expect("the built-in rules parse")
    }

    pub fn stage(&self, kind: StageKind) -> &Stage {
        match kind {
            StageKind::TooOld => &self.too_old,
            StageKind::Armed => &self.armed,
            StageKind::Dangerous => &self.dangerous,
            StageKind::Enter => &self.enter,
        }
    }

    /// Read the text form. Blank lines and `#` comments are ignored; every stage must appear
    /// exactly once; unknown features, parameters and comparisons are errors.
    pub fn parse(text: &str) -> Result<RuleSet, RuleError> {
        let mut seen_header = false;
        let mut stages: [Option<Stage>; 4] = [None, None, None, None];
        for (n, raw) in text.lines().enumerate() {
            let n = n + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if !seen_header {
                if line != "rules v1" {
                    return err(n, format!("expected `rules v1`, found `{line}`"));
                }
                seen_header = true;
                continue;
            }
            let Some((head, body)) = line.split_once(':') else {
                return err(n, "expected `<stage> <all|any>: <conditions>`");
            };
            let mut h = head.split_whitespace();
            let (Some(name), Some(mode), None) = (h.next(), h.next(), h.next()) else {
                return err(n, "expected `<stage> <all|any>` before the colon");
            };
            let Some(idx) = STAGES.iter().position(|(_, s)| *s == name) else {
                return err(n, format!("unknown stage `{name}`"));
            };
            if stages[idx].is_some() {
                return err(n, format!("stage `{name}` appears twice"));
            }
            let mode = match mode {
                "all" => Mode::All,
                "any" => Mode::Any,
                other => return err(n, format!("`{other}` is not `all` or `any`")),
            };
            let mut conditions = Vec::new();
            for c in body.split(';').map(str::trim).filter(|c| !c.is_empty()) {
                conditions.push(
                    parse_condition(c).map_err(|m| RuleError(format!("rules line {n}: {m}")))?,
                );
            }
            stages[idx] = Some(Stage { mode, conditions });
        }
        if !seen_header {
            return Err(RuleError("rules: missing `rules v1` header".to_owned()));
        }
        let mut it = stages.into_iter().zip(STAGES);
        let mut take = || -> Result<Stage, RuleError> {
            let (s, (_, name)) = it.next().expect("four stages");
            s.ok_or_else(|| RuleError(format!("rules: stage `{name}` is missing")))
        };
        let (too_old, armed, dangerous, enter) = (take()?, take()?, take()?, take()?);
        if enter.conditions.is_empty() {
            return Err(RuleError(
                "rules: `enter` needs at least one condition (an empty `all` would buy everything)"
                    .to_owned(),
            ));
        }
        Ok(RuleSet {
            too_old,
            armed,
            dangerous,
            enter,
        })
    }

    /// The canonical text. `parse(render())` gives back an equal rule set.
    pub fn render(&self) -> String {
        let mut s = String::from("rules v1\n");
        for (kind, name) in STAGES {
            let st = self.stage(kind);
            let _ = write!(
                s,
                "{name} {}:",
                if st.mode == Mode::All { "all" } else { "any" }
            );
            for (i, c) in st.conditions.iter().enumerate() {
                let _ = write!(
                    s,
                    "{} {} {} ",
                    if i == 0 { "" } else { ";" },
                    c.feature.name(),
                    c.cmp.symbol()
                );
                match c.threshold {
                    Threshold::Value(v) => {
                        let _ = write!(s, "{v}");
                    }
                    Threshold::Param(i) => {
                        let _ = write!(s, "@{}", TUNABLES[i].name);
                    }
                }
            }
            s.push('\n');
        }
        s
    }

    /// A short identifier of the canonical text (FNV-1a, 64 bit: for telling versions
    /// apart in records, not a security hash).
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.render().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Every condition of every stage, evaluated (no short-circuit), for the record.
    pub fn evaluate(
        &self,
        f: &PullbackFeatures,
        impulse: i64,
        p: &MomentumParams,
    ) -> Vec<Evaluation> {
        let mut out = Vec::new();
        for (kind, _) in STAGES {
            let st = self.stage(kind);
            for c in &st.conditions {
                out.push(Evaluation {
                    stage: kind,
                    mode: st.mode,
                    condition: *c,
                    limit: c.threshold.resolve(p),
                    value: c.feature.value(f, impulse),
                    pass: c.holds(f, impulse, p),
                });
            }
        }
        out
    }
}

fn parse_condition(c: &str) -> Result<Condition, String> {
    let mut it = c.split_whitespace();
    let (Some(feature), Some(cmp), Some(threshold), None) =
        (it.next(), it.next(), it.next(), it.next())
    else {
        return Err(format!("`{c}` is not `<feature> <comparison> <threshold>`"));
    };
    let Some((feature, _)) = FEATURES.iter().find(|(_, n)| *n == feature) else {
        return Err(format!("unknown feature `{feature}`"));
    };
    let cmp = match cmp {
        ">=" => Cmp::Ge,
        ">" => Cmp::Gt,
        "<=" => Cmp::Le,
        "<" => Cmp::Lt,
        other => return Err(format!("unknown comparison `{other}`")),
    };
    let threshold = if let Some(name) = threshold.strip_prefix('@') {
        match TUNABLES.iter().position(|t| t.name == name) {
            Some(i) => Threshold::Param(i),
            None => return Err(format!("`@{name}` is not a tunable parameter")),
        }
    } else {
        match threshold.parse::<i64>() {
            Ok(v) => Threshold::Value(v),
            Err(_) => return Err(format!("`{threshold}` is not a number or an `@parameter`")),
        }
    };
    Ok(Condition {
        feature: *feature,
        cmp,
        threshold,
    })
}
