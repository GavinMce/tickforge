//! A variant's results from its trades, and a refinement's difference from its plain version.
//!
//! Every result is per trade, in basis points of the money put in (always there) and in R, the money at risk (only for
//! trades whose opening order stated a stop). Standard errors are by day ([`crate::boot`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::StatsError;
use crate::boot::{BootResult, Bootstrap, DaySeries, paired_bootstrap};
use crate::registry::Registry;
use crate::sharpe::{Moments, deflated_sharpe, moments, variance};

/// One round trip, as far as statistics need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub symbol: String,
    pub entry_ts: u64,
    pub exit_ts: u64,
    /// Net over the entry cost, in hundredths of a basis point.
    pub net_bps_x100: i64,
    /// Net over the money at risk, in thousandths of R.
    pub r_milli: Option<i64>,
}

/// A variant's trades over the days of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub fingerprint: u64,
    pub name: String,
    pub outcomes: Vec<Outcome>,
}

/// An average per trade with what is known of how sure it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricStats {
    /// Trades the average is over.
    pub trades: u64,
    pub mean: Option<f64>,
    /// The standard error from the days as clusters, an analytic cross-check of the bootstrap's.
    pub cluster_se: Option<f64>,
    pub boot: Option<BootResult>,
}

impl MetricStats {
    fn of(series: &DaySeries, cfg: Bootstrap) -> MetricStats {
        MetricStats {
            trades: series.trades(),
            mean: series.mean(),
            cluster_se: series.cluster_se(),
            boot: series.bootstrap(cfg),
        }
    }
}

/// What the trades of one variant say.
#[derive(Clone, Debug, PartialEq)]
pub struct VariantStats {
    pub fingerprint: u64,
    pub name: String,
    /// Variants in the registry: the trials the deflated Sharpe ratio allows for.
    pub trials: usize,
    pub days: usize,
    pub trades: u64,
    /// Per trade, in basis points.
    pub bp: MetricStats,
    /// Per trade, in R, over the trades that have one.
    pub r: MetricStats,
    /// The share of trades that made money (a trade of nothing is not a win).
    pub hit_rate: Option<f64>,
    /// The average win over the average loss, in basis points; `None` without both.
    pub payoff: Option<f64>,
    /// The deepest fall of the running total of basis points below its highest, trades in the order they closed.
    pub max_drawdown_bp: f64,
    /// The moments of the day-by-day total in basis points (days with no trade are zero).
    pub daily: Option<Moments>,
    /// The deflated Sharpe ratio of the daily results; `None` without a registry of two or more variants, or with fewer
    /// than two variants in this report whose Sharpe ratios could be measured.
    pub deflated_sharpe: Option<f64>,
}

fn day_index(days: &[String]) -> Result<BTreeMap<&str, usize>, StatsError> {
    if days.windows(2).any(|w| w[0] >= w[1]) {
        return Err(StatsError::Days(
            "the days of the run are not in order, each once".into(),
        ));
    }
    Ok(days
        .iter()
        .enumerate()
        .map(|(i, d)| (d.as_str(), i))
        .collect())
}

fn require(registry: &Registry, v: &Variant) -> Result<(), StatsError> {
    if registry.contains(v.fingerprint) {
        Ok(())
    } else {
        Err(StatsError::NotRegistered {
            name: v.name.clone(),
            fingerprint: v.fingerprint,
        })
    }
}

/// A variant's results summed by day: of `value` over the outcomes that have one.
fn series(
    v: &Variant,
    ix: &BTreeMap<&str, usize>,
    value: impl Fn(&Outcome) -> Option<f64>,
) -> Result<DaySeries, StatsError> {
    let mut sums = vec![0.0; ix.len()];
    let mut counts = vec![0u32; ix.len()];
    for o in &v.outcomes {
        let &i = ix.get(o.day.as_str()).ok_or_else(|| {
            StatsError::Days(format!(
                "{} has a trade on {}, which is not a day of the run",
                v.name, o.day
            ))
        })?;
        if let Some(x) = value(o) {
            sums[i] += x;
            counts[i] += 1;
        }
    }
    Ok(DaySeries::from_pairs(sums.into_iter().zip(counts)))
}

fn bp(o: &Outcome) -> Option<f64> {
    Some(o.net_bps_x100 as f64 / 100.0)
}

fn r(o: &Outcome) -> Option<f64> {
    o.r_milli.map(|m| m as f64 / 1000.0)
}

