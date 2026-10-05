//! The backtest report: what a run earned, risked and paid.
//!
//! A [`ReportBuilder`] follows a run: it is told every fill and every market
//! event, in stream order ([`crate::sim::run_backtest_observed`] does this), and
//! [`ReportBuilder::finish`] produces a [`Report`]. All amounts are integers in
//! raw price units (1e-9 dollars), so a report is exactly reproducible and can be
//! stored as run-result metrics ([`Report::metrics`]).
//!
//! Definitions, so the numbers can be argued with:
//! - A **trade** is a round trip: from flat to flat in one instrument (a position
//!   flipping through zero ends one trade and starts another). Its profit is the
//!   realised profit at average cost, before borrow fees. It is a **win** if
//!   positive; **hit rate** is wins over trades, in permille.
//! - **Net P&L** is realised profit plus the mark-to-market of what is still open,
//!   minus borrow fees. Marks are the last trade price (or the fill price until
//!   one is seen).
//! - **Max drawdown** is the largest fall from a peak of the equity curve
//!   (starting at zero), sampled at every event. The curve does not include borrow
//!   fees, which are only known at the end, so the drawdown is slightly optimistic
//!   for short books.
//! - **Slippage** is as recorded on each fill: price paid against the intent's
//!   reference price, positive when worse. The report gives its total cost
//!   (per share x shares) and the average per share.
//! - The **breakdown** groups instruments by a label chosen by the caller (for
//!   example the scenario the instrument was generated from).
//!
//! The report cannot show what the simulator does not model (ADR 0011), so every
//! rendering says so.

use std::collections::BTreeMap;

use tf_core::{Event, Nanos};

use crate::sim::Fill;

/// Figures for one group of instruments (or all of them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub fills: u64,
    pub shares: u64,
    pub trades: u64,
    pub wins: u64,
    /// Sum of winning trades' profit.
    pub gross_profit: u128,
    /// Sum of losing trades' loss (positive number).
    pub gross_loss: u128,
    /// Realised profit, before borrow fees (negative = loss).
    pub realized: i128,
    /// Mark-to-market of positions still open at the end.
    pub open_pnl: i128,
    pub borrow_fee: u128,
    /// Total slippage cost: per-share slippage x shares, summed.
    pub slippage_cost: i128,
    /// Largest per-share slippage on any fill (raw units).
    pub worst_slippage: i64,
}

impl Stats {
    /// Realised plus open, less borrow.
    pub fn net_pnl(&self) -> i128 {
        self.realized + self.open_pnl - self.borrow_fee as i128
    }

    /// Wins as permille of trades; `None` with no trades.
    pub fn hit_rate_permille(&self) -> Option<u64> {
        (self.trades > 0).then(|| self.wins * 1000 / self.trades)
    }

    /// Average slippage per share traded, raw units, rounded toward zero.
    pub fn avg_slippage(&self) -> Option<i64> {
        (self.shares > 0).then(|| (self.slippage_cost / i128::from(self.shares)) as i64)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub total: Stats,
    /// By label, in label order.
    pub by_label: BTreeMap<String, Stats>,
    /// Largest fall from an equity peak, raw units.
    pub max_drawdown: u128,
    pub events: u64,
    pub first_ts: Nanos,
    pub last_ts: Nanos,
}

#[derive(Clone, Copy, Default)]
struct Pos {
    qty: i64,
    avg: i64,
    mark: i64,
    /// Realised profit of the trade in progress.
    trip: i128,
}

impl Pos {
    /// Unrealised profit of the open position at its mark.
    fn open_value(&self) -> i128 {
        if self.qty == 0 || self.mark == 0 {
            0
        } else {
            i128::from(self.mark - self.avg) * i128::from(self.qty)
        }
    }
}

pub struct ReportBuilder {
    labels: Vec<String>,
    pos: Vec<Pos>,
    per_inst: Vec<Stats>,
    realized: i128,
    unrealized: i128,
    peak: i128,
    drawdown: u128,
    events: u64,
    first_ts: Nanos,
    last_ts: Nanos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReportError {
    /// Labels end up in metric names: letters, digits and `. _ -` only.
    BadLabel(String),
}

impl ReportBuilder {
    /// `labels[i]` is the group of instrument `i`.
    pub fn new(labels: Vec<String>) -> Result<ReportBuilder, ReportError> {
        for l in &labels {
            let ok = !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
            if !ok {
                return Err(ReportError::BadLabel(l.clone()));
            }
        }
        let n = labels.len();
        Ok(ReportBuilder {
            labels,
            pos: vec![Pos::default(); n],
            per_inst: vec![Stats::default(); n],
            realized: 0,
            unrealized: 0,
            peak: 0,
            drawdown: 0,
            events: 0,
            first_ts: 0,
            last_ts: 0,
        })
    }

