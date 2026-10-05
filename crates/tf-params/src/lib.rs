//! A store of strategy parameters that an agent may tune, within hard bounds.
//!
//! Live tuning is allowed only through this store and only inside what each
//! parameter declares: `min`, `max`, the largest single `max_step`, a `cooldown`
//! between changes, and the `scope` it may be set at. Anything else is refused
//! with a reason. Risk limits are not here at all (`tf-risk` fixes them at
//! construction); a parameter with `max_step` 0 can never change.
//!
//! **Changes are events.** A proposal is checked by [`ParamStore::check`], which
//! changes nothing and returns the [`ParamChange`] event to put on the tape.
//! [`ParamStore::apply`] is the only way state changes, and it is what a replay
//! of the tape does too, so a session replays with exactly the parameters it ran
//! with. `apply` re-checks everything, so two proposals checked against the same
//! state cannot both land if together they break a bound, a step or a cooldown.
//!
//! **Entry-time values.** Every applied change bumps [`ParamStore::revision`]. A
//! strategy that opens a position records the revision and reads the parameters
//! that govern its exit with [`ParamStore::value_at`], so a later change applies
//! to new entries only.
//!
//! Values are `i64` (booleans as 0/1, prices as raw units). Per-instrument values
//! override the global one for that instrument and persist until changed; a later
//! global change does not clear them. History is kept in memory: changes are rare
//! by construction (the cooldown), but a store that is never restarted grows with
//! them.
//!
//! The event carries a reason code and an evidence id, not free text; text belongs
//! in a journal beside the tape keyed by the event's `seq`. Integers only, time
//! from the caller.

use std::collections::BTreeMap;

use tf_core::{Event, Header, InstrumentId, Nanos, ParamChange, ParamScope, ProviderId};

/// Index of a parameter in the store's declaration list.
pub type ParamId = u16;

/// At what level a parameter may be set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One value for everything.
    Global,
    /// A global value that individual instruments may override.
    PerInstrument,
}

/// What a parameter is allowed to do. `baseline` is the value it starts at and what
/// a revert returns to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub baseline: i64,
    pub min: i64,
    pub max: i64,
    /// Largest single change. Zero means the parameter cannot be changed.
    pub max_step: u64,
    /// Least time between changes of the same parameter at the same target.
    pub cooldown: Nanos,
    pub scope: Scope,
}

/// What a proposal sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Global,
    Instrument(InstrumentId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub param: ParamId,
    pub target: Target,
    pub value: i64,
    pub proposer: u16,
    pub reason: u16,
    pub evidence: u64,
}

