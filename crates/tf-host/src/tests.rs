use tf_budget::{Group, LossLimits, Strategy as BudgetStrategy, Tree};
use tf_core::{
    Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Quote, SymbolTable, Trade, TradeFlags,
};
use tf_engine::{PromoterConfig, ScannerConfig};
use tf_ledger::{Journal, MemStore};
use tf_risk::{Budgets, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{OrderState, OrderUpdate};
use tf_strategy::sim::{FaultPlan, SimBroker, SimConfig};
use tf_strategy::{CrossStrategy, Ctx, MemberView, Request};
use tf_universe::{LiveFeature, Snapshot, Spec};

use crate::{
    AdmitError, Certificate, CertifyError, Host, HostConfig, Reference, Route, SlotState,
    StopReason, StrategyDef, certify, runner,
};

pub(crate) const SEC: Nanos = NANOS_PER_SEC;
pub(crate) const MS: Nanos = 1_000_000;
pub(crate) const T0: Nanos = 100 * SEC;
pub(crate) const D: u128 = 1_000_000_000;
pub(crate) const SYMBOLS: u32 = 12;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Plan {
    /// Buy `qty` of the member with the most trades each review, for the first `n` reviews.
    Buy { qty: u32, n: u32 },
    /// As `Buy`, and panic at review `at`.
    BuyThenPanic { qty: u32, n: u32, at: u32 },
    /// Panic in the very first order update.
    PanicOnUpdate { qty: u32 },
    /// Buy the member ranked `reviews % 3` by trades, for the first `n` reviews: positions in three symbols.
    Rotate { qty: u32, n: u32 },
    /// Buy for `n` reviews, then rest two orders far below the market, and after that try to buy from
    /// inside the update that says one was cancelled.
    Chase { qty: u32, n: u32 },
    /// Rest a buy far below the market at each of the first `n` reviews, and never trade.
    Rest { qty: u32, n: u32 },
}

pub(crate) struct Trader {
    id: u16,
    plan: Plan,
    reviews: u32,
    seen: Vec<OrderState>,
    revoked: Vec<u32>,
}

impl Trader {
    fn buy(&self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, qty: u32) {
        self.buy_rank(ctx, view, qty, 0, 50_000_000);
    }

    /// Buy the member ranked `rank` by trades, at the last price plus `over` (raw).
    fn buy_rank(&self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, qty: u32, rank: usize, over: i64) {
        let Some((_, id)) = view
            .top_by(LiveFeature::Trades, rank + 1, true)
            .get(rank)
            .copied()
        else {
            return;
        };
        let Some(last) = view.state(id).and_then(|s| s.last_px) else {
            return;
        };
        let limit = Px::from_raw(last.raw() + over);
        let req = Request {
            side: Side::Buy,
            qty,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(limit),
            protect: Some(Protective {
                stop_trigger: Px::from_raw(last.raw() / 2),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            reason: 1,
        };
        let _ = ctx.submit(id, req);
    }
}

impl CrossStrategy for Trader {
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.reviews += 1;
        match self.plan {
            Plan::Buy { qty, n } if self.reviews <= n => self.buy(ctx, view, qty),
            Plan::BuyThenPanic { qty, n, at } => {
                if self.reviews == at {
                    panic!("the strategy broke at review {at}");
                }
                if self.reviews <= n {
                    self.buy(ctx, view, qty);
                }
            }
            Plan::PanicOnUpdate { qty } if self.reviews <= 2 => self.buy(ctx, view, qty),
            Plan::Rotate { qty, n } if self.reviews <= n => {
                self.buy_rank(ctx, view, qty, (self.reviews % 3) as usize, 50_000_000)
            }
            Plan::Rest { qty, n } if self.reviews <= n => {
                self.buy_rank(ctx, view, qty, 0, -5_000_000_000)
            }
            Plan::Chase { qty, n } if self.reviews <= n => self.buy(ctx, view, qty),
            Plan::Chase { qty, n } if self.reviews <= n + 2 => {
                self.buy_rank(ctx, view, qty, 0, -5_000_000_000)
            }
            _ => {}
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, u: &OrderUpdate) {
        self.seen.push(u.state);
        if let Plan::Chase { qty, n } = self.plan {
            if self.reviews > n && u.state == OrderState::Cancelled {
                if let Some(last) = ctx.state(2).and_then(|s| s.last_px) {
                    let req = Request {
                        side: Side::Buy,
                        qty,
                        purpose: Purpose::Open,
                        pricing: Pricing::Limit(Px::from_raw(last.raw() + 50_000_000)),
                        protect: Some(Protective {
                            stop_trigger: Px::from_raw(last.raw() / 2),
                            stop_limit: None,
                            take_profit: None,
                        }),
                        tif: Tif::Day,
                        reason: 2,
                    };
                    let _ = ctx.submit(2, req);
                }
            }
        }
        if matches!(self.plan, Plan::PanicOnUpdate { .. }) {
            panic!("could not cope with an update");
        }
    }

    fn on_tier1_revoked(&mut self, _ctx: &mut Ctx<'_>, _v: &MemberView<'_>, id: u32) {
        self.revoked.push(id);
    }
}

pub(crate) fn names() -> SymbolTable {
    let mut t = SymbolTable::new();
    for i in 0..SYMBOLS {
        t.intern(&format!("S{i:02}"));
    }
    t
}

pub(crate) fn snapshot() -> Snapshot {
    let mut text = String::from("# as_of 2026-10-02\nsymbol,price,adv_shares\n");
    for i in 0..SYMBOLS {
        text.push_str(&format!("S{i:02},20.00,{}\n", (i + 1) * 100));
    }
    Snapshot::parse(&text).unwrap()
}

pub(crate) fn reference() -> Reference {
    Reference {
        symbols: names(),
        snapshot: snapshot(),
    }
}

pub(crate) fn tree(n: u32) -> Tree {
    let strategies = (1..=n)
        .map(|i| BudgetStrategy {
            id: format!("s{i}"),
            share: 10_000 / n,
        })
        .collect();
    Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits {
            soft: 300,
            hard: 600,
        },
        strategies,
    }])
    .unwrap()
}

