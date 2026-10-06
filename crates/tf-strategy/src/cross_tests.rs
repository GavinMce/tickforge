use tf_core::{Event, Header, ProviderId, Px, SymbolTable, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::{Change, LiveFeature, RefInfo};

use crate::cross::{CrossRunner, CrossStrategy, Market, MemberView, Members};
use crate::intent::{Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};

const D: i64 = 1_000_000_000;
const SEC: u64 = 1_000_000_000;

fn trade(id: u32, ts: u64, px: i64) -> Event {
    Event::Trade(Trade {
        hdr: Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument: id,
            provider: ProviderId::Synthetic,
        },
        px: Px::from_raw(px),
        size: 10,
        flags: TradeFlags::NONE,
    })
}

#[test]
fn members_are_a_set_across_word_boundaries() {
    let mut m = Members::new();
    assert!(m.is_empty() && !m.contains(0) && !m.contains(100_000));
    for id in [63, 64, 65, 0, 1000, 127, 128] {
        assert!(m.insert(id), "{id}");
        assert!(!m.insert(id), "{id} again");
    }
    assert_eq!(m.len(), 7);
    assert_eq!(
        m.iter().collect::<Vec<_>>(),
        [0, 63, 64, 65, 127, 128, 1000]
    );
    for id in [0, 63, 64, 65, 127, 128, 1000] {
        assert!(m.contains(id));
    }
    for id in [1, 62, 66, 126, 129, 999, 1001, 5000] {
        assert!(!m.contains(id), "{id}");
    }
    assert!(m.remove(64) && !m.remove(64) && !m.remove(9999));
    assert_eq!(m.len(), 6);
    assert!(!m.contains(64) && m.contains(63) && m.contains(65));
    assert_eq!(m, Members::from_ids([0, 63, 65, 127, 128, 1000]));
    m.apply(&Change {
        entered: vec![5, 63],
        left: vec![0, 1000, 77],
    });
    assert_eq!(m.iter().collect::<Vec<_>>(), [5, 63, 65, 127, 128]);
    assert_eq!(m.len(), 5);
}

#[test]
fn a_selection_becomes_members_and_unknown_names_are_reported() {
    let mut table = SymbolTable::new();
    let a = table.intern("AAA");
    let c = table.intern("CCC");
    let sel = tf_universe::Selection {
        as_of: "2026-10-02".into(),
        spec_fp: 1,
        snapshot_fp: 2,
        params: vec![],
        symbols: vec!["AAA".into(), "BBB".into(), "CCC".into()],
    };
    let (m, unknown) = Members::from_selection(&sel, &table);
    assert_eq!(m.iter().collect::<Vec<_>>(), [a, c]);
    assert_eq!(unknown, ["BBB"]);
}

struct World {
    tier0: Tier0,
    refs: Vec<RefInfo>,
}

impl World {
    fn new(n: usize) -> Self {
        World {
            tier0: Tier0::new(n),
            refs: vec![
                RefInfo {
                    price: Some(10 * D),
                    adv_shares: Some(1000)
                };
                n
            ],
        }
    }

    fn feed(&mut self, ev: &Event) {
        self.tier0.on_event(ev);
    }

    fn market(&self) -> Market<'_> {
        Market {
            tier0: &self.tier0,
            refs: &self.refs,
        }
    }
}

/// Ranks `n` members by an arbitrary key for the property test.
#[test]
fn top_k_agrees_with_sorting_everything() {
    let mut w = World::new(300);
    let mut x: u64 = 12345;
    let mut next = || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x >> 33
    };
    let mut members = Members::new();
    for id in 0..300u32 {
        if next() % 3 != 0 {
            members.insert(id);
        }
        // Few distinct values so ties are common; some symbols never trade.
        if next() % 5 != 0 {
            w.feed(&trade(id, 1, (next() % 7 + 1) as i64 * D));
        }
    }
    let view = MemberView::new(w.market(), &members);
    let key = |_: u32, s: &tf_engine::SymbolState| s.last_px.map(|p| p.raw() / D);
    for descending in [true, false] {
        let mut all: Vec<(i64, u32)> = members
            .iter()
            .filter_map(|id| key(id, w.tier0.symbol(id)?).map(|v| (v, id)))
            .collect();
        if descending {
            all.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        } else {
            all.sort();
        }
        for k in [0, 1, 2, 5, 17, all.len(), all.len() + 10] {
            assert_eq!(
                view.top_k(k, descending, key),
                all[..k.min(all.len())],
                "k {k} desc {descending}"
            );
        }
    }
}

