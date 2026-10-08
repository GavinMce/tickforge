//! One trade, replayed (E19-S35): the data of its page and the page.
//!
//! Everything comes from the results directory: the trip (what happened and what it cost), the day's decision log (every
//! decision about the symbol and the gateway's answer, every fill), the market kept around the trade (ADR 0064) and the strategy's
//! own account of why (its traces, ADR 0063). The page is one file with its data embedded that asks nobody for anything, in the
//! policy of the explorer's page. What is not there is said so on the page, not guessed.
//!
//! Times in the data are microseconds from the second the trade was entered in (`base_ns`), because event time in nanoseconds
//! does not fit a number in the page's script; the page adds them to the New York time of that second.

use std::path::Path;

use tf_core::Nanos;
use tf_strategy::intent::{Purpose, Side};
use tf_strategy::trace::Trace;

use super::cost::CostModel;
use super::keep::EvEvent;
use super::run::INSTRUMENTS;
use super::trips::Trip;
use super::view::{
    ViewError, bp, day_of, dollars, et, js, milli, open, price, reason_text, strategy_of,
};
use crate::equiv::{Answer, Rec};
use tf_calendar::Calendar;

/// Where the page takes its data.
const MARK: &str = "/*TRADE_DATA*/null";
const PAGE: &str = include_str!("../../viewer/trade.html");

/// The most points of each kind charted: events beyond are thinned, away from the trade first.
const CHART_CAP: usize = 4000;
/// Events this close to the trade are all kept (unless there are more than the cap).
const NEAR: Nanos = 2_000_000_000;
/// The trace rows shown when a trace is long: the first of them, and every row about this symbol.
const TRACE_HEAD: usize = 20;
const TRACE_WHOLE: usize = 40;

pub(super) fn px4(raw: i64) -> i64 {
    (i128::from(raw) + 50_000).div_euclid(100_000) as i64
}

fn side_text(s: Side) -> &'static str {
    match s {
        Side::Buy => "buy",
        Side::Sell => "sell",
        Side::SellShort => "sell short",
    }
}

fn sells(s: Side) -> bool {
    s != Side::Buy
}

/// The page that replays trade `n` of a strategy on a day, its data embedded.
pub fn trade_page(
    root: &Path,
    scenario: &str,
    day: &str,
    strategy: u16,
    n: usize,
) -> Result<String, ViewError> {
    let data = trade_json(root, scenario, day, strategy, n)?;
    debug_assert_eq!(PAGE.matches(MARK).count(), 1);
    Ok(PAGE.replacen(MARK, &data, 1))
}

/// Keep at most `cap` of `events`: all of those within `[from, to]` (up to the cap), the rest thinned evenly, the last of each
/// group.
pub(super) fn thin<T: Copy>(
    events: &[T],
    ts: impl Fn(&T) -> Nanos,
    from: Nanos,
    to: Nanos,
    cap: usize,
) -> Vec<T> {
    if events.len() <= cap {
        return events.to_vec();
    }
    let near = events
        .iter()
        .filter(|e| ts(e) >= from && ts(e) <= to)
        .count();
    let far = events.len() - near;
    let room = cap.saturating_sub(near).max(cap / 10);
    let step = far.div_ceil(room.max(1)).max(1);
    let mut out = Vec::new();
    let mut seen = 0usize;
    for (i, e) in events.iter().enumerate() {
        if ts(e) >= from && ts(e) <= to {
            out.push(*e);
            continue;
        }
        seen += 1;
        let last_of_group = seen % step == 0;
        let next_is_near = events
            .get(i + 1)
            .is_some_and(|x| ts(x) >= from && ts(x) <= to);
        if last_of_group || next_is_near {
            out.push(*e);
        }
    }
    if out.len() > cap {
        // Too many near the trade: thin the lot evenly.
        let step = out.len().div_ceil(cap);
        return out
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % step == 0)
            .map(|(_, e)| e)
            .collect();
    }
    out
}

/// One decision about the symbol and the gateway's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Dec {
    pub ts: Nanos,
    pub side: Side,
    pub qty: u32,
    pub purpose: Purpose,
    pub limit: i64,
    pub reason: u16,
    /// The order it was accepted as; none if the gateway refused it.
    pub order: Option<u64>,
    pub why: Option<String>,
}

/// A fill: time, order, shares, price.
pub(super) type Fill = (Nanos, u64, u32, i64);