pub(crate) fn config(strategies: u32) -> HostConfig {
    let ids = (1..=strategies).map(|i| (i as u16, format!("s{i}")));
    HostConfig {
        id_space: SYMBOLS as usize,
        limits: Limits::new(50_000 * D, 100_000, 5_000_000 * D, 90_000 * D, 10_000, SEC).unwrap(),
        budgets: Some(Budgets::new(tree(strategies), 100_000 * D, ids).unwrap()),
        promoter: PromoterConfig {
            max_tier1: 4,
            min_dwell_secs: 0,
            ..PromoterConfig::default()
        },
        scanner: ScannerConfig {
            min_volume: 1_000_000,
            ..ScannerConfig::default()
        },
        sim: SimConfig {
            latency_ns: 0,
            borrow_bps_per_year: 0,
        },
        min_certified_events: 10,
        start_ts: 0,
    }
}

/// A market: every symbol quotes and trades each second, `1 + sym % 3` trades, so within any set of
/// symbols the one with the lowest id of those with `sym % 3 == 2` has the most trades.
/// `price(sym, sec)` is in cents.
pub(crate) fn market(secs: u64, price: impl Fn(u32, u64) -> i64) -> Vec<Event> {
    market_with(secs, 1, price)
}

/// [`market`] with the quote `half` cents either side of the price.
pub(crate) fn market_with(secs: u64, half: i64, price: impl Fn(u32, u64) -> i64) -> Vec<Event> {
    let mut v = Vec::new();
    for sec in 0..secs {
        for sym in 0..SYMBOLS {
            let ts = T0 + sec * SEC + u64::from(sym) * MS;
            let p = price(sym, sec);
            let hdr = |ts: Nanos| Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument: sym,
                provider: ProviderId::Synthetic,
            };
            v.push(Event::Quote(Quote {
                hdr: hdr(ts),
                bid_px: Px::from_cents(p - half),
                ask_px: Px::from_cents(p + half),
                bid_sz: 100_000,
                ask_sz: 100_000,
            }));
            for k in 0..=(sym % 3) {
                let t = ts + 1 + u64::from(k);
                v.push(Event::Trade(Trade {
                    hdr: hdr(t),
                    px: Px::from_cents(p),
                    size: 100,
                    flags: TradeFlags::NONE,
                }));
            }
        }
    }
    v.sort_by_key(Event::ts_recv);
    v
}

pub(crate) fn flat(_: u32, _: u64) -> i64 {
    2_000
}

pub(crate) fn def(id: u16, universe: &str, plan: Plan, route: Route) -> StrategyDef {
    StrategyDef {
        id,
        name: format!("trader{id}"),
        params: format!("{plan:?}"),
        universe: Spec::parse(universe).unwrap(),
        priority: 1,
        route,
        build: Box::new(move || {
            runner(Trader {
                id,
                plan,
                reviews: 0,
                seen: vec![],
                revoked: vec![],
            })
        }),
    }
}

pub(crate) const LOW: &str = "universe v1\nstatic adv_shares <= 600\n";
pub(crate) const HIGH: &str = "universe v1\nstatic adv_shares >= 700\n";

pub(crate) fn certify_all(
    h: &mut Host<MemStore>,
    cfg: &HostConfig,
    defs: &[StrategyDef],
    tape: &[Event],
) {
    for d in defs {
        let cert = certify(d, cfg, &reference(), tape, 7).unwrap();
        h.add_strategy(d, &cert).unwrap();
    }
}

pub(crate) fn host(cfg: &HostConfig) -> Host<MemStore> {
    Host::new(cfg.clone(), reference(), MemStore::from_records(vec![])).unwrap()
}

pub(crate) fn run(h: &mut Host<MemStore>, events: &[Event]) {
    for e in events {
        h.on_event(e).unwrap();
    }
    h.end_of_day(events.last().unwrap().ts_recv()).unwrap();
}

#[test]
fn a_strategy_is_admitted_only_with_a_certificate_of_exactly_that_strategy() {
    let cfg = config(2);
    let tape = market(5, flat);
    let a = def(1, LOW, Plan::Buy { qty: 100, n: 3 }, Route::Sim);
    let cert = certify(&a, &cfg, &reference(), &tape, 7).unwrap();
    assert!(
        cert.is_intact()
            && cert.events == tape.len() as u64
            && cert.tape_id == 7
            && cert.intents == 3
            && cert.accepted == 3
    );
    let mut h = host(&cfg);
    h.add_strategy(&a, &cert).unwrap();
    assert_eq!(h.state_of(1), Some(&SlotState::Running));
    // The same strategy twice.
    assert_eq!(h.add_strategy(&a, &cert), Err(AdmitError::Duplicate(1)));
    // Another strategy, or the same one with other parameters or another universe, or another number.
    let b = def(2, LOW, Plan::Buy { qty: 100, n: 3 }, Route::Sim);
    assert!(
        matches!(h.add_strategy(&b, &cert), Err(AdmitError::NotCertified(m)) if m.contains("another strategy"))
    );
    let b_cert = certify(&b, &cfg, &reference(), &tape, 7).unwrap();
    let b_other_params = def(2, LOW, Plan::Buy { qty: 200, n: 3 }, Route::Sim);
    assert!(matches!(
        h.add_strategy(&b_other_params, &b_cert),
        Err(AdmitError::NotCertified(_))
    ));
    let b_other_universe = def(2, HIGH, Plan::Buy { qty: 100, n: 3 }, Route::Sim);
    assert!(matches!(
        h.add_strategy(&b_other_universe, &b_cert),
        Err(AdmitError::NotCertified(_))
    ));
    // A certificate with a field changed has lost its seal.
    let mut forged: Certificate = b_cert.clone();
    forged.events += 1;
    assert!(!forged.is_intact());
    assert!(
        matches!(h.add_strategy(&b, &forged), Err(AdmitError::NotCertified(m)) if m.contains("altered"))
    );
    let mut other_tape = b_cert.clone();
    other_tape.strategy_fp ^= 1;
    assert!(matches!(
        h.add_strategy(&b, &other_tape),
        Err(AdmitError::NotCertified(_))
    ));
    h.add_strategy(&b, &b_cert).unwrap();
    // A replay that was too short to mean anything.
    let c = def(3, LOW, Plan::Buy { qty: 100, n: 1 }, Route::Sim);
    let cfg3 = config(3);
    let short = certify(&c, &cfg3, &reference(), &tape[..30], 7).unwrap();
    assert_eq!(short.events, 30);
    let mut strict = Host::new(
        HostConfig {
            min_certified_events: 31,
            ..cfg3.clone()
        },
        reference(),
        MemStore::from_records(vec![]),
    )
    .unwrap();
    assert!(
        matches!(strict.add_strategy(&c, &short), Err(AdmitError::NotCertified(m)) if m.contains("30 events"))
    );
    let mut lax = Host::new(
        HostConfig {
            min_certified_events: 30,
            ..cfg3
        },
        reference(),
        MemStore::from_records(vec![]),
    )
    .unwrap();
    lax.add_strategy(&c, &short).unwrap();
}

