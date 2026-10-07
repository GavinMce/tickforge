//! The shared multi-timeframe bars in the multi-strategy host (E19-S03).

use tf_core::{Event, Nanos, Px};
use tf_engine::{BarsStats, MtfConfig, Timeframe};
use tf_ledger::MemStore;
use tf_strategy::intent::{Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::{CrossStrategy, Ctx, MemberView, Request};

use crate::tests::*;
use crate::{
    BarsConfig, Host, HostConfig, Route, SlotState, StrategyDef, Verdict, certify, compare,
    replay_events, runner,
};

/// Claims the bars of its first two members at its first review, and buys the first of them whose
/// first one-minute bar has closed: a decision that exists only if the bars do.
struct BarTrader {
    id: u16,
    reviews: u32,
    panic_at: Option<u32>,
    claimed: Vec<u32>,
    bought: bool,
}

impl CrossStrategy for BarTrader {
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.reviews += 1;
        if self.panic_at == Some(self.reviews) {
            panic!("the strategy broke at review {}", self.reviews);
        }
        if self.claimed.is_empty() {
            for id in view.ids().take(2) {
                if ctx.track_bars(id).is_ok() {
                    self.claimed.push(id);
                }
            }
        }
        if self.bought {
            return;
        }
        for &id in &self.claimed {
            let closed = ctx.bars(id).map_or(0, |b| b.closed_len(Timeframe::M1));
            let Some(last) = view.state(id).and_then(|s| s.last_px) else {
                continue;
            };
            if closed >= 1 {
                let req = Request {
                    side: Side::Buy,
                    qty: 100,
                    purpose: Purpose::Open,
                    pricing: Pricing::Limit(Px::from_raw(last.raw() + 50_000_000)),
                    protect: Some(Protective {
                        stop_trigger: Px::from_raw(last.raw() / 2),
                        stop_limit: None,
                        take_profit: None,
                    }),
                    tif: Tif::Day,
                    reason: 7,
                };
                let _ = ctx.submit(id, req);
                self.bought = true;
                return;
            }
        }
    }
}

fn bar_def(id: u16, universe: &str, panic_at: Option<u32>) -> StrategyDef {
    StrategyDef {
        id,
        name: format!("bars{id}"),
        params: format!("panic_at={panic_at:?}"),
        universe: tf_universe::Spec::parse(universe).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(BarTrader {
                id,
                reviews: 0,
                panic_at,
                claimed: vec![],
                bought: false,
            })
        }),
    }
}

fn cfg_with_bars(strategies: u32, max_tracked: usize) -> HostConfig {
    HostConfig {
        bars: Some(BarsConfig {
            mtf: MtfConfig::default(),
            max_tracked,
        }),
        ..config(strategies)
    }
}

fn day(cfg: &HostConfig, defs: &[StrategyDef], tape: &[Event]) -> Host<MemStore> {
    let mut h = host(cfg).record();
    certify_all(&mut h, cfg, defs, tape);
    for e in tape {
        h.on_event(e).unwrap();
    }
    h.end_of_day(tape.last().unwrap().ts_recv()).unwrap();
    h
}

#[test]
fn strategies_share_one_set_of_bars_and_a_stopped_one_gives_its_claims_up() {
    let cfg = cfg_with_bars(3, 8);
    let tape = market(150, flat);
    // Certified on a stretch too short to reach the panic, as a strategy that is going to break must be.
    let short = market(40, flat);
    let defs = [
        bar_def(1, LOW, Some(70)),
        bar_def(2, LOW, None),
        bar_def(3, HIGH, None),
    ];
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &defs, &short);
    assert_eq!(
        h.bars().unwrap().tracked(),
        0,
        "nothing is tracked until a strategy asks"
    );
    let mut owners = Vec::new();
    for e in &tape {
        h.on_event(e).unwrap();
        let sec = (e.ts_recv() - T0) / SEC;
        if sec == 30 && owners.is_empty() {
            let b = h.bars().unwrap();
            // Strategies 1 and 2 have the same two members: one set of bars, two claims each.
            // Strategy 3 has two others.
            assert_eq!(b.tracked(), 4);
            owners = b
                .stats()
                .iter()
                .map(|(o, s)| (*o, s.started + s.joined))
                .collect();
        }
    }
    assert_eq!(owners, [(1, 2), (2, 2), (3, 2)]);
    // Strategy 1 broke at review 70: its claims are gone, the others' bars carry on.
    assert!(matches!(h.state_of(1), Some(SlotState::Stopped(_))));
    let b = h.bars().unwrap();
    assert_eq!(b.tracked(), 4);
    let shared: Vec<u32> = (0..SYMBOLS).filter(|&i| b.owners_of(i) == [2]).collect();
    assert_eq!(shared.len(), 2);
    assert!(
        b.symbol(2, shared[0]).unwrap().closed_len(Timeframe::M1) >= 2,
        "bars kept building after the other strategy stopped"
    );
    assert!(b.symbol(1, shared[0]).is_none());
    // Killing strategy 2 drops its symbols' bars, and nobody else's.
    h.kill_strategy(2, h.now()).unwrap();
    let b = h.bars().unwrap();
    assert_eq!(b.tracked(), 2);
    assert!((0..SYMBOLS).all(|i| b.owners_of(i).is_empty() || b.owners_of(i) == [3]));
}

