//! The null strategy's distribution (E19-S28): what a result has to beat.
//!
//! A run of the null strategy ([`tf_strategy::RandomEntries`]) with one seed is one draw of what random entries with the same
//! exits earn. Many seeds give a distribution; they all run **in one pass** over each day, one definition per seed
//! ([`null_defs`]), so a hundred seeds cost one read of the data and a hundred small strategies. The mean of the distribution
//! after costs is what the exits and the spread alone do: a market with no signal pays the spread and the fees on every round
//! trip, so it is a cost.
//!
//! The seeds are **controls, not trials**. They do not go in the trial registry (ADR 0060): a hundred of them would raise the
//! number of variants tried and deflate every real strategy's Sharpe ratio for nothing. Their figures are taken through a
//! registry made for the call and thrown away.

use std::collections::BTreeSet;

use tf_stats::{Bootstrap, NullVerdict, Registry, against_null};
use tf_strategy::closing_reversal::ParamError;
use tf_strategy::random_entries::RandomEntriesParams;
use tf_universe::Spec;

use super::run::{ResearchError, Results};
use super::stats::variants;
use crate::def::StrategyDef;
use crate::library::random_entries;

/// One null definition for each seed: numbered from `first_id`, named `{prefix}{seed}`, the same universe and parameters
/// but for the seed. A seed given twice is refused (it would be one variant twice).
pub fn null_defs(
    first_id: u16,
    prefix: &str,
    universe: &Spec,
    base: RandomEntriesParams,
    seeds: &[u64],
) -> Result<Vec<StrategyDef>, ParamError> {
    let distinct: BTreeSet<u64> = seeds.iter().copied().collect();
    if distinct.len() != seeds.len() {
        return Err(ParamError("a seed is given twice".into()));
    }
    seeds
        .iter()
        .enumerate()
        .map(|(i, &seed)| {
            let id = u16::try_from(i)
                .ok()
                .and_then(|i| first_id.checked_add(i))
                .ok_or_else(|| ParamError("more strategies than numbers".into()))?;
            random_entries(
                id,
                &format!("{prefix}{seed}"),
                universe.clone(),
                RandomEntriesParams { seed, ..base },
            )
        })
        .collect()
}

/// What the null runs of one results directory say.
#[derive(Clone, Debug, PartialEq)]
pub struct NullDistribution {
    /// Null strategies in the run.
    pub seeds: usize,
    /// Of them, those that made a trade: only these have a mean.
    pub traded: usize,
    /// Trades across all of them.
    pub trades: u64,
    /// The per-trade mean in basis points of each seed that traded, in the order of the definitions.
    pub means_bp: Vec<f64>,
    /// The average of those means: what random entries with these exits earn, after costs.
    pub mean_of_means_bp: f64,
    /// The same weighted by trades: every null trade counted once.
    pub pooled_mean_bp: f64,
    /// The spread of the seeds' means; `None` for fewer than two.
    pub sd_of_means_bp: Option<f64>,
}

impl NullDistribution {
    /// Where a result (a per-trade mean in basis points) falls among the seeds ([`tf_stats::against_null`]).
    pub fn against(&self, observed_bp: f64) -> Option<NullVerdict> {
        against_null(observed_bp, &self.means_bp)
    }
}

/// The distribution of the null strategies `null` (their fingerprints, from [`null_defs`]) in `results`; `None` if none of
/// them made a trade. A fingerprint the run does not have is an error.
pub fn null_distribution(
    results: &Results,
    null: &[u64],
) -> Result<Option<NullDistribution>, ResearchError> {
    let wanted: BTreeSet<u64> = null.iter().copied().collect();
    let all = variants(results)?;
    let mine: Vec<_> = all
        .into_iter()
        .filter(|v| wanted.contains(&v.fingerprint))
        .collect();
    if mine.len() != wanted.len() {
        return Err(ResearchError::Results(
            "the run does not have every null strategy asked for".into(),
        ));
    }
    // A registry made for this call, not the trial registry: the seeds are controls.
    let mut local = Registry::new();
    for v in &mine {
        local.register(v.fingerprint, &v.name, "2000-01-01")?;
    }
    let days = results.dates()?;
    // Only the means are wanted: the least a bootstrap will do.
    let cheap = Bootstrap {
        replicates: 2,
        block: Some(1),
        seed: 0,
    };
    let stats = tf_stats::report(&mine, &days, &local, cheap)?;
    let traded: Vec<_> = stats
        .iter()
        .filter_map(|s| s.bp.mean.map(|m| (m, s.bp.trades)))
        .collect();
    if traded.is_empty() {
        return Ok(None);
    }
    let means: Vec<f64> = traded.iter().map(|t| t.0).collect();
    let trades: u64 = traded.iter().map(|t| t.1).sum();
    let n = means.len() as f64;
    let mean = means.iter().sum::<f64>() / n;
    Ok(Some(NullDistribution {
        seeds: stats.len(),
        traded: means.len(),
        trades,
        pooled_mean_bp: traded.iter().map(|&(m, t)| m * t as f64).sum::<f64>() / trades as f64,
        mean_of_means_bp: mean,
        sd_of_means_bp: (means.len() > 1)
            .then(|| (means.iter().map(|m| (m - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()),
        means_bp: means,
    }))
}