#[test]
fn a_strategy_that_cannot_run_cannot_be_certified() {
    let cfg = config(2);
    let tape = market(6, flat);
    // Panics during the replay.
    let bad = def(
        1,
        LOW,
        Plan::BuyThenPanic {
            qty: 100,
            n: 5,
            at: 3,
        },
        Route::Sim,
    );
    match certify(&bad, &cfg, &reference(), &tape, 7) {
        Err(CertifyError::Panicked(m)) => assert!(m.contains("broke at review 3"), "{m}"),
        other => panic!("{other:?}"),
    }
    // Panics in an order update.
    let bad = def(1, LOW, Plan::PanicOnUpdate { qty: 100 }, Route::Sim);
    assert!(matches!(
        certify(&bad, &cfg, &reference(), &tape, 7),
        Err(CertifyError::Panicked(_))
    ));
    // No events.
    let ok = def(1, LOW, Plan::Buy { qty: 100, n: 1 }, Route::Sim);
    assert!(matches!(
        certify(&ok, &cfg, &reference(), &[], 7),
        Err(CertifyError::NoEvents)
    ));
    // No sub-account: id 9 is not in the budget tree.
    let nobudget = def(9, LOW, Plan::Buy { qty: 100, n: 1 }, Route::Sim);
    assert!(matches!(
        certify(&nobudget, &cfg, &reference(), &tape, 7),
        Err(CertifyError::Admit(AdmitError::NoBudget(9)))
    ));
    // A universe that needs data the snapshot does not have.
    let float = def(
        1,
        "universe v1\nstatic float >= 1\n",
        Plan::Buy { qty: 100, n: 1 },
        Route::Sim,
    );
    assert!(matches!(
        certify(&float, &cfg, &reference(), &tape, 7),
        Err(CertifyError::Admit(AdmitError::Universe(_)))
    ));
    // A symbol the host has never heard of.
    let mut r = reference();
    r.snapshot = Snapshot::parse(&format!("{}GHOST,20.00,100\n", snapshot().render())).unwrap();
    assert!(
        matches!(certify(&ok, &cfg, &r, &tape, 7), Err(CertifyError::Admit(AdmitError::UnknownSymbols(v))) if v == ["GHOST"])
    );
    // The paper route needs a paper broker on the host, though a replay simulates it.
    let paper = def(1, LOW, Plan::Buy { qty: 100, n: 1 }, Route::Paper);
    let cert = certify(&paper, &cfg, &reference(), &tape, 7).unwrap();
    assert_eq!(
        host(&cfg).add_strategy(&paper, &cert),
        Err(AdmitError::NoPaperBroker)
    );
}

#[test]
fn two_strategies_trade_their_own_universes_through_their_own_budgets_into_one_ledger() {
    let cfg = config(2);
    let tape = market(12, flat);
    let (a, b) = (
        def(1, LOW, Plan::Buy { qty: 100, n: 5 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 5 }, Route::Sim),
    );
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a, b], &tape);
    assert_eq!(h.members_of(1).unwrap(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(h.members_of(2).unwrap(), [6, 7, 8, 9, 10, 11]);
    run(&mut h, &tape);
    // Strategy 1 bought S02 (most trades among S00..S05), 2 bought S08 (among S06..S11).
    let g = h.journal().gateway();
    assert_eq!(
        (g.strategy_position(1, 2), g.strategy_position(2, 8)),
        (500, 250)
    );
    assert_eq!(
        (g.strategy_position(1, 8), g.strategy_position(2, 2)),
        (0, 0)
    );
    assert_eq!(h.sim().position(2), 500);
    assert_eq!(h.sim().position(8), 250);
    let (s1, s2) = (h.stats_of(1).unwrap(), h.stats_of(2).unwrap());
    assert_eq!((s1.intents, s1.accepted, s1.filled_shares), (5, 5, 500));
    assert_eq!((s2.intents, s2.accepted, s2.filled_shares), (5, 5, 250));
    assert_eq!(h.ledger_refusals(), 0, "{:?}", h.anomalies());
    assert!(h.working_orders().is_empty());
    // What the ledger holds is what a restart finds.
    let records = h.journal().store().records().to_vec();
    let (back, _) = Journal::open_recorded(MemStore::from_records(records)).unwrap();
    assert_eq!(back.snapshot(), h.journal().snapshot());
}

#[test]
fn a_strategys_budget_is_its_own() {
    // Strategy 1 is given a tiny share, strategy 2 a large one: the same buying is refused for one.
    let mut cfg = config(2);
    let ids = [(1u16, "s1".to_owned()), (2, "s2".to_owned())];
    let tree = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            BudgetStrategy {
                id: "s1".into(),
                share: 100,
            },
            BudgetStrategy {
                id: "s2".into(),
                share: 9_900,
            },
        ],
    }])
    .unwrap();
    cfg.budgets = Some(Budgets::new(tree, 100_000 * D, ids).unwrap());
    let tape = market(12, flat);
    let (a, b) = (
        def(1, LOW, Plan::Buy { qty: 100, n: 6 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 100, n: 6 }, Route::Sim),
    );
    let mut h = host(&cfg);
    // Certification used the same budgets, and the small one is simply refused more.
    certify_all(&mut h, &cfg, &[a, b], &tape);
    run(&mut h, &tape);
    let (s1, s2) = (h.stats_of(1).unwrap(), h.stats_of(2).unwrap());
    assert!(s1.rejected_by_gateway > 0 && s1.accepted < 6, "{s1:?}");
    assert_eq!((s2.rejected_by_gateway, s2.accepted), (0, 6), "{s2:?}");
}

