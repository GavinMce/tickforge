use tf_core::{Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Trade, TradeFlags};
use tf_manifest::{DataRange, Manifest, RunResult};
use tf_synth::{PullbackKind, Scenario, SplitMix64, SymbolSpec, SynthConfig, SynthStream};

use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderId;
use crate::report::{Report, ReportBuilder, ReportError};
use crate::sim::{Fill, SimBroker, SimConfig, run_backtest_observed};
use crate::strategy::{Ctx, Host, Request, Strategy, TimerId};

const D: i128 = 1_000_000_000;
const SEC: Nanos = NANOS_PER_SEC;

fn px(cents: i64) -> Px {
    Px::from_cents(cents)
}

fn fill(inst: u32, side: Side, qty: u32, cents: i64, slippage_cents: i64) -> Fill {
    Fill {
        order: OrderId(0),
        intent: IntentId {
            strategy: StrategyId(1),
            seq: 0,
        },
        instrument: inst,
        side,
        qty,
        px: px(cents),
        ts: 0,
        slippage: px(slippage_cents).raw(),
        leg: None,
    }
}

fn trade(inst: u32, ts: Nanos, cents: i64) -> Event {
    Event::Trade(Trade {
        hdr: Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument: inst,
            provider: ProviderId::Synthetic,
        },
        px: px(cents),
        size: 1,
        flags: TradeFlags::NONE,
    })
}

fn labels(l: &[&str]) -> Vec<String> {
    l.iter().map(|s| (*s).to_owned()).collect()
}

/// The worked example: one winner, one loser, one open position, with borrow on the loser.
fn worked() -> Report {
    let mut b = ReportBuilder::new(labels(&["healthy", "dangerous", "healthy"])).unwrap();
    let mut t = 0;
    let mut ev = |b: &mut ReportBuilder, inst, cents| {
        t += 1;
        b.on_event(&trade(inst, t, cents));
    };
    b.on_fill(&fill(0, Side::Buy, 100, 1000, 5)); // buys 100 at 10.00, 5c worse than its reference
    ev(&mut b, 0, 1050); // +$50 open
    b.on_fill(&fill(0, Side::Sell, 100, 1040, 10)); // sells at 10.40: +$40, 10c worse
    b.on_fill(&fill(1, Side::SellShort, 50, 2000, -2)); // shorts 50 at 20.00, 2c better
    ev(&mut b, 1, 2200); // short is $100 under water
    b.on_fill(&fill(1, Side::Buy, 50, 2100, 0)); // covers at 21.00: -$50
    b.on_fill(&fill(2, Side::Buy, 10, 500, 0)); // still open at the end
    ev(&mut b, 2, 600); // +$10
    b.finish(&[0, 3 * D as u128, 0])
}

#[test]
fn a_worked_example_gives_exactly_the_hand_figures() {
    let r = worked();
    let t = &r.total;
    assert_eq!(
        (t.fills, t.shares, t.trades, t.wins),
        (5, 100 + 100 + 50 + 50 + 10, 2, 1)
    );
    assert_eq!(
        (t.gross_profit, t.gross_loss),
        (40 * D as u128, 50 * D as u128)
    );
    assert_eq!(t.realized, -10 * D, "+40 and -50");
    assert_eq!(t.open_pnl, 10 * D);
    assert_eq!(t.borrow_fee, 3 * D as u128);
    assert_eq!(t.net_pnl(), -3 * D, "-10 realised + 10 open - 3 borrow");
    assert_eq!(t.hit_rate_permille(), Some(500));
    // Slippage: 100 x 5c + 100 x 10c + 50 x -2c = $5 + $10 - $1.
    assert_eq!(t.slippage_cost, 14 * D);
    assert_eq!(t.worst_slippage, px(10).raw());
    assert_eq!(t.avg_slippage(), Some(14 * D as i64 / 310));
    // Equity: 0, +50 (peak), +40, -60 (the short under water), -10, 0. Fall from 50 to -60.
    assert_eq!(r.max_drawdown, 110 * D as u128);
    assert_eq!(r.events, 3);
}

#[test]
fn the_breakdown_groups_by_label_and_adds_up_to_the_total() {
    let r = worked();
    assert_eq!(
        r.by_label.keys().map(String::as_str).collect::<Vec<_>>(),
        ["dangerous", "healthy"]
    );
    let (h, d) = (&r.by_label["healthy"], &r.by_label["dangerous"]);
    assert_eq!(
        (h.trades, h.wins, h.realized, h.open_pnl, h.net_pnl()),
        (1, 1, 40 * D, 10 * D, 50 * D)
    );
    assert_eq!(
        (d.trades, d.wins, d.realized, d.borrow_fee, d.net_pnl()),
        (1, 0, -50 * D, 3 * D as u128, -53 * D)
    );
    assert_eq!(h.net_pnl() + d.net_pnl(), r.total.net_pnl());
    assert_eq!(h.fills + d.fills, r.total.fills);
    assert_eq!(h.slippage_cost + d.slippage_cost, r.total.slippage_cost);
}