/// The instrument number of `symbol`, from the host's own trace of the day's instruments.
pub(super) fn instrument_of(traces: &[(u16, Trace)], symbol: &str) -> Option<u32> {
    traces
        .iter()
        .find(|(s, t)| *s == 0 && t.kind == INSTRUMENTS)
        .and_then(|(_, t)| {
            let (ids, names) = (t.column("instrument")?, t.column("symbol")?);
            ids.into_iter()
                .zip(names)
                .find(|(_, s)| *s == symbol)
                .and_then(|(i, _)| i.parse().ok())
        })
}

/// When the strategy's latest earlier trade in `symbol` ended, before its `n`th trade.
pub(super) fn previous_exit(mine: &[&Trip], n: usize, symbol: &str) -> Option<Nanos> {
    mine[..n]
        .iter()
        .filter(|t| t.symbol == symbol)
        .map(|t| t.exit_ts)
        .max()
}

/// The decisions a strategy made about an instrument after `previous_exit` (exclusive) up to `exit_ts` (inclusive), and
/// the fills of the orders they were accepted as.
pub(super) fn trade_recs(
    recs: &[Rec],
    strategy: u16,
    instrument: u32,
    previous_exit: Option<Nanos>,
    exit_ts: Nanos,
) -> (Vec<Dec>, Vec<Fill>) {
    let mut decisions = Vec::new();
    for rec in recs {
        if let Rec::Decision {
            ts,
            strategy: s,
            instrument: ins,
            side,
            qty,
            purpose,
            limit,
            reason,
            answer,
            ..
        } = rec
        {
            if *s == strategy
                && *ins == instrument
                && *ts <= exit_ts
                && previous_exit.is_none_or(|p| *ts > p)
            {
                decisions.push(Dec {
                    ts: *ts,
                    side: *side,
                    qty: *qty,
                    purpose: *purpose,
                    limit: *limit,
                    reason: *reason,
                    order: match answer {
                        Answer::Accepted(o) => Some(*o),
                        Answer::Rejected(_) => None,
                    },
                    why: match answer {
                        Answer::Rejected(w) => Some(w.clone()),
                        Answer::Accepted(_) => None,
                    },
                });
            }
        }
    }
    let orders: Vec<u64> = decisions.iter().filter_map(|d| d.order).collect();
    let mut fills = Vec::new();
    for rec in recs {
        if let Rec::Fill {
            ts, order, qty, px, ..
        } = rec
        {
            if orders.contains(order) {
                fills.push((*ts, *order, *qty, *px));
            }
        }
    }
    (decisions, fills)
}

