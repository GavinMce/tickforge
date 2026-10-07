//! Round trips from executions (E19-S13).
//!
//! A **round trip** is what one strategy did in one instrument from flat to flat: the executions that took it from no
//! position to a position and back to none. It is one record: when and at what price it entered and left (the
//! volume-weighted average of its executions), how many shares, its side, what it paid in fees, what it lost against the
//! price the strategy asked for (slippage), what it made in dollars, in basis points of what it put in, and in R, and
//! why it ended.
//!
//! - **Money** is exact: raw price units (1e-9 dollars) times shares, in integers. Gross is exit less entry for a long,
//!   entry less exit for a short; net is gross less the regulatory fees on its sales (see [`crate::research::CostModel`])
//!   and the borrow fee. **Basis points** are net over what the entry cost, in hundredths of a basis point.
//! - **Slippage** is, execution by execution, what was paid above (buys) or received below (sells) the price the intent
//!   was nearest to (its limit, or a collar's reference), times shares. Positive means we did worse than the strategy
//!   asked. It is what the quote did between the decision and the fill, plus the spread.
//! - **R** is net over the money at risk at entry: the shares times the distance from the entry price to the stop on
//!   the opening intent's protective orders. A strategy that holds its stop itself ([`tf_strategy::exits`]) states no
//!   stop to the host, so its R is absent (`None`) until the strategies report their initial risk (E19-S18).
//! - **The exit reason** is the code on the intent that closed the trip: the stop, target and time codes of
//!   `tf_strategy::exits`, the host's flatten code, or the strategy's own. A trip still open when the day's events end
//!   is closed at the last trade price, at the fees a sale of it would pay, with the reason [`OPEN_AT_END`] and the flag set,
//!   so a day's numbers never hide a position.
//! - **A fill that goes through flat** (closes the position and opens the other way, which the host's own orders never
//!   do) is split: the rest begins a new trip.
//!
//! The text of a record is tab separated and reads back exactly.

use std::collections::BTreeMap;

use tf_core::{InstrumentId, Nanos};
use tf_strategy::intent::Side;

use super::cost::{CostError, CostModel};
use crate::host::FillNote;

/// The exit reason of a trip still open when the day's events ended.
pub const OPEN_AT_END: u16 = 0xFFFF;

/// One round trip. Prices and money are raw (1e-9 dollars).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trip {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub strategy: u16,
    pub name: String,
    /// The strategy as configured: its fingerprint, so another parameter is another variant.
    pub variant: u64,
    pub symbol: String,
    pub long: bool,
    /// Shares entered (and exited).
    pub qty: u32,
    pub entry_ts: Nanos,
    pub entry_px: i64,
    pub exit_ts: Nanos,
    pub exit_px: i64,
    pub gross: i64,
    pub fees: i64,
    pub borrow: i64,
    /// Total against the reference prices, positive when we did worse.
    pub slippage: i64,
    pub net: i64,
    /// Net over the entry cost, in hundredths of a basis point.
    pub net_bps_x100: i64,
    /// Slippage over the reference value traded, in hundredths of a basis point.
    pub slip_bps_x100: i64,
    /// Net over the money at risk, in thousandths of R; `None` if the opening intent stated no stop.
    pub r_milli: Option<i64>,
    pub entry_reason: u16,
    pub exit_reason: u16,
    pub open_at_end: bool,
}

pub const COLUMNS: &str = "day\tstrategy\tname\tvariant\tsymbol\tside\tqty\tentry_ts\tentry_px\texit_ts\texit_px\tgross\tfees\tborrow\tslippage\tnet\tnet_bps_x100\tslip_bps_x100\tr_milli\tentry_reason\texit_reason\topen_at_end";