#[test]
fn a_position_flipping_through_zero_ends_one_trade_and_starts_another() {
    let mut b = ReportBuilder::new(labels(&["x"])).unwrap();
    b.on_fill(&fill(0, Side::Buy, 100, 1000, 0));
    b.on_fill(&fill(0, Side::Sell, 150, 1100, 0)); // closes 100 at +$1 each, shorts 50 at 11.00
    b.on_event(&trade(0, 1, 1200)); // -$50 open
    b.on_fill(&fill(0, Side::Buy, 50, 1200, 0)); // covers: -$50
    let r = b.finish(&[0]);
    assert_eq!((r.total.trades, r.total.wins), (2, 1));
    assert_eq!(
        (r.total.gross_profit, r.total.gross_loss),
        (100 * D as u128, 50 * D as u128)
    );
    assert_eq!((r.total.realized, r.total.open_pnl), (50 * D, 0));
}

#[test]
fn no_trades_means_no_hit_rate_and_no_average_slippage() {
    let b = ReportBuilder::new(labels(&["x"])).unwrap();
    let r = b.finish(&[0]);
    assert_eq!(
        (r.total.hit_rate_permille(), r.total.avg_slippage()),
        (None, None)
    );
    assert_eq!(r.max_drawdown, 0);
    let names: Vec<String> = r.metrics().into_iter().map(|(n, _)| n).collect();
    assert!(
        !names
            .iter()
            .any(|n| n.contains("hit_rate") || n.contains("slippage_avg")),
        "{names:?}"
    );
}

#[test]
fn labels_must_be_usable_in_metric_names() {
    assert_eq!(
        ReportBuilder::new(labels(&["ok", "bad label"])).err(),
        Some(ReportError::BadLabel("bad label".into()))
    );
    assert_eq!(
        ReportBuilder::new(labels(&[""])).err(),
        Some(ReportError::BadLabel(String::new()))
    );
    assert!(ReportBuilder::new(labels(&["halt-up", "runner_healthy.v2"])).is_ok());
}

#[test]
fn metrics_are_valid_unique_and_storable_as_a_run_result() {
    let r = worked();
    let m = r.metrics();
    let get = |n: &str| m.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
    assert_eq!(get("trades"), Some(2));
    assert_eq!(get("hit_rate_permille"), Some(500));
    assert_eq!(get("pnl_net"), Some(-3 * D as i64));
    assert_eq!(get("group.dangerous.pnl_net"), Some(-53 * D as i64));
    assert_eq!(get("max_drawdown"), Some(110 * D as i64));
    let data = DataRange {
        source: "synth:test".into(),
        from: 0,
        to: 1,
    };
    let mut res = RunResult::new(Manifest::new("abc", "backtest", 1, data).unwrap(), 3, 0);
    for (n, v) in m {
        res = res
            .with_metric(&n, v)
            .unwrap_or_else(|e| panic!("{n}: {e:?}"));
    }
    assert_eq!(res.metric("group.healthy.trades"), Some(1));
}

#[test]
fn the_rendering_is_in_dollars_and_says_what_is_not_modelled() {
    let text = worked().render();
    assert!(text.contains("total"), "{text}");
    assert!(text.contains("-$3.00"), "net of the total: {text}");
    assert!(
        text.contains("-$53.00") && text.contains("$50.00"),
        "{text}"
    );
    assert!(text.contains("max drawdown $110.00"), "{text}");
    assert!(text.contains("hit  50.0%"), "{text}");
    assert!(text.contains("Not modelled"), "{text}");
}

// ---- the independent check: cash accounting ----

/// Equity by a different route: cash from every fill plus positions at the marks.
struct Ledger {
    cash: i128,
    qty: Vec<i64>,
    mark: Vec<i64>,
    peak: i128,
    dd: i128,
}