    /// A fill, in the order the broker made them (before the event at the same time).
    pub fn on_fill(&mut self, f: &Fill) {
        let i = f.instrument as usize;
        let Some(p) = self.pos.get_mut(i) else { return };
        let st = &mut self.per_inst[i];
        st.fills += 1;
        st.shares += u64::from(f.qty);
        st.slippage_cost += i128::from(f.slippage) * i128::from(f.qty);
        st.worst_slippage = st.worst_slippage.max(f.slippage);

        let before = p.open_value();
        // Until a trade prints, the fill price is the best mark there is.
        if p.mark == 0 {
            p.mark = f.px.raw();
        }

        let signed = if f.side.is_buy() {
            i64::from(f.qty)
        } else {
            -i64::from(f.qty)
        };
        let (realized, finished) = apply(p, signed, f.px.raw());
        self.realized += realized;
        st.realized += realized;
        for trip in finished {
            st.trades += 1;
            if trip > 0 {
                st.wins += 1;
                st.gross_profit += trip as u128;
            } else {
                st.gross_loss += trip.unsigned_abs();
            }
        }
        self.unrealized += p.open_value() - before;
        self.sample();
    }

    /// A market event: trades move marks and the equity curve is sampled.
    pub fn on_event(&mut self, ev: &Event) {
        let ts = ev.ts_recv();
        if self.events == 0 {
            self.first_ts = ts;
        }
        self.events += 1;
        self.last_ts = ts;
        if let Event::Trade(t) = ev {
            if let Some(p) = self.pos.get_mut(t.hdr.instrument as usize) {
                let before = p.open_value();
                p.mark = t.px.raw();
                self.unrealized += p.open_value() - before;
            }
        }
        self.sample();
    }

    /// Profit and loss so far, realised plus marked, before borrow fees (which are only
    /// known at the end). The same curve the drawdown is read from.
    pub fn equity(&self) -> i128 {
        self.realized + self.unrealized
    }

    fn sample(&mut self) {
        let equity = self.realized + self.unrealized;
        self.peak = self.peak.max(equity);
        self.drawdown = self.drawdown.max((self.peak - equity) as u128);
    }

