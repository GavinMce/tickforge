//! Who wants which symbol in Tier 1 (E18-S04).
//!
//! Many strategies share one Tier 1, which holds only a few dozen symbols. A strategy has two ways to
//! lay claim to a symbol:
//!
//! - a **hold** (`pin`): it has a position or a working order in the symbol. A symbol with any hold is
//!   never demoted and never evicted. Holds are counted per strategy, so one strategy letting go does
//!   not release another's.
//! - an **interest** (`request`): it wants to watch the symbol. A symbol with any interest is not
//!   demoted by the cool-down sweep, but it can be *evicted* by a request from a strategy of higher
//!   priority when Tier 1 is full.
//!
//! The rule when a request finds Tier 1 full, in full: the candidates are the promoted symbols that have
//! no hold and have dwelt at least the minimum dwell. Each has a *level*: 0 if nobody has claimed it (it
//! is there on a scanner hit alone), else the highest priority among the strategies interested in it. A
//! request may evict only a candidate whose level is strictly below the requester's priority; among
//! those the victim is the lowest level, then the fewest interested strategies, then the longest
//! since it was last hot, then the lowest id. If there is none, the request is denied and counted.
//! Priorities are `u8`; a strategy that was never given one has [`DEFAULT_PRIORITY`] (1), so it
//! outranks a symbol that is there on a scanner hit alone, and two default strategies never evict each
//! other.

use std::collections::BTreeMap;

use tf_core::InstrumentId;

/// A strategy, by the number in its `StrategyId`.
pub type Owner = u16;

/// What a strategy that was not given a priority has.
pub const DEFAULT_PRIORITY: u8 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Entry {
    owner: Owner,
    hold: bool,
    interest: bool,
}

/// What became of a request for Tier 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    /// The symbol was already in Tier 1; the strategy's interest is recorded.
    Already,
    /// There was room; the symbol was promoted.
    Promoted,
    /// Tier 1 was full; `evicted` was demoted to make room and the symbol promoted.
    PromotedByEviction {
        evicted: InstrumentId,
    },
    Denied(Denied),
}

impl Grant {
    pub fn is_granted(self) -> bool {
        !matches!(self, Grant::Denied(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// Tier 1 is full and no symbol may be evicted for this request.
    Full,
    /// The id is outside the id space.
    Unknown,
    /// The promoter follows a tape and does not decide: membership is what the tape says.
    Following,
}

/// Per strategy counts, for the metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerStats {
    pub requests: u64,
    /// Requests for a symbol already in Tier 1.
    pub already: u64,
    pub promoted: u64,
    /// Promotions that needed an eviction (included in `promoted`).
    pub evicted_for: u64,
    pub denied_full: u64,
    pub denied_other: u64,
    /// Interests lost because the symbol was demoted or evicted.
    pub lost: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Claims {
    by_symbol: BTreeMap<InstrumentId, Vec<Entry>>,
    priority: BTreeMap<Owner, u8>,
    stats: BTreeMap<Owner, OwnerStats>,
    revoked: Vec<(Owner, InstrumentId)>,
}

impl Claims {
    pub(crate) fn set_priority(&mut self, owner: Owner, p: u8) {
        self.priority.insert(owner, p);
    }

    pub(crate) fn priority(&self, owner: Owner) -> u8 {
        self.priority
            .get(&owner)
            .copied()
            .unwrap_or(DEFAULT_PRIORITY)
    }

    pub(crate) fn stat(&mut self, owner: Owner) -> &mut OwnerStats {
        self.stats.entry(owner).or_default()
    }

    pub(crate) fn stats(&self) -> Vec<(Owner, OwnerStats)> {
        self.stats.iter().map(|(o, s)| (*o, *s)).collect()
    }

    fn entry(&mut self, owner: Owner, id: InstrumentId) -> &mut Entry {
        let v = self.by_symbol.entry(id).or_default();
        let at = match v.binary_search_by_key(&owner, |e| e.owner) {
            Ok(i) => i,
            Err(i) => {
                v.insert(
                    i,
                    Entry {
                        owner,
                        hold: false,
                        interest: false,
                    },
                );
                i
            }
        };
        &mut v[at]
    }

    pub(crate) fn set_hold(&mut self, owner: Owner, id: InstrumentId, on: bool) {
        if on || self.has_entry(owner, id) {
            self.entry(owner, id).hold = on;
            self.tidy(id);
        }
    }

    pub(crate) fn set_interest(&mut self, owner: Owner, id: InstrumentId, on: bool) {
        if on || self.has_entry(owner, id) {
            self.entry(owner, id).interest = on;
            self.tidy(id);
        }
    }

    fn has_entry(&self, owner: Owner, id: InstrumentId) -> bool {
        self.by_symbol
            .get(&id)
            .is_some_and(|v| v.iter().any(|e| e.owner == owner))
    }

    fn tidy(&mut self, id: InstrumentId) {
        if let Some(v) = self.by_symbol.get_mut(&id) {
            v.retain(|e| e.hold || e.interest);
            if v.is_empty() {
                self.by_symbol.remove(&id);
            }
        }
    }

    pub(crate) fn has_hold(&self, id: InstrumentId) -> bool {
        self.by_symbol
            .get(&id)
            .is_some_and(|v| v.iter().any(|e| e.hold))
    }

    pub(crate) fn is_held_by(&self, owner: Owner, id: InstrumentId) -> bool {
        self.by_symbol
            .get(&id)
            .is_some_and(|v| v.iter().any(|e| e.owner == owner && e.hold))
    }

    pub(crate) fn has_interest_of(&self, owner: Owner, id: InstrumentId) -> bool {
        self.by_symbol
            .get(&id)
            .is_some_and(|v| v.iter().any(|e| e.owner == owner && e.interest))
    }

    /// Anyone's claim of either kind.
    pub(crate) fn is_claimed(&self, id: InstrumentId) -> bool {
        self.by_symbol.contains_key(&id)
    }

    /// 0 if unclaimed, else the highest priority among those interested, and how many are. (Asked only
    /// of symbols nobody holds, whose every entry is an interest.)
    pub(crate) fn level(&self, id: InstrumentId) -> (u8, usize) {
        match self.by_symbol.get(&id) {
            None => (0, 0),
            Some(v) => {
                let mut max = 0;
                let mut n = 0;
                for e in v {
                    max = max.max(self.priority(e.owner));
                    n += 1;
                }
                (max, n)
            }
        }
    }

    /// The symbol left Tier 1: its interests are gone (holds stay: a tape that demotes a held symbol
    /// is replayed against strategies that will pin it again).
    pub(crate) fn revoke_interests(&mut self, id: InstrumentId) {
        let Some(v) = self.by_symbol.get(&id) else {
            return;
        };
        let lost: Vec<Owner> = v.iter().filter(|e| e.interest).map(|e| e.owner).collect();
        for o in lost {
            self.revoked.push((o, id));
            self.stat(o).lost += 1;
            self.set_interest(o, id, false);
        }
    }

    pub(crate) fn drain_revoked(&mut self) -> Vec<(Owner, InstrumentId)> {
        std::mem::take(&mut self.revoked)
    }
}
