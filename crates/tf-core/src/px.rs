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

    /// Decimal dollars as text (`-12.5`, `0.0001`, `150`), exactly and without a float. `None` for
    /// anything that is not a plain decimal (a sign other than `-`, an exponent, spaces, no digits),
    /// that does not fit, or that has non-zero digits past a billionth of a dollar.
    pub fn parse(text: &str) -> Option<Px> {
        let (neg, rest) = match text.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, text),
        };
        let (whole, frac) = rest.split_once('.').unwrap_or((rest, ""));
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || !digits(whole) || !digits(frac) {
            return None;
        }
        if frac.len() > 9 && frac[9..].bytes().any(|b| b != b'0') {
            return None;
        }
        let frac9: String = frac
            .chars()
            .take(9)
            .chain(std::iter::repeat('0'))
            .take(9)
            .collect();
        let raw = i128::from(whole.parse::<u64>().ok()?)
            .checked_mul(i128::from(PX_SCALE))?
            .checked_add(i128::from(frac9.parse::<u64>().ok()?))?;
        i64::try_from(if neg { -raw } else { raw }).ok().map(Px)
    }

    /// Dollars as text with at least two decimals and no more than needed: `2.00`, `0.0001`, `150.25`.
    pub fn to_decimal(self) -> String {
        let raw = i128::from(self.0);
        let sign = if raw < 0 { "-" } else { "" };
        let abs = raw.unsigned_abs();
        let scale = PX_SCALE as u128;
        let (whole, frac) = (abs / scale, abs % scale);
        let mut f = format!("{frac:09}");
        while f.len() > 2 && f.ends_with('0') {
            f.pop();
        }
        format!("{sign}{whole}.{f}")
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
    fn decimal_text_is_read_and_written_exactly() {
        assert_eq!(
            Px::parse("105.8988475"),
            Some(Px::from_raw(105_898_847_500))
        );
        assert_eq!(Px::parse("0.0001"), Some(Px::from_raw(100_000)));
        assert_eq!(Px::parse("150"), Some(Px::from_raw(150 * PX_SCALE)));
        assert_eq!(Px::parse("150."), Some(Px::from_raw(150 * PX_SCALE)));
        assert_eq!(Px::parse("-1.5"), Some(Px::from_raw(-1_500_000_000)));
        assert_eq!(Px::parse("1.0000000000"), Some(Px::from_raw(PX_SCALE)));
        assert_eq!(Px::parse("0.000000001"), Some(Px::from_raw(1)));
        for bad in [
            "",
            ".5",
            "1e3",
            "1.0000000001",
            "abc",
            "1,5",
            "--1",
            "+1",
            " 1",
            "1 ",
            "99999999999999999999",
            "-",
        ] {
            assert_eq!(Px::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Px::from_raw(2 * PX_SCALE).to_decimal(), "2.00");
        assert_eq!(Px::from_raw(100_000).to_decimal(), "0.0001");
        assert_eq!(Px::from_raw(150_250_000_000).to_decimal(), "150.25");
        assert_eq!(Px::from_raw(-1_500_000_000).to_decimal(), "-1.50");
        assert_eq!(Px::from_raw(1).to_decimal(), "0.000000001");
        assert_eq!(Px::from_raw(0).to_decimal(), "0.00");
        for raw in [
            0,
            1,
            -1,
            123,
            100_000,
            999_999_999,
            1_000_000_000,
            150_250_000_000,
            -5_000_000_007,
            i64::MAX / 3,
        ] {
            assert_eq!(
                Px::parse(&Px::from_raw(raw).to_decimal()),
                Some(Px::from_raw(raw)),
                "{raw}"
            );
        }
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
