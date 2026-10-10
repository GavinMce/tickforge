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
    /// The engine's shared bars (capacity 4); fed with every event by `feed`.
    bars: tf_engine::SharedBars,
}

impl World {
    fn new(n: usize) -> Self {
        World {
            tier0: Tier0::new(n),
            refs: vec![
                RefInfo {
                    price: Some(10 * D),
                    adv_shares: Some(1000),
                    ..RefInfo::default()
                };
                n
            ],
            bars: tf_engine::SharedBars::new(tf_engine::MtfConfig::default(), n, 4),
        }
    }

    fn feed(&mut self, ev: &Event) {
        self.tier0.on_event(ev);
        self.bars.on_event(ev, &mut Vec::new());
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

#[test]
fn the_view_gives_a_members_session_state_and_nothing_for_others_and_nothing_is_kept_before_a_day_is_set()
 {
    use tf_calendar::{Calendar, Date};
    let day = Calendar::us_equities()
        .times(Date::new(2026, 5, 1).unwrap())
        .unwrap()
        .unwrap();
    let mut tier0 = Tier0::new(3);
    let refs = vec![RefInfo::default(); 3];
    let members = Members::from_ids([0, 2]);
    let sized = |id: u32, ts: u64, px: i64, size: u32| match trade(id, ts, px) {
        Event::Trade(mut t) => {
            t.size = size;
            Event::Trade(t)
        }
        e => e,
    };
    // Before the day is set nothing is kept.
    tier0.on_event(&sized(0, day.premarket + SEC, 10 * D, 100));
    {
        let v = MemberView::new(
            Market {
                tier0: &tier0,
                refs: &refs,
            },
            &members,
        );
        let s = v.session(0).expect("a member's slot exists");
        assert_eq!((s.premarket.volume, s.regular.volume, s.open), (0, 0, None));
    }
    tier0.set_day(day);
    // Premarket: 100 at $10 and 300 at $12; the open: 200 at $11, then 100 at $11.50 a minute in.
    for ev in [
        sized(0, day.premarket + 10 * SEC, 10 * D, 100),
        sized(0, day.premarket + 20 * SEC, 12 * D, 300),
        sized(0, day.open + SEC, 11 * D, 200),
        sized(0, day.open + 70 * SEC, 23 * D / 2, 100),
        sized(1, day.premarket + 10 * SEC, 5 * D, 50),
    ] {
        tier0.on_event(&ev);
    }
    let v = MemberView::new(
        Market {
            tier0: &tier0,
            refs: &refs,
        },
        &members,
    );
    let s = v.session(0).expect("a member that traded");
    assert_eq!(
        (s.premarket.high, s.premarket.low, s.premarket.volume),
        (Some(Px::from_raw(12 * D)), Some(Px::from_raw(10 * D)), 400)
    );
    // The premarket VWAP is (100 x 10 + 300 x 12) / 400 = 11.50 and the regular one starts at the open: (200 x 11 + 100 x 11.5) / 300.
    assert_eq!(s.premarket.vwap(), Some(Px::from_raw(23 * D / 2)));
    assert_eq!(s.regular.volume, 300);
    assert_eq!(s.regular.vwap(), Some(Px::from_raw(11_166_666_666)));
    assert_eq!(s.open, Some((Px::from_raw(11 * D), day.open + SEC)));
    assert_eq!(s.first_minute_volume, 200);
    assert_eq!(s.first_5m_volume, 300);
    // A non-member, and a member that has not traded, give nothing useful.
    assert!(
        v.session(1).is_none(),
        "a non-member's session is not visible"
    );
    assert_eq!(v.session(2).unwrap().regular.volume, 0);
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
        r.on_event(w.market(), None, None, &ev);
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
        r.on_event(w.market(), None, None, &ev);
    }
    r.advance_to(w.market(), None, None, SEC + 5);
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
    r.advance_to(w.market(), None, None, SEC + 5 + SEC / 2);
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
    b.on_event(w.market(), None, None, &trade(0, 1, D));
    b.on_event(w.market(), None, None, &trade(0, SEC, D));
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
        r.on_event(w.market(), None, None, &ev);
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
        None,
        None,
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
        a.on_event(w.market(), None, None, &ev);
        b.on_event(w.market(), None, None, &ev);
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
            r.on_event(w.market(), None, None, &ev);
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
    r.on_order_update(&w.tier0, None, None, &u);
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
        r.on_event(w.market(), None, None, &ev);
    }
    // Events at 0.1 s .. 2.9 s: reviews at the first event at or after 1 s and 2 s, each with its timer.
    assert_eq!(
        (r.reviews(), r.member_events(), r.strategy().0),
        (2, 0, 202)
    );
}

