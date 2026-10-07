//! Exits held by the strategy, through the multi-strategy host (E19-S05): a day in the premarket, with an entry that
//! carries no protective orders, a stop, a time exit from the calendar, and a decision log that replays equal.

use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_ledger::MemStore;
use tf_strategy::exits::{REASON_STOP, REASON_TIME};
use tf_strategy::intent::{Pricing, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{OrderState, OrderUpdate};
use tf_strategy::testing::T0 as REGULAR;
use tf_strategy::{CrossStrategy, Ctx, ExitBook, ExitPlan, IntentId, MemberView, Request, TimerId};
use tf_universe::LiveFeature;

use crate::tests::*;
use crate::{Host, HostConfig, Rec, Route, StrategyDef, Verdict, compare, replay_events, runner};

/// 08:00 New York time on 2 October 2026.
const PRE: Nanos = REGULAR - 3 * 3600 * SEC;

/// Buys 100 of the member with the most trades at its first review, with no protective orders (it is the premarket),
/// and then holds its own exits: a stop 10 cents under its fill, a target 50 cents over it, and a time exit.
struct Exiter {
    id: u16,
    stop_cents: i64,
    /// A time exit this many minutes before the regular close of the day (from the calendar).
    flat_minutes_before_close: Option<u32>,
    book: ExitBook,
    entered: Option<(u32, IntentId)>,
}

impl CrossStrategy for Exiter {
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }
    fn period(&self) -> Nanos {
        SEC
    }
    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        if self.entered.is_some() {
            return;
        }
        let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first().copied() else {
            return;
        };
        let Some(last) = view.state(id).and_then(|s| s.last_px) else {
            return;
        };
        let req = Request {
            side: Side::Buy,
            qty: 100,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(Px::from_raw(last.raw() + 50_000_000)),
            protect: None,
            tif: Tif::Day,
            reason: 1,
        };
        if let Ok(intent) = ctx.submit(id, req) {
            self.entered = Some((id, intent));
        }
    }
    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, ev: &Event) {
        if let Event::Trade(t) = ev {
            self.book.on_trade(ctx, t.hdr.instrument, t.px);
        }
    }
    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, timer: TimerId) {
        self.book.on_timer(ctx, timer);
    }
    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, u: &OrderUpdate) {
        if self.book.on_order_update(ctx, u) {
            return;
        }
        if let (Some((id, intent)), OrderState::Filled, Some(avg)) =
            (self.entered, u.state, u.avg_px)
        {
            if u.intent == intent {
                let plan = ExitPlan {
                    stop: Some(Px::from_raw(avg.raw() - self.stop_cents * 10_000_000)),
                    target: Some(Px::from_raw(avg.raw() + 500_000_000)),
                    flat_by: self
                        .flat_minutes_before_close
                        .and_then(|m| tf_strategy::flat_by(u.ts, m)),
                    ..ExitPlan::new()
                };
                self.book.arm(ctx, id, true, u.filled_qty, plan);
            }
        }
    }
}

fn def(id: u16, universe: &str, stop_cents: i64, flat_minutes: Option<u32>) -> StrategyDef {
    StrategyDef {
        id,
        name: format!("exiter{id}"),
        params: format!("stop {stop_cents} flat {flat_minutes:?}"),
        universe: tf_universe::Spec::parse(universe).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(Exiter {
                id,
                stop_cents,
                flat_minutes_before_close: flat_minutes,
                book: ExitBook::new(100),
                entered: None,
            })
        }),
    }
}