/// Where on the chart each thing happened: the first accepted opening decision and its fill, the last accepted closing
/// decision and its fill, or the end of the day for a trade still held.
pub(super) fn marks_of(
    decisions: &[Dec],
    fills: &[Fill],
    trip: &Trip,
    us: &dyn Fn(Nanos) -> i64,
) -> Vec<(i64, &'static str, String)> {
    let mut marks: Vec<(i64, &'static str, String)> = Vec::new();
    let entry_dec = decisions
        .iter()
        .find(|d| d.purpose == Purpose::Open && d.order.is_some());
    if let Some(d) = entry_dec {
        marks.push((
            us(d.ts),
            "decision",
            format!(
                "Decision: {} {} at most {}; accepted and sent as order {}",
                side_text(d.side),
                d.qty,
                price(d.limit),
                d.order.unwrap_or(0)
            ),
        ));
        if let Some(&(ts, _, q, px)) = fills.iter().find(|f| Some(f.1) == d.order) {
            marks.push((
                us(ts),
                "fill",
                format!(
                    "Filled {q} at {}, {} ms after the decision",
                    price(px),
                    (ts - d.ts) / 1_000_000
                ),
            ));
        }
    }
    if trip.open_at_end {
        marks.push((
            us(trip.exit_ts),
            "end",
            format!(
                "End of the day: still held, counted at the last trade price {}",
                price(trip.exit_px)
            ),
        ));
    } else {
        let exit_dec = decisions
            .iter()
            .rev()
            .find(|d| d.purpose == Purpose::Close && d.order.is_some());
        if let Some(d) = exit_dec {
            marks.push((
                us(d.ts),
                "exit_decision",
                format!(
                    "Exit decision: {} {}, {}; sent as order {}",
                    side_text(d.side),
                    d.qty,
                    reason_text(trip.exit_reason),
                    d.order.unwrap_or(0)
                ),
            ));
            if let Some(&(ts, _, q, px)) = fills.iter().rev().find(|f| Some(f.1) == d.order) {
                marks.push((
                    us(ts),
                    "exit_fill",
                    format!(
                        "Exit filled {q} at {}, {} ms after the decision",
                        price(px),
                        (ts - d.ts) / 1_000_000
                    ),
                ));
            }
        }
    }
    marks.sort_by_key(|m| m.0);
    marks
}

/// The fees of a trade's sales as Section 31 and the Trading Activity Fee, recomputed from the sale fills (and, for a long
/// still held at the end of the day, the sale at the mark that was counted). `None` if the cost model has no rate for the day
/// or the two do not add up to what the trade paid: then only the total can be shown.
pub(super) fn fee_parts(
    cost: &CostModel,
    day: &str,
    decisions: &[Dec],
    fills: &[Fill],
    trip: &Trip,
) -> Option<(u128, u128)> {
    let decided = |o: u64| decisions.iter().find(|d| d.order == Some(o));
    let (mut sec, mut taf) = (0u128, 0u128);
    for &(_, o, q, px) in fills {
        if decided(o).is_some_and(|d| sells(d.side)) {
            let (a, b) = cost.sale_fee_parts(day, q, px).ok()?;
            sec += a;
            taf += b;
        }
    }
    if trip.open_at_end && trip.long {
        let (a, b) = cost.sale_fee_parts(day, trip.qty, trip.exit_px).ok()?;
        sec += a;
        taf += b;
    }
    (i128::try_from(sec + taf).ok() == Some(i128::from(trip.fees))).then_some((sec, taf))
}

struct Order {
    us: i64,
    text: String,
}

/// The data of one trade's page as JSON.
pub(super) fn trade_json(
    root: &Path,
    scenario: &str,
    day: &str,
    strategy: u16,
    n: usize,
) -> Result<String, ViewError> {
    let r = open(root, scenario)?;
    day_of(&r, day)?;
    let line = strategy_of(&r, strategy)?;
    let file = r.day(day)?;
    let mine: Vec<&Trip> = file
        .trips
        .iter()
        .filter(|t| t.strategy == strategy)
        .collect();
    let trip = *mine.get(n).ok_or_else(|| {
        ViewError::NotFound(format!(
            "{} made {} trades on {day}, so there is no trade {}",
            line.name,
            mine.len(),
            n + 1
        ))
    })?;
    // The log and the traces are what a day is whole with: a day without them is not shown.
    let log = r.log(day)?;
    let traces = r.traces(day)?;
    let base: Nanos = trip.entry_ts / 1_000_000_000 * 1_000_000_000;
    let us = |ts: Nanos| -> i64 { ((i128::from(ts) - i128::from(base)) / 1000) as i64 };
    let base_s = Calendar::us_equities().local(base).map_or(0, |(_, s)| s);
    let mut notes: Vec<String> = Vec::new();

    // Which instrument the symbol was, from the host's own trace; what the strategy decided about it during the trade.
    let instrument = instrument_of(&traces, &trip.symbol);
    let (decisions, fills) = match instrument {
        Some(i) => trade_recs(
            &log.recs,
            strategy,
            i,
            previous_exit(&mine, n, &trip.symbol),
            trip.exit_ts,
        ),
        None => {
            notes.push(
                "This day was kept before the host recorded which instrument each symbol was, so its orders cannot be followed. Run the scenario again to have them."
                    .to_owned(),
            );
            (Vec::new(), Vec::new())
        }
    };

    // The order lifecycle: each decision, then what the broker did with it.
    let mut life: Vec<Order> = Vec::new();
    for d in &decisions {
        let answer = match (&d.order, &d.why) {
            (Some(o), _) => format!("\"answer\":\"accepted\",\"order\":{o}"),
            (None, Some(w)) => format!("\"answer\":\"rejected\",\"why\":{}", js(w)),
            (None, None) => "\"answer\":\"none\"".to_owned(),
        };
        life.push(Order {
            us: us(d.ts),
            text: format!(
                "{{\"kind\":\"decision\",\"us\":{},\"side\":{},\"qty\":{},\"purpose\":{},\"limit\":{},\"limit_px\":{},\"reason\":{},{answer}}}",
                us(d.ts),
                js(side_text(d.side)),
                d.qty,
                js(if d.purpose == Purpose::Open { "open" } else { "close" }),
                px4(d.limit),
                js(&price(d.limit)),
                js(&reason_text(d.reason)),
            ),
        });
    }
    let decided = |o: u64| decisions.iter().find(|d| d.order == Some(o));
    for &(ts, o, qty, px) in &fills {
        let latency =
            decided(o).map_or("null".to_owned(), |d| ((ts - d.ts) / 1_000_000).to_string());
        life.push(Order {
            us: us(ts),
            text: format!(
                "{{\"kind\":\"fill\",\"us\":{},\"order\":{o},\"qty\":{qty},\"px\":{},\"px4\":{},\"latency_ms\":{latency}}}",
                us(ts),
                js(&price(px)),
                px4(px)
            ),
        });
    }
    life.sort_by_key(|o| o.us);

    let marks = marks_of(&decisions, &fills, trip, &us);

    // The costs. The fees split into their two kinds from the sale fills; if that does not add up to what the trip paid
    // (a leg the log does not show), only the total is given.
    let parts = if instrument.is_some() {
        fee_parts(r.cost(), day, &decisions, &fills, trip)
    } else {
        None
    };
    let parts_ok = parts.is_some();
    let (sec, taf) = parts.unwrap_or((0, 0));
    if !parts_ok {
        notes.push("The fees are shown as one amount: the orders in the log do not add up to what the trade paid.".to_owned());
    }
    let split = |v: u128| {
        if parts_ok {
            js(&price(i64::try_from(v).unwrap_or(i64::MAX)))
        } else {
            "null".to_owned()
        }
    };
    let money = format!(
        "{{\"gross\":{},\"sec\":{},\"taf\":{},\"fees\":{},\"borrow\":{},\"slippage\":{},\"slip_bp\":{},\"net\":{},\"net_cents\":{},\"bp\":{},\"r\":{}}}",
        js(&price(trip.gross)),
        split(sec),
        split(taf),
        js(&price(trip.fees)),
        js(&price(trip.borrow)),
        js(&price(trip.slippage)),
        js(&bp(trip.slip_bps_x100)),
        js(&price(trip.net)),
        js(&dollars(i128::from(trip.net))),
        js(&bp(trip.net_bps_x100)),
        trip.r_milli.map_or("null".to_owned(), |m| js(&milli(m))),
    );

    // The market around the trade.
    let market = match r.evidence(day) {
        Err(_) => {
            notes.push(
                "This day was run without evidence, so there is no market data to chart."
                    .to_owned(),
            );
            "null".to_owned()
        }
        Ok(ev) => {
            let window = ev.window.unwrap_or_default();
            let from = trip.entry_ts.saturating_sub(window.before);
            let to = trip.exit_ts.saturating_add(window.after);
            let all = ev.slice(&trip.symbol, from, to);
            if all.is_empty() {
                notes.push(format!(
                    "No market data was kept for {} around this trade.",
                    trip.symbol
                ));
                "null".to_owned()
            } else {
                let mut quotes = Vec::new();
                let mut trades = Vec::new();
                let mut status = Vec::new();
                for e in &all {
                    match *e {
                        EvEvent::Quote { ts, bid, ask, .. } => quotes.push((ts, bid, ask)),
                        EvEvent::Trade { ts, px, size } => trades.push((ts, px, size)),
                        EvEvent::Status { ts, kind, .. } => status.push((ts, kind)),
                    }
                }
                let (near_from, near_to) = (
                    trip.entry_ts.saturating_sub(NEAR),
                    trip.exit_ts.saturating_add(NEAR),
                );
                let (nq, nt) = (quotes.len(), trades.len());
                let quotes = thin(&quotes, |q| q.0, near_from, near_to, CHART_CAP);
                let trades = thin(&trades, |t| t.0, near_from, near_to, CHART_CAP);
                let q: Vec<String> = quotes
                    .iter()
                    .map(|&(t, b, a)| format!("[{},{},{}]", us(t), px4(b), px4(a)))
                    .collect();
                let t: Vec<String> = trades
                    .iter()
                    .map(|&(t, p, s)| format!("[{},{},{s}]", us(t), px4(p)))
                    .collect();
                let s: Vec<String> = status
                    .iter()
                    .take(50)
                    .map(|&(t, k)| format!("[{},{k}]", us(t)))
                    .collect();
                format!(
                    "{{\"from_us\":{},\"to_us\":{},\"quotes\":[{}],\"trades\":[{}],\"status\":[{}],\"quotes_total\":{nq},\"trades_total\":{nt}}}",
                    us(from.max(all.first().map_or(from, |e| e.ts()))),
                    us(to.min(all.last().map_or(to, |e| e.ts()))),
                    q.join(","),
                    t.join(","),
                    s.join(",")
                )
            }
        }
    };

    // The strategy's own account.
    let own: Vec<&Trace> = traces
        .iter()
        .filter(|(s, t)| *s == strategy && t.kind != "stats")
        .map(|(_, t)| t)
        .collect();
    if own.is_empty() {
        notes.push("This strategy recorded no account of why it acted: there is nothing more to show of the decision.".to_owned());
    }
    let evidence: Vec<String> = own.iter().map(|t| trace_json(t, &trip.symbol)).collect();

    let marks_json: Vec<String> = marks
        .iter()
        .map(|(u, k, l)| format!("{{\"us\":{u},\"kind\":{},\"label\":{}}}", js(k), js(l)))
        .collect();
    let notes_json: Vec<String> = notes.iter().map(|n| js(n)).collect();
    let life_json: Vec<&str> = life.iter().map(|o| o.text.as_str()).collect();
    Ok(format!(
        "{{\"scenario\":{},\"day\":{},\"strategy\":{{\"id\":{},\"name\":{}}},\"n\":{n},\"of\":{},\"symbol\":{},\"side\":{},\"qty\":{},\"open_at_end\":{},\"base_s\":{base_s},\"entry_us\":{},\"exit_us\":{},\"entry\":{},\"exit\":{},\"entry_px\":{},\"exit_px\":{},\"entry_px4\":{},\"exit_px4\":{},\"exit_reason\":{},\"money\":{money},\"orders\":[{}],\"marks\":[{}],\"market\":{market},\"evidence\":[{}],\"notes\":[{}]}}",
        js(scenario),
        js(day),
        line.id,
        js(&line.name),
        mine.len(),
        js(&trip.symbol),
        js(if trip.long { "long" } else { "short" }),
        trip.qty,
        trip.open_at_end,
        us(trip.entry_ts),
        us(trip.exit_ts),
        js(&et(trip.entry_ts)),
        js(&et(trip.exit_ts)),
        js(&price(trip.entry_px)),
        js(&price(trip.exit_px)),
        px4(trip.entry_px),
        px4(trip.exit_px),
        js(&reason_text(trip.exit_reason)),
        life_json.join(","),
        marks_json.join(","),
        evidence.join(","),
        notes_json.join(",")
    ))
}

/// Columns of the library's traces that hold a raw price (1e-9 dollars), shown as dollars; and the head values that hold a time.
const PRICE_COLUMNS: [&str; 4] = ["prior_close", "ref_px", "bid", "ask"];
const TIME_HEADS: [&str; 1] = ["close"];

/// One trace as JSON: its head, and its rows if few or else the first of them and every one about `symbol`.
pub(super) fn trace_json(t: &Trace, symbol: &str) -> String {
    let sym = t.columns.iter().position(|c| c == "symbol");
    let hit = |row: &Vec<String>| sym.is_some_and(|i| row.get(i).is_some_and(|c| c == symbol));
    let picked: Vec<usize> = if t.rows.len() <= TRACE_WHOLE {
        (0..t.rows.len()).collect()
    } else {
        (0..t.rows.len())
            .filter(|&i| i < TRACE_HEAD || hit(&t.rows[i]))
            .collect()
    };
    let head: Vec<String> = t
        .head
        .iter()
        .map(|(k, v)| {
            let shown = match v.parse::<u64>() {
                Ok(ts) if TIME_HEADS.contains(&k.as_str()) => et(ts),
                _ => v.clone(),
            };
            format!("[{},{}]", js(k), js(&shown))
        })
        .collect();
    let priced: Vec<bool> = t
        .columns
        .iter()
        .map(|c| PRICE_COLUMNS.contains(&c.as_str()))
        .collect();
    let cols: Vec<String> = t.columns.iter().map(|c| js(c)).collect();
    let rows: Vec<String> = picked
        .iter()
        .map(|&i| {
            let cells: Vec<String> = t.rows[i]
                .iter()
                .enumerate()
                .map(|(j, c)| match c.parse::<i64>() {
                    Ok(raw) if priced.get(j).copied().unwrap_or(false) => js(&price(raw)),
                    _ => js(c),
                })
                .collect();
            format!(
                "[{},{},[{}]]",
                i,
                u8::from(hit(&t.rows[i])),
                cells.join(",")
            )
        })
        .collect();
    format!(
        "{{\"kind\":{},\"time\":{},\"head\":[{}],\"columns\":[{}],\"rows\":[{}],\"total\":{}}}",
        js(&t.kind),
        js(&et(t.ts)),
        head.join(","),
        cols.join(","),
        rows.join(","),
        t.rows.len()
    )
}
