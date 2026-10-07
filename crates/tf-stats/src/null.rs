//! What a result has to beat: the distribution of the same result from a strategy with no edge (the null strategy, E19-S28:
//! random entries on the same universe with the same exits and costs), run many times.

use crate::boot::quantile;

/// A result against the null runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NullVerdict {
    pub runs: usize,
    /// Null runs at or above the result.
    pub at_or_above: usize,
    /// `(at_or_above + 1) / (runs + 1)`: the chance of a null run doing this well, never nothing (the result is one more
    /// draw), so its smallest value says how many runs there were.
    pub p_value: f64,
    pub median: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Where `observed` falls among the results of the null runs; `None` with no runs.
pub fn against_null(observed: f64, null: &[f64]) -> Option<NullVerdict> {
    if null.is_empty() {
        return None;
    }
    let mut sorted = null.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at_or_above = sorted.iter().filter(|&&v| v >= observed).count();
    Some(NullVerdict {
        runs: sorted.len(),
        at_or_above,
        p_value: (at_or_above as f64 + 1.0) / (sorted.len() as f64 + 1.0),
        median: quantile(&sorted, 0.5),
        p95: quantile(&sorted, 0.95),
        p99: quantile(&sorted, 0.99),
    })
}