/// Why a proposal (or a replayed change) was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    UnknownParam(ParamId),
    /// The parameter is global only and the target is an instrument.
    ScopeNotAllowed,
    /// The parameter cannot be changed at all (`max_step` is zero).
    Frozen,
    OutOfBounds {
        min: i64,
        max: i64,
    },
    StepTooLarge {
        step: u64,
        max_step: u64,
    },
    /// Changed too recently; allowed again at `until`.
    Cooldown {
        until: Nanos,
    },
    /// The value is already the current one.
    NoChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecError {
    /// `min` is above `max`.
    BadBounds(&'static str),
    /// The baseline is outside its own bounds.
    BaselineOutOfBounds(&'static str),
    DuplicateName(&'static str),
    TooManyParams,
}

/// A change that has been applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The revision the store reached by applying it (1 for the first change).
    pub revision: u64,
    pub ts: Nanos,
    pub param: ParamId,
    pub target: Target,
    pub old: i64,
    pub new: i64,
    pub proposer: u16,
    pub reason: u16,
    pub evidence: u64,
    pub seq: u64,
}

pub struct ParamStore {
    specs: Vec<ParamSpec>,
    global: Vec<i64>,
    overrides: BTreeMap<(ParamId, InstrumentId), i64>,
    last_change: BTreeMap<(ParamId, Target), Nanos>,
    history: Vec<Applied>,
    next_seq: u64,
}

fn target_of(c: &ParamChange) -> Target {
    match c.scope {
        ParamScope::Global => Target::Global,
        ParamScope::Instrument => Target::Instrument(c.hdr.instrument),
    }
}

impl ParamStore {
    pub fn new(specs: Vec<ParamSpec>) -> Result<ParamStore, SpecError> {
        if specs.len() > usize::from(ParamId::MAX) {
            return Err(SpecError::TooManyParams);
        }
        for (i, s) in specs.iter().enumerate() {
            if s.min > s.max {
                return Err(SpecError::BadBounds(s.name));
            }
            if s.baseline < s.min || s.baseline > s.max {
                return Err(SpecError::BaselineOutOfBounds(s.name));
            }
            if specs[..i].iter().any(|o| o.name == s.name) {
                return Err(SpecError::DuplicateName(s.name));
            }
        }
        Ok(ParamStore {
            global: specs.iter().map(|s| s.baseline).collect(),
            specs,
            overrides: BTreeMap::new(),
            last_change: BTreeMap::new(),
            history: Vec::new(),
            next_seq: 0,
        })
    }

    pub fn specs(&self) -> &[ParamSpec] {
        &self.specs
    }

    /// The id of the parameter called `name`.
    pub fn id_of(&self, name: &str) -> Option<ParamId> {
        self.specs
            .iter()
            .position(|s| s.name == name)
            .map(|i| i as ParamId)
    }

    /// Changes applied so far. A position records this when it opens.
    pub fn revision(&self) -> u64 {
        self.history.len() as u64
    }

    pub fn history(&self) -> &[Applied] {
        &self.history
    }

    /// The global value now. Panics on an id that is not in the store.
    pub fn value(&self, param: ParamId) -> i64 {
        self.global[usize::from(param)]
    }

    /// The value for `instrument` now: its override if it has one, else the global.
    pub fn value_for(&self, param: ParamId, instrument: InstrumentId) -> i64 {
        self.overrides
            .get(&(param, instrument))
            .copied()
            .unwrap_or_else(|| self.value(param))
    }

    /// The value for `instrument` as it stood after `revision` changes: what a
    /// position opened then was governed by.
    pub fn value_at(&self, param: ParamId, instrument: InstrumentId, revision: u64) -> i64 {
        let upto = (revision as usize).min(self.history.len());
        let mut global = self.specs[usize::from(param)].baseline;
        let mut over = None;
        for a in &self.history[..upto] {
            if a.param != param {
                continue;
            }
            match a.target {
                Target::Global => global = a.new,
                Target::Instrument(i) if i == instrument => over = Some(a.new),
                Target::Instrument(_) => {}
            }
        }
        over.unwrap_or(global)
    }

    fn current(&self, param: ParamId, target: Target) -> i64 {
        match target {
            Target::Global => self.value(param),
            Target::Instrument(i) => self.value_for(param, i),
        }
    }

    fn validate(
        &self,
        param: ParamId,
        target: Target,
        value: i64,
        now: Nanos,
    ) -> Result<(), Reject> {
        let spec = self
            .specs
            .get(usize::from(param))
            .ok_or(Reject::UnknownParam(param))?;
        if matches!(target, Target::Instrument(_)) && spec.scope == Scope::Global {
            return Err(Reject::ScopeNotAllowed);
        }
        if spec.max_step == 0 {
            return Err(Reject::Frozen);
        }
        if value < spec.min || value > spec.max {
            return Err(Reject::OutOfBounds {
                min: spec.min,
                max: spec.max,
            });
        }
        let cur = self.current(param, target);
        if value == cur {
            return Err(Reject::NoChange);
        }
        let step = (i128::from(value) - i128::from(cur)).unsigned_abs();
        if step > u128::from(spec.max_step) {
            return Err(Reject::StepTooLarge {
                step: step.min(u128::from(u64::MAX)) as u64,
                max_step: spec.max_step,
            });
        }
        if let Some(&last) = self.last_change.get(&(param, target)) {
            let until = last.saturating_add(spec.cooldown);
            if now < until {
                return Err(Reject::Cooldown { until });
            }
        }
        Ok(())
    }

    /// Check a proposal at time `now` without changing anything. On success, the
    /// event to append to the tape; feed it to [`ParamStore::apply`] (live, once it
    /// is on the tape) and on replay.
    pub fn check(&self, p: &Proposal, now: Nanos) -> Result<ParamChange, Reject> {
        self.validate(p.param, p.target, p.value, now)?;
        let (scope, instrument) = match p.target {
            Target::Global => (ParamScope::Global, 0),
            Target::Instrument(i) => (ParamScope::Instrument, i),
        };
        Ok(ParamChange {
            hdr: Header {
                ts_event: now,
                ts_recv: now,
                seq: self.next_seq,
                instrument,
                provider: ProviderId::Internal,
            },
            param: p.param,
            scope,
            proposer: p.proposer,
            reason: p.reason,
            new_value: p.value,
            evidence: p.evidence,
        })
    }

    /// Apply a change event. Checked again against the current state, so it is the
    /// single place state changes, live and in replay. Nothing changes on error.
    pub fn apply(&mut self, c: &ParamChange) -> Result<(), Reject> {
        let target = target_of(c);
        let now = c.hdr.ts_recv;
        self.validate(c.param, target, c.new_value, now)?;
        let old = self.current(c.param, target);
        match target {
            Target::Global => self.global[usize::from(c.param)] = c.new_value,
            Target::Instrument(i) => {
                self.overrides.insert((c.param, i), c.new_value);
            }
        }
        self.last_change.insert((c.param, target), now);
        self.next_seq = self.next_seq.max(c.hdr.seq.saturating_add(1));
        self.history.push(Applied {
            revision: self.history.len() as u64 + 1,
            ts: now,
            param: c.param,
            target,
            old,
            new: c.new_value,
            proposer: c.proposer,
            reason: c.reason,
            evidence: c.evidence,
            seq: c.hdr.seq,
        });
        Ok(())
    }

    /// Apply an event if it is a parameter change; anything else is ignored.
    pub fn apply_event(&mut self, ev: &Event) -> Option<Result<(), Reject>> {
        match ev {
            Event::ParamChange(c) => Some(self.apply(c)),
            _ => None,
        }
    }

    /// [`ParamStore::check`] then [`ParamStore::apply`], for a proposer that owns the
    /// stream. Returns the event it applied, to be written to the tape.
    pub fn propose(&mut self, p: &Proposal, now: Nanos) -> Result<ParamChange, Reject> {
        let ev = self.check(p, now)?;
        self.apply(&ev)?;
        Ok(ev)
    }
}

#[cfg(test)]
mod tests;
