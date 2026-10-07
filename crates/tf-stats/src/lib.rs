//! Statistics for research results (E19-S14).
//!
//! What a history run can say about a rule, and how sure. Pure: no clock, no I/O but the registry's file, and a seeded
//! generator, so the same trades and the same configuration always give the same numbers (ADR 0060).
//!
//! - [`boot`]: results by day, their standard error from a circular block bootstrap of the days, and the difference of two
//!   results over the same days.
//! - [`sharpe`]: the daily Sharpe ratio and its deflation for the number of variants tried.
//! - [`registry`]: every variant ever run, which is what that number is.
//! - [`report`](mod@report): a variant's figures from its trades (`report`) and a refinement's difference from its plain
//!   version (`paired`); a variant that is not in the registry cannot be reported.
//! - [`null`]: a result against the distribution the null strategy gives.
//! - [`norm`], [`rng`]: the normal distribution and the generator the rest stands on.

pub mod boot;
pub mod norm;
pub mod null;
pub mod registry;
pub mod report;
pub mod rng;
pub mod sharpe;

pub use boot::{BootResult, Bootstrap, DaySeries, paired_bootstrap};
pub use null::{NullVerdict, against_null};
pub use registry::{Registered, Registry, Trial};
pub use report::{MetricStats, Outcome, Paired, Variant, VariantStats, paired, report};
pub use sharpe::{Moments, deflated_sharpe, expected_max_sharpe, moments, probabilistic_sharpe};

/// Why statistics were refused.
#[derive(Debug, PartialEq, Eq)]
pub enum StatsError {
    /// The variant was never entered in the trial registry, so its result cannot be reported.
    NotRegistered {
        name: String,
        fingerprint: u64,
    },
    /// The days of a run are not usable: out of order, or a trade is on a day that is not among them.
    Days(String),
    /// Text that is not what it should be, and why.
    Parse(String),
    Io(String),
}

impl std::fmt::Display for StatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatsError::NotRegistered { name, fingerprint } => write!(
                f,
                "variant {name} ({fingerprint:016x}) is not in the trial registry: register it before its result is reported"
            ),
            StatsError::Days(m) | StatsError::Parse(m) | StatsError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for StatsError {}

#[cfg(test)]
mod tests;
