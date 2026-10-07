//! The Sharpe ratio of a daily result and its correction for the number of things tried.
//!
//! Picking the best of many variants overstates it: the more tried, the higher the best of them is by chance. The deflated
//! Sharpe ratio (Bailey and Lopez de Prado, Journal of Portfolio Management 2014) is the probability that a variant's true
//! Sharpe ratio is above what the best of that many unskilled trials would show, allowing for how few days there are and
//! for skew and fat tails in the daily results.

use crate::norm::{cdf, inv_cdf};

/// The Euler-Mascheroni constant.
const EULER_GAMMA: f64 = 0.577_215_664_901_532_9;

/// The first four moments of a series of daily results.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moments {
    pub n: usize,
    pub mean: f64,
    /// With `n - 1` in the divisor.
    pub sd: f64,
    /// Fisher-Pearson, with `n` in the divisor.
    pub skew: f64,
    /// Pearson's, not the excess over 3.
    pub kurt: f64,
}

impl Moments {
    /// The ratio of the mean to the standard deviation, per day (not annualised).
    pub fn sharpe(&self) -> f64 {
        self.mean / self.sd
    }
}

/// Moments of a series; `None` for fewer than three values or no spread.
pub fn moments(x: &[f64]) -> Option<Moments> {
    let n = x.len();
    if n < 3 {
        return None;
    }
    let nf = n as f64;
    let mean = x.iter().sum::<f64>() / nf;
    let m2 = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / nf;
    if m2 <= 0.0 {
        return None;
    }
    let m3 = x.iter().map(|v| (v - mean).powi(3)).sum::<f64>() / nf;
    let m4 = x.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / nf;
    Some(Moments {
        n,
        mean,
        sd: (m2 * nf / (nf - 1.0)).sqrt(),
        skew: m3 / m2.powf(1.5),
        kurt: m4 / (m2 * m2),
    })
}

/// The Sharpe ratio the best of `trials` unskilled trials is expected to show, when the Sharpe ratios of the trials
/// have the variance `var`: `sqrt(var) ((1 - g) z(1 - 1/N) + g z(1 - 1/(N e)))`, `g` Euler's constant and `z` the normal
/// quantile. `None` for fewer than two trials or a negative variance.
pub fn expected_max_sharpe(trials: usize, var: f64) -> Option<f64> {
    if trials < 2 || var < 0.0 || var.is_nan() {
        return None;
    }
    let n = trials as f64;
    // The upper quantiles through the lower tail, where a probability of 1/N is exact.
    let z1 = -inv_cdf(1.0 / n);
    let z2 = -inv_cdf(1.0 / (n * std::f64::consts::E));
    Some(var.sqrt() * ((1.0 - EULER_GAMMA) * z1 + EULER_GAMMA * z2))
}

/// The probability that the true Sharpe ratio is above `bench`, given the one observed over `n` days with the daily
/// results' skew and kurtosis: `cdf((sr - bench) sqrt(n - 1) / sqrt(1 - skew sr + (kurt - 1)/4 sr^2))`. `None` when
/// the estimate's variance comes out as nothing or less (an `sr` the skew and kurtosis cannot support).
pub fn probabilistic_sharpe(sr: f64, bench: f64, n: usize, skew: f64, kurt: f64) -> Option<f64> {
    if n < 2 {
        return None;
    }
    let denom = 1.0 - skew * sr + (kurt - 1.0) / 4.0 * sr * sr;
    if denom <= 0.0 || denom.is_nan() {
        return None;
    }
    Some(cdf((sr - bench) * ((n - 1) as f64).sqrt() / denom.sqrt()))
}

/// The deflated Sharpe ratio: [`probabilistic_sharpe`] against [`expected_max_sharpe`] for `trials` trials whose Sharpe
/// ratios have the variance `var_trials`.
pub fn deflated_sharpe(m: &Moments, trials: usize, var_trials: f64) -> Option<f64> {
    let bench = expected_max_sharpe(trials, var_trials)?;
    probabilistic_sharpe(m.sharpe(), bench, m.n, m.skew, m.kurt)
}

/// The sample variance (`n - 1` in the divisor) of the values; `None` for fewer than two.
pub fn variance(x: &[f64]) -> Option<f64> {
    if x.len() < 2 {
        return None;
    }
    let mean = x.iter().sum::<f64>() / x.len() as f64;
    Some(x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (x.len() as f64 - 1.0))
}
