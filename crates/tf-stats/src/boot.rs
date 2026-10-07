//! Day-level standard errors.
//!
//! Trades on one day share the market's move, so they are not independent and the number of trades says little about how
//! sure an average is. The unit here is the day: a result is a ratio of totals (sum of trade results over the number of
//! trades) over days, and its standard error comes from resampling whole days, in blocks of consecutive days so that a
//! run of days that resemble each other is kept together (a circular block bootstrap, Politis and Romano 1992).

use crate::rng::SplitMix64;

/// How a bootstrap is run. The seed is part of the result: the same days and the same configuration give the same
/// numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bootstrap {
    pub replicates: usize,
    /// Days in a block; `None` is the cube root of the number of days, rounded up.
    pub block: Option<usize>,
    pub seed: u64,
}

impl Default for Bootstrap {
    fn default() -> Bootstrap {
        Bootstrap {
            replicates: 2_000,
            block: None,
            seed: 0x7F4A_7C15,
        }
    }
}

/// What a bootstrap says about an estimate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BootResult {
    pub estimate: f64,
    /// The standard deviation of the estimate over the resampled day sets.
    pub se: f64,
    /// `estimate / se`; `None` when the standard error is nothing.
    pub t: Option<f64>,
    /// The 2.5th and 97.5th percentiles of the resampled estimates.
    pub lo: f64,
    pub hi: f64,
    /// Resamples that gave an estimate (a resample with no trade in it gives none).
    pub replicates: usize,
    pub block: usize,
    pub days: usize,
}

/// A result summed by day: for each day of the run, the sum of the trades' results and how many trades there were.
/// Days with no trade are in it, as zero and zero: a day on which the rule found nothing is still a day.
#[derive(Clone, Debug, PartialEq)]
pub struct DaySeries {
    sums: Vec<f64>,
    counts: Vec<u32>,
}

impl DaySeries {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (f64, u32)>) -> DaySeries {
        let (sums, counts) = pairs.into_iter().unzip();
        DaySeries { sums, counts }
    }

    pub fn days(&self) -> usize {
        self.sums.len()
    }

    pub fn trades(&self) -> u64 {
        self.counts.iter().map(|&c| u64::from(c)).sum()
    }

    pub fn sums(&self) -> &[f64] {
        &self.sums
    }

    pub fn counts(&self) -> &[u32] {
        &self.counts
    }

    /// The ratio of the totals over the days at `idx`; `None` if they hold no trade.
    fn ratio(&self, idx: impl Iterator<Item = usize>) -> Option<f64> {
        let (mut s, mut n) = (0.0, 0u64);
        for i in idx {
            s += self.sums[i];
            n += u64::from(self.counts[i]);
        }
        (n > 0).then(|| s / n as f64)
    }

    /// The average result per trade; `None` if there is no trade.
    pub fn mean(&self) -> Option<f64> {
        self.ratio(0..self.days())
    }

    /// The standard error of [`DaySeries::mean`] from the days as clusters, the textbook estimate for a ratio of totals:
    /// `sqrt(D / (D - 1) * sum over days of (s_d - m n_d)^2) / sum of n`. `None` for fewer than two days or no trade.
    pub fn cluster_se(&self) -> Option<f64> {
        let d = self.days();
        let m = self.mean()?;
        if d < 2 {
            return None;
        }
        let n = self.trades() as f64;
        let ss: f64 = self
            .sums
            .iter()
            .zip(&self.counts)
            .map(|(&s, &c)| (s - m * f64::from(c)).powi(2))
            .sum();
        Some(no_noise((d as f64 / (d as f64 - 1.0) * ss).sqrt() / n, m))
    }

    /// The mean with its standard error, 95 percent interval and t-statistic from a circular block bootstrap of the days.
    pub fn bootstrap(&self, cfg: Bootstrap) -> Option<BootResult> {
        let estimate = self.mean()?;
        run(self.days(), estimate, cfg, |idx| {
            self.ratio(idx.iter().copied())
        })
    }
}

/// Two results over the same days, for the difference of their means.
///
/// Only the days on which both traded are used, and each resample of days is applied to both, so that what the two share
/// (the market's move on those days) cancels in the difference.
pub fn paired_bootstrap(a: &DaySeries, b: &DaySeries, cfg: Bootstrap) -> Option<BootResult> {
    if a.days() != b.days() {
        return None;
    }
    let common: Vec<usize> = (0..a.days())
        .filter(|&i| a.counts[i] > 0 && b.counts[i] > 0)
        .collect();
    let diff = |idx: &mut dyn Iterator<Item = usize>| -> Option<f64> {
        let v: Vec<usize> = idx.collect();
        Some(a.ratio(v.iter().copied())? - b.ratio(v.iter().copied())?)
    };
    let estimate = diff(&mut common.iter().copied())?;
    run(common.len(), estimate, cfg, |idx| {
        diff(&mut idx.iter().map(|&i| common[i]))
    })
}

/// A standard error that is only the rounding of the arithmetic is nothing: three trades of -10.29 basis points on three
/// days average -10.29 however the days are drawn, but the sums are not exact, and an error of 1e-14 would make a
/// t-statistic of 1e15 out of it. Anything within 1e-12 of the estimate, relatively, is taken to be no spread at all.
fn no_noise(se: f64, around: f64) -> f64 {
    if se <= 1e-12 * (1.0 + around.abs()) {
        0.0
    } else {
        se
    }
}

/// The number of days in a block when none is asked for.
pub fn default_block(days: usize) -> usize {
    (days as f64).cbrt().ceil() as usize
}

/// The days of one resample: blocks of `block` consecutive days (wrapping round the end) from random starts, until there
/// are as many days as there were.
fn resample(rng: &mut SplitMix64, days: usize, block: usize, out: &mut Vec<usize>) {
    out.clear();
    while out.len() < days {
        let start = rng.below(days);
        for k in 0..block {
            if out.len() == days {
                break;
            }
            out.push((start + k) % days);
        }
    }
}

fn run(
    days: usize,
    estimate: f64,
    cfg: Bootstrap,
    stat: impl Fn(&[usize]) -> Option<f64>,
) -> Option<BootResult> {
    if days < 2 || cfg.replicates < 2 {
        return None;
    }
    let block = cfg
        .block
        .unwrap_or_else(|| default_block(days))
        .clamp(1, days);
    let mut rng = SplitMix64::new(cfg.seed);
    let mut idx = Vec::with_capacity(days);
    let mut stats = Vec::with_capacity(cfg.replicates);
    for _ in 0..cfg.replicates {
        resample(&mut rng, days, block, &mut idx);
        if let Some(v) = stat(&idx) {
            stats.push(v);
        }
    }
    let n = stats.len();
    if n < 2 {
        return None;
    }
    let mean = stats.iter().sum::<f64>() / n as f64;
    let var = stats.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    let se = no_noise(var.sqrt(), mean);
    stats.sort_by(f64::total_cmp);
    Some(BootResult {
        estimate,
        se,
        t: (se > 0.0).then(|| estimate / se),
        lo: quantile(&stats, 0.025),
        hi: quantile(&stats, 0.975),
        replicates: n,
        block,
        days,
    })
}

/// The `p` quantile of values in ascending order by linear interpolation between the two nearest (R's type 7).
pub fn quantile(sorted: &[f64], p: f64) -> f64 {
    let h = (sorted.len() - 1) as f64 * p;
    let lo = h.floor() as usize;
    let frac = h - lo as f64;
    match sorted.get(lo + 1) {
        Some(next) => sorted[lo] + frac * (next - sorted[lo]),
        None => sorted[lo],
    }
}