// ---- shared Tier 1 (E18-S04) ----

use tf_core::TierAction;
use tf_engine::{Denied, Grant, Promoter, PromoterConfig, ScannerConfig, tier_reason};

fn tier1(max: usize) -> Promoter {
    let cfg = PromoterConfig {
        max_tier1: max,
        min_dwell_secs: 0,
        ..PromoterConfig::default()
    };
    Promoter::new(
        cfg,
        ScannerConfig {
            min_volume: 1_000,
            ..ScannerConfig::default()
        },
        16,
    )
    .unwrap()
}

/// Asks for Tier 1 for the ids it is given at each review, and pins `pin` once.
struct Wanter {
    id: u16,
    wants: Vec<u32>,
    pin: Option<u32>,
    unpin_at_review: Option<u32>,
    reviews: u32,
    grants: Vec<Option<Grant>>,
    revoked: Vec<u32>,
}

impl Wanter {
    fn new(id: u16, wants: &[u32]) -> Self {
        Wanter {
            id,
            wants: wants.to_vec(),
            pin: None,
            unpin_at_review: None,
            reviews: 0,
            grants: vec![],
            revoked: vec![],
        }
    }
}

impl CrossStrategy for Wanter {
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> u64 {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>) {
        self.reviews += 1;
        for w in self.wants.clone() {
            self.grants.push(ctx.request_tier1(w));
        }
        if let Some(p) = self.pin {
            ctx.pin_tier1(p);
        }
        if self.unpin_at_review == Some(self.reviews) {
            ctx.unpin_tier1(self.pin.unwrap());
        }
    }

    fn on_tier1_revoked(&mut self, _ctx: &mut Ctx<'_>, _view: &MemberView<'_>, id: u32) {
        self.revoked.push(id);
    }
}

fn review_both(
    w: &World,
    p: &mut Promoter,
    a: &mut CrossRunner<Wanter>,
    b: &mut CrossRunner<Wanter>,
    ts: u64,
) {
    let ev = trade(0, ts, 10 * D);
    a.on_event(w.market(), Some(p), None, &ev);
    b.on_event(w.market(), Some(p), None, &ev);
}

