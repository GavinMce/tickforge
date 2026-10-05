//! The backtest loop: strategy -> risk gateway -> simulated broker -> report.
//!
//! Each piece was built and tested alone; this wires them the way a live run
//! would be, so the gateway's limits apply to a strategy's real intents and the
//! fills the broker makes flow back to the gateway's book and to the strategy.
//!
//! Per market event, in this order:
//! 1. the broker processes the event (orders that have reached the venue meet the
//!    market as it was before it);
//! 2. new fills go to the gateway's position book and the report; orders that
//!    ended unfilled (expired, cancelled) release the gateway's working exposure;
//!    the strategy is told what became of its orders;
//! 3. the report and the gateway's marks see the event;
//! 4. the strategy sees the event;
//! 5. every intent it emitted goes to the gateway. An accepted one is sent to the
//!    broker. A rejected one comes back to the strategy as a rejection, at once,
//!    so it can free whatever it was holding for the order.
//!
//! At the end the broker's remaining orders expire and borrow fees are charged.
//! The result carries the report, the gateway's rejection counts and audit log,
//! and both books' final positions so a caller can check they agree.
//!
//! Event time throughout; nothing here reads a clock or does I/O.

pub mod ab;
pub mod compare;
pub mod export;

use std::collections::BTreeMap;

use tf_core::{Event, NANOS_PER_SEC, Nanos};
use tf_risk::{Audit, GapRule, Gateway, Limits, reason_name};
use tf_strategy::report::{Report, ReportBuilder, ReportError};
use tf_strategy::{
    Decision, Decline, EntryTrace, Fill, Host, Intent, IntentId, MomentumLong, MomentumParams,
    MtfBars, MtfConfig, OrderId, OrderState, OrderUpdate, RuleSet, SimBroker, SimConfig, Strategy,
    StrategyId, TrendLong, TrendParams,
};
use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

const DOLLAR: u128 = 1_000_000_000;

#[derive(Clone, Debug)]
pub struct BacktestConfig {
    pub sim: SimConfig,
    pub limits: Limits,
    pub params: MomentumParams,
    pub trend: TrendParams,
    /// Entry rules for the momentum strategy; `None` is the built-in set.
    pub rules: Option<RuleSet>,
}

/// Limits for demonstration runs: $5,000 an order, 5,000 shares a name, $20,000
/// gross, $1,000 daily loss, 20 orders per 10 s, and shorts sized so a doubling
/// loses at most 2% of $100,000.
pub fn default_limits() -> Limits {
    Limits::new(
        5_000 * DOLLAR,
        5_000,
        20_000 * DOLLAR,
        1_000 * DOLLAR,
        20,
        10 * NANOS_PER_SEC,
    )
    .expect("valid limits")
    .with_gap_rule(GapRule::new(100_000 * DOLLAR, 20_000, 1000).expect("valid rule"))
}