#[test]
fn one_strategy_panicking_stops_it_alone_and_what_it_held_is_closed() {
    let cfg = config(2);
    let tape = market(30, flat);
    let mk = |panics: bool| {
        let a = if panics {
            Plan::BuyThenPanic {
                qty: 100,
                n: 6,
                at: 4,
            }
        } else {
            Plan::Buy { qty: 100, n: 6 }
        };
        let mut h = host(&cfg);
        // Certify the well-behaved variant of the first strategy (a certificate for the one that
        // panics cannot be had), then run the panicking one under that name: only for this test.
        let ok = def(1, LOW, Plan::Buy { qty: 100, n: 6 }, Route::Sim);
        let cert = certify(&ok, &cfg, &reference(), &tape, 7).unwrap();
        h.install_for_test(&def(1, LOW, a, Route::Sim));
        let _ = cert;
        let b = def(2, HIGH, Plan::Buy { qty: 50, n: 6 }, Route::Sim);
        h.add_strategy(&b, &certify(&b, &cfg, &reference(), &tape, 7).unwrap())
            .unwrap();
        run(&mut h, &tape);
        h
    };
    let (broken, healthy) = (mk(true), mk(false));
    match broken.state_of(1).unwrap() {
        SlotState::Stopped(StopReason::Panicked(m)) => assert!(m.contains("broke at review 4")),
        s => panic!("{s:?}"),
    }
    assert_eq!(healthy.state_of(1), Some(&SlotState::Running));
    assert_eq!(broken.stats_of(1).unwrap().panics, 1);
    // It had bought for three reviews (300 shares), the host closed them, and nothing is working.
    let g = broken.journal().gateway();
    assert_eq!(
        g.strategy_position(1, 2),
        0,
        "flattened: {:?}",
        broken.anomalies()
    );
    assert!(broken.stats_of(1).unwrap().flatten_orders >= 1);
    assert_eq!(broken.sim().position(2), 0);
    assert!(broken.working_orders().is_empty());
    // The other strategy did exactly what it does without the broken one next to it.
    let (a, b) = (broken.stats_of(2).unwrap(), healthy.stats_of(2).unwrap());
    assert_eq!(a, b);
    assert_eq!(broken.journal().gateway().strategy_position(2, 8), 300);
    assert_eq!(broken.reviews_of(2), healthy.reviews_of(2));
    assert_eq!(broken.ledger_refusals(), 0, "{:?}", broken.anomalies());
}

#[test]
fn a_panic_while_hearing_about_an_order_is_contained_too() {
    let cfg = config(2);
    let tape = market(20, flat);
    let mut h = host(&cfg);
    h.install_for_test(&def(1, LOW, Plan::PanicOnUpdate { qty: 100 }, Route::Sim));
    let b = def(2, HIGH, Plan::Buy { qty: 50, n: 4 }, Route::Sim);
    h.add_strategy(&b, &certify(&b, &cfg, &reference(), &tape, 7).unwrap())
        .unwrap();
    run(&mut h, &tape);
    assert!(
        matches!(h.state_of(1), Some(SlotState::Stopped(StopReason::Panicked(m))) if m.contains("update"))
    );
    assert_eq!(h.journal().gateway().strategy_position(1, 2), 0);
    assert_eq!(h.journal().gateway().strategy_position(2, 8), 200);
    assert_eq!(h.state_of(2), Some(&SlotState::Running));
}

#[test]
fn an_operator_kills_one_strategy_and_the_others_carry_on() {
    let cfg = config(2);
    let tape = market(30, flat);
    let (a, b) = (
        def(1, LOW, Plan::Buy { qty: 100, n: 20 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 20 }, Route::Sim),
    );
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a, b], &tape);
    let cut = tape
        .iter()
        .position(|e| e.ts_recv() >= T0 + 10 * SEC)
        .unwrap();
    for e in &tape[..cut] {
        h.on_event(e).unwrap();
    }
    assert!(h.journal().gateway().strategy_position(1, 2) > 0);
    assert!(h.kill_strategy(1, T0 + 10 * SEC).unwrap());
    assert!(!h.kill_strategy(99, T0 + 10 * SEC).unwrap());
    for e in &tape[cut..] {
        h.on_event(e).unwrap();
    }
    h.end_of_day(T0 + 40 * SEC).unwrap();
    assert_eq!(h.state_of(1), Some(&SlotState::Stopped(StopReason::Killed)));
    assert_eq!(h.journal().gateway().strategy_position(1, 2), 0);
    let killed_intents = h.stats_of(1).unwrap().intents;
    assert!(killed_intents <= 11, "it stops asking: {killed_intents}");
    assert!(h.stats_of(2).unwrap().intents == 20);
    assert_eq!(h.journal().gateway().strategy_position(2, 8), 1_000);
}

