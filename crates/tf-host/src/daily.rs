//! The daily report (E18-S07): per strategy, and for the system, after a session.
//!
//! A pure reading of what the host kept (and of what the live driver gives it: the ingest queue's
//! counters, engine lag, capture facts, the replay check): the same host state and inputs always make
//! the same text. What was not measured says so, and nothing is filled in with a guess.

use std::fmt::Write as _;

use tf_core::{InstrumentId, Nanos, SymbolTable};
use tf_engine::OwnerStats;
use tf_ingest::{Lost, Stats};
use tf_ledger::LedgerStore;

use crate::def::Route;
use crate::equiv::clock;
use crate::host::{GapNote, Host, SlotState, StopReason};
use crate::replay::Report as ReplayReport;

/// What the live driver knows that the host does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemInputs {
    /// The ingest queue's counters for the day.
    pub ingest: Option<Stats>,
    /// How long the engine took, from an event being taken off the queue to its step finishing: the 99th
    /// percentile and the worst, in nanoseconds (the host reads no clock, so the driver measures it).
    pub engine_lag: Option<(Nanos, Nanos)>,
    pub capture: Option<CaptureFacts>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureFacts {
    pub segments: u64,
    pub records: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StrategySection {
    pub id: u16,
    pub name: String,
    pub route: Route,
    pub state: String,
    pub universe_fp: u64,
    /// Chosen before the open by the static layer.
    pub static_symbols: usize,
    /// Held at some time by a dynamic layer (0 for a static universe), and held at the end.
    pub ever_held: usize,
    pub members_now: usize,
    pub reviews: u64,
    pub intents: u64,
    pub accepted: u64,
    pub rejections: Vec<(String, u64)>,
    pub refused_by_broker: u64,
    /// What the brokers refused and why: `(was_a_short_sale, reason, count)`.
    pub broker_refusals: Vec<(bool, String, u64)>,
    pub rate_limited: u64,
    pub unanswered: u64,
    pub fills: u64,
    pub shares: u64,
    /// Dollars traded, in raw price units.
    pub turnover: u128,
    pub realized: i128,
    pub unrealized: i128,
    pub flatten_orders: u64,
    pub top_symbols: Vec<(String, u64)>,
    pub tier1: Option<OwnerStats>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemSection {
    pub events: u64,
    pub active_seconds: u64,
    pub peak_events: u32,
    pub peak_at: Nanos,
    /// Upper bounds on how long events took to reach us, nanoseconds.
    pub feed_lag_p50: Nanos,
    pub feed_lag_p99: Nanos,
    pub feed_lag_max: Nanos,
    pub inputs: SystemInputs,
    pub gaps: Vec<GapNote>,
    pub anomalies: Vec<String>,
    pub anomaly_count: usize,
    pub ledger_refusals: u64,
    pub working_orders: usize,
    pub tier1_symbols: usize,
    pub tier1_capacity: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyReport {
    pub label: String,
    pub strategies: Vec<StrategySection>,
    pub system: SystemSection,
    /// The replay check's text and whether it was equal, if it was run.
    pub replay: Option<(bool, String)>,
}

/// Dollars and cents from raw price units (a billionth of a dollar), cut toward zero.
pub fn money(raw: i128) -> String {
    let cents = raw / 10_000_000;
    let (sign, c) = if cents < 0 {
        ("-", -cents)
    } else {
        ("", cents)
    };
    let dollars = (c / 100).to_string();
    let mut grouped = String::new();
    for (i, ch) in dollars.chars().enumerate() {
        if i > 0 && (dollars.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}${grouped}.{:02}", c % 100)
}

pub(crate) fn ns(n: Nanos) -> String {
    match n {
        u64::MAX => "more than 9 s".to_owned(),
        n if n >= 1_000_000_000 => format!("{}.{:03} s", n / 1_000_000_000, n / 1_000_000 % 1000),
        n if n >= 1_000_000 => format!("{}.{:03} ms", n / 1_000_000, n / 1_000 % 1000),
        n if n >= 1_000 => format!("{}.{:03} us", n / 1_000, n % 1_000),
        n => format!("{n} ns"),
    }
}

fn name_of(t: &SymbolTable, id: InstrumentId) -> String {
    t.name(id)
        .map_or_else(|| format!("instrument {id}"), str::to_owned)
}

impl DailyReport {
    pub fn build<S: LedgerStore>(
        host: &Host<S>,
        label: &str,
        inputs: SystemInputs,
        replay: Option<&ReplayReport>,
    ) -> DailyReport {
        let names = &host.reference().symbols;
        let g = host.journal().gateway();
        let promoter_stats = host.promoter().owner_stats();
        let strategies = host
            .strategies()
            .into_iter()
            .map(|(id, name, state, st)| {
                let (fills, shares, turnover) = host.fills_of(id);
                let (fp, chosen, ever, now) = host.watched_by(id).unwrap_or((0, 0, 0, 0));
                StrategySection {
                    id,
                    name: name.to_owned(),
                    route: host.route_of(id).unwrap_or(Route::Sim),
                    state: match state {
                        SlotState::Running => "ran all day".to_owned(),
                        SlotState::Stopped(StopReason::Panicked(m)) => {
                            format!("STOPPED: its code panicked ({m})")
                        }
                        SlotState::Stopped(StopReason::SoftLoss) => {
                            "STOPPED: crossed its soft loss limit".to_owned()
                        }
                        SlotState::Stopped(StopReason::HardLoss) => {
                            "STOPPED: crossed its hard loss limit and was flattened".to_owned()
                        }
                        SlotState::Stopped(StopReason::Killed) => {
                            "STOPPED: killed by an operator and flattened".to_owned()
                        }
                    },
                    universe_fp: fp,
                    static_symbols: chosen,
                    ever_held: ever,
                    members_now: now,
                    reviews: host.reviews_of(id).unwrap_or(0),
                    intents: st.intents,
                    accepted: st.accepted,
                    rejections: host
                        .rejections_of(id)
                        .into_iter()
                        .map(|(r, n)| (r.to_owned(), n))
                        .collect(),
                    refused_by_broker: st.refused_by_broker,
                    broker_refusals: host.broker_refusals_of(id),
                    rate_limited: st.rate_limited,
                    unanswered: st.unanswered,
                    fills,
                    shares,
                    turnover,
                    realized: g.strategy_realized(id),
                    unrealized: g.strategy_unrealized(id),
                    flatten_orders: st.flatten_orders,
                    top_symbols: host
                        .top_symbols_of(id, 5)
                        .into_iter()
                        .map(|(i, n)| (name_of(names, i), n))
                        .collect(),
                    tier1: promoter_stats
                        .iter()
                        .find(|(o, _)| *o == id)
                        .map(|(_, s)| *s),
                }
            })
            .collect();
        let (events, active_seconds, peak_events, peak_at) = host.rate();
        let lag_max = host.feed_lag_quantile(1000);
        DailyReport {
            label: label.to_owned(),
            strategies,
            system: SystemSection {
                events,
                active_seconds,
                peak_events,
                peak_at,
                feed_lag_p50: host.feed_lag_quantile(500),
                feed_lag_p99: host.feed_lag_quantile(990),
                feed_lag_max: lag_max,
                inputs,
                gaps: host.gaps().to_vec(),
                anomalies: host.anomalies().iter().take(10).cloned().collect(),
                anomaly_count: host.anomalies().len(),
                ledger_refusals: host.ledger_refusals(),
                working_orders: host.working_orders().len(),
                tier1_symbols: host.promoter().promoted().len(),
                tier1_capacity: host.promoter().config().max_tier1,
            },
            replay: replay.map(|r| (r.verdict.is_equal(), r.text())),
        }
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "Daily report: {}", self.label);
        let _ = writeln!(s, "{}", "=".repeat(60));
        s.push_str("No fill in this report is real. Strategies on the simulated broker were filled against the\nrecorded quotes with latency, with no queue position, market impact or commissions. Strategies on a\npaper broker were filled by it, and paper fills are optimistic: they ignore queue position and the\nmarket impact of our own orders. Profit and loss here is therefore an upper bound, not a result.\n\n");
        let _ = writeln!(s, "STRATEGIES ({})", self.strategies.len());
        for t in &self.strategies {
            let label = match t.route {
                Route::Sim => "simulated fills",
                Route::Paper => "paper fills, optimistic",
            };
            let _ = writeln!(s, "\n  {} {} [{}] {}", t.id, t.name, label, t.state);
            let _ = writeln!(
                s,
                "    watched:  universe {:016x}: {} symbols chosen before the open{}, {} held at the end, {} reviews",
                t.universe_fp,
                t.static_symbols,
                if t.ever_held > 0 {
                    format!(", {} held at some time by its dynamic layer", t.ever_held)
                } else {
                    String::new()
                },
                t.members_now,
                t.reviews
            );
            let _ = writeln!(
                s,
                "    orders:   {} intents, {} accepted by the gateway, {} fills of {} shares, {} traded",
                t.intents,
                t.accepted,
                t.fills,
                t.shares,
                money(i128::try_from(t.turnover).unwrap_or(i128::MAX))
            );
            let refused: u64 = t.rejections.iter().map(|r| r.1).sum();
            if refused == 0 {
                let _ = writeln!(s, "    refused:  none by the gateway");
            } else {
                let by: Vec<String> = t
                    .rejections
                    .iter()
                    .map(|(r, n)| format!("{r} {n}"))
                    .collect();
                let _ = writeln!(
                    s,
                    "    refused:  {refused} by the gateway ({})",
                    by.join(", ")
                );
            }
            if t.refused_by_broker + t.rate_limited + t.unanswered > 0 {
                let _ = writeln!(
                    s,
                    "    broker:   {} refused, {} rate limited, {} with no answer",
                    t.refused_by_broker, t.rate_limited, t.unanswered
                );
            }
            // Short sales the broker would not take, with its reason, and the other refusals.
            for (short, why, n) in &t.broker_refusals {
                let _ = writeln!(
                    s,
                    "      {} refused {n}x: {why}",
                    if *short { "short sale" } else { "order" }
                );
            }
            let _ = writeln!(
                s,
                "    p&l:      {} realized, {} unrealized, {} in all",
                money(t.realized),
                money(t.unrealized),
                money(t.realized + t.unrealized)
            );
            if t.flatten_orders > 0 {
                let _ = writeln!(s, "    closed by the host: {} orders", t.flatten_orders);
            }
            if !t.top_symbols.is_empty() {
                let top: Vec<String> = t
                    .top_symbols
                    .iter()
                    .map(|(n, q)| format!("{n} {q}"))
                    .collect();
                let _ = writeln!(s, "    traded most (shares): {}", top.join(", "));
            }
            if let Some(o) = &t.tier1 {
                let _ = writeln!(
                    s,
                    "    tier 1:   {} asked, {} already there, {} promoted, {} denied for room, {} lost to others",
                    o.requests, o.already, o.promoted, o.denied_full, o.lost
                );
            }
        }
        let y = &self.system;
        let _ = writeln!(s, "\nSYSTEM");
        let mean = y.events.checked_div(y.active_seconds).unwrap_or(0);
        let _ = writeln!(
            s,
            "  events:     {} over {} active seconds, {} a second on average",
            y.events, y.active_seconds, mean
        );
        let _ = writeln!(
            s,
            "  busiest second: {} events at {}",
            y.peak_events,
            clock(y.peak_at)
        );
        let _ = writeln!(
            s,
            "  feed lag (received less event time): half within {}, 99% within {}, all within {}",
            ns(y.feed_lag_p50),
            ns(y.feed_lag_p99),
            ns(y.feed_lag_max)
        );
        match &y.inputs.engine_lag {
            Some((p99, max)) => {
                let _ = writeln!(
                    s,
                    "  engine lag: 99% within {}, worst {}",
                    ns(*p99),
                    ns(*max)
                );
            }
            None => {
                let _ = writeln!(s, "  engine lag: not measured");
            }
        }
        match &y.inputs.ingest {
            Some(i) => {
                let lost = i.dropped_trades + i.dropped_quotes + i.dropped_control;
                let _ = writeln!(
                    s,
                    "  ingest queue: {} offered, {} conflated, {} dropped ({} trades, {} quotes, {} control), {} gaps, fullest {}",
                    i.offered,
                    i.conflated,
                    lost,
                    i.dropped_trades,
                    i.dropped_quotes,
                    i.dropped_control,
                    i.gaps,
                    i.max_depth
                );
                if lost > 0 {
                    let _ = writeln!(
                        s,
                        "    EVENTS WERE LOST: the engine saw less than the feed sent, and the capture holds more than the engine did"
                    );
                }
            }
            None => {
                let _ = writeln!(s, "  ingest queue: not measured");
            }
        }
        if y.gaps.is_empty() {
            let _ = writeln!(s, "  gaps:       none reported");
        } else {
            let _ = writeln!(s, "  gaps:       {}", y.gaps.len());
            for g in &y.gaps {
                let _ = writeln!(
                    s,
                    "    {} {} between {} and {}",
                    g.count,
                    match g.lost {
                        Lost::Trades => "trades lost",
                        Lost::Control => "control events lost",
                        Lost::Skipped =>
                            "gateway skip notices (an unknown number of records, skipped because we read too slowly)",
                    },
                    clock(g.first_ts),
                    clock(g.last_ts)
                );
            }
        }
        match &y.inputs.capture {
            Some(c) => {
                let _ = writeln!(
                    s,
                    "  capture:    {} records in {} segments, {} bytes",
                    c.records, c.segments, c.bytes
                );
            }
            None => {
                let _ = writeln!(s, "  capture:    not reported");
            }
        }
        let _ = writeln!(
            s,
            "  tier 1:     {} of {} places held at the end",
            y.tier1_symbols, y.tier1_capacity
        );
        let _ = writeln!(
            s,
            "  ledger:     {} things it would not take; {} orders still working",
            y.ledger_refusals, y.working_orders
        );
        if y.anomaly_count > 0 {
            let _ = writeln!(s, "  notes ({}):", y.anomaly_count);
            for a in &y.anomalies {
                let _ = writeln!(s, "    {a}");
            }
            if y.anomaly_count > y.anomalies.len() {
                let _ = writeln!(
                    s,
                    "    ... and {} more",
                    y.anomaly_count - y.anomalies.len()
                );
            }
        }
        let _ = writeln!(s, "\nREPLAY CHECK");
        match &self.replay {
            Some((_, text)) => {
                for l in text.lines() {
                    let _ = writeln!(s, "  {l}");
                }
            }
            None => {
                let _ = writeln!(s, "  not run");
            }
        }
        s
    }
}