impl Trip {
    pub fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{:016x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.day,
            self.strategy,
            self.name,
            self.variant,
            self.symbol,
            if self.long { 'L' } else { 'S' },
            self.qty,
            self.entry_ts,
            self.entry_px,
            self.exit_ts,
            self.exit_px,
            self.gross,
            self.fees,
            self.borrow,
            self.slippage,
            self.net,
            self.net_bps_x100,
            self.slip_bps_x100,
            self.r_milli.map_or("-".to_owned(), |r| r.to_string()),
            self.entry_reason,
            self.exit_reason,
            u8::from(self.open_at_end),
        )
    }

    pub fn parse(line: &str) -> Result<Trip, String> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 22 {
            return Err(format!("{} fields, not 22", f.len()));
        }
        fn n<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, String> {
            s.parse().map_err(|_| format!("{what} `{s}`"))
        }
        Ok(Trip {
            day: f[0].to_owned(),
            strategy: n(f[1], "strategy")?,
            name: f[2].to_owned(),
            variant: u64::from_str_radix(f[3], 16).map_err(|_| format!("variant `{}`", f[3]))?,
            symbol: f[4].to_owned(),
            long: match f[5] {
                "L" => true,
                "S" => false,
                s => return Err(format!("side `{s}`")),
            },
            qty: n(f[6], "qty")?,
            entry_ts: n(f[7], "entry_ts")?,
            entry_px: n(f[8], "entry_px")?,
            exit_ts: n(f[9], "exit_ts")?,
            exit_px: n(f[10], "exit_px")?,
            gross: n(f[11], "gross")?,
            fees: n(f[12], "fees")?,
            borrow: n(f[13], "borrow")?,
            slippage: n(f[14], "slippage")?,
            net: n(f[15], "net")?,
            net_bps_x100: n(f[16], "net_bps_x100")?,
            slip_bps_x100: n(f[17], "slip_bps_x100")?,
            r_milli: match f[18] {
                "-" => None,
                s => Some(n(s, "r_milli")?),
            },
            entry_reason: n(f[19], "entry_reason")?,
            exit_reason: n(f[20], "exit_reason")?,
            open_at_end: match f[21] {
                "0" => false,
                "1" => true,
                s => return Err(format!("open_at_end `{s}`")),
            },
        })
    }
}

#[derive(Clone, Copy)]
struct Leg {
    qty: u32,
    px: i64,
    ts: Nanos,
    reference: i64,
    reason: u16,
    stop: Option<i64>,
    buy: bool,
}

struct Open {
    long: bool,
    pos: u32,
    entries: Vec<Leg>,
    exits: Vec<Leg>,
}

/// Who a strategy is, for the record.
#[derive(Clone, Debug)]
pub struct Who {
    pub name: String,
    pub variant: u64,
}

/// Turns the executions of one day into round trips.
pub struct Assembler<'a> {
    day: &'a str,
    cost: &'a CostModel,
    who: &'a BTreeMap<u16, Who>,
    symbol: &'a dyn Fn(InstrumentId) -> String,
    /// Whether the broker charges nothing to borrow this name (it is easy to borrow).
    easy: &'a dyn Fn(InstrumentId) -> bool,
    open: BTreeMap<(u16, InstrumentId), Open>,
    done: Vec<Trip>,
    failed: Option<CostError>,
}

