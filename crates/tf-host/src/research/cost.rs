//! What a trade costs in a research run (E19-S13).
//!
//! The simulated broker fills against the recorded quote after a latency; this is the rest: the regulatory fees on
//! sales and the borrow fee, at the rates in force on the day of the trade. It is data, not code: it is written to the
//! results directory as text, fingerprinted, and a result set whose configuration is missing or is not the one a run
//! was asked for is refused.
//!
//! - **SEC Section 31 fee**: dollars per million dollars of covered sales, from a date. The rate is set by the SEC for
//!   each fiscal year and sometimes in the middle of it: $27.80 through 13 May 2025, $0.00 from 14 May 2025
//!   ([fee rate advisory 2025-2](https://www.sec.gov/rules-regulations/fee-rate-advisories/2025-2)), $20.60 from
//!   4 April 2026 ([2026-2](https://www.sec.gov/rules-regulations/fee-rate-advisories/2026-2)).
//! - **FINRA Trading Activity Fee** on covered equity sales: a rate per share sold with a cap per trade, from 1 January
//!   of each year: $0.000166 up to $8.30 in 2024 and 2025, $0.000195 up to $9.79 in 2026, $0.000232 up to $11.61 in
//!   2027 ([FINRA fee adjustment schedule](https://www.finra.org/rules-guidance/rule-filings/sr-finra-2024-019/fee-adjustment-schedule)).
//!   The cap is applied to each execution.
//! - **A date the table does not cover is refused**, never given the nearest rate: each table says the last day it is
//!   known for (`through`) and the first rate it has. The SEC rate for the fiscal year that began on 1 October 2026 is
//!   not in the table until someone looks it up.
//! - **Borrow**: basis points a year of the value shorted, for the time held, charged on names that are not easy to
//!   borrow (the broker charges none on easy-to-borrow names). Zero by default.
//! - Fees are exact in raw price units (1e-9 dollars) and are not rounded to a cent per trade: brokers round, and the
//!   difference is far below the other costs a research run is about.
//!
//! Only sales pay the regulatory fees: a long pays on its exit, a short on its entry.

use tf_core::{Nanos, Px};
use tf_strategy::sim::SimConfig;

use crate::def::fnv;

const HEADER: &str = "cost model v1";
/// Dollars per million dollars times raw price units per dollar, divided out: raw fee = raw notional x rate / this.
const PER_MILLION: u128 = 1_000_000 * 1_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CostModel {
    /// Delay from a strategy's decision to the order reaching the venue.
    pub latency_ns: Nanos,
    /// Annual borrow fee on shorts of names that are not easy to borrow, in basis points.
    pub borrow_bps_per_year: u32,
    /// Section 31: from this date (`YYYY-MM-DD`), dollars per million dollars sold. In date order.
    pub sec: Vec<(String, Px)>,
    /// The last date the Section 31 table is known for.
    pub sec_through: String,
    /// Trading Activity Fee: from this date, dollars per share sold and the most one execution pays. In date order.
    pub taf: Vec<(String, Px, Px)>,
    /// The last date the fee table is known for.
    pub taf_through: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CostError {
    /// The date is before the first rate or after the last day the table is known for.
    NoRate { fee: &'static str, date: String },
    /// Text that is not a cost model, and why.
    Parse(String),
}

impl std::fmt::Display for CostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CostError::NoRate { fee, date } => write!(
                f,
                "the cost model has no {fee} rate for {date}: add the published rate for that date"
            ),
            CostError::Parse(m) => write!(f, "cost model: {m}"),
        }
    }
}

impl std::error::Error for CostError {}

/// `YYYY-MM-DD`, with a month 01 to 12 and a day 01 to 31: a shape that sorts as a date does.
pub(crate) fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && matches!(s[5..7].parse::<u8>(), Ok(1..=12))
        && matches!(s[8..10].parse::<u8>(), Ok(1..=31))
}

impl CostModel {
    /// The published rates as of the table's last revision, 50 ms of latency and no borrow fee.
    pub fn published() -> CostModel {
        let px = |t: &str| Px::parse(t).expect("a literal price");
        CostModel {
            latency_ns: 50_000_000,
            borrow_bps_per_year: 0,
            sec: vec![
                ("2025-05-14".into(), px("0.00")),
                ("2026-04-04".into(), px("20.60")),
            ],
            sec_through: "2026-09-30".into(),
            taf: vec![
                ("2024-01-01".into(), px("0.000166"), px("8.30")),
                ("2025-01-01".into(), px("0.000166"), px("8.30")),
                ("2026-01-01".into(), px("0.000195"), px("9.79")),
                ("2027-01-01".into(), px("0.000232"), px("11.61")),
            ],
            taf_through: "2027-12-31".into(),
        }
    }

    /// What the simulated broker is configured with.
    pub fn sim(&self) -> SimConfig {
        SimConfig {
            latency_ns: self.latency_ns,
            borrow_bps_per_year: self.borrow_bps_per_year,
        }
    }

    /// Dollars per million dollars sold on `date`.
    pub fn sec_rate(&self, date: &str) -> Result<Px, CostError> {
        let none = || CostError::NoRate {
            fee: "Section 31",
            date: date.to_owned(),
        };
        if date > self.sec_through.as_str() {
            return Err(none());
        }
        self.sec
            .iter()
            .rev()
            .find(|(from, _)| from.as_str() <= date)
            .map(|(_, r)| *r)
            .ok_or_else(none)
    }

