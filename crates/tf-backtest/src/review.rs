//! Judging a proposed rule set before anything uses it.
//!
//! A proposal is a candidate rule set, who proposed it and why. [`evaluate`] compares what the
//! candidate did against what the base rules did on the same held-out sessions and applies
//! gates. A failed gate rejects. A candidate that passes but loosens a protective stage (see
//! `tf_strategy::rule_diff`) goes to a person (`NeedsHuman`); one that passes cleanly is
//! `Accepted`. Neither changes anything live: a person's approval is recorded separately, and a
//! rejected proposal cannot be approved.
//!
//! The gates test internal consistency on synthetic sessions: that the edit does not make the
//! strategy enter the dangerous scenarios more, does not lose money overall or in too many
//! sessions, and does not deepen the worst drawdown. They are not evidence of an edge.
//! Sessions the proposer tuned against should not be the review sessions; the review seeds are
//! a separate, fixed range.
//!
//! The record of a proposal is an append-only text file ([`Record`]): the verdict, every gate
//! with its reading, the structural changes, the stored runs the numbers came from, and each
//! approval as a line appended later.

use std::collections::BTreeMap;

/// What one backtest earned, in the units of the stored metrics (1e-9 dollars).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub net: i64,
    pub max_drawdown: i64,
    pub trades: i64,
    /// Trades in the dangerous scenarios; the strategy should make none.
    pub dangerous_trades: i64,
}

impl Outcome {
    /// Read the stored metrics of a run. `None` if one the review needs is missing.
    pub fn from_metrics(m: &BTreeMap<String, i64>) -> Option<Outcome> {
        Some(Outcome {
            net: *m.get("pnl_net")?,
            max_drawdown: *m.get("max_drawdown")?,
            trades: *m.get("trades")?,
            dangerous_trades: m.get("group.dangerous.trades").copied().unwrap_or(0),
        })
    }
}