impl<'a> Assembler<'a> {
    pub fn new(
        day: &'a str,
        cost: &'a CostModel,
        who: &'a BTreeMap<u16, Who>,
        symbol: &'a dyn Fn(InstrumentId) -> String,
        easy: &'a dyn Fn(InstrumentId) -> bool,
    ) -> Assembler<'a> {
        Assembler {
            day,
            cost,
            who,
            symbol,
            easy,
            open: BTreeMap::new(),
            done: Vec::new(),
            failed: None,
        }
    }

    /// One execution, in the order the host recorded them.
    pub fn fill(&mut self, n: &FillNote) {
        let buy = n.side == Side::Buy;
        let key = (n.strategy, n.instrument);
        let mut left = n.qty;
        while left > 0 {
            let st = self.open.entry(key).or_insert_with(|| Open {
                long: buy,
                pos: 0,
                entries: Vec::new(),
                exits: Vec::new(),
            });
            let leg = |qty| Leg {
                qty,
                px: n.px,
                ts: n.ts,
                reference: n.reference,
                reason: n.reason,
                stop: n.stop,
                buy,
            };
            if st.long == buy {
                st.entries.push(leg(left));
                st.pos += left;
                left = 0;
            } else {
                let take = left.min(st.pos);
                st.exits.push(leg(take));
                st.pos -= take;
                left -= take;
                if st.pos == 0 {
                    let st = self.open.remove(&key).expect("just used");
                    self.finish(key, st, false);
                }
            }
        }
    }

    /// The day's events ended: what is still held is closed at `mark(instrument)`, the last trade price (an
    /// instrument with no trade is closed at its entry price), and the trips are given.
    pub fn end(
        mut self,
        ts: Nanos,
        mark: impl Fn(InstrumentId) -> i64,
    ) -> Result<Vec<Trip>, CostError> {
        let open = std::mem::take(&mut self.open);
        for (key, mut st) in open {
            let entry_n: i128 = st
                .entries
                .iter()
                .map(|l| i128::from(l.qty) * i128::from(l.px))
                .sum();
            let entry_q: u32 = st.entries.iter().map(|l| l.qty).sum();
            let m = mark(key.1);
            let px = if m > 0 {
                m
            } else {
                round_div(entry_n, i128::from(entry_q))
            };
            st.exits.push(Leg {
                qty: st.pos,
                px,
                ts: ts.max(st.entries.last().map_or(0, |l| l.ts)),
                reference: px,
                reason: OPEN_AT_END,
                stop: None,
                buy: !st.long,
            });
            self.finish(key, st, true);
        }
        match self.failed {
            Some(e) => Err(e),
            None => Ok(self.done),
        }
    }

    fn finish(&mut self, key: (u16, InstrumentId), st: Open, open_at_end: bool) {
        match self.trip(key, &st, open_at_end) {
            Ok(t) => self.done.push(t),
            Err(e) => {
                self.failed.get_or_insert(e);
            }
        }
    }

    fn trip(
        &self,
        (strategy, instrument): (u16, InstrumentId),
        st: &Open,
        open_at_end: bool,
    ) -> Result<Trip, CostError> {
        let sum = |legs: &[Leg]| -> (u32, i128) {
            (
                legs.iter().map(|l| l.qty).sum(),
                legs.iter()
                    .map(|l| i128::from(l.qty) * i128::from(l.px))
                    .sum(),
            )
        };
        let (qty, entry_n) = sum(&st.entries);
        let (_, exit_n) = sum(&st.exits);
        let gross = if st.long {
            exit_n - entry_n
        } else {
            entry_n - exit_n
        };
        // Only sales pay the fees: a long's exits, a short's entries.
        let sales = if st.long { &st.exits } else { &st.entries };
        let mut fees: i128 = 0;
        for l in sales {
            fees += self.cost.sale_fees(self.day, l.qty, l.px)? as i128;
        }
        let entry_ts = st.entries.first().map_or(0, |l| l.ts);
        let exit_ts = st.exits.last().map_or(entry_ts, |l| l.ts);
        let borrow = if st.long || (self.easy)(instrument) {
            0
        } else {
            self.cost.borrow_fee(
                u128::try_from(entry_n).unwrap_or(0),
                exit_ts.saturating_sub(entry_ts),
            ) as i128
        };
        let mut slip: i128 = 0;
        let mut reference_value: i128 = 0;
        for l in st.entries.iter().chain(&st.exits) {
            if l.reference > 0 {
                let adverse = if l.buy {
                    l.px - l.reference
                } else {
                    l.reference - l.px
                };
                slip += i128::from(adverse) * i128::from(l.qty);
                reference_value += i128::from(l.reference) * i128::from(l.qty);
            }
        }
        let net = gross - fees - borrow;
        let entry_px = round_div(entry_n, i128::from(qty));
        let exit_px = round_div(exit_n, i128::from(qty));
        let risk = st
            .entries
            .first()
            .and_then(|l| l.stop)
            .map(|stop| i128::from(qty) * i128::from((entry_px - stop).abs()));
        let who = self.who.get(&strategy);
        Ok(Trip {
            day: self.day.to_owned(),
            strategy,
            name: who.map_or_else(|| format!("s{strategy}"), |w| w.name.clone()),
            variant: who.map_or(0, |w| w.variant),
            symbol: (self.symbol)(instrument),
            long: st.long,
            qty,
            entry_ts,
            entry_px,
            exit_ts,
            exit_px,
            gross: to_i64(gross),
            fees: to_i64(fees),
            borrow: to_i64(borrow),
            slippage: to_i64(slip),
            net: to_i64(net),
            net_bps_x100: if entry_n > 0 {
                to_i64(net * 1_000_000 / entry_n)
            } else {
                0
            },
            slip_bps_x100: if reference_value > 0 {
                to_i64(slip * 1_000_000 / reference_value)
            } else {
                0
            },
            r_milli: risk.filter(|r| *r > 0).map(|r| to_i64(net * 1000 / r)),
            entry_reason: st.entries.first().map_or(0, |l| l.reason),
            exit_reason: st.exits.last().map_or(0, |l| l.reason),
            open_at_end,
        })
    }
}

fn to_i64(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

/// `a / b` rounded half up, for positive `b`.
pub(super) fn round_div(a: i128, b: i128) -> i64 {
    if b <= 0 {
        return 0;
    }
    to_i64((a + b / 2).div_euclid(b))
}