#[test]
fn strategies_share_tier_one_by_priority_and_every_denial_is_counted() {
    let mut w = World::new(16);
    let mut p = tier1(2);
    p.set_priority(2, 5);
    let all = Members::from_ids(0..16);
    let mut a = CrossRunner::new(Wanter::new(1, &[10, 11, 12]), all.clone());
    let mut b = CrossRunner::new(Wanter::new(2, &[12]), all);
    w.feed(&trade(0, 1, 10 * D));
    // First events set the grid; the reviews come at 1 s.
    review_both(&w, &mut p, &mut a, &mut b, 1);
    assert_eq!(a.reviews(), 0);
    review_both(&w, &mut p, &mut a, &mut b, SEC);
    // Strategy 1 (default priority) got 10 and 11 and was denied 12; strategy 2 (priority 5) then
    // evicted the lowest id of the two to get 12.
    assert_eq!(
        a.strategy().grants,
        [
            Some(Grant::Promoted),
            Some(Grant::Promoted),
            Some(Grant::Denied(Denied::Full))
        ]
    );
    assert_eq!(
        b.strategy().grants,
        [Some(Grant::PromotedByEviction { evicted: 10 })]
    );
    assert_eq!(p.promoted(), [11, 12]);
    // The changes each runner's requests caused are theirs to hand to the tape, in order.
    let ta = a.drain_tier_events();
    let tb = b.drain_tier_events();
    assert_eq!(
        ta.iter()
            .map(|c| (c.hdr.instrument, c.action))
            .collect::<Vec<_>>(),
        [(10, TierAction::Promote), (11, TierAction::Promote)]
    );
    assert_eq!(
        tb.iter()
            .map(|c| (c.hdr.instrument, c.action, c.reason))
            .collect::<Vec<_>>(),
        [
            (10, TierAction::Demote, tier_reason::EVICTED),
            (12, TierAction::Promote, tier_reason::STRATEGY_REQUEST)
        ]
    );
    // Counted per strategy, never lost.
    let st = p.owner_stats();
    let (s1, s2) = (st[0].1, st[1].1);
    assert_eq!(
        (s1.requests, s1.promoted, s1.denied_full, s1.lost),
        (3, 2, 1, 1)
    );
    assert_eq!(
        (s2.requests, s2.promoted, s2.evicted_for, s2.denied_full),
        (1, 1, 1, 0)
    );
    assert!(
        p.metrics_text()
            .contains("tier1_denied_full{strategy=\"1\"} 1")
    );
    // The host tells the loser, which hears of exactly the symbol it lost.
    for (owner, id) in p.drain_revoked() {
        assert_eq!(owner, 1);
        a.on_tier1_revoked(w.market(), Some(&mut p), None, id);
    }
    assert_eq!(a.strategy().revoked, [10]);
    // Without a promoter a request has no answer.
    let mut lone = CrossRunner::new(Wanter::new(3, &[1]), Members::from_ids(0..16));
    lone.on_event(w.market(), None, None, &trade(0, 1, D));
    lone.on_event(w.market(), None, None, &trade(0, SEC, D));
    assert_eq!(lone.strategy().grants, [None]);
}

#[test]
fn two_strategies_pinning_one_symbol_do_not_release_each_other() {
    let mut w = World::new(16);
    let mut p = tier1(2);
    let all = Members::from_ids(0..16);
    let mut a = CrossRunner::new(Wanter::new(1, &[7]), all.clone());
    let mut b = CrossRunner::new(Wanter::new(2, &[7]), all);
    a.strategy_mut().pin = Some(7);
    b.strategy_mut().pin = Some(7);
    a.strategy_mut().unpin_at_review = Some(1);
    w.feed(&trade(0, 1, 10 * D));
    review_both(&w, &mut p, &mut a, &mut b, 1);
    review_both(&w, &mut p, &mut a, &mut b, SEC);
    // A pinned then unpinned in the same review; B's pin stands.
    assert!(p.is_pinned(7) && p.is_pinned_by(2, 7) && !p.is_pinned_by(1, 7));
    assert_eq!(b.strategy().grants, [Some(Grant::Already)]);
}

#[test]
fn a_strategy_can_release_what_it_asked_for() {
    struct Once(Vec<(bool, bool)>, u32);
    impl CrossStrategy for Once {
        fn id(&self) -> StrategyId {
            StrategyId(6)
        }
        fn period(&self) -> u64 {
            SEC
        }
        fn on_review(&mut self, ctx: &mut Ctx<'_>, _v: &MemberView<'_>) {
            self.1 += 1;
            if self.1 == 1 {
                ctx.request_tier1(3);
            }
            let before = ctx.wants_tier1(3);
            if self.1 == 2 {
                ctx.release_tier1(3);
            }
            self.0.push((before, ctx.wants_tier1(3)));
        }
    }
    let w = World::new(16);
    let mut p = tier1(2);
    let mut r = CrossRunner::new(Once(vec![], 0), Members::new());
    for ts in [1, SEC, 2 * SEC] {
        r.on_event(w.market(), Some(&mut p), None, &trade(0, ts, D));
    }
    assert_eq!(r.strategy().0, [(true, true), (true, false)]);
    assert!(
        p.is_promoted(3),
        "released but not yet cold: it leaves at the cool-down, not at once"
    );
}

