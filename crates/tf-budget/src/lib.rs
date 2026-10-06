//! The budget tree.
//!
//! A workspace's balance is split into **reserved** shares: each group takes a share of the
//! balance, each strategy a share of its group, and whatever is not assigned stays unassigned at
//! that level. Shares are whole basis points (1/10,000) of the level above, so 10,000 is all of
//! it. Dollar amounts are raw units (1e-9 dollars, like prices), rounded down at every level, so
//! the children of a node never add up to more than the node.
//!
//! The rules this crate holds, so that no screen or service has to:
//! - children never exceed their parent (checked when a tree is built and on every edit);
//! - a share cannot be cut below what is in use (given a [`Usage`]), nor raised above what is
//!   unassigned in its parent;
//! - a node that is already over its budget (prices moved) may stay where it is or go up, never
//!   down.
//!
//! A tree has a canonical text form with a fingerprint ([`Tree::render`], [`Tree::fingerprint`])
//! so a ledger can record exactly which budgets were in force, and [`diff`] says what changed
//! between two trees.
//!
//! Ids are short ASCII names (`a-z`, `0-9`, `_`, `-`). Display names belong to the app.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Basis points of the level above: 10,000 is the whole of it.
pub type Bp = u32;
pub const FULL: Bp = 10_000;

/// `amount` times `bp` of a whole, rounded down.
pub fn share_of(amount: u128, bp: Bp) -> u128 {
    amount * u128::from(bp) / u128::from(FULL)
}

/// When a strategy stops opening (`soft`) and when it is flattened (`hard`), as basis points of its
/// own budget. `0 < soft < hard <= FULL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossLimits {
    pub soft: Bp,
    pub hard: Bp,
}

impl Default for LossLimits {
    /// 3% and 6%.
    fn default() -> Self {
        LossLimits {
            soft: 300,
            hard: 600,
        }
    }
}

impl LossLimits {
    /// The loss, in raw units, at which a strategy with `budget` stops opening.
    pub fn soft_amount(&self, budget: u128) -> u128 {
        share_of(budget, self.soft)
    }

    /// The loss at which it is flattened.
    pub fn hard_amount(&self, budget: u128) -> u128 {
        share_of(budget, self.hard)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Strategy {
    pub id: String,
    /// Of its group.
    pub share: Bp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    /// Of the balance.
    pub share: Bp,
    /// Applies to each strategy of the group, against its own budget.
    pub loss: LossLimits,
    pub strategies: Vec<Strategy>,
}

/// What is in use, per strategy, in raw units.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage(BTreeMap<String, u128>);

impl Usage {
    pub fn new() -> Usage {
        Usage::default()
    }

    pub fn with(mut self, strategy: &str, used: u128) -> Usage {
        self.0.insert(strategy.to_owned(), used);
        self
    }

    pub fn strategy(&self, id: &str) -> u128 {
        self.0.get(id).copied().unwrap_or(0)
    }

    /// Everything the group's strategies use.
    pub fn group(&self, g: &Group) -> u128 {
        g.strategies.iter().map(|s| self.strategy(&s.id)).sum()
    }
}

/// What bounds a share, for showing the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// Nothing but zero.
    Nothing,
    /// What is unassigned in the parent.
    Unassigned,
    /// What a node has in use: `who` is a strategy id, or the group's own id when it is the sum.
    InUse { who: String, used: u128 },
    /// The node is already over budget: it may not go lower than it is.
    OverBudget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub value: Bp,
    pub why: Why,
}

/// The shares a node may be set to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    pub min: Bound,
    pub max: Bound,
}

impl Range {
    pub fn contains(&self, bp: Bp) -> bool {
        (self.min.value..=self.max.value).contains(&bp)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetError {
    /// An id that is empty or has characters other than `a-z 0-9 _ -`.
    BadId(String),
    Duplicate(String),
    /// A share above the whole.
    ShareTooBig {
        what: String,
        share: Bp,
    },
    /// The children of `parent` add up to more than it.
    OverAllocated {
        parent: String,
        total: u32,
    },
    BadLossLimits {
        group: String,
        soft: Bp,
        hard: Bp,
    },
    Unknown(String),
    /// A share outside what the rules allow.
    OutOfRange {
        what: String,
        asked: Bp,
        range: Box<Range>,
    },
    Parse {
        line: usize,
        why: String,
    },
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BudgetError::BadId(i) => write!(f, "`{i}` is not a valid id (a-z, 0-9, _ and -)"),
            BudgetError::Duplicate(i) => write!(f, "`{i}` appears twice"),
            BudgetError::ShareTooBig { what, share } => write!(
                f,
                "{what}: a share of {share} is more than the whole ({FULL})"
            ),
            BudgetError::OverAllocated { parent, total } => write!(
                f,
                "the shares under `{parent}` add up to {total}, more than the whole ({FULL})"
            ),
            BudgetError::BadLossLimits { group, soft, hard } => write!(
                f,
                "{group}: loss limits need 0 < soft < hard <= {FULL}, found {soft} and {hard}"
            ),
            BudgetError::Unknown(i) => write!(f, "no such group or strategy `{i}`"),
            BudgetError::OutOfRange { what, asked, range } => write!(
                f,
                "{what}: {asked} is outside {} to {}",
                range.min.value, range.max.value
            ),
            BudgetError::Parse { line, why } => write!(f, "budgets line {line}: {why}"),
        }
    }
}

