//! Exponentially weighted baselines in fixed-point integer maths.
//!
//! State carries a 16-bit fraction, so truncation error stays far below one
//! unit of the input however long the series. `alpha` is in permille (100 is
//! 0.1): each sample moves the mean `alpha / 1000` of the way to the sample.
//! The first sample initialises the mean exactly.
//!
//! Inputs are clamped to +/- 2^40 (about 1.1e12), which keeps every
//! intermediate within `i128`.

const SHIFT: u32 = 16;
const LIMIT: i64 = 1 << 40;

/// Exponentially weighted mean.
#[derive(Clone, Copy, Debug)]
pub struct Ewma {
    alpha: u32,
    scaled: i128,
    seen: bool,
}

impl Ewma {
    /// `alpha_permille` is clamped to `1..=1000`.
    pub const fn new(alpha_permille: u32) -> Self {
        let alpha = if alpha_permille < 1 {
            1
        } else if alpha_permille > 1000 {
            1000
        } else {
            alpha_permille
        };
        Ewma {
            alpha,
            scaled: 0,
            seen: false,
        }
    }

    pub fn update(&mut self, x: i64) {
        let x = i128::from(x.clamp(-LIMIT, LIMIT)) << SHIFT;
        if self.seen {
            self.scaled += (x - self.scaled) * i128::from(self.alpha) / 1000;
        } else {
            self.scaled = x;
            self.seen = true;
        }
    }

    /// The mean, rounded to the nearest integer; `None` before any sample.
    pub fn value(&self) -> Option<i64> {
        self.seen
            .then(|| ((self.scaled + (1 << (SHIFT - 1))) >> SHIFT) as i64)
    }
}

/// Exponentially weighted mean and variance (West's recurrence), for z-scores.
#[derive(Clone, Copy, Debug)]
pub struct EwmaVar {
    alpha: u32,
    mean: i128,
    var: i128,
    seen: bool,
}

impl EwmaVar {
    /// `alpha_permille` is clamped to `1..=1000`.
    pub const fn new(alpha_permille: u32) -> Self {
        let alpha = if alpha_permille < 1 {
            1
        } else if alpha_permille > 1000 {
            1000
        } else {
            alpha_permille
        };
        EwmaVar {
            alpha,
            mean: 0,
            var: 0,
            seen: false,
        }
    }

    pub fn update(&mut self, x: i64) {
        let x = i128::from(x.clamp(-LIMIT, LIMIT)) << SHIFT;
        if !self.seen {
            self.mean = x;
            self.var = 0;
            self.seen = true;
            return;
        }
        let a = i128::from(self.alpha);
        let diff = x - self.mean;
        self.mean += diff * a / 1000;
        // var' = (1 - a) * (var + a * diff^2); diff is scaled by 2^SHIFT, so
        // diff^2 carries 2^(2*SHIFT) and is brought back to 2^SHIFT.
        let incr = ((diff * diff) >> SHIFT) * a / 1000;
        self.var = (self.var + incr) * (1000 - a) / 1000;
    }

    pub fn mean(&self) -> Option<i64> {
        self.seen
            .then(|| ((self.mean + (1 << (SHIFT - 1))) >> SHIFT) as i64)
    }

    /// The variance in input units squared, rounded; `None` before any sample.
    pub fn variance(&self) -> Option<u64> {
        self.seen
            .then(|| ((self.var + (1 << (SHIFT - 1))) >> SHIFT) as u64)
    }

    /// The standard deviation, rounded; `None` before any sample.
    pub fn std_dev(&self) -> Option<u64> {
        // sqrt(var * 2^16) = std * 2^8
        self.seen
            .then(|| (((self.var as u128).isqrt() + (1 << 7)) >> (SHIFT / 2)) as u64)
    }
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<Ewma>();
    is_copy::<EwmaVar>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_sets_the_mean_and_alpha_is_clamped() {
        let mut e = Ewma::new(100);
        assert_eq!(e.value(), None);
        e.update(-7);
        assert_eq!(e.value(), Some(-7));
        let mut one = Ewma::new(5000); // clamped to 1000: follows the last sample
        one.update(10);
        one.update(99);
        assert_eq!(one.value(), Some(99));
        let mut slow = Ewma::new(0); // clamped to 1
        slow.update(0);
        slow.update(1000);
        assert_eq!(slow.value(), Some(1));
    }

    #[test]
    fn a_constant_input_is_a_fixed_point_with_no_variance() {
        let mut v = EwmaVar::new(50);
        for _ in 0..5000 {
            v.update(1234);
        }
        assert_eq!(
            (v.mean(), v.variance(), v.std_dev()),
            (Some(1234), Some(0), Some(0))
        );
    }

    #[test]
    fn huge_inputs_are_clamped_not_overflowed() {
        let mut v = EwmaVar::new(1000);
        v.update(i64::MIN);
        v.update(i64::MAX);
        assert!(v.variance().is_some() && v.std_dev().is_some());
    }
}
