//! What a results directory says, by the numbers of `tf-stats` (E19-S14): the runner's records as that crate's input, and
//! the three steps around it. A strategy is entered in the trial registry **before** it is run ([`register_defs`]), so that
//! a variant that was tried and did nothing is still counted; a result is reported only for variants that are in it.

use std::collections::BTreeMap;

use tf_stats::{Bootstrap, Outcome, Paired, Registry, Variant, VariantStats};

use super::run::{ResearchError, Results};
use super::trips::Trip;
use crate::def::StrategyDef;

/// A trip as statistics see it.
pub fn outcome(t: &Trip) -> Outcome {
    Outcome {
        day: t.day.clone(),
        symbol: t.symbol.clone(),
        entry_ts: t.entry_ts,
        exit_ts: t.exit_ts,
        net_bps_x100: t.net_bps_x100,
        r_milli: t.r_milli,
    }
}

/// Enter the definitions about to be run in the trial registry, as first run on `date` (`YYYY-MM-DD`). Every one is
/// entered, whether or not it will trade. Returns how many were new.
pub fn register_defs(
    registry: &mut Registry,
    defs: &[StrategyDef],
    date: &str,
) -> Result<usize, ResearchError> {
    let mut new = 0;
    for d in defs {
        if registry.register(d.fingerprint(), &d.name, date)? == tf_stats::Registered::New {
            new += 1;
        }
    }
    Ok(new)
}

/// The variants of a run with their trades: one for every strategy in its configuration, in that order, with the trades
/// whose variant it is. A trade of a variant the configuration does not list is an error.
pub fn variants(results: &Results) -> Result<Vec<Variant>, ResearchError> {
    group(results.definitions()?, &results.trips()?)
}

/// The trips shared out among the strategies listed, each with the trades of its own fingerprint.
pub(crate) fn group(
    defs: Vec<(u64, String)>,
    trips: &[Trip],
) -> Result<Vec<Variant>, ResearchError> {
    let mut by_fp: BTreeMap<u64, Vec<Outcome>> = BTreeMap::new();
    for (fp, _) in &defs {
        by_fp.entry(*fp).or_default();
    }
    for t in trips {
        by_fp
            .get_mut(&t.variant)
            .ok_or_else(|| {
                ResearchError::Results(format!(
                    "a trip of {} ({:016x}) on {} is of a strategy the configuration does not list",
                    t.name, t.variant, t.day
                ))
            })?
            .push(outcome(t));
    }
    Ok(defs
        .into_iter()
        .map(|(fp, name)| Variant {
            fingerprint: fp,
            outcomes: by_fp.remove(&fp).unwrap_or_default(),
            name,
        })
        .collect())
}

/// The figures of every variant of a run ([`tf_stats::report`]) over the days it ran.
pub fn report_results(
    results: &Results,
    registry: &Registry,
    boot: Bootstrap,
) -> Result<Vec<VariantStats>, ResearchError> {
    let days = results.dates()?;
    Ok(tf_stats::report(
        &variants(results)?,
        &days,
        registry,
        boot,
    )?)
}

/// A refinement of a run against its plain version, by the fingerprints of the two ([`tf_stats::paired`]).
pub fn paired_results(
    results: &Results,
    refined: u64,
    plain: u64,
    registry: &Registry,
    boot: Bootstrap,
) -> Result<Paired, ResearchError> {
    let days = results.dates()?;
    let vs = variants(results)?;
    let find = |fp: u64| {
        vs.iter()
            .find(|v| v.fingerprint == fp)
            .ok_or_else(|| ResearchError::Results(format!("the run has no strategy {fp:016x}")))
    };
    Ok(tf_stats::paired(
        find(refined)?,
        find(plain)?,
        &days,
        registry,
        boot,
    )?)
}
