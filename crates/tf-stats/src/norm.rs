//! The standard normal distribution, to double precision, with no libm beyond `exp` and `sqrt`.
//!
//! `erfc` is a series for small arguments (all terms positive, so no cancellation) and a continued fraction for the
//! tail. The inverse is bisection on the distribution function, which needs no table of constants and is as exact as
//! the function it inverts.

use std::f64::consts::{PI, SQRT_2};

/// The complementary error function.
pub fn erfc(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x < 0.0 {
        return 2.0 - erfc(-x);
    }
    if x < 3.0 {
        1.0 - erf_series(x)
    } else {
        erfc_tail(x)
    }
}

/// erf(x) = 2/sqrt(pi) exp(-x^2) sum over n of 2^n x^(2n+1) / (1 x 3 x ... x (2n+1)), for x >= 0.
fn erf_series(x: f64) -> f64 {
    let x2 = x * x;
    let mut term = x;
    let mut sum = x;
    for n in 1..500 {
        term *= 2.0 * x2 / (2.0 * f64::from(n) + 1.0);
        sum += term;
        if term <= sum * 1e-17 {
            break;
        }
    }
    2.0 / PI.sqrt() * (-x2).exp() * sum
}

/// erfc(x) = exp(-x^2) / sqrt(pi) / (x + (1/2) / (x + (2/2) / (x + (3/2) / (x + ...)))), for x >= 3.
fn erfc_tail(x: f64) -> f64 {
    let mut f = x;
    for k in (1..=60).rev() {
        f = x + (f64::from(k) / 2.0) / f;
    }
    (-x * x).exp() / (PI.sqrt() * f)
}

/// The standard normal distribution function.
pub fn cdf(x: f64) -> f64 {
    0.5 * erfc(-x / SQRT_2)
}

/// The quantile function: the x with `cdf(x) == p`. Infinite at 0 and 1, NaN outside them.
///
/// For a probability very near 1 pass its complement `q` to `-inv_cdf(q)`: `1 - p` loses what `q` keeps.
pub fn inv_cdf(p: f64) -> f64 {
    if p.is_nan() || !(0.0..=1.0).contains(&p) {
        return f64::NAN;
    }
    if p == 0.0 {
        return f64::NEG_INFINITY;
    }
    if p == 1.0 {
        return f64::INFINITY;
    }
    if p > 0.5 {
        -inv_lower(1.0 - p)
    } else {
        inv_lower(p)
    }
}

/// The quantile for a probability up to one half, found in the lower tail where the distribution function is exact.
fn inv_lower(p: f64) -> f64 {
    let (mut lo, mut hi) = (-40.0_f64, 0.0_f64);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if cdf(mid) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}