    /// Finish. `borrow_fee[i]` is the fee accrued on instrument `i` (raw units).
    pub fn finish(self, borrow_fee: &[u128]) -> Report {
        let mut by_label: BTreeMap<String, Stats> = BTreeMap::new();
        let mut total = Stats::default();
        for (i, (st, p)) in self.per_inst.iter().zip(&self.pos).enumerate() {
            let mut st = st.clone();
            st.open_pnl = p.open_value();
            st.borrow_fee = borrow_fee.get(i).copied().unwrap_or(0);
            merge(by_label.entry(self.labels[i].clone()).or_default(), &st);
            merge(&mut total, &st);
        }
        Report {
            total,
            by_label,
            max_drawdown: self.drawdown,
            events: self.events,
            first_ts: self.first_ts,
            last_ts: self.last_ts,
        }
    }
}

fn merge(into: &mut Stats, s: &Stats) {
    into.fills += s.fills;
    into.shares += s.shares;
    into.trades += s.trades;
    into.wins += s.wins;
    into.gross_profit += s.gross_profit;
    into.gross_loss += s.gross_loss;
    into.realized += s.realized;
    into.open_pnl += s.open_pnl;
    into.borrow_fee += s.borrow_fee;
    into.slippage_cost += s.slippage_cost;
    into.worst_slippage = into.worst_slippage.max(s.worst_slippage);
}

/// Apply a signed fill at average cost. Returns the realised profit and the
/// profits of any trades it finished (at most one: a flip ends one and starts another).
fn apply(p: &mut Pos, signed: i64, px: i64) -> (i128, Vec<i128>) {
    let mut done = Vec::new();
    if p.qty == 0 || p.qty.signum() == signed.signum() {
        let (old, add) = (i128::from(p.qty.abs()), i128::from(signed.abs()));
        let total = old + add;
        p.avg = ((old * i128::from(p.avg) + add * i128::from(px) + total / 2) / total) as i64;
        p.qty += signed;
        return (0, done);
    }
    let closing = p.qty.abs().min(signed.abs());
    let realized = i128::from(px - p.avg) * i128::from(closing) * i128::from(p.qty.signum());
    p.trip += realized;
    let before = p.qty.signum();
    p.qty += signed;
    if p.qty == 0 {
        p.avg = 0;
        done.push(std::mem::take(&mut p.trip));
    } else if p.qty.signum() != before {
        done.push(std::mem::take(&mut p.trip));
        p.avg = px;
    }
    (realized, done)
}

/// Raw units as dollars with two decimals, rounded half away from zero.
fn dollars(raw: i128) -> String {
    const CENT: i128 = 10_000_000;
    let cents = (raw.abs() + CENT / 2) / CENT;
    let sign = if raw < 0 && cents != 0 { "-" } else { "" };
    format!("{sign}${}.{:02}", cents / 100, cents % 100)
}

fn clamp(v: i128) -> i64 {
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

impl Report {
    /// Named integers, for storing as run-result metrics: totals as
    /// `pnl_net`, `trades`, ... and each group as `group.<label>.<name>`.
    /// Money is in raw units; the hit rate is in permille (absent with no trades).
    pub fn metrics(&self) -> Vec<(String, i64)> {
        let mut m = Vec::new();
        let mut put = |prefix: &str, s: &Stats| {
            let mut add = |name: &str, v: i64| m.push((format!("{prefix}{name}"), v));
            add("fills", s.fills as i64);
            add("shares", s.shares as i64);
            add("trades", s.trades as i64);
            add("wins", s.wins as i64);
            if let Some(h) = s.hit_rate_permille() {
                add("hit_rate_permille", h as i64);
            }
            add("gross_profit", clamp(s.gross_profit as i128));
            add("gross_loss", clamp(s.gross_loss as i128));
            add("pnl_realized", clamp(s.realized));
            add("pnl_open", clamp(s.open_pnl));
            add("borrow_fee", clamp(s.borrow_fee as i128));
            add("pnl_net", clamp(s.net_pnl()));
            add("slippage_cost", clamp(s.slippage_cost));
            if let Some(a) = s.avg_slippage() {
                add("slippage_avg_per_share", a);
            }
            add("slippage_worst_per_share", s.worst_slippage);
        };
        put("", &self.total);
        for (label, s) in &self.by_label {
            put(&format!("group.{label}."), s);
        }
        m.push(("max_drawdown".into(), clamp(self.max_drawdown as i128)));
        m.push(("events".into(), self.events as i64));
        m
    }

    /// A readable summary in dollars.
    pub fn render(&self) -> String {
        let line = |name: &str, s: &Stats| {
            let hit = s
                .hit_rate_permille()
                .map_or("n/a".to_owned(), |h| format!("{}.{}%", h / 10, h % 10));
            let slip = s
                .avg_slippage()
                .map_or("n/a".to_owned(), |a| dollars(i128::from(a) * 100));
            format!(
                "{name:<14} trades {:>4}  hit {hit:>6}  net {:>11}  realised {:>11}  open {:>11}  borrow {:>9}  slippage {:>10} (avg/100sh {slip})\n",
                s.trades,
                dollars(s.net_pnl()),
                dollars(s.realized),
                dollars(s.open_pnl),
                dollars(s.borrow_fee as i128),
                dollars(s.slippage_cost),
            )
        };
        let mut out = String::new();
        out.push_str(&line("total", &self.total));
        for (label, s) in &self.by_label {
            out.push_str(&line(label, s));
        }
        out.push_str(&format!(
            "max drawdown {}  events {}  span {} ms\n",
            dollars(self.max_drawdown as i128),
            self.events,
            (self.last_ts - self.first_ts) / 1_000_000,
        ));
        out.push_str(
            "Not modelled: queue position, depth beyond the best quote, market impact, commissions, \
             protective stops. Results are optimistic for strategies that depend on those.\n",
        );
        out
    }
}
