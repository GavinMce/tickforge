//! What changed between two rule sets, and whether it loosens a protective stage.
//!
//! `too_old` and `dangerous` are vetoes: a condition that holds there makes the strategy give up
//! on a symbol. Removing a veto condition, moving its threshold so it fires less often, or
//! requiring all of several where any would do, makes the strategy willing to enter where it
//! used to refuse. That is the edit a reviewer must look at, so it is flagged
//! (`Change::loosens`). Changes to `armed` and `enter` are shown but not flagged: their effect
//! shows in the outcomes, not in the structure.
//!
//! Conditions are paired by feature and direction (greater-than or less-than), in order.
//! Thresholds that follow a parameter are compared at the values in `params`.

use crate::momentum::MomentumParams;
use crate::rules::{Cmp, Condition, Mode, RuleSet, StageKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added(Condition),
    Removed(Condition),
    Changed { from: Condition, to: Condition },
    Mode { from: Mode, to: Mode },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub stage: StageKind,
    pub kind: ChangeKind,
    /// A veto stage fires less often than before.
    pub loosens: bool,
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.stage.name();
        match self.kind {
            ChangeKind::Added(c) => write!(f, "+ {st}: {c}")?,
            ChangeKind::Removed(c) => write!(f, "- {st}: {c}")?,
            ChangeKind::Changed { from, to } => write!(f, "~ {st}: {from}  ->  {to}")?,
            ChangeKind::Mode { from, to } => {
                let m = |m| if m == Mode::All { "all" } else { "any" };
                write!(f, "~ {st}: {} -> {}", m(from), m(to))?;
            }
        }
        if self.loosens {
            f.write_str("   [LOOSENS A VETO]")?;
        }
        Ok(())
    }
}

fn is_veto(k: StageKind) -> bool {
    matches!(k, StageKind::TooOld | StageKind::Dangerous)
}

/// Fires when the value is greater (`true`) or smaller (`false`) than the limit.
fn up(c: Cmp) -> bool {
    matches!(c, Cmp::Gt | Cmp::Ge)
}

/// The condition as a strict inequality on integers: `x > t` (up) or `x < t` (down). A lower `t`
/// for up, or a higher `t` for down, fires on more values.
fn strict(c: &Condition, p: &MomentumParams) -> i64 {
    let t = c.threshold.resolve(p);
    match c.cmp {
        Cmp::Gt | Cmp::Lt => t,
        Cmp::Ge => t.saturating_sub(1),
        Cmp::Le => t.saturating_add(1),
    }
}

fn fires_less_often(from: &Condition, to: &Condition, p: &MomentumParams) -> bool {
    let (a, b) = (strict(from, p), strict(to, p));
    if up(from.cmp) { b > a } else { b < a }
}

/// The changes from `base` to `cand`, stage by stage in the order the strategy asks them.
pub fn diff(base: &RuleSet, cand: &RuleSet, p: &MomentumParams) -> Vec<Change> {
    let mut out = Vec::new();
    for kind in [
        StageKind::TooOld,
        StageKind::Armed,
        StageKind::Dangerous,
        StageKind::Enter,
    ] {
        let (b, c) = (base.stage(kind), cand.stage(kind));
        let veto = is_veto(kind);
        if b.mode != c.mode && (b.conditions.len() > 1 || c.conditions.len() > 1) {
            out.push(Change {
                stage: kind,
                kind: ChangeKind::Mode {
                    from: b.mode,
                    to: c.mode,
                },
                loosens: veto && b.mode == Mode::Any,
            });
        }
        let mut used = vec![false; c.conditions.len()];
        for bc in &b.conditions {
            let hit =
                c.conditions.iter().enumerate().find(|(j, cc)| {
                    !used[*j] && cc.feature == bc.feature && up(cc.cmp) == up(bc.cmp)
                });
            match hit {
                Some((j, cc)) => {
                    used[j] = true;
                    if cc != bc {
                        out.push(Change {
                            stage: kind,
                            kind: ChangeKind::Changed { from: *bc, to: *cc },
                            loosens: veto && fires_less_often(bc, cc, p),
                        });
                    }
                }
                None => out.push(Change {
                    stage: kind,
                    kind: ChangeKind::Removed(*bc),
                    loosens: veto,
                }),
            }
        }
        for (_, cc) in c.conditions.iter().enumerate().filter(|(j, _)| !used[*j]) {
            out.push(Change {
                stage: kind,
                kind: ChangeKind::Added(*cc),
                loosens: false,
            });
        }
    }
    out
}

/// Whether any change loosens a veto.
pub fn loosens_a_veto(changes: &[Change]) -> bool {
    changes.iter().any(|c| c.loosens)
}