impl std::error::Error for BudgetError {}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The whole tree. Always valid: it can only be built, edited or parsed into a valid one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    groups: Vec<Group>,
}

impl Tree {
    pub fn new(groups: Vec<Group>) -> Result<Tree, BudgetError> {
        let t = Tree { groups };
        t.validate()?;
        Ok(t)
    }

    fn validate(&self) -> Result<(), BudgetError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut top = 0u32;
        for g in &self.groups {
            if !valid_id(&g.id) {
                return Err(BudgetError::BadId(g.id.clone()));
            }
            if !seen.insert(g.id.clone()) {
                return Err(BudgetError::Duplicate(g.id.clone()));
            }
            if g.share > FULL {
                return Err(BudgetError::ShareTooBig {
                    what: g.id.clone(),
                    share: g.share,
                });
            }
            top += g.share;
            if !(0 < g.loss.soft && g.loss.soft < g.loss.hard && g.loss.hard <= FULL) {
                return Err(BudgetError::BadLossLimits {
                    group: g.id.clone(),
                    soft: g.loss.soft,
                    hard: g.loss.hard,
                });
            }
            let mut inner = 0u32;
            for s in &g.strategies {
                if !valid_id(&s.id) {
                    return Err(BudgetError::BadId(s.id.clone()));
                }
                if !seen.insert(s.id.clone()) {
                    return Err(BudgetError::Duplicate(s.id.clone()));
                }
                if s.share > FULL {
                    return Err(BudgetError::ShareTooBig {
                        what: s.id.clone(),
                        share: s.share,
                    });
                }
                inner += s.share;
            }
            if inner > FULL {
                return Err(BudgetError::OverAllocated {
                    parent: g.id.clone(),
                    total: inner,
                });
            }
        }
        if top > FULL {
            return Err(BudgetError::OverAllocated {
                parent: "workspace".to_owned(),
                total: top,
            });
        }
        Ok(())
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// The group a strategy belongs to.
    pub fn group_of(&self, strategy: &str) -> Option<&Group> {
        self.groups
            .iter()
            .find(|g| g.strategies.iter().any(|s| s.id == strategy))
    }

    pub fn strategy(&self, id: &str) -> Option<&Strategy> {
        self.groups
            .iter()
            .flat_map(|g| &g.strategies)
            .find(|s| s.id == id)
    }

    /// Unassigned at the top of the workspace.
    pub fn unassigned(&self) -> Bp {
        FULL - self.groups.iter().map(|g| g.share).sum::<u32>()
    }

    /// Unassigned inside a group.
    pub fn unassigned_in(&self, group: &str) -> Option<Bp> {
        self.group(group)
            .map(|g| FULL - g.strategies.iter().map(|s| s.share).sum::<u32>())
    }

    /// A group's budget in raw units for a balance.
    pub fn group_budget(&self, balance: u128, group: &str) -> Option<u128> {
        self.group(group).map(|g| share_of(balance, g.share))
    }

    /// A strategy's budget in raw units: its share of its group's budget.
    pub fn strategy_budget(&self, balance: u128, strategy: &str) -> Option<u128> {
        let g = self.group_of(strategy)?;
        let s = self.strategy(strategy)?;
        Some(share_of(share_of(balance, g.share), s.share))
    }

    /// The soft and hard loss amounts for a strategy.
    pub fn loss_amounts(&self, balance: u128, strategy: &str) -> Option<(u128, u128)> {
        let g = self.group_of(strategy)?;
        let b = self.strategy_budget(balance, strategy)?;
        Some((g.loss.soft_amount(b), g.loss.hard_amount(b)))
    }