#[test]
fn the_view_shows_only_members() {
    let mut w = World::new(4);
    w.feed(&trade(0, 1, 10 * D));
    w.feed(&trade(0, 2, 11 * D));
    w.feed(&trade(1, 1, 12 * D));
    let members = Members::from_ids([0, 3]);
    let v = MemberView::new(w.market(), &members);
    assert_eq!(v.len(), 2);
    assert!(v.contains(0) && !v.contains(1));
    assert_eq!(v.ids().collect::<Vec<_>>(), [0, 3]);
    assert_eq!(v.state(0).unwrap().trades, 2);
    assert!(v.state(1).is_none(), "a non-member's state is not visible");
    assert_eq!(v.feature(0, LiveFeature::GapPermille), Some(100));
    assert_eq!(v.feature(1, LiveFeature::GapPermille), None);
    assert_eq!(
        v.feature(3, LiveFeature::GapPermille),
        None,
        "member that has not traded"
    );
    // A member that has not traded has zero trades, and ranks after one that has.
    assert_eq!(v.top_by(LiveFeature::Trades, 5, true), [(2, 0), (0, 3)]);
}

/// Sells one share of its top-1 by trades at each review; sets a timer; records what it was shown.
struct Probe {
    id: u16,
    period: u64,
    reviews: Vec<(u64, usize)>,
    member_events: Vec<(u64, u32)>,
    timers: Vec<u64>,
    updates: u32,
}

impl Probe {
    fn new(id: u16, period: u64) -> Self {
        Probe {
            id,
            period,
            reviews: vec![],
            member_events: vec![],
            timers: vec![],
            updates: 0,
        }
    }
}

impl CrossStrategy for Probe {
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> u64 {
        self.period
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.reviews.push((ctx.now(), view.len()));
        if let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first() {
            let r = Request {
                side: Side::Sell,
                qty: 1,
                purpose: Purpose::Close,
                pricing: Pricing::Limit(Px::from_raw(D)),
                protect: None,
                tif: Tif::Day,
                reason: 7,
            };
            ctx.submit(*id, r).unwrap();
        }
        ctx.set_timer_in(TimerId(1), SEC / 2);
    }

    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>, ev: &Event) {
        self.member_events.push((ctx.now(), ev.instrument()));
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>, _t: TimerId) {
        self.timers.push(ctx.now());
    }

    fn on_order_update(&mut self, _ctx: &mut Ctx<'_>, _u: &OrderUpdate) {
        self.updates += 1;
    }
}

#[test]
fn reviews_fall_on_a_grid_of_event_time_and_a_gap_gives_one() {
    let mut w = World::new(4);
    let mut r = CrossRunner::new(Probe::new(1, SEC), Members::from_ids([0, 1]));
    let at = |w: &mut World, r: &mut CrossRunner<Probe>, ts: u64, id: u32| {
        let ev = trade(id, ts, 10 * D);
        w.feed(&ev);
        r.on_event(w.market(), &ev);
    };
    at(&mut w, &mut r, SEC / 2, 0); // first event: the grid starts, no review
    assert_eq!(r.reviews(), 0);
    at(&mut w, &mut r, SEC - 1, 1);
    assert_eq!(r.reviews(), 0);
    at(&mut w, &mut r, SEC, 0); // exactly on the line
    assert_eq!(r.strategy().reviews, [(SEC, 2)]);
    at(&mut w, &mut r, SEC + 1, 0);
    at(&mut w, &mut r, 2 * SEC - 1, 0);
    assert_eq!(r.reviews(), 1);
    // The timer set at the review (SEC + SEC/2) fired before the event at 2*SEC - 1... and a long
    // silence gives one review, at the next event, not five.
    at(&mut w, &mut r, 7 * SEC + 3, 1);
    assert_eq!(
        r.strategy().reviews.iter().map(|x| x.0).collect::<Vec<_>>(),
        [SEC, 7 * SEC + 3]
    );
    at(&mut w, &mut r, 8 * SEC - 1, 1);
    assert_eq!(r.reviews(), 2, "the next line is 8 seconds");
    at(&mut w, &mut r, 8 * SEC, 1);
    assert_eq!(r.reviews(), 3);
    // Timers fire at their own time, before the event that passes them: the one set at the review
    // at 1 s fires at 1.5 s (by the event at 1.999 s), the one set at 7 s + 3 ns at 7.5 s + 3 ns.
    assert_eq!(r.strategy().timers, [SEC + SEC / 2, 7 * SEC + 3 + SEC / 2]);
    assert_eq!(r.pending_timers(), 1);
}