    /// Dollars per share sold, and the most one execution pays, on `date`.
    pub fn taf_rate(&self, date: &str) -> Result<(Px, Px), CostError> {
        let none = || CostError::NoRate {
            fee: "Trading Activity Fee",
            date: date.to_owned(),
        };
        if date > self.taf_through.as_str() {
            return Err(none());
        }
        self.taf
            .iter()
            .rev()
            .find(|(from, _, _)| from.as_str() <= date)
            .map(|(_, r, cap)| (*r, *cap))
            .ok_or_else(none)
    }

    /// The regulatory fees on a sale of `qty` shares at `px` (raw) on `date`, in raw price units.
    pub fn sale_fees(&self, date: &str, qty: u32, px: i64) -> Result<u128, CostError> {
        let sec = self.sec_rate(date)?;
        let (taf, cap) = self.taf_rate(date)?;
        let notional = u128::from(qty) * u128::try_from(px).unwrap_or(0);
        let sec_fee = notional * u128::try_from(sec.raw()).unwrap_or(0) / PER_MILLION;
        let taf_fee = (u128::from(qty) * u128::try_from(taf.raw()).unwrap_or(0))
            .min(u128::try_from(cap.raw()).unwrap_or(0));
        Ok(sec_fee + taf_fee)
    }

    /// The borrow fee on a short of `notional` (raw) held for `held` nanoseconds.
    pub fn borrow_fee(&self, notional: u128, held: Nanos) -> u128 {
        const YEAR_NS: u128 = 365 * 86_400 * 1_000_000_000;
        notional * u128::from(self.borrow_bps_per_year) * u128::from(held) / (10_000 * YEAR_NS)
    }

    /// The text kept with the results.
    pub fn render(&self) -> String {
        let mut s = format!(
            "{HEADER}\nlatency_ns {}\nborrow_bps_per_year {}\nsec_through {}\n",
            self.latency_ns, self.borrow_bps_per_year, self.sec_through
        );
        for (d, r) in &self.sec {
            s.push_str(&format!("sec {d} {}\n", r.to_decimal()));
        }
        s.push_str(&format!("taf_through {}\n", self.taf_through));
        for (d, r, cap) in &self.taf {
            s.push_str(&format!(
                "taf {d} {} {}\n",
                r.to_decimal(),
                cap.to_decimal()
            ));
        }
        s.push_str("end\n");
        s
    }

    /// Identifies this model: two runs with the same fingerprint charged the same costs.
    pub fn fingerprint(&self) -> u64 {
        fnv(&[self.render().as_bytes()])
    }

    pub fn parse(text: &str) -> Result<CostModel, CostError> {
        let bad = |m: String| CostError::Parse(m);
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(bad(format!("not `{HEADER}`")));
        }
        let mut m = CostModel {
            latency_ns: 0,
            borrow_bps_per_year: 0,
            sec: Vec::new(),
            sec_through: String::new(),
            taf: Vec::new(),
            taf_through: String::new(),
        };
        let (mut got_latency, mut got_borrow, mut ended) = (false, false, false);
        for line in lines {
            if ended {
                return Err(bad("text after `end`".into()));
            }
            let w: Vec<&str> = line.split(' ').collect();
            let px = |t: &str| {
                Px::parse(t)
                    .filter(|p| p.raw() >= 0)
                    .ok_or_else(|| bad(format!("`{t}` is not a rate in dollars")))
            };
            let date = |t: &str| {
                if is_date(t) {
                    Ok(t.to_owned())
                } else {
                    Err(bad(format!("`{t}` is not a date (YYYY-MM-DD)")))
                }
            };
            match w.as_slice() {
                ["latency_ns", v] => {
                    m.latency_ns = v.parse().map_err(|_| bad(format!("latency `{v}`")))?;
                    got_latency = true;
                }
                ["borrow_bps_per_year", v] => {
                    m.borrow_bps_per_year =
                        v.parse().map_err(|_| bad(format!("borrow rate `{v}`")))?;
                    got_borrow = true;
                }
                ["sec_through", d] => m.sec_through = date(d)?,
                ["sec", d, r] => m.sec.push((date(d)?, px(r)?)),
                ["taf_through", d] => m.taf_through = date(d)?,
                ["taf", d, r, cap] => m.taf.push((date(d)?, px(r)?, px(cap)?)),
                ["end"] => ended = true,
                _ => return Err(bad(format!("a line this version does not know: `{line}`"))),
            }
        }
        if !ended {
            return Err(bad("cut short: no `end`".into()));
        }
        if !got_latency || !got_borrow || m.sec.is_empty() || m.taf.is_empty() {
            return Err(bad(
                "it lacks the latency, the borrow rate, or a fee table".into()
            ));
        }
        if m.sec_through.is_empty() || m.taf_through.is_empty() {
            return Err(bad(
                "a fee table must say the date it is known through".into()
            ));
        }
        if !m.sec.windows(2).all(|w| w[0].0 < w[1].0) || !m.taf.windows(2).all(|w| w[0].0 < w[1].0)
        {
            return Err(bad("the rates are not in date order".into()));
        }
        if m.sec.first().is_some_and(|(d, _)| *d > m.sec_through)
            || m.taf.first().is_some_and(|(d, _, _)| *d > m.taf_through)
        {
            return Err(bad(
                "a table starts after the date it is known through".into()
            ));
        }
        Ok(m)
    }
}