/// Every variant's results. `days` are the days of the run, in order: a day on which a variant found nothing is a day of
/// zero for it. A variant that is not in `registry` is an error, and so is a trade on a day that is not in `days`.
pub fn report(
    variants: &[Variant],
    days: &[String],
    registry: &Registry,
    boot: Bootstrap,
) -> Result<Vec<VariantStats>, StatsError> {
    let ix = day_index(days)?;
    let mut out = Vec::with_capacity(variants.len());
    for v in variants {
        require(registry, v)?;
        let bps = series(v, &ix, bp)?;
        let rs = series(v, &ix, r)?;
        let mut closed: Vec<&Outcome> = v.outcomes.iter().collect();
        closed.sort_by(|a, b| {
            (&a.day, a.exit_ts, a.entry_ts, &a.symbol)
                .cmp(&(&b.day, b.exit_ts, b.entry_ts, &b.symbol))
        });
        let (mut total, mut peak, mut max_dd) = (0.0_f64, 0.0_f64, 0.0_f64);
        let (mut wins, mut losses) = ((0u64, 0.0_f64), (0u64, 0.0_f64));
        for o in &closed {
            let x = o.net_bps_x100 as f64 / 100.0;
            total += x;
            peak = peak.max(total);
            max_dd = max_dd.max(peak - total);
            if o.net_bps_x100 > 0 {
                wins = (wins.0 + 1, wins.1 + x);
            } else if o.net_bps_x100 < 0 {
                losses = (losses.0 + 1, losses.1 - x);
            }
        }
        let n = closed.len() as u64;
        out.push(VariantStats {
            fingerprint: v.fingerprint,
            name: v.name.clone(),
            trials: registry.len(),
            days: days.len(),
            trades: n,
            bp: MetricStats::of(&bps, boot),
            r: MetricStats::of(&rs, boot),
            hit_rate: (n > 0).then(|| wins.0 as f64 / n as f64),
            payoff: (wins.0 > 0 && losses.0 > 0)
                .then(|| (wins.1 / wins.0 as f64) / (losses.1 / losses.0 as f64)),
            max_drawdown_bp: max_dd,
            daily: moments(bps.sums()),
            deflated_sharpe: None,
        });
    }
    // The spread of Sharpe ratios among the trials, estimated from the variants in this report.
    let sharpes: Vec<f64> = out
        .iter()
        .filter_map(|s| s.daily.map(|m| m.sharpe()))
        .collect();
    if let Some(var) = variance(&sharpes) {
        for s in &mut out {
            s.deflated_sharpe = s
                .daily
                .and_then(|m| deflated_sharpe(&m, registry.len(), var));
        }
    }
    Ok(out)
}

/// A refinement against its plain version.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paired {
    /// Days on which both took a trade: the days the difference is over.
    pub common_days: usize,
    /// The refinement's average per trade less the plain version's, in basis points.
    pub bp: Option<BootResult>,
    /// The same in R, over the days on which both have a trade with one.
    pub r: Option<BootResult>,
    /// The refinement's trades whose signal (day, symbol, entry time) the plain version also traded.
    pub shared_signals: u64,
    /// The refinement's trades the plain version did not take: not a refinement of it if there are any.
    pub other_signals: u64,
}

/// The paired difference of a refinement from its plain version, with its own standard error: the days on which both
/// traded are resampled together, so the market's move on those days cancels. Both must be in `registry`.
pub fn paired(
    refined: &Variant,
    plain: &Variant,
    days: &[String],
    registry: &Registry,
    boot: Bootstrap,
) -> Result<Paired, StatsError> {
    require(registry, refined)?;
    require(registry, plain)?;
    let ix = day_index(days)?;
    let signals: BTreeSet<(&str, &str, u64)> = plain
        .outcomes
        .iter()
        .map(|o| (o.day.as_str(), o.symbol.as_str(), o.entry_ts))
        .collect();
    let shared = refined
        .outcomes
        .iter()
        .filter(|o| signals.contains(&(o.day.as_str(), o.symbol.as_str(), o.entry_ts)))
        .count() as u64;
    let (a, b) = (series(refined, &ix, bp)?, series(plain, &ix, bp)?);
    let (ar, br) = (series(refined, &ix, r)?, series(plain, &ix, r)?);
    Ok(Paired {
        common_days: (0..days.len())
            .filter(|&i| a.counts()[i] > 0 && b.counts()[i] > 0)
            .count(),
        bp: paired_bootstrap(&a, &b, boot),
        r: paired_bootstrap(&ar, &br, boot),
        shared_signals: shared,
        other_signals: refined.outcomes.len() as u64 - shared,
    })
}