#[test]
fn a_request_past_the_bound_is_refused_and_shows_in_the_counts() {
    // Strategy 1 asks first (two symbols), strategy 3 second (two others): four do not fit in three.
    let cfg = cfg_with_bars(3, 3);
    let tape = market(40, flat);
    let defs = [bar_def(1, LOW, None), bar_def(3, HIGH, None)];
    let h = day(&cfg, &defs, &tape);
    let b = h.bars().unwrap();
    assert_eq!(b.tracked(), 3);
    let st: std::collections::BTreeMap<_, _> = b.stats().into_iter().collect();
    assert_eq!(
        st[&3],
        BarsStats {
            requests: 2,
            started: 1,
            joined: 0,
            already: 0,
            refused_full: 1,
            refused_unknown: 0,
            released: 0,
        }
    );
}

#[test]
fn a_day_that_used_bars_replays_equal_only_with_the_same_bars() {
    let cfg = cfg_with_bars(2, 8);
    let tape = market(150, flat);
    let defs = [bar_def(1, LOW, None), bar_def(2, HIGH, None)];
    let live = day(&cfg, &defs, &tape);
    let log = live.log().unwrap();
    assert!(
        log.recs
            .iter()
            .any(|r| matches!(r, crate::Rec::Decision { .. })),
        "the strategies decided something, from bars"
    );
    // Replayed with the same configuration, the decision logs are equal.
    let again = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    assert_eq!(
        compare(log, &again.log, &reference().symbols),
        Verdict::Equal {
            records: log.recs.len()
        }
    );
    // Replayed by a host without the bars, the strategies see none and decide nothing: the difference
    // is reported, so the bars are part of what a run is.
    let bare = HostConfig {
        bars: None,
        ..cfg.clone()
    };
    let again = replay_events(log, &bare, &reference(), &defs, &tape).unwrap();
    assert!(matches!(
        compare(log, &again.log, &reference().symbols),
        Verdict::Differs(_)
    ));
}

#[test]
fn a_host_without_bars_says_so_to_the_strategy() {
    let cfg = config(1);
    assert!(cfg.bars.is_none());
    let tape = market(40, flat);
    let defs = [bar_def(1, LOW, None)];
    let h = day(&cfg, &defs, &tape);
    assert!(h.bars().is_none());
    assert!(
        !h.log()
            .unwrap()
            .recs
            .iter()
            .any(|r| matches!(r, crate::Rec::Decision { .. })),
        "no bars, no decision"
    );
    // The certificate is made through the same machinery, so a host without bars certifies it too.
    certify(&defs[0], &cfg, &reference(), &tape, 7).unwrap();
}

#[test]
fn a_strategy_stopped_by_its_soft_loss_limit_gives_up_its_bars() {
    // The scenario of `crossing_the_loss_limits_stops_then_flattens_one_strategy`: the price falls at
    // second 8 past strategy 1's soft limit. Strategy 1 holds the bars of two symbols; strategy 2 none.
    let cfg = cfg_with_bars(2, 8);
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
    let calm = market(40, flat);
    let defs = [
        def_claiming(1, LOW, Plan::Chase { qty: 100, n: 6 }, Route::Sim, true),
        def(2, HIGH, Plan::Buy { qty: 100, n: 20 }, Route::Sim),
    ];
    let mut h = host(&cfg);
    certify_all(&mut h, &cfg, &defs, &calm);
    let mut before = None;
    let mut at_soft = None;
    for e in &tape {
        h.on_event(e).unwrap();
        match h.state_of(1) {
            Some(SlotState::Running) => before = Some(h.bars().unwrap().tracked()),
            Some(SlotState::Stopped(_)) if at_soft.is_none() => {
                at_soft = Some(h.bars().unwrap().tracked());
                assert!(matches!(
                    h.state_of(1),
                    Some(SlotState::Stopped(crate::StopReason::SoftLoss))
                ));
            }
            _ => {}
        }
    }
    assert_eq!(
        before,
        Some(2),
        "it held the bars of two symbols while it ran"
    );
    assert_eq!(
        at_soft,
        Some(0),
        "the step that stopped it for the soft limit also released them"
    );
}