    /// The smallest `x` in `0..=hi` for which `ok(x)` holds, if `ok` is monotone (false then true).
    fn smallest(hi: Bp, ok: impl Fn(Bp) -> bool) -> Bp {
        let (mut lo, mut hi) = (0, hi);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if ok(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }

    /// What `group`'s share of the balance may be set to.
    pub fn group_range(
        &self,
        group: &str,
        balance: u128,
        usage: &Usage,
    ) -> Result<Range, BudgetError> {
        let g = self
            .group(group)
            .ok_or_else(|| BudgetError::Unknown(group.to_owned()))?;
        let max = g.share + self.unassigned();
        // Every strategy's use must fit its budget. (The group's total then fits too: the
        // strategies' shares add up to at most the whole.)
        let fits =
            |x: Bp, s: &Strategy| share_of(share_of(balance, x), s.share) >= usage.strategy(&s.id);
        let need = Tree::smallest(FULL, |x| g.strategies.iter().all(|s| fits(x, s)));
        // Why the floor is where it is: the strategy that fails just below it.
        let why = match need.checked_sub(1) {
            None => Why::Nothing,
            Some(below) => {
                let s = g
                    .strategies
                    .iter()
                    .find(|s| !fits(below, s))
                    .expect("one strategy fails just below the floor");
                Why::InUse {
                    who: s.id.clone(),
                    used: usage.strategy(&s.id),
                }
            }
        };
        let (min, why) = if need > g.share {
            (g.share, Why::OverBudget)
        } else {
            (need, why)
        };
        Ok(Range {
            min: Bound { value: min, why },
            max: Bound {
                value: max,
                why: Why::Unassigned,
            },
        })
    }

    /// What `strategy`'s share of its group may be set to.
    pub fn strategy_range(
        &self,
        strategy: &str,
        balance: u128,
        usage: &Usage,
    ) -> Result<Range, BudgetError> {
        let g = self
            .group_of(strategy)
            .ok_or_else(|| BudgetError::Unknown(strategy.to_owned()))?;
        let s = self.strategy(strategy).expect("found with its group");
        let gb = share_of(balance, g.share);
        let used = usage.strategy(strategy);
        let max = s.share + self.unassigned_in(&g.id).expect("the group exists");
        let need = Tree::smallest(FULL, |x| share_of(gb, x) >= used);
        let (min, why) = if need > s.share {
            (s.share, Why::OverBudget)
        } else if need > 0 {
            (
                need,
                Why::InUse {
                    who: strategy.to_owned(),
                    used,
                },
            )
        } else {
            (0, Why::Nothing)
        };
        Ok(Range {
            min: Bound { value: min, why },
            max: Bound {
                value: max,
                why: Why::Unassigned,
            },
        })
    }

    /// A tree with `group`'s share set to `bp`, if the rules allow it.
    pub fn with_group_share(
        &self,
        group: &str,
        bp: Bp,
        balance: u128,
        usage: &Usage,
    ) -> Result<Tree, BudgetError> {
        let range = self.group_range(group, balance, usage)?;
        if !range.contains(bp) {
            return Err(BudgetError::OutOfRange {
                what: group.to_owned(),
                asked: bp,
                range: Box::new(range),
            });
        }
        let mut t = self.clone();
        t.groups
            .iter_mut()
            .find(|g| g.id == group)
            .expect("checked")
            .share = bp;
        t.validate()?;
        Ok(t)
    }

    /// A tree with `strategy`'s share of its group set to `bp`, if the rules allow it.
    pub fn with_strategy_share(
        &self,
        strategy: &str,
        bp: Bp,
        balance: u128,
        usage: &Usage,
    ) -> Result<Tree, BudgetError> {
        let range = self.strategy_range(strategy, balance, usage)?;
        if !range.contains(bp) {
            return Err(BudgetError::OutOfRange {
                what: strategy.to_owned(),
                asked: bp,
                range: Box::new(range),
            });
        }
        let mut t = self.clone();
        for g in &mut t.groups {
            if let Some(s) = g.strategies.iter_mut().find(|s| s.id == strategy) {
                s.share = bp;
            }
        }
        t.validate()?;
        Ok(t)
    }

    /// A tree with `group`'s loss limits replaced.
    pub fn with_loss(&self, group: &str, loss: LossLimits) -> Result<Tree, BudgetError> {
        let mut t = self.clone();
        t.groups
            .iter_mut()
            .find(|g| g.id == group)
            .ok_or_else(|| BudgetError::Unknown(group.to_owned()))?
            .loss = loss;
        t.validate()?;
        Ok(t)
    }

    /// The canonical text of the tree:
    ///
    /// ```text
    /// budgets v1
    /// group <id> <share> <soft> <hard>
    /// strategy <group> <id> <share>
    /// ```
    pub fn render(&self) -> String {
        let mut s = String::from("budgets v1\n");
        for g in &self.groups {
            let _ = writeln!(
                s,
                "group {} {} {} {}",
                g.id, g.share, g.loss.soft, g.loss.hard
            );
            for st in &g.strategies {
                let _ = writeln!(s, "strategy {} {} {}", g.id, st.id, st.share);
            }
        }
        s
    }

    /// A short identifier of the canonical text (FNV-1a, 64 bit: for telling versions apart).
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.render().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Read the text form. Blank lines and `#` comments are ignored; a strategy line must follow
    /// its group; the result is validated like any tree.
    pub fn parse(text: &str) -> Result<Tree, BudgetError> {
        let bad = |line: usize, why: String| BudgetError::Parse { line, why };
        let num = |line: usize, t: &str| {
            t.parse::<Bp>()
                .map_err(|_| bad(line, format!("`{t}` is not a whole number")))
        };
        let mut header = false;
        let mut groups: Vec<Group> = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let n = n + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let t: Vec<&str> = line.split_whitespace().collect();
            if !header {
                if t != ["budgets", "v1"] {
                    return Err(bad(n, format!("expected `budgets v1`, found `{line}`")));
                }
                header = true;
                continue;
            }
            match t[..] {
                ["group", id, share, soft, hard] => groups.push(Group {
                    id: id.to_owned(),
                    share: num(n, share)?,
                    loss: LossLimits {
                        soft: num(n, soft)?,
                        hard: num(n, hard)?,
                    },
                    strategies: Vec::new(),
                }),
                ["strategy", group, id, share] => {
                    let share = num(n, share)?;
                    let g = groups.iter_mut().rfind(|g| g.id == group).ok_or_else(|| {
                        bad(
                            n,
                            format!(
                                "strategy `{id}` names group `{group}`, which has not been declared"
                            ),
                        )
                    })?;
                    g.strategies.push(Strategy {
                        id: id.to_owned(),
                        share,
                    });
                }
                _ => return Err(bad(n, format!("`{line}` is not a group or strategy line"))),
            }
        }
        if !header {
            return Err(bad(0, "missing `budgets v1` header".to_owned()));
        }
        Tree::new(groups)
    }
}