#[test]
fn crossing_the_loss_limits_stops_then_flattens_one_strategy() {
    let cfg = config(2);
    // The price falls at second 8, by $3 (past strategy 1's soft limit, $1,500, for the 600 shares it
    // holds), and again to $13.50 at second 14 (past its hard limit, $3,000).
    let drop = |sym: u32, sec: u64| {
        if sec < 8 || sym >= 6 {
            2_000
        } else if sec < 14 {
            1_700
        } else {
            1_350
        }
    };
    let tape = market(40, drop);
    // Certified on a calm tape of the same length: a replay that ends stopped by a loss limit is not one.
    let calm = market(40, flat);
    // Strategy 1 buys 100 a second for 6 seconds in S02 (ends holding 600), rests two orders far below
    // the market and then tries to buy from inside the update that says one was cancelled; strategy 2 trades S08, whose
    // price never moves.
    let (a, b) = (
        def(1, LOW, Plan::Chase { qty: 100, n: 6 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 100, n: 20 }, Route::Sim),
    );
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a, b], &calm);
    let mut states = Vec::new();
    let mut at_soft = None;
    for (i, e) in tape.iter().enumerate() {
        h.on_event(e).unwrap();
        let s = h.state_of(1).cloned().unwrap();
        if states.last() != Some(&s) {
            if s == SlotState::Stopped(StopReason::SoftLoss) {
                at_soft = Some((i, h.reviews_of(1).unwrap(), h.stats_of(1).unwrap().intents));
            }
            states.push(s);
        }
        // A few events after the soft limit, what it had resting is cancelled and its position is
        // still there (a soft limit stops opening, it does not sell), and it asks for nothing more.
        if let Some((k, reviews, intents)) = at_soft {
            if i == k + 40 {
                let mine = h
                    .journal()
                    .open_orders()
                    .iter()
                    .filter(|o| o.intent.id.strategy.0 == 1)
                    .count();
                assert_eq!(mine, 0, "resting orders cancelled");
                assert_eq!(h.journal().gateway().strategy_position(1, 2), 600);
                assert_eq!(
                    h.reviews_of(1),
                    Some(reviews),
                    "no more reviews once stopped"
                );
                assert_eq!(
                    h.stats_of(1).unwrap().intents,
                    intents,
                    "nor anything asked for from inside an update"
                );
            }
        }
    }
    h.end_of_day(T0 + 41 * SEC).unwrap();
    assert_eq!(
        states,
        [
            SlotState::Running,
            SlotState::Stopped(StopReason::SoftLoss),
            SlotState::Stopped(StopReason::HardLoss)
        ]
    );
    assert_eq!(
        h.journal().gateway().strategy_position(1, 2),
        0,
        "flattened after the hard limit: {:?}",
        h.anomalies()
    );
    assert!(h.stats_of(1).unwrap().flatten_orders >= 1);
    assert_eq!(h.state_of(2), Some(&SlotState::Running));
    assert_eq!(
        h.stats_of(2).unwrap().intents,
        20,
        "the other strategy kept trading"
    );
    assert_eq!(h.journal().gateway().strategy_position(2, 8), 2_000);
    assert_eq!(h.ledger_refusals(), 0, "{:?}", h.anomalies());
    // The orders the host made itself are numbered apart from the strategies' own.
    let own: Vec<u64> = h
        .sim()
        .fills()
        .iter()
        .filter(|f| f.intent.strategy.0 == 1)
        .map(|f| f.intent.seq)
        .collect();
    assert!(
        own.iter().any(|s| *s >= 1 << 48) && own.iter().any(|s| *s < 1 << 48),
        "{own:?}"
    );
}

#[test]
fn flattening_is_pushed_on_every_second_until_nothing_is_left() {
    // Three orders a second pass the gateway's rate limit of two: the strategy holds three symbols and
    // killing it needs three closing orders at once, so one is refused and tried again a second later.
    let mut cfg = config(1);
    cfg.limits = Limits::new(50_000 * D, 100_000, 5_000_000 * D, 90_000 * D, 2, SEC).unwrap();
    let tape = market(30, flat);
    let a = def(1, LOW, Plan::Rotate { qty: 100, n: 6 }, Route::Sim);
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a], &tape);
    let cut = tape
        .iter()
        .position(|e| e.ts_recv() >= T0 + 8 * SEC)
        .unwrap();
    for e in &tape[..cut] {
        h.on_event(e).unwrap();
    }
    let g = h.journal().gateway();
    assert!((0..6).filter(|i| g.strategy_position(1, *i) != 0).count() >= 3);
    h.kill_strategy(1, T0 + 8 * SEC).unwrap();
    for e in &tape[cut..] {
        h.on_event(e).unwrap();
    }
    h.end_of_day(T0 + 40 * SEC).unwrap();
    assert!(
        (0..6).all(|i| h.journal().gateway().strategy_position(1, i) == 0),
        "{:?}",
        h.anomalies()
    );
    assert!(
        h.stats_of(1).unwrap().rejected_by_gateway >= 1,
        "the rate limit refused some of the closes"
    );
    assert!(h.stats_of(1).unwrap().flatten_orders > 3);
}