impl Default for BacktestConfig {
    fn default() -> Self {
        BacktestConfig {
            sim: SimConfig {
                latency_ns: 50_000_000,
                borrow_bps_per_year: 0,
            },
            limits: default_limits(),
            params: MomentumParams::default(),
            trend: TrendParams::default(),
            rules: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BacktestResult {
    pub report: Report,
    /// Intents the strategy emitted.
    pub intents: u64,
    pub accepted: u64,
    /// Gateway rejections by reason name.
    pub rejections: BTreeMap<&'static str, u64>,
    pub fills: u64,
    /// Things that should never happen: a fill for an order the gateway did not
    /// accept, or the gateway refusing a fill or close the broker reported.
    pub bookkeeping_errors: u64,
    pub gateway_positions: Vec<i64>,
    pub broker_positions: Vec<i64>,
    /// Orders the gateway still counts as working at the end. Zero unless something
    /// was left open on purpose.
    pub gateway_working: usize,
    /// The gateway's P&L (realised plus marked) at the end, raw units.
    pub gateway_pnl: i128,
    /// A hash of every decision and fill, for comparing runs.
    pub outcome_hash: u64,
    pub audit: Vec<Audit>,
    /// Every intent the strategy emitted with the gateway's answer, in order.
    pub decisions: Vec<(Intent, Decision)>,
    /// Every fill the simulated broker made, in order.
    pub fill_log: Vec<Fill>,
}

impl BacktestResult {
    /// The two position books agree on every instrument.
    pub fn books_agree(&self) -> bool {
        self.gateway_positions == self.broker_positions
    }
}

#[derive(Default)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn put(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
}

/// The state of one gated run.
struct Loop<'a, S: Strategy> {
    host: &'a mut Host<S>,
    broker: &'a mut SimBroker,
    gateway: &'a mut Gateway,
    report: ReportBuilder,
    /// The gateway's order for each accepted intent.
    orders: BTreeMap<IntentId, OrderId>,
    seen_fills: usize,
    hash: Fnv,
    intents: u64,
    accepted: u64,
    errors: u64,
    n: usize,
    last_ts: Nanos,
    decisions: Vec<(Intent, Decision)>,
}

impl<'a, S: Strategy> Loop<'a, S> {
    fn new(
        host: &'a mut Host<S>,
        broker: &'a mut SimBroker,
        gateway: &'a mut Gateway,
        labels: Vec<String>,
    ) -> Result<Self, ReportError> {
        let n = labels.len();
        Ok(Loop {
            host,
            broker,
            gateway,
            report: ReportBuilder::new(labels)?,
            orders: BTreeMap::new(),
            seen_fills: 0,
            hash: Fnv::new(),
            intents: 0,
            accepted: 0,
            errors: 0,
            n,
            last_ts: 0,
            decisions: Vec::new(),
        })
    }

    /// One event, in the order described in the module docs. A parameter change is not
    /// market data: it goes to the host (which applies it) and nowhere else.
    fn step(&mut self, ev: &Event) {
        if matches!(ev, Event::ParamChange(_)) {
            self.host.on_event(ev);
            self.submit_pending();
            return;
        }
        self.last_ts = ev.ts_recv();
        self.broker.on_event(ev);
        self.settle();
        self.report.on_event(ev);
        if let Event::Trade(t) = ev {
            self.gateway.mark(t.hdr.instrument, t.px);
        }
        self.host.on_event(ev);
        self.submit_pending();
    }

    /// Profit and loss so far (realised plus marked), before borrow fees.
    fn equity(&self) -> i128 {
        self.report.equity()
    }

    /// The end of the session: orders still at the venue expire, borrow is charged.
    fn finish(mut self) -> BacktestResult {
        self.broker.end_of_day(self.last_ts);
        self.settle();
        let n = self.n;
        let borrow: Vec<u128> = (0..n as u32).map(|i| self.broker.borrow_fee(i)).collect();
        let Loop {
            broker,
            gateway,
            report,
            hash,
            intents,
            accepted,
            errors,
            decisions,
            ..
        } = self;
        BacktestResult {
            report: report.finish(&borrow),
            intents,
            accepted,
            rejections: gateway.rejection_counts().clone(),
            fills: broker.fills().len() as u64,
            bookkeeping_errors: errors,
            gateway_positions: (0..n as u32).map(|i| gateway.position(i)).collect(),
            broker_positions: (0..n as u32).map(|i| broker.position(i)).collect(),
            gateway_working: gateway.working_orders(),
            gateway_pnl: gateway.daily_pnl(),
            outcome_hash: hash.0,
            audit: gateway.drain_audit(),
            decisions,
            fill_log: broker.fills().to_vec(),
        }
    }

