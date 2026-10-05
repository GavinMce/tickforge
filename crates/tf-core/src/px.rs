//! Fixed-point price.
//!
//! The scale (1e-9 dollars) matches Databento's `FIXED_PRICE_SCALE`, so
//! Databento records convert without rounding. JSON providers (Alpaca) go
//! through [`Px::from_f64`] exactly once, at the adapter boundary.

use std::fmt;
use std::ops::{Add, Sub};

pub const PX_SCALE: i64 = 1_000_000_000;
const CENT: i64 = PX_SCALE / 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Px(i64);

impl Px {
    pub const ZERO: Px = Px(0);

    pub const fn from_raw(raw: i64) -> Self {
        Px(raw)
    }

    pub const fn raw(self) -> i64 {
        self.0
    }

    pub const fn from_cents(cents: i64) -> Self {
        Px(cents * CENT)
    }

    /// Whole cents, rounded half away from zero.
    pub const fn to_cents(self) -> i64 {
        let half = CENT / 2;
        if self.0 >= 0 {
            (self.0 + half) / CENT
        } else {
            (self.0 - half) / CENT
        }
    }

    pub fn from_f64(dollars: f64) -> Self {
        Px((dollars * PX_SCALE as f64).round() as i64)
    }

    pub fn to_f64(self) -> f64 {
        self.0 as f64 / PX_SCALE as f64
    }
}

impl Add for Px {
    type Output = Px;
    fn add(self, rhs: Px) -> Px {
        Px(self.0 + rhs.0)
    }
}

impl Sub for Px {
    type Output = Px;
    fn sub(self, rhs: Px) -> Px {
        Px(self.0 - rhs.0)
    }
}

impl fmt::Display for Px {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4}", self.to_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cents_roundtrip() {
        for c in [-12_345, -1, 0, 1, 99, 12_345] {
            assert_eq!(Px::from_cents(c).to_cents(), c);
        }
    }

    #[test]
    fn f64_boundary_rounds_to_nearest_raw() {
        assert_eq!(Px::from_f64(1.23).raw(), 1_230_000_000);
        assert_eq!(Px::from_f64(0.0001).raw(), 100_000);
    }

    #[test]
    fn arithmetic_and_display() {
        let a = Px::from_cents(1050);
        let b = Px::from_cents(25);
        assert_eq!((a + b).to_cents(), 1075);
        assert_eq!((a - b).to_cents(), 1025);
        assert_eq!(a.to_string(), "10.5000");
    }
}