#[test]
fn the_kill_switch_refuses_every_open_and_cancels_what_is_working() {
    let cfg = config(2);
    let tape = market(20, flat);
    let (a, b) = (
        def(1, LOW, Plan::Chase { qty: 100, n: 3 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 20 }, Route::Sim),
    );
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a, b], &tape);
    let cut = tape
        .iter()
        .position(|e| e.ts_recv() >= T0 + 6 * SEC)
        .unwrap();
    for e in &tape[..cut] {
        h.on_event(e).unwrap();
    }
    let before = (
        h.stats_of(1).unwrap().accepted,
        h.stats_of(2).unwrap().accepted,
    );
    let resting = |h: &Host<MemStore>| {
        h.journal()
            .open_orders()
            .iter()
            .filter(|o| o.intent.id.strategy.0 == 1)
            .count()
    };
    assert!(
        resting(&h) >= 2,
        "it has orders resting when the switch is thrown"
    );
    h.kill_switch(T0 + 6 * SEC).unwrap();
    for e in &tape[cut..cut + 40] {
        h.on_event(e).unwrap();
    }
    assert_eq!(resting(&h), 0, "what was working to open is cancelled");
    for e in &tape[cut + 40..] {
        h.on_event(e).unwrap();
    }
    h.end_of_day(T0 + 30 * SEC).unwrap();
    let (s1, s2) = (h.stats_of(1).unwrap(), h.stats_of(2).unwrap());
    assert_eq!(
        (s1.accepted, s2.accepted),
        before,
        "nothing accepted after the switch"
    );
    assert!(s1.rejected_by_gateway >= 1 && s2.rejected_by_gateway > 5);
    assert!(h.working_orders().is_empty());
    assert_eq!(
        h.state_of(1),
        Some(&SlotState::Running),
        "strategies keep running to exit; the gateway stops opens"
    );
}

#[test]
fn a_paper_route_that_fails_is_handled_order_by_order() {
    let cfg = config(2);
    let tape = market(30, flat);
    let faults = FaultPlan {
        refuse_every: 3,
        rate_limit_every: 5,
        rate_limit_retry_ns: SEC,
        unknown_every: 4,
        venue_reject_every: 7,
        ..FaultPlan::default()
    };
    let paper = SimBroker::new(cfg.sim, SYMBOLS as usize).with_faults(faults);
    let mut h = Host::new(cfg.clone(), reference(), MemStore::from_records(vec![]))
        .unwrap()
        .with_paper(Box::new(paper));
    let (a, b) = (
        def(1, LOW, Plan::Buy { qty: 100, n: 24 }, Route::Paper),
        def(2, HIGH, Plan::Buy { qty: 50, n: 24 }, Route::Sim),
    );
    certify_all(&mut h, &cfg, &[a, b], &tape);
    run(&mut h, &tape);
    let s1 = h.stats_of(1).unwrap();
    assert!(
        s1.refused_by_broker > 0 && s1.rate_limited > 0 && s1.unanswered > 0,
        "{s1:?}"
    );
    // The simulated-route strategy saw none of it.
    let s2 = h.stats_of(2).unwrap();
    assert_eq!(
        (s2.refused_by_broker, s2.rate_limited, s2.unanswered),
        (0, 0, 0)
    );
    assert_eq!(h.journal().gateway().strategy_position(2, 8), 1_200);
    // Refused and rate-limited orders are closed in the ledger; the ones with no answer that never
    // arrived stay working; and the ledger took everything it was told.
    assert_eq!(h.ledger_refusals(), 0, "{:?}", h.anomalies());
    let open = h.working_orders();
    assert!(
        !open.is_empty() && open.len() as u64 <= s1.unanswered,
        "{} open, {} unanswered",
        open.len(),
        s1.unanswered
    );
    // And an order still in doubt keeps its symbol in Tier 1 (held), while the settled ones do not.
    assert!(h.promoter().is_pinned_by(1, 2));
}

#[test]
fn holds_follow_positions_and_working_orders_and_protect_the_symbol() {
    let cfg = config(2);
    let tape = market(12, flat);
    // Strategy 1 holds shares of S02; strategy 2 only rests orders in S08.
    let (a, b) = (
        def(1, LOW, Plan::Buy { qty: 100, n: 3 }, Route::Sim),
        def(2, HIGH, Plan::Rest { qty: 100, n: 3 }, Route::Sim),
    );
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a, b], &tape);
    for e in &tape {
        h.on_event(e).unwrap();
    }
    assert!(h.promoter().is_pinned_by(1, 2) && !h.promoter().is_pinned_by(2, 2));
    assert!(!h.promoter().is_pinned_by(1, 8));
    // An order working with no position is a hold too.
    assert_eq!(h.journal().gateway().strategy_position(2, 8), 0);
    assert!(
        h.promoter().is_pinned_by(2, 8),
        "orders resting: {:?}",
        h.working_orders()
    );
    h.end_of_day(T0 + 13 * SEC).unwrap();
    // The orders expired: the hold is gone. The position is still held.
    assert!(!h.promoter().is_pinned_by(2, 8));
    assert!(h.promoter().is_pinned_by(1, 2));
}

#[test]
fn a_dynamic_universe_follows_the_market() {
    let cfg = config(1);
    let tape = market(10, flat);
    let d = def(
        1,
        "universe v1\ndynamic top 2 by trades desc keep 2 every 3\n",
        Plan::Buy { qty: 100, n: 8 },
        Route::Sim,
    );
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, std::slice::from_ref(&d), &tape);
    assert!(
        h.members_of(1).unwrap().is_empty(),
        "nothing is chosen before there is a market"
    );
    run(&mut h, &tape);
    // Most trades: symbols 2, 5, 8, 11 (three a second); ties go to the lower ids.
    assert_eq!(h.members_of(1).unwrap(), [2, 5]);
    assert!(h.stats_of(1).unwrap().accepted > 0);
}