// ---- shared bars (E19-S03) ----

mod shared_bars {
    use super::*;
    use crate::strategy::{BarsError, Host, Strategy};
    use crate::{MtfBars, MtfConfig, Timeframe};
    use std::collections::BTreeSet;
    use tf_engine::{BarsStats, SharedBars, SymbolBars, TrackError};

    const T0: u64 = 1_767_571_200 * SEC;

    /// A cross strategy that claims bars for the symbols it wants on the first event it sees of each,
    /// and optionally lets them all go after a number of events.
    struct BarUser {
        id: u16,
        want: Vec<u32>,
        release_after: Option<u64>,
        let_go: bool,
        /// Ask again right after asking, and let go twice.
        twice: bool,
        claimed: BTreeSet<u32>,
        results: Vec<(u32, Result<(), BarsError>)>,
        events: u64,
        /// What `ctx.bars` showed for each symbol at the last event.
        reads: Vec<(u32, bool)>,
    }

    impl BarUser {
        fn new(id: u16, want: &[u32]) -> Self {
            BarUser {
                id,
                want: want.to_vec(),
                release_after: None,
                let_go: false,
                twice: false,
                claimed: BTreeSet::new(),
                results: Vec::new(),
                events: 0,
                reads: Vec::new(),
            }
        }
    }

    impl CrossStrategy for BarUser {
        const WANTS_MEMBER_EVENTS: bool = true;