#[test]
fn reviews_see_the_members_and_their_intents_are_stamped() {
    let mut w = World::new(4);
    let mut r = CrossRunner::new(Probe::new(9, SEC), Members::from_ids([0, 1]));
    for (ts, id) in [(1, 0), (2, 1), (3, 1), (4, 2), (5, 2), (6, 2)] {
        let ev = trade(id, ts, 10 * D);
        w.feed(&ev);
        r.on_event(w.market(), &ev);
    }
    r.advance_to(w.market(), SEC + 5);
    let out = r.drain_intents();
    // Symbol 2 has the most trades but is not a member: the top of the members is 1.
    assert_eq!(out.len(), 1);
    assert_eq!(
        (
            out[0].instrument,
            out[0].id.strategy,
            out[0].id.seq,
            out[0].ts,
            out[0].reason
        ),
        (1, StrategyId(9), 0, SEC + 5, 7)
    );
    assert!(r.drain_intents().is_empty());
    // A review runs at the time it is noticed (here the time we advanced to), not at the grid line.
    assert_eq!(r.strategy().reviews, [(SEC + 5, 2)]);
    // advance_to also fires the timer set by that review once its time comes.
    r.advance_to(w.market(), SEC + 5 + SEC / 2);
    assert_eq!(r.strategy().timers, [SEC + 5 + SEC / 2]);
    // An intent the strategy gets wrong is refused and counted, not emitted (qty 0).
    struct Bad;
    impl CrossStrategy for Bad {
        fn id(&self) -> StrategyId {
            StrategyId(3)
        }
        fn period(&self) -> u64 {
            SEC
        }
        fn on_review(&mut self, ctx: &mut Ctx<'_>, _v: &MemberView<'_>) {
            let r = Request {
                side: Side::Sell,
                qty: 0,
                purpose: Purpose::Close,
                pricing: Pricing::Limit(Px::from_raw(D)),
                protect: None,
                tif: Tif::Day,
                reason: 0,
            };
            assert!(ctx.submit(0, r).is_err());
        }
    }
    let mut b = CrossRunner::new(Bad, Members::from_ids([0]));
    b.on_event(w.market(), &trade(0, 1, D));
    b.on_event(w.market(), &trade(0, SEC, D));
    assert_eq!(
        (b.reviews(), b.invalid_intents(), b.drain_intents().len()),
        (1, 1, 0)
    );
}

#[test]
fn member_events_reach_only_members_and_membership_can_change() {
    let mut w = World::new(4);
    let mut r = CrossRunner::new(Probe::new(1, SEC), Members::from_ids([1, 2]));
    let feed = |w: &mut World, r: &mut CrossRunner<Probe>, ts: u64, id: u32| {
        let ev = trade(id, ts, 10 * D);
        w.feed(&ev);
        r.on_event(w.market(), &ev);
    };
    for (ts, id) in [(1, 0), (2, 1), (3, 2), (4, 3), (5, 1)] {
        feed(&mut w, &mut r, ts, id);
    }
    assert_eq!(r.strategy().member_events, [(2, 1), (3, 2), (5, 1)]);
    assert_eq!(r.member_events(), 3);
    // The strategy never saw symbols 0 or 3, but the shared Tier 0 holds them for others.
    assert_eq!(w.tier0.symbol(0).unwrap().trades, 1);
    r.members_mut().apply(&Change {
        entered: vec![3],
        left: vec![1],
    });
    feed(&mut w, &mut r, 6, 1);
    feed(&mut w, &mut r, 7, 3);
    assert_eq!(r.strategy().member_events.last(), Some(&(7, 3)));
    assert_eq!(r.member_events(), 4);
    // Control events carry no market instrument and are not routed.
    let before = r.member_events();
    r.on_event(
        w.market(),
        &Event::TierChange(tf_core::TierChange {
            hdr: Header {
                ts_event: 8,
                ts_recv: 8,
                seq: 8,
                instrument: 2,
                provider: ProviderId::Internal,
            },
            action: tf_core::TierAction::Promote,
            reason: 0,
            score: 0,
        }),
    );
    assert_eq!(r.member_events(), before);
}