#[test]
fn the_same_events_give_the_same_decisions_and_the_same_ledger() {
    let cfg = config(2);
    let tape = market(25, |s, sec| {
        2_000 - (i64::from(s) * 3) + ((sec * 7 + u64::from(s)) % 11) as i64 * 5
    });
    let go = || {
        let (a, b) = (
            def(1, LOW, Plan::Buy { qty: 100, n: 10 }, Route::Sim),
            def(2, HIGH, Plan::Buy { qty: 50, n: 10 }, Route::Sim),
        );
        let mut h = host(&cfg);
        certify_all(&mut h, &cfg, &[a, b], &tape);
        run(&mut h, &tape);
        h
    };
    let (x, y) = (go(), go());
    assert_eq!(x.outcome_hash(), y.outcome_hash());
    assert_eq!(x.journal().store().records(), y.journal().store().records());
    assert!(x.intents() >= 20);
    // And the certificate's hash is the outcome of replaying that strategy alone: the same machinery.
    let a = def(1, LOW, Plan::Buy { qty: 100, n: 10 }, Route::Sim);
    let c1 = certify(&a, &cfg, &reference(), &tape, 1).unwrap();
    let c2 = certify(&a, &cfg, &reference(), &tape, 1).unwrap();
    assert_eq!(c1, c2);
}

#[test]
fn an_intent_left_by_a_stopped_strategy_never_reaches_the_gateway() {
    let cfg = config(1);
    let tape = market(12, flat);
    let a = def(1, LOW, Plan::Buy { qty: 100, n: 10 }, Route::Sim);
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &[a], &tape);
    let cut = tape
        .iter()
        .position(|e| e.ts_recv() >= T0 + 4 * SEC)
        .unwrap();
    for e in &tape[..cut] {
        h.on_event(e).unwrap();
    }
    let before = h.intents();
    h.kill_strategy(1, T0 + 4 * SEC).unwrap();
    for e in &tape[cut..] {
        h.on_event(e).unwrap();
    }
    assert!(
        h.intents() - before <= 2,
        "only flattening orders: {}",
        h.intents() - before
    );
    let _ = (
        IntentId {
            strategy: StrategyId(1),
            seq: 0,
        },
        Intent::validate as fn(&Intent) -> _,
    );
    let _ = (Side::Buy, Tif::Day);
}

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use tf_strategy::broker::{Broker, BrokerEvent, CancelOutcome, Kind, Submission};

static CALLS: AtomicU32 = AtomicU32::new(0);

/// Counts every callback it gets, and panics at its second review.
struct Counting(u32);

impl CrossStrategy for Counting {
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        CALLS.fetch_add(1, Ordering::SeqCst);
        self.0 += 1;
        if self.0 == 1 {
            // Buy, so there are order updates and a position for the host to deal with afterwards.
            Trader {
                id: 1,
                plan: Plan::Buy { qty: 100, n: 1 },
                reviews: 0,
                seen: vec![],
                revoked: vec![],
            }
            .buy(ctx, view, 100);
        }
        if self.0 == 2 {
            panic!("second review");
        }
    }

    fn on_order_update(&mut self, _ctx: &mut Ctx<'_>, _u: &OrderUpdate) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    fn on_tier1_revoked(&mut self, _ctx: &mut Ctx<'_>, _v: &MemberView<'_>, _id: u32) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn nothing_more_is_called_on_a_strategy_that_panicked() {
    let cfg = config(1);
    let tape = market(30, flat);
    let d = StrategyDef {
        id: 1,
        name: "counting".into(),
        params: String::new(),
        universe: Spec::parse(LOW).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(|| runner(Counting(0))),
    };
    let mut h = host(&cfg);
    h.install_for_test(&d);
    let mut after = None;
    for e in &tape {
        h.on_event(e).unwrap();
        if after.is_none()
            && matches!(
                h.state_of(1),
                Some(SlotState::Stopped(StopReason::Panicked(_)))
            )
        {
            after = Some(CALLS.load(Ordering::SeqCst));
        }
    }
    h.end_of_day(T0 + 31 * SEC).unwrap();
    assert!(after.is_some());
    // It had a fill or two to hear about before it broke; the host's closing orders and their fills
    // are told to nobody.
    assert_eq!(CALLS.load(Ordering::SeqCst), after.unwrap());
    assert_eq!(
        h.journal().gateway().strategy_position(1, 2),
        0,
        "and what it held was closed"
    );
}

static REVOKED: Mutex<Vec<(u16, u32)>> = Mutex::new(Vec::new());

/// Asks for Tier 1 for its top member at its first review, and notes what it loses.
struct Wants(u16, bool);

impl CrossStrategy for Wants {
    fn id(&self) -> StrategyId {
        StrategyId(self.0)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        if let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first() {
            ctx.request_tier1(*id);
        }
    }

    fn on_tier1_revoked(&mut self, _ctx: &mut Ctx<'_>, _v: &MemberView<'_>, id: u32) {
        if self.1 {
            REVOKED.lock().unwrap().push((self.0, id));
        }
    }
}

#[test]
fn a_symbol_taken_from_a_strategy_is_told_to_it_and_to_no_one_else() {
    let mut cfg = config(2);
    cfg.promoter.max_tier1 = 1;
    let tape = market(10, flat);
    let mk = |id: u16, universe: &str, priority: u8| StrategyDef {
        id,
        name: format!("wants{id}"),
        params: String::new(),
        universe: Spec::parse(universe).unwrap(),
        priority,
        route: Route::Sim,
        build: Box::new(move || runner(Wants(id, true))),
    };
    // Strategy 1 (priority 1) takes S02 first; strategy 2 (priority 5) then wants S08 and there is room for one.
    let mut h = host(&cfg);
    h.install_for_test(&mk(1, LOW, 1));
    h.install_for_test(&mk(2, HIGH, 5));
    for e in &tape {
        h.on_event(e).unwrap();
    }
    assert_eq!(h.promoter().promoted(), [8]);
    assert_eq!(*REVOKED.lock().unwrap(), [(1, 2)]);
    let m = h.promoter().metrics_text();
    assert!(
        m.contains("tier1_lost{strategy=\"1\"} 1")
            && m.contains("tier1_evicted_for{strategy=\"2\"} 1"),
        "{m}"
    );
    // What the host makes is on the tape in order: the promotion, then the eviction and the other promotion.
    let tape_changes = h.drain_tier_events();
    assert!(tape_changes.len() >= 3, "{tape_changes:?}");
}