    /// Everything the broker did since the last call goes to the gateway, the
    /// report and the strategy.
    fn settle(&mut self) {
        for f in &self.broker.fills()[self.seen_fills..] {
            match self.orders.get(&f.intent) {
                Some(&id) if self.gateway.on_fill(id, f.qty, f.px).is_ok() => {}
                _ => self.errors += 1,
            }
            self.report.on_fill(f);
            self.hash.put(f.intent.seq);
            self.hash.put(u64::from(f.qty));
            self.hash.put(f.px.raw() as u64);
            self.hash.put(f.ts);
        }
        self.seen_fills = self.broker.fills().len();
        for u in self.broker.drain_updates() {
            // An order that finished short of its size frees what it was holding. One
            // that filled completely was already removed by its last fill.
            if u.state.is_terminal() && u.state != OrderState::Filled {
                match self.orders.get(&u.intent) {
                    Some(&id) if self.gateway.on_closed(id).is_ok() => {}
                    _ => self.errors += 1,
                }
            }
            self.host.on_order_update(&u);
        }
    }

    /// Send what the strategy asked for through the gateway.
    fn submit_pending(&mut self) {
        // A rejection can make the strategy submit again, so go round until quiet.
        for _ in 0..8 {
            let batch = self.host.drain_intents();
            if batch.is_empty() {
                return;
            }
            for i in batch {
                self.intents += 1;
                let decision = self.gateway.decide(&i, i.ts);
                self.decisions.push((i, decision));
                match decision {
                    Decision::Accepted(id) => {
                        self.accepted += 1;
                        self.orders.insert(i.id, id);
                        self.hash.put(i.id.seq << 1);
                        self.broker.submit(&i);
                    }
                    Decision::Rejected(r) => {
                        self.hash.put(i.id.seq << 1 | 1);
                        for b in reason_name(&r).bytes() {
                            self.hash.put(u64::from(b));
                        }
                        self.host
                            .on_order_update(&OrderUpdate::rejected(i.id, r, i.ts));
                    }
                }
            }
        }
    }
}

/// Run `host` over `events` with its intents gated by `gateway` and executed by
/// `broker`. `labels[i]` groups instrument `i` in the report. `before_event` is
/// called before each event so a caller can act on the gateway (for example
/// engage the kill switch at a chosen time).
pub fn run_gated<S: Strategy>(
    host: &mut Host<S>,
    broker: &mut SimBroker,
    gateway: &mut Gateway,
    labels: Vec<String>,
    events: impl IntoIterator<Item = Event>,
    mut before_event: impl FnMut(&Event, &mut Gateway),
) -> Result<BacktestResult, ReportError> {
    let mut l = Loop::new(host, broker, gateway, labels)?;
    for ev in events {
        before_event(&ev, l.gateway);
        l.step(&ev);
    }
    Ok(l.finish())
}

/// Strategy 1 (long side) over `events`, gated by the configured limits.
pub fn momentum_backtest(
    events: impl IntoIterator<Item = Event>,
    labels: Vec<String>,
    cfg: &BacktestConfig,
) -> Result<BacktestResult, String> {
    momentum_backtest_traced(events, labels, cfg).map(|t| t.result)
}

/// A backtest with the strategy's own account of its decisions.
pub struct Traced {
    pub result: BacktestResult,
    /// Why each entry happened.
    pub entries: Vec<EntryTrace>,
    /// The symbols it watched and declined.
    pub declines: Vec<Decline>,
}

/// [`momentum_backtest`], also returning the evidence behind each entry and each decline.
pub fn momentum_backtest_traced(
    events: impl IntoIterator<Item = Event>,
    labels: Vec<String>,
    cfg: &BacktestConfig,
) -> Result<Traced, String> {
    let n = labels.len();
    let strat = MomentumLong::new(StrategyId(1), cfg.params, n).map_err(|e| e.0.to_owned())?;
    let strat = match &cfg.rules {
        Some(r) => strat.with_rules(r.clone()),
        None => strat,
    };
    let mut host = Host::new(strat, n);
    let mut broker = SimBroker::new(cfg.sim, n);
    let mut gateway = Gateway::new(cfg.limits, n);
    let result = run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        events,
        |_, _| {},
    )
    .map_err(|e| format!("{e:?}"))?;
    Ok(Traced {
        result,
        entries: host.strategy().entry_traces().to_vec(),
        declines: host.strategy().declines().to_vec(),
    })
}