/// Every symbol quotes and trades each second of the premarket (`1 + sym % 3` trades), at `price(sym, sec)` cents.
fn premarket(secs: u64, price: impl Fn(u32, u64) -> i64) -> Vec<Event> {
    let mut v = Vec::new();
    for sec in 0..secs {
        for sym in 0..SYMBOLS {
            let ts = PRE + sec * SEC + u64::from(sym) * MS;
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
                bid_px: Px::from_cents(p - 1),
                ask_px: Px::from_cents(p + 1),
                bid_sz: 100_000,
                ask_sz: 100_000,
            }));
            for k in 0..=(sym % 3) {
                v.push(Event::Trade(Trade {
                    hdr: hdr(ts + 1 + u64::from(k)),
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

fn flat(_: u32, _: u64) -> i64 {
    2_000
}

/// S02 (strategy 1's pick) falls through its stop at second 15; S08 (strategy 2's) never moves.
fn falls(sym: u32, sec: u64) -> i64 {
    if sym == 2 && sec >= 15 { 1_850 } else { 2_000 }
}

fn day(cfg: &HostConfig, defs: &[StrategyDef], tape: &[Event], calm: &[Event]) -> Host<MemStore> {
    let mut h = host(cfg).record();
    // Certified on a calm tape (a replay that ends stopped out is not one).
    certify_all(&mut h, cfg, defs, calm);
    for e in tape {
        h.on_event(e).unwrap();
    }
    h.end_of_day(tape.last().unwrap().ts_recv()).unwrap();
    h
}

fn reasons(h: &Host<MemStore>, strategy: u16) -> Vec<(u16, Side, Purpose, Nanos)> {
    h.log()
        .unwrap()
        .recs
        .iter()
        .filter_map(|r| match r {
            Rec::Decision {
                strategy: s,
                reason,
                side,
                purpose,
                ts,
                ..
            } if *s == strategy => Some((*reason, *side, *purpose, *ts)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_premarket_day_with_held_exits_closes_both_positions_and_replays_equal() {
    let cfg = config(2);
    let tape = premarket(75, falls);
    let calm = premarket(75, flat);
    // Strategy 1 holds a stop; strategy 2 only a time exit, 20 seconds after its fill.
    let defs = [def(1, LOW, 10, None), def(2, HIGH, 10_000, Some(479))];
    let h = day(&cfg, &defs, &tape, &calm);

    // Both entered with no protective orders (the gateway took them: the premarket needs none), and each left by the
    // exit it held: strategy 1 by its stop, strategy 2 by the clock.
    let s1 = reasons(&h, 1);
    let s2 = reasons(&h, 2);
    assert_eq!(
        s1.iter().map(|d| d.0).collect::<Vec<_>>(),
        [1, REASON_STOP],
        "{s1:?}"
    );
    assert_eq!(
        s2.iter().map(|d| d.0).collect::<Vec<_>>(),
        [1, REASON_TIME],
        "{s2:?}"
    );
    assert_eq!((s1[1].1, s1[1].2), (Side::Sell, Purpose::Close));
    // The stop fired on the first print through it (second 15). The time exit is stamped with the instant the
    // calendar gave, exactly: 08:01:00.
    assert!(
        s1[1].3 >= PRE + 15 * SEC && s1[1].3 < PRE + 16 * SEC,
        "{}",
        s1[1].3
    );
    assert_eq!(s2[1].3, PRE + 60 * SEC);
    let g = h.journal().gateway();
    assert_eq!(g.strategy_position(1, 2), 0, "strategy 1 is flat in S02");
    assert_eq!(g.strategy_position(2, 8), 0, "strategy 2 is flat in S08");
    assert_eq!(h.stats_of(1).unwrap().filled_shares, 200, "100 in, 100 out");
    assert_eq!(h.stats_of(2).unwrap().filled_shares, 200);
    assert!(h.anomalies().is_empty(), "{:?}", h.anomalies());

    // Replayed over the same events, the decision logs compare equal: the exits are the strategy's own decisions
    // on the events, so nothing outside them can differ.
    let log = h.log().unwrap();
    let again = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    assert_eq!(
        compare(log, &again.log, &reference().symbols),
        Verdict::Equal {
            records: log.recs.len()
        }
    );
    // A replay with a different stop is a different day: the difference is found.
    let other = [def(1, LOW, 5, None), def(2, HIGH, 10_000, Some(479))];
    assert!(matches!(
        replay_events(log, &cfg, &reference(), &other, &tape),
        Err(crate::ReplayError::StrategyChanged { .. })
    ));
}