        fn id(&self) -> StrategyId {
            StrategyId(self.id)
        }
        fn period(&self) -> u64 {
            60 * SEC
        }
        fn on_review(&mut self, _: &mut Ctx<'_>, _: &MemberView<'_>) {}
        fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, ev: &Event) {
            let i = ev.instrument();
            self.events += 1;
            if !self.let_go && self.want.contains(&i) && self.claimed.insert(i) {
                self.results.push((i, ctx.track_bars(i)));
                if self.twice {
                    self.results.push((i, ctx.track_bars(i)));
                    assert!(ctx.untrack_bars(i), "it had the claim");
                    assert!(!ctx.untrack_bars(i), "and has not any more");
                    assert!(ctx.bars(i).is_none());
                    self.results.push((i, ctx.track_bars(i)));
                }
            }
            if self.release_after == Some(self.events) {
                self.let_go = true;
                for i in std::mem::take(&mut self.claimed) {
                    assert!(ctx.untrack_bars(i));
                }
            }
            self.reads = (0..4).map(|i| (i, ctx.bars(i).is_some())).collect();
        }
    }

    /// The same thing for one symbol at a time, on the per-symbol host. It keeps a copy of the bars it
    /// sees after every event, so they can be compared with the shared ones.
    struct PerSymbol {
        want: Vec<u32>,
        tracked: BTreeSet<u32>,
        last: Vec<Option<Box<SymbolBars>>>,
    }

    impl Strategy for PerSymbol {
        fn id(&self) -> StrategyId {
            StrategyId(99)
        }
        fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
            let i = ev.instrument();
            if self.want.contains(&i) && self.tracked.insert(i) {
                ctx.track_bars(i).unwrap();
            }
            for s in 0..self.last.len() {
                if let Some(b) = ctx.bars(s as u32) {
                    self.last[s] = Some(Box::new(*b));
                }
            }
        }
        fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {}
    }

    fn stream(n: usize, symbols: u32, seed: u64) -> Vec<Event> {
        let mut x = seed;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut ts = T0 + 3_600 * SEC;
        (0..n)
            .map(|_| {
                ts += next() % (4 * SEC);
                let px = 1000 + (next() % 300) as i64;
                Event::Trade(Trade {
                    hdr: Header {
                        ts_event: ts,
                        ts_recv: ts,
                        seq: ts,
                        instrument: (next() % u64::from(symbols)) as u32,
                        provider: ProviderId::Synthetic,
                    },
                    px: Px::from_cents(px),
                    size: 1 + (next() % 90) as u32,
                    flags: TradeFlags::NONE,
                })
            })
            .collect()
    }

    fn same(a: &SymbolBars, b: &SymbolBars) {
        for tf in Timeframe::ALL {
            assert_eq!(a.closed_total(tf), b.closed_total(tf), "{tf:?}");
            assert_eq!(a.forming(tf), b.forming(tf), "{tf:?}");
            for i in 0..a.closed_len(tf) {
                assert_eq!(a.closed(tf, i), b.closed(tf, i), "{tf:?} {i}");
            }
        }
    }

    fn per_symbol_host(want: &[u32], events: &[Event]) -> Vec<Option<Box<SymbolBars>>> {
        let strat = PerSymbol {
            want: want.to_vec(),
            tracked: BTreeSet::new(),
            last: vec![None; 4],
        };
        let mut h = Host::new(strat, 4).with_bars(MtfBars::new(MtfConfig::default(), 4, 4));
        for e in events {
            h.on_event(e);
        }
        h.strategy().last.clone()
    }

    #[test]
    fn a_cross_strategy_reads_exactly_the_bars_a_per_symbol_host_builds() {
        // Two hours of random trades in four symbols; the strategy wants three of them.
        let events = stream(6000, 4, 0x1234_5678_9abc_def1);
        let want = [0u32, 1, 3];
        let alone = per_symbol_host(&want, &events);

        let mut w = World::new(4);
        let mut r = CrossRunner::new(BarUser::new(7, &want), Members::from_ids(0..4));
        for e in &events {
            w.feed(e);
            let m = Market {
                tier0: &w.tier0,
                refs: &w.refs,
            };
            r.on_event(m, None, Some(&mut w.bars), e);
        }
        assert!(r.strategy().results.iter().all(|(_, res)| res.is_ok()));
        assert_eq!(r.strategy().results.len(), 3);
        let mut checked = 0;
        for i in 0..4u32 {
            let got = w.bars.symbol(7, i);
            match (got, &alone[i as usize]) {
                (Some(g), Some(a)) => {
                    same(g, a);
                    checked += 1;
                }
                (None, None) => assert!(!want.contains(&i)),
                _ => panic!(
                    "symbol {i}: shared {} alone {}",
                    got.is_some(),
                    alone[i as usize].is_some()
                ),
            }
        }
        assert_eq!(checked, 3);
        assert!(
            w.bars.symbol(7, 0).unwrap().closed_total(Timeframe::M1) > 100,
            "a real stretch of minutes"
        );
        // What the strategy saw through its context was the same bars: a symbol it did not claim reads
        // as nothing.
        assert_eq!(
            r.strategy().reads,
            [(0, true), (1, true), (2, false), (3, true)]
        );
    }

    #[test]
    fn one_strategy_letting_go_does_not_stop_anothers_bars() {
        let events = stream(4000, 4, 0xfeed_beef_0bad_f00d);
        let alone = per_symbol_host(&[0, 1], &events);
        let mut w = World::new(4);
        let mut a = BarUser::new(1, &[0, 1]);
        a.release_after = Some(500);
        let mut a = CrossRunner::new(a, Members::from_ids(0..4));
        let mut b = CrossRunner::new(BarUser::new(2, &[0, 1]), Members::from_ids(0..4));
        for (k, e) in events.iter().enumerate() {
            w.feed(e);
            let m = Market {
                tier0: &w.tier0,
                refs: &w.refs,
            };
            a.on_event(m, None, Some(&mut w.bars), e);
            b.on_event(m, None, Some(&mut w.bars), e);
            if k == 499 {
                // Strategy 1 has let go of everything by now; strategy 2 still has both.
                assert!(!w.bars.is_claimed_by(1, 0) && !w.bars.is_claimed_by(1, 1));
                assert_eq!(w.bars.owners_of(0), [2]);
            }
        }
        assert!(
            w.bars.symbol(1, 0).is_none(),
            "the one that let go reads nothing"
        );
        for i in 0..2u32 {
            same(
                w.bars.symbol(2, i).unwrap(),
                alone[i as usize].as_ref().unwrap(),
            );
        }
        // The second strategy joined symbols the first had started, and the first's release cost nothing.
        let st: std::collections::BTreeMap<_, _> = w.bars.stats().into_iter().collect();
        assert_eq!((st[&1].started, st[&1].released), (2, 2));
        assert_eq!((st[&2].started, st[&2].joined), (0, 2));
    }

    #[test]
    fn a_request_past_the_bound_is_refused_to_the_strategy_and_counted() {
        let mut w = World::new(8);
        w.bars = SharedBars::new(MtfConfig::default(), 8, 2);
        let mut a = CrossRunner::new(BarUser::new(1, &[0, 1]), Members::from_ids(0..8));
        let mut b = CrossRunner::new(BarUser::new(2, &[2, 0, 9]), Members::from_ids(0..8));
        for (k, id) in [0u32, 1, 2, 0].into_iter().enumerate() {
            let e = trade(id, SEC * (k as u64 + 1), 10 * D);
            w.feed(&e);
            let m = Market {
                tier0: &w.tier0,
                refs: &w.refs,
            };
            a.on_event(m, None, Some(&mut w.bars), &e);
            b.on_event(m, None, Some(&mut w.bars), &e);
        }
        assert_eq!(a.strategy().results, [(0, Ok(())), (1, Ok(()))]);
        // Symbol 2 does not fit; symbol 0 is already tracked, so it costs nothing.
        assert_eq!(
            b.strategy().results,
            [(0, Ok(())), (2, Err(BarsError::Track(TrackError::Full)))]
        );
        let st: std::collections::BTreeMap<_, _> = w.bars.stats().into_iter().collect();
        assert_eq!(
            st[&2],
            BarsStats {
                requests: 2,
                started: 0,
                joined: 1,
                already: 0,
                refused_full: 1,
                refused_unknown: 0,
                released: 0,
            }
        );
        assert!(
            w.bars
                .metrics_text()
                .contains("bars_refused_full{strategy=\"2\"} 1")
        );
    }

    #[test]
    fn asking_twice_is_told_so_and_letting_go_is_a_release() {
        let mut w = World::new(4);
        let mut u = BarUser::new(1, &[0]);
        u.twice = true;
        let mut r = CrossRunner::new(u, Members::from_ids(0..4));
        let e = trade(0, SEC, 10 * D);
        w.feed(&e);
        let m = Market {
            tier0: &w.tier0,
            refs: &w.refs,
        };
        r.on_event(m, None, Some(&mut w.bars), &e);
        assert_eq!(
            r.strategy().results,
            [
                (0, Ok(())),
                (0, Err(BarsError::Track(TrackError::AlreadyTracked))),
                (0, Ok(())),
            ]
        );
        assert_eq!(w.bars.tracked(), 1, "it asked again after letting go");
    }

    #[test]
    fn without_shared_bars_a_claim_says_so() {
        let mut w = World::new(4);
        let mut r = CrossRunner::new(BarUser::new(1, &[0]), Members::from_ids(0..4));
        let e = trade(0, SEC, 10 * D);
        w.feed(&e);
        let m = Market {
            tier0: &w.tier0,
            refs: &w.refs,
        };
        r.on_event(m, None, None, &e);
        assert_eq!(r.strategy().results, [(0, Err(BarsError::NotConfigured))]);
        assert_eq!(
            r.strategy().reads,
            [(0, false), (1, false), (2, false), (3, false)]
        );
    }
}

#[test]
fn a_member_view_gives_the_reference_row_of_members_only() {
    let mut w = World::new(4);
    w.refs[1].hist.prev_close = Some(7 * D);
    w.refs[2].hist.prev_close = Some(9 * D);
    let members = Members::from_ids([1, 3]);
    let view = MemberView::new(w.market(), &members);
    assert_eq!(view.reference(1).unwrap().hist.prev_close, Some(7 * D));
    assert_eq!(view.reference(3).unwrap().price, Some(10 * D));
    assert!(
        view.reference(2).is_none(),
        "a symbol outside the universe reads as nothing"
    );
    assert!(view.reference(99).is_none(), "and one outside the id space");
}
