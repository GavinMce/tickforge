//! The auto-revert policy: take tuning away from an agent that is losing to the shadow.
//!
//! A tuned strategy is judged against a fixed-parameter shadow on the same feed. The
//! policy watches the *relative* curve, tuned equity minus shadow equity, which starts
//! at zero and moves only when the two behave differently. Its **drawdown versus the
//! shadow** is the fall from the relative curve's highest point. When that reaches the
//! configured limit the policy trips, and the caller returns the parameters to baseline
//! ([`crate::ParamStore::revert_events`]).
//!
//! After a trip the peak is reset to the current relative value, so the next trip
//! needs a fresh fall of the full limit from here; the loss that caused this one is
//! not counted twice. Integers only; the equities come from the caller.

/// Why a policy could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    /// A limit of zero would trip on any difference at all.
    ZeroDrawdown,
}

/// A trip: the numbers behind the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trip {
    /// Tuned minus shadow equity now.
    pub relative: i128,
    /// The highest the relative curve had been since the policy was armed.
    pub peak: i128,
    /// `peak - relative`, at least the limit.
    pub drawdown: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevertPolicy {
    max_drawdown: u128,
    peak: i128,
}

impl RevertPolicy {
    /// Trip when the tuned side has fallen `max_drawdown` (raw price units, as the
    /// equities) behind its own best relative showing.
    pub fn new(max_drawdown: u128) -> Result<RevertPolicy, PolicyError> {
        if max_drawdown == 0 {
            return Err(PolicyError::ZeroDrawdown);
        }
        Ok(RevertPolicy {
            max_drawdown,
            peak: 0,
        })
    }

    pub fn max_drawdown(&self) -> u128 {
        self.max_drawdown
    }

    /// The drawdown versus the shadow as of the last observation.
    pub fn peak(&self) -> i128 {
        self.peak
    }

    /// See both equities after an event. Returns a [`Trip`] the first time the limit is
    /// reached; the policy then re-arms from the current relative value.
    pub fn observe(&mut self, tuned_equity: i128, shadow_equity: i128) -> Option<Trip> {
        let relative = tuned_equity - shadow_equity;
        self.peak = self.peak.max(relative);
        let drawdown = (self.peak - relative) as u128;
        if drawdown >= self.max_drawdown {
            let trip = Trip {
                relative,
                peak: self.peak,
                drawdown,
            };
            self.peak = relative;
            Some(trip)
        } else {
            None
        }
    }
}