/// The example indicator strategy ([`TrendLong`]) over `events`, with one-minute bars
/// built for the symbols it follows, gated by the configured limits. Its indicators need
/// several minutes to warm up, so sessions for it should be long and start quiet (see
/// [`demo_session_with_lead`]).
pub fn trend_backtest(
    events: impl IntoIterator<Item = Event>,
    labels: Vec<String>,
    cfg: &BacktestConfig,
) -> Result<BacktestResult, String> {
    let n = labels.len();
    let strat = TrendLong::new(StrategyId(2), cfg.trend, n).map_err(|e| e.0.to_owned())?;
    let bars = MtfBars::new(MtfConfig::default(), n, cfg.trend.max_tracked as usize);
    let mut host = Host::new(strat, n).with_bars(bars);
    let mut broker = SimBroker::new(cfg.sim, n);
    let mut gateway = Gateway::new(cfg.limits, n);
    run_gated(
        &mut host,
        &mut broker,
        &mut gateway,
        labels,
        events,
        |_, _| {},
    )
    .map_err(|e| format!("{e:?}"))
}

/// A synthetic session: `healthy` and `dangerous` runners (staggered lead-ins so
/// they do not move in lockstep) and `quiet` symbols. Returns the events and the
/// label of each instrument (`healthy`, `dangerous`, `quiet`).
pub fn demo_session(
    seed: u64,
    secs: u64,
    healthy: u32,
    dangerous: u32,
    quiet: u32,
) -> (Vec<Event>, Vec<String>) {
    demo_session_with_lead(seed, secs, healthy, dangerous, quiet, 20)
}

/// [`demo_session`] with the first runner's quiet lead-in set to `base_lead_secs`
/// (each later one starts 7 s after the one before).
pub fn demo_session_with_lead(
    seed: u64,
    secs: u64,
    healthy: u32,
    dangerous: u32,
    quiet: u32,
    base_lead_secs: u64,
) -> (Vec<Event>, Vec<String>) {
    let lead = |i: usize| (base_lead_secs + 7 * i as u64) * NANOS_PER_SEC;
    let mut specs: Vec<(&str, u32, Option<PullbackKind>)> = Vec::new();
    specs.extend((0..healthy).map(|k| ("healthy", k, Some(PullbackKind::Healthy))));
    specs.extend((0..dangerous).map(|k| ("dangerous", k, Some(PullbackKind::Dangerous))));
    specs.extend((0..quiet).map(|k| ("quiet", k, None)));
    let symbols: Vec<SymbolSpec> = specs
        .iter()
        .enumerate()
        .map(|(i, &(label, k, kind))| SymbolSpec {
            symbol: format!("{}{k:02}", label.to_uppercase()),
            base_px_cents: 300 + 50 * (i as i64 % 20),
            base_interval_ns: 300_000_000,
            quote_every: 2,
            scenario: kind.map_or_else(Scenario::quiet, |kind| Scenario::runner(kind, lead(i))),
            news: Vec::new(),
        })
        .collect();
    let labels = specs.iter().map(|&(label, ..)| label.to_owned()).collect();
    let cfg = SynthConfig {
        seed,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: secs * NANOS_PER_SEC,
        symbols,
    };
    (SynthStream::new(&cfg).collect(), labels)
}

/// Raw price units for whole dollars.
pub fn dollars(d: u64) -> u128 {
    u128::from(d) * DOLLAR
}

#[cfg(test)]
mod ab_tests;
#[cfg(test)]
mod tests;