/// One held-out session run under both rule sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paired {
    pub seed: u64,
    pub base: Outcome,
    pub cand: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Fewest paired sessions to judge on.
    pub min_sessions: usize,
    /// Fewest trades, both rule sets together, to judge on.
    pub min_trades: i64,
    /// Net P&L the candidate may lose against the base over all sessions (1e-9 dollars).
    pub allow_net_drop: i64,
    /// The worst session drawdown may rise by this much (1e-9 dollars).
    pub allow_drawdown_rise: i64,
    /// Sessions the candidate may do worse in, permille of all sessions.
    pub worse_sessions_permille: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            min_sessions: 12,
            min_trades: 10,
            allow_net_drop: 0,
            allow_drawdown_rise: 0,
            worse_sessions_permille: 250,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    NeedsHuman,
    Rejected,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Accepted => "accepted",
            Verdict::NeedsHuman => "needs-human",
            Verdict::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Verdict> {
        [Verdict::Accepted, Verdict::NeedsHuman, Verdict::Rejected]
            .into_iter()
            .find(|v| v.name() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub gates: Vec<Gate>,
    pub verdict: Verdict,
}

fn usd(raw: i64) -> String {
    let cents = (i128::from(raw).abs() + 5_000_000) / 10_000_000;
    format!(
        "{}${}.{:02}",
        if raw < 0 && cents > 0 { "-" } else { "" },
        cents / 100,
        cents % 100
    )
}

/// Apply the gates. `same_rules` is true when the candidate is the base (nothing to review);
/// `loosens` is whether the structural diff loosens a veto.
pub fn evaluate(pairs: &[Paired], same_rules: bool, loosens: bool, policy: &Policy) -> Review {
    let sum = |f: &dyn Fn(&Outcome) -> i64, cand: bool| -> i64 {
        pairs
            .iter()
            .map(|p| f(if cand { &p.cand } else { &p.base }))
            .sum()
    };
    let (bn, cn) = (sum(&|o| o.net, false), sum(&|o| o.net, true));
    let (bd, cd) = (
        sum(&|o| o.dangerous_trades, false),
        sum(&|o| o.dangerous_trades, true),
    );
    let trades = sum(&|o| o.trades, false) + sum(&|o| o.trades, true);
    let worst = |cand: bool| {
        pairs
            .iter()
            .map(|p| if cand { p.cand } else { p.base }.max_drawdown)
            .max()
            .unwrap_or(0)
    };
    let (bdd, cdd) = (worst(false), worst(true));
    let worse = pairs.iter().filter(|p| p.cand.net < p.base.net).count();
    let better = pairs.iter().filter(|p| p.cand.net > p.base.net).count();
    let allowed_worse = pairs.len() * policy.worse_sessions_permille as usize / 1000;

    let mut gates = vec![
        Gate {
            name: "different",
            pass: !same_rules,
            detail: if same_rules {
                "the candidate is the base rule set".to_owned()
            } else {
                "the candidate differs from the base".to_owned()
            },
        },
        Gate {
            name: "sessions",
            pass: pairs.len() >= policy.min_sessions,
            detail: format!(
                "{} paired sessions, at least {} needed",
                pairs.len(),
                policy.min_sessions
            ),
        },
        Gate {
            name: "trades",
            pass: trades >= policy.min_trades,
            detail: format!(
                "{trades} trades across both, at least {} needed",
                policy.min_trades
            ),
        },
        Gate {
            name: "dangerous",
            pass: cd <= bd,
            detail: format!("entries in the dangerous scenarios: base {bd}, candidate {cd}"),
        },
        Gate {
            name: "net",
            pass: i128::from(cn) >= i128::from(bn) - i128::from(policy.allow_net_drop),
            detail: format!(
                "net P&L over all sessions: base {}, candidate {} (allowed drop {})",
                usd(bn),
                usd(cn),
                usd(policy.allow_net_drop)
            ),
        },
        Gate {
            name: "drawdown",
            pass: i128::from(cdd) <= i128::from(bdd) + i128::from(policy.allow_drawdown_rise),
            detail: format!(
                "worst session drawdown: base {}, candidate {} (allowed rise {})",
                usd(bdd),
                usd(cdd),
                usd(policy.allow_drawdown_rise)
            ),
        },
        Gate {
            name: "breadth",
            pass: worse <= allowed_worse,
            detail: format!(
                "candidate better in {better}, worse in {worse} of {} sessions (at most {allowed_worse} worse)",
                pairs.len()
            ),
        },
    ];
    let failed = gates.iter().any(|g| !g.pass);
    gates.push(Gate {
        name: "veto",
        pass: !loosens,
        detail: if loosens {
            "loosens a protective stage: needs a person".to_owned()
        } else {
            "no protective stage is loosened".to_owned()
        },
    });
    let verdict = if failed {
        Verdict::Rejected
    } else if loosens {
        Verdict::NeedsHuman
    } else {
        Verdict::Accepted
    };
    Review { gates, verdict }
}

/// What is kept about a proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub proposer: String,
    pub reason: String,
    /// Fingerprints of the base and candidate rule sets.
    pub base: String,
    pub candidate: String,
    /// What the sessions were, for the reader (not parsed).
    pub suite: String,
    pub review: Review,
    /// The structural changes, one line each.
    pub changes: Vec<String>,
    /// For each session: seed, base run hash, candidate run hash.
    pub runs: Vec<(u64, String, String)>,
    pub approvals: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordError(pub String);

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn one_line(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl Record {
    /// The id of a proposal: the candidate and the base it was judged against.
    pub fn id(&self) -> String {
        format!("{}-{}", self.candidate, self.base)
    }

    /// Approved by at least one person, and not rejected.
    pub fn approved(&self) -> bool {
        self.review.verdict != Verdict::Rejected && !self.approvals.is_empty()
    }

    /// Record `name`'s approval. A rejected proposal cannot be approved.
    pub fn approve(&mut self, name: &str) -> Result<(), RecordError> {
        let name = one_line(name, 80);
        if name.is_empty() {
            return Err(RecordError("an approval needs a name".to_owned()));
        }
        if self.review.verdict == Verdict::Rejected {
            return Err(RecordError(
                "this proposal was rejected and cannot be approved".to_owned(),
            ));
        }
        if self.approvals.contains(&name) {
            return Err(RecordError(format!("{name} has already approved it")));
        }
        self.approvals.push(name);
        Ok(())
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("tfpr 1\n");
        let mut line = |k: &str, v: &str| {
            s.push_str(k);
            s.push(' ');
            s.push_str(v);
            s.push('\n');
        };
        line("proposer", &one_line(&self.proposer, 80));
        line("reason", &one_line(&self.reason, 500));
        line("base", &self.base);
        line("candidate", &self.candidate);
        line("suite", &one_line(&self.suite, 300));
        line("verdict", self.review.verdict.name());
        for g in &self.review.gates {
            line(
                "gate",
                &format!(
                    "{} {} {}",
                    g.name,
                    if g.pass { "pass" } else { "fail" },
                    one_line(&g.detail, 300)
                ),
            );
        }
        for c in &self.changes {
            line("change", &one_line(c, 300));
        }
        for (seed, b, c) in &self.runs {
            line("run", &format!("{seed} {b} {c}"));
        }
        for a in &self.approvals {
            line("approval", a);
        }
        s
    }

    pub fn parse(text: &str) -> Result<Record, RecordError> {
        let bad = |m: String| Err(RecordError(m));
        let mut lines = text.lines();
        if lines.next() != Some("tfpr 1") {
            return bad("not a proposal record (expected `tfpr 1`)".to_owned());
        }
        let (mut proposer, mut reason, mut base, mut candidate, mut suite) =
            (None, None, None, None, None);
        let mut verdict = None;
        let (mut gates, mut changes, mut runs, mut approvals) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (n, l) in lines.enumerate() {
            let n = n + 2;
            let Some((k, v)) = l.split_once(' ') else {
                return bad(format!("line {n}: `{l}` is not `key value`"));
            };
            match k {
                "proposer" => proposer = Some(v.to_owned()),
                "reason" => reason = Some(v.to_owned()),
                "base" => base = Some(v.to_owned()),
                "candidate" => candidate = Some(v.to_owned()),
                "suite" => suite = Some(v.to_owned()),
                "verdict" => {
                    verdict =
                        Some(Verdict::parse(v).ok_or_else(|| {
                            RecordError(format!("line {n}: unknown verdict `{v}`"))
                        })?);
                }
                "gate" => {
                    let mut p = v.splitn(3, ' ');
                    let (Some(name), Some(res), Some(detail)) = (p.next(), p.next(), p.next())
                    else {
                        return bad(format!("line {n}: a gate is `name pass|fail detail`"));
                    };
                    let name = GATE_NAMES
                        .iter()
                        .find(|g| **g == name)
                        .ok_or_else(|| RecordError(format!("line {n}: unknown gate `{name}`")))?;
                    let pass = match res {
                        "pass" => true,
                        "fail" => false,
                        other => return bad(format!("line {n}: `{other}` is not pass or fail")),
                    };
                    gates.push(Gate {
                        name,
                        pass,
                        detail: detail.to_owned(),
                    });
                }
                "change" => changes.push(v.to_owned()),
                "run" => {
                    let p: Vec<&str> = v.split(' ').collect();
                    let [seed, b, c] = p[..] else {
                        return bad(format!("line {n}: a run is `seed base candidate`"));
                    };
                    let seed = seed
                        .parse::<u64>()
                        .map_err(|e| RecordError(format!("line {n}: seed: {e}")))?;
                    runs.push((seed, b.to_owned(), c.to_owned()));
                }
                "approval" => approvals.push(v.to_owned()),
                other => return bad(format!("line {n}: unknown key `{other}`")),
            }
        }
        let need = |v: Option<String>, name: &str| {
            v.ok_or_else(|| RecordError(format!("the record has no `{name}`")))
        };
        let verdict =
            verdict.ok_or_else(|| RecordError("the record has no `verdict`".to_owned()))?;
        // The verdict must be what the gates say, so a hand-edited record cannot claim more.
        let names: Vec<&str> = gates.iter().map(|g| g.name).collect();
        if names != GATE_NAMES {
            return bad(format!(
                "the record's gates are {names:?}, expected {GATE_NAMES:?}"
            ));
        }
        let hard_failed = gates.iter().any(|g| g.name != "veto" && !g.pass);
        let veto_failed = gates.iter().any(|g| g.name == "veto" && !g.pass);
        let implied = if hard_failed {
            Verdict::Rejected
        } else if veto_failed {
            Verdict::NeedsHuman
        } else {
            Verdict::Accepted
        };
        if implied != verdict {
            return bad(format!(
                "the record says `{}` but its gates say `{}`",
                verdict.name(),
                implied.name()
            ));
        }
        if verdict == Verdict::Rejected && !approvals.is_empty() {
            return bad("a rejected proposal has an approval".to_owned());
        }
        Ok(Record {
            proposer: need(proposer, "proposer")?,
            reason: need(reason, "reason")?,
            base: need(base, "base")?,
            candidate: need(candidate, "candidate")?,
            suite: need(suite, "suite")?,
            review: Review { gates, verdict },
            changes,
            runs,
            approvals,
        })
    }
}

const GATE_NAMES: [&str; 8] = [
    "different",
    "sessions",
    "trades",
    "dangerous",
    "net",
    "drawdown",
    "breadth",
    "veto",
];