impl Ledger {
    fn new(n: usize) -> Ledger {
        Ledger {
            cash: 0,
            qty: vec![0; n],
            mark: vec![0; n],
            peak: 0,
            dd: 0,
        }
    }
    fn fill(&mut self, f: &Fill) {
        let i = f.instrument as usize;
        let signed = if f.side.is_buy() {
            i64::from(f.qty)
        } else {
            -i64::from(f.qty)
        };
        self.cash -= i128::from(signed) * i128::from(f.px.raw());
        self.qty[i] += signed;
        if self.mark[i] == 0 {
            self.mark[i] = f.px.raw();
        }
        self.sample();
    }
    fn trade(&mut self, inst: u32, p: i64) {
        self.mark[inst as usize] = p;
        self.sample();
    }
    fn equity(&self) -> i128 {
        self.cash
            + self
                .qty
                .iter()
                .zip(&self.mark)
                .map(|(&q, &m)| i128::from(q) * i128::from(m))
                .sum::<i128>()
    }
    fn sample(&mut self) {
        let e = self.equity();
        self.peak = self.peak.max(e);
        self.dd = self.dd.max(self.peak - e);
    }
}

#[test]
fn realised_plus_open_equals_cash_plus_positions_and_drawdown_matches_on_random_runs() {
    for seed in 0..40u64 {
        let mut rng = SplitMix64::new(seed);
        let n = 3;
        let mut b = ReportBuilder::new(labels(&["a", "b", "a"])).unwrap();
        let mut l = Ledger::new(n);
        for t in 0..300u64 {
            let inst = (rng.next_u64() % n as u64) as u32;
            let cents = 100 + (rng.next_u64() % 400) as i64;
            if rng.next_u64() % 3 == 0 {
                let held = l.qty[inst as usize];
                let f = if held != 0 && rng.next_u64() % 2 == 0 {
                    // Flatten it exactly, so round trips complete.
                    let side = if held > 0 { Side::Sell } else { Side::Buy };
                    fill(inst, side, held.unsigned_abs() as u32, cents, 0)
                } else {
                    let side =
                        [Side::Buy, Side::Sell, Side::SellShort][(rng.next_u64() % 3) as usize];
                    fill(inst, side, 1 + (rng.next_u64() % 50) as u32, cents, 0)
                };
                b.on_fill(&f);
                l.fill(&f);
            } else {
                b.on_event(&trade(inst, t, cents));
                l.trade(inst, px(cents).raw());
            }
        }
        let want_dd = l.dd as u128;
        let r = b.finish(&[0, 0, 0]);
        // Average cost is held to a raw unit, so the two routes may differ by under
        // a raw unit per share traded.
        let slack = i128::from(r.total.shares);
        let diff = r.total.realized + r.total.open_pnl - l.equity();
        assert!(diff.abs() <= slack, "seed {seed}: {diff} vs {slack}");
        let dd = r.max_drawdown.abs_diff(want_dd);
        assert!(
            dd <= 2 * r.total.shares as u128,
            "seed {seed}: drawdown off by {dd}"
        );
        let by: i128 = r.by_label.values().map(|s| s.net_pnl()).sum();
        assert_eq!(by, r.total.net_pnl(), "seed {seed}");
        assert!(r.total.trades > 0, "seed {seed} exercised round trips");
    }
}

#[test]
fn money_is_rounded_to_cents_without_a_negative_zero() {
    let t = |raw: i128| {
        let s = crate::report::Stats {
            realized: raw,
            ..Default::default()
        };
        let r = Report {
            total: s,
            by_label: Default::default(),
            max_drawdown: 0,
            events: 0,
            first_ts: 0,
            last_ts: 0,
        };
        r.render()
    };
    assert!(t(-4_999_999).contains(" $0.00"), "{}", t(-4_999_999));
    assert!(t(5_000_000).contains("$0.01"));
    assert!(t(-5_000_000).contains("-$0.01"));
    assert!(t(123 * D).contains("$123.00"));
}

// ---- end to end ----

/// Buy 100 on the first quote of each instrument; if that filled, sell it 20 s later.
struct RoundTrip {
    sent: Vec<bool>,
    held: Vec<bool>,
    entries: Vec<(IntentId, u32)>,
}