#[test]
fn strategies_share_one_tier0_and_each_sees_its_own_members() {
    let mut w = World::new(6);
    let mut a = CrossRunner::new(Probe::new(1, SEC), Members::from_ids([0, 1, 2]));
    let mut b = CrossRunner::new(Probe::new(2, SEC), Members::from_ids([3, 4]));
    for ts in 1..=40u64 {
        let ev = trade((ts % 6) as u32, ts * SEC / 8, 10 * D);
        w.feed(&ev);
        a.on_event(w.market(), &ev);
        b.on_event(w.market(), &ev);
    }
    assert!(a.reviews() >= 4 && a.reviews() == b.reviews());
    assert!(
        a.strategy().reviews.iter().all(|x| x.1 == 3)
            && b.strategy().reviews.iter().all(|x| x.1 == 2)
    );
    assert!(
        a.strategy().member_events.iter().all(|e| e.1 < 3)
            && b.strategy()
                .member_events
                .iter()
                .all(|e| (3..5).contains(&e.1))
    );
    let (ia, ib) = (a.drain_intents(), b.drain_intents());
    assert!(
        ia.iter()
            .all(|i| i.id.strategy == StrategyId(1) && i.instrument < 3)
    );
    assert!(
        ib.iter()
            .all(|i| i.id.strategy == StrategyId(2) && (3..5).contains(&i.instrument))
    );
    // Each strategy numbers its own intents from zero.
    assert_eq!(
        ia.iter().map(|i| i.id.seq).collect::<Vec<_>>(),
        (0..ia.len() as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        ib.iter().map(|i| i.id.seq).collect::<Vec<_>>(),
        (0..ib.len() as u64).collect::<Vec<_>>()
    );
}

#[test]
fn a_replay_of_the_same_events_gives_the_same_intents_and_reviews() {
    let run = || {
        let mut w = World::new(6);
        let mut r = CrossRunner::new(Probe::new(1, SEC / 3), Members::from_ids([0, 2, 4]));
        for i in 0..500u64 {
            let ev = trade(
                ((i * 7) % 6) as u32,
                i * 37_000_000 + 1,
                (10 + (i % 5) as i64) * D,
            );
            w.feed(&ev);
            r.on_event(w.market(), &ev);
        }
        (
            r.drain_intents(),
            r.strategy().reviews.clone(),
            r.strategy().timers.clone(),
        )
    };
    let (a, b) = (run(), run());
    assert!(a.0.len() > 10 && a.1.len() > 10);
    assert_eq!(a, b);
}

#[test]
fn order_updates_reach_the_strategy() {
    let w = World::new(2);
    let mut r = CrossRunner::new(Probe::new(1, SEC), Members::new());
    let u = OrderUpdate {
        intent: crate::intent::IntentId {
            strategy: StrategyId(1),
            seq: 0,
        },
        order: None,
        state: crate::lifecycle::OrderState::Rejected,
        filled_qty: 0,
        avg_px: None,
        reject: None,
        ts: 0,
    };
    r.on_order_update(&w.tier0, &u);
    assert_eq!(r.strategy().updates, 1);
}

#[test]
fn a_strategy_that_does_not_want_member_events_gets_none_and_still_reviews_on_time() {
    struct Quiet(u32);
    impl CrossStrategy for Quiet {
        fn id(&self) -> StrategyId {
            StrategyId(4)
        }
        fn period(&self) -> u64 {
            SEC
        }
        fn on_review(&mut self, ctx: &mut Ctx<'_>, _v: &MemberView<'_>) {
            self.0 += 1;
            ctx.set_timer_in(TimerId(5), SEC / 4);
        }
        fn on_member_event(&mut self, _c: &mut Ctx<'_>, _v: &MemberView<'_>, _e: &Event) {
            panic!("not wanted");
        }
        fn on_timer(&mut self, _c: &mut Ctx<'_>, _v: &MemberView<'_>, _t: TimerId) {
            self.0 += 100;
        }
    }
    let mut w = World::new(2);
    let mut r = CrossRunner::new(Quiet(0), Members::from_ids([0, 1]));
    for i in 0..30u64 {
        let ev = trade((i % 2) as u32, i * SEC / 10 + 1, 10 * D);
        w.feed(&ev);
        r.on_event(w.market(), &ev);
    }
    // Events at 0.1 s .. 2.9 s: reviews at the first event at or after 1 s and 2 s, each with its timer.
    assert_eq!(
        (r.reviews(), r.member_events(), r.strategy().0),
        (2, 0, 202)
    );
}