/// A broker that says things that cannot be true.
struct Rogue(Vec<BrokerEvent>);

impl Broker for Rogue {
    fn place(&mut self, _i: &Intent, order: tf_strategy::lifecycle::OrderId) -> Submission {
        // Accepts, then tells the ledger of an order nobody placed and acknowledges its own twice.
        self.0.push(BrokerEvent {
            order: tf_strategy::lifecycle::OrderId(9_999),
            ts: 1,
            kind: Kind::Ack,
        });
        self.0.push(BrokerEvent {
            order,
            ts: 2,
            kind: Kind::Ack,
        });
        self.0.push(BrokerEvent {
            order,
            ts: 3,
            kind: Kind::Ack,
        });
        Submission::Accepted
    }

    fn cancel_order(&mut self, _o: tf_strategy::lifecycle::OrderId, _ts: Nanos) -> CancelOutcome {
        CancelOutcome::Finished
    }

    fn observe(&mut self, _e: &Event) {}

    fn close_day(&mut self, _ts: Nanos) {}

    fn take_events(&mut self) -> Vec<BrokerEvent> {
        std::mem::take(&mut self.0)
    }
}

#[test]
fn a_broker_that_says_impossible_things_is_noted_and_does_not_stop_the_host() {
    let cfg = config(1);
    let tape = market(8, flat);
    let mut h = Host::new(cfg.clone(), reference(), MemStore::from_records(vec![]))
        .unwrap()
        .with_paper(Box::new(Rogue(vec![])));
    h.install_for_test(&def(1, LOW, Plan::Buy { qty: 100, n: 3 }, Route::Paper));
    for e in &tape {
        h.on_event(e).unwrap();
    }
    // Per order: an acknowledgement for an order that does not exist, and a second acknowledgement.
    assert!(h.ledger_refusals() >= 6, "{}", h.ledger_refusals());
    assert!(h.anomalies().iter().any(|a| a.contains("order 9999")));
    assert_eq!(h.state_of(1), Some(&SlotState::Running));
}

#[test]
fn the_outcome_hash_sees_the_prices_of_fills() {
    let cfg = config(1);
    let go = |half: i64| {
        let tape = market_with(10, half, flat);
        let mut h = host(&cfg);
        h.install_for_test(&def(1, LOW, Plan::Buy { qty: 100, n: 4 }, Route::Sim));
        for e in &tape {
            h.on_event(e).unwrap();
        }
        (
            h.intents(),
            h.accepted(),
            h.stats_of(1).unwrap().filled_shares,
            h.outcome_hash(),
        )
    };
    let (a, b) = (go(1), go(2));
    assert_eq!(
        (a.0, a.1, a.2),
        (b.0, b.1, b.2),
        "the same decisions and the same shares"
    );
    assert_ne!(a.3, b.3, "but not the same prices");
    assert_eq!(go(1), a);
}

#[test]
fn twenty_strategies_run_in_one_engine_each_in_its_own_account() {
    let cfg = config(20);
    let tape = market(20, flat);
    let mut h = host(&cfg);
    let defs: Vec<StrategyDef> = (1..=20u16)
        .map(|i| {
            let (universe, plan) = if i % 2 == 0 {
                (
                    HIGH,
                    Plan::Buy {
                        qty: 2 * u32::from(i),
                        n: 5,
                    },
                )
            } else {
                (
                    LOW,
                    Plan::Rotate {
                        qty: 2 * u32::from(i),
                        n: 6,
                    },
                )
            };
            def(i, universe, plan, Route::Sim)
        })
        .collect();
    certify_all(&mut h, &cfg, &defs, &tape);
    run(&mut h, &tape);
    assert_eq!(h.strategies().len(), 20);
    for (id, _, state, stats) in h.strategies() {
        assert_eq!(state, &SlotState::Running, "strategy {id}");
        // (5% of the balance each: some are held back by their own budget, and that is counted.)
        assert_eq!(stats.accepted + stats.rejected_by_gateway, stats.intents);
        assert!(
            stats.accepted >= 3 && stats.filled_shares > 0,
            "strategy {id}: {stats:?}"
        );
        assert_eq!(h.reviews_of(id), Some(19));
    }
    // Each strategy's position is its own: the sum over strategies is what the broker holds.
    for sym in 0..SYMBOLS {
        let total: i64 = (1..=20u16)
            .map(|s| h.journal().gateway().strategy_position(s, sym))
            .sum();
        assert_eq!(total, h.sim().position(sym), "S{sym:02}");
    }
    assert_eq!(h.ledger_refusals(), 0, "{:?}", h.anomalies());
    assert!(h.working_orders().is_empty());
}

/// A strategy that asks for Tier 1 for its top member at each review (for the replay tests).
pub(crate) fn wants(id: u16) -> impl CrossStrategy + 'static {
    Wants(id, false)
}

#[test]
fn start_day_switches_on_the_session_state_the_strategies_read() {
    let cfg = config(2);
    let tape = market(5, flat);
    let first = tape
        .iter()
        .find_map(|e| match e {
            Event::Trade(t) => Some(t.hdr.instrument),
            _ => None,
        })
        .expect("a trade");
    // Without a day nothing is kept by session; with one, the whole tape is "regular session".
    let mut h = host(&cfg);
    run(&mut h, &tape);
    assert_eq!(h.tier0().session(first).unwrap().regular.volume, 0);
    let mut h = host(&cfg);
    h.start_day(tf_calendar::SessionTimes {
        premarket: 0,
        open: 1,
        close: u64::MAX - 1,
        after_hours_end: u64::MAX,
    });
    run(&mut h, &tape);
    let s = h.tier0().session(first).unwrap();
    assert!(s.regular.volume > 0 && s.open.is_some());
    assert_eq!(s.regular.volume, h.tier0().symbol(first).unwrap().volume);
}