impl Strategy for RoundTrip {
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }
    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
        // Wait for a quote: an order sent before one exists has nothing to trade against.
        let Event::Quote(q) = ev else { return };
        let i = q.hdr.instrument;
        if self.sent[i as usize] {
            return;
        }
        self.sent[i as usize] = true;
        let req = Request {
            side: Side::Buy,
            qty: 100,
            purpose: Purpose::Open,
            pricing: Pricing::Collar {
                reference: q.ask_px,
                collar_permille: 50,
            },
            protect: Some(Protective {
                stop_trigger: Px::from_raw(q.ask_px.raw() / 2),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Ioc,
            reason: 0,
        };
        if let Ok(id) = ctx.submit(i, req) {
            self.entries.push((id, i));
            ctx.set_timer_in(TimerId(i), 20 * SEC);
        }
    }
    fn on_timer(&mut self, ctx: &mut Ctx<'_>, timer: TimerId) {
        let i = timer.0;
        if !self.held[i as usize] {
            return;
        }
        let last = ctx.state(i).and_then(|s| s.last_px).unwrap();
        let req = Request {
            side: Side::Sell,
            qty: 100,
            purpose: Purpose::Close,
            pricing: Pricing::Collar {
                reference: last,
                collar_permille: 50,
            },
            protect: None,
            tif: Tif::Ioc,
            reason: 1,
        };
        ctx.submit(i, req).unwrap();
    }
    fn on_order_update(&mut self, _: &mut Ctx<'_>, u: &crate::OrderUpdate) {
        if u.state != crate::OrderState::Filled {
            return;
        }
        if let Some(&(_, i)) = self.entries.iter().find(|(id, _)| *id == u.intent) {
            self.held[i as usize] = true;
        }
    }
}

fn session() -> (Vec<Event>, Vec<String>) {
    let spec = |symbol: &str, base, scenario| SymbolSpec {
        symbol: symbol.into(),
        base_px_cents: base,
        base_interval_ns: 300_000_000,
        quote_every: 2,
        scenario,
        news: Vec::new(),
    };
    let cfg = SynthConfig {
        seed: 9,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: 120 * SEC,
        symbols: vec![
            spec(
                "RUNA",
                500,
                Scenario::runner(PullbackKind::Healthy, 5 * SEC),
            ),
            spec(
                "RUNB",
                700,
                Scenario::runner(PullbackKind::Dangerous, 5 * SEC),
            ),
            spec("CALM", 1000, Scenario::quiet()),
        ],
    };
    (
        SynthStream::new(&cfg).collect(),
        labels(&["healthy", "dangerous", "quiet"]),
    )
}

fn backtest() -> (Report, i64) {
    let (events, lab) = session();
    let mut host = Host::new(
        RoundTrip {
            sent: vec![false; 3],
            held: vec![false; 3],
            entries: Vec::new(),
        },
        3,
    );
    let mut broker = SimBroker::new(
        SimConfig {
            latency_ns: 50_000_000,
            borrow_bps_per_year: 0,
        },
        3,
    );
    let mut b = ReportBuilder::new(lab).unwrap();
    let mut l = Ledger::new(3);
    run_backtest_observed(&mut host, &mut broker, events, |ev, fills| {
        for f in fills {
            b.on_fill(f);
            l.fill(f);
        }
        b.on_event(ev);
        if let Event::Trade(t) = ev {
            l.trade(t.hdr.instrument, t.px.raw());
        }
    });
    let fee: Vec<u128> = (0..3).map(|i| broker.borrow_fee(i)).collect();
    let r = b.finish(&fee);
    assert_eq!(r.total.fills as usize, broker.fills().len());
    let diff = r.total.realized + r.total.open_pnl - l.equity();
    assert!(
        diff.abs() <= i128::from(r.total.shares),
        "report agrees with cash accounting: {diff}"
    );
    assert!(r.max_drawdown.abs_diff(l.dd as u128) <= 2 * r.total.shares as u128);
    (r, broker.fills().len() as i64)
}

#[test]
fn a_whole_backtest_reports_consistently_and_reproducibly() {
    let (a, fills) = backtest();
    let (b, _) = backtest();
    assert_eq!(a, b, "same stream, same report");
    assert!(fills >= 4, "entries and exits happened: {fills}");
    assert!(a.total.trades >= 2);
    assert_eq!(
        a.by_label.keys().map(String::as_str).collect::<Vec<_>>(),
        ["dangerous", "healthy", "quiet"]
    );
    assert_eq!(
        a.by_label.values().map(|s| s.net_pnl()).sum::<i128>(),
        a.total.net_pnl()
    );
    assert_eq!(a.events as usize, session().0.len());
}

#[test]
fn a_break_even_trade_is_not_a_win() {
    let mut b = ReportBuilder::new(labels(&["x"])).unwrap();
    b.on_fill(&fill(0, Side::Buy, 10, 1000, 0));
    b.on_fill(&fill(0, Side::Sell, 10, 1000, 0));
    let r = b.finish(&[0]);
    assert_eq!(
        (r.total.trades, r.total.wins, r.total.hit_rate_permille()),
        (1, 0, Some(0))
    );
    assert_eq!((r.total.gross_profit, r.total.gross_loss), (0, 0));
}