/// One difference between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    GroupAdded(String),
    GroupRemoved(String),
    GroupShare {
        id: String,
        from: Bp,
        to: Bp,
    },
    Loss {
        id: String,
        from: LossLimits,
        to: LossLimits,
    },
    StrategyAdded {
        group: String,
        id: String,
    },
    StrategyRemoved {
        group: String,
        id: String,
    },
    StrategyShare {
        group: String,
        id: String,
        from: Bp,
        to: Bp,
    },
}

/// What changed from `a` to `b`: groups in `a`'s order, then groups only in `b`.
pub fn diff(a: &Tree, b: &Tree) -> Vec<Change> {
    let mut out = Vec::new();
    for ga in &a.groups {
        let Some(gb) = b.group(&ga.id) else {
            out.push(Change::GroupRemoved(ga.id.clone()));
            continue;
        };
        if ga.share != gb.share {
            out.push(Change::GroupShare {
                id: ga.id.clone(),
                from: ga.share,
                to: gb.share,
            });
        }
        if ga.loss != gb.loss {
            out.push(Change::Loss {
                id: ga.id.clone(),
                from: ga.loss,
                to: gb.loss,
            });
        }
        for sa in &ga.strategies {
            match gb.strategies.iter().find(|s| s.id == sa.id) {
                None => out.push(Change::StrategyRemoved {
                    group: ga.id.clone(),
                    id: sa.id.clone(),
                }),
                Some(sb) if sb.share != sa.share => out.push(Change::StrategyShare {
                    group: ga.id.clone(),
                    id: sa.id.clone(),
                    from: sa.share,
                    to: sb.share,
                }),
                Some(_) => {}
            }
        }
        for sb in gb
            .strategies
            .iter()
            .filter(|s| !ga.strategies.iter().any(|x| x.id == s.id))
        {
            out.push(Change::StrategyAdded {
                group: ga.id.clone(),
                id: sb.id.clone(),
            });
        }
    }
    for gb in b.groups.iter().filter(|g| a.group(&g.id).is_none()) {
        out.push(Change::GroupAdded(gb.id.clone()));
    }
    out
}

#[cfg(test)]
mod tests;
