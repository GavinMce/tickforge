use tf_core::{Event, Header, Nanos, ProviderId, Px, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::budget::{BudgetSource, BudgetView};
use crate::cross::{CrossRunner, CrossStrategy, Market, MemberView, Members};
use crate::intent::{IntentId, StrategyId};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::strategy::{Ctx, TimerId};

const SEC: Nanos = 1_000_000_000;
const D: u128 = 1_000_000_000;

fn view(total: u128, used: u128) -> BudgetView {
    BudgetView { total, used }
}

fn px(c: i64) -> Px {
    Px::from_cents(c)
}

#[test]
fn what_is_left_is_the_budget_less_what_is_used_and_never_below_zero() {
    assert_eq!(view(1000, 400).left(), 600);
    assert_eq!(view(1000, 1000).left(), 0);
    assert_eq!(view(1000, 1500).left(), 0, "marks moved against it");
    assert_eq!(view(0, 0).left(), 0);
}

#[test]
fn shares_for_risk_are_the_risk_over_the_stop_distance_and_no_more_than_the_room_left() {
    // A budget of 100,000 dollars, 1% (10,000 ppm) risked is 1,000 dollars; a stop 2 dollars away: 500 shares; at 20 dollars they cost 10,000.
    let b = view(100_000 * D, 0);
    assert_eq!(b.shares_for_risk(10_000, 2 * D as i64, px(2000)), 500);
    // Rounds down: 1,000 dollars over a 3-dollar stop is 333.33 shares.
    assert_eq!(b.shares_for_risk(10_000, 3 * D as i64, px(2000)), 333);
    // A tighter stop is more shares: 25 cents is 4,000, which at 20 dollars is 80,000 dollars (inside the budget).
    assert_eq!(b.shares_for_risk(10_000, D as i64 / 4, px(2000)), 4000);
    // The room caps it: 4,000 shares at 20 dollars needs 80,000, and only 60,000 are left: 3,000.
    assert_eq!(
        view(100_000 * D, 40_000 * D).shares_for_risk(10_000, D as i64 / 4, px(2000)),
        3000
    );
    // Exactly enough room is allowed, one dollar less is not (59,999 dollars at 20 holds 2,999 shares).
    assert_eq!(
        view(100_000 * D, 40_000 * D).shares_for_risk(10_000, D as i64 / 4, px(2000)),
        3000
    );
    assert_eq!(
        view(100_000 * D, 40_001 * D).shares_for_risk(10_000, D as i64 / 4, px(2000)),
        2999
    );
    // Nothing when nothing is left, or the budget is over-used.
    assert_eq!(
        view(100_000 * D, 100_000 * D).shares_for_risk(10_000, 2 * D as i64, px(2000)),
        0
    );
    assert_eq!(
        view(100_000 * D, 120_000 * D).shares_for_risk(10_000, 2 * D as i64, px(2000)),
        0
    );
    // Nothing for a stop that is no distance, a negative one, a price of nothing, or no risk.
    assert_eq!(b.shares_for_risk(10_000, 0, px(2000)), 0);
    assert_eq!(b.shares_for_risk(10_000, -(D as i64), px(2000)), 0);
    assert_eq!(b.shares_for_risk(10_000, 2 * D as i64, Px::from_raw(0)), 0);
    assert_eq!(b.shares_for_risk(0, 2 * D as i64, px(2000)), 0);
    // A whole budget risked over a tiny stop saturates instead of wrapping.
    assert_eq!(
        view(u128::MAX / 2, 0).shares_for_risk(1_000_000, 1, px(1)),
        u32::MAX
    );
    // Risk is of the whole budget, not of what is left: using half of it halves the room, not the risk.
    assert_eq!(
        view(100_000 * D, 50_000 * D).shares_for_risk(10_000, 2 * D as i64, px(2000)),
        500
    );
}

/// A stand-in for the gateway: one budget for strategy 1, none for the others.
struct Fixed(BudgetView);

impl BudgetSource for Fixed {
    fn budget(&self, strategy: StrategyId) -> Option<BudgetView> {
        (strategy == StrategyId(1)).then_some(self.0)
    }
}

/// Reads its budget in every callback that can and keeps what it saw.
struct Reader {
    id: u16,
    in_review: Vec<Option<BudgetView>>,
    in_event: Vec<Option<BudgetView>>,
    in_timer: Vec<Option<BudgetView>>,
    in_update: Vec<Option<BudgetView>>,
}

impl CrossStrategy for Reader {
    const WANTS_MEMBER_EVENTS: bool = true;
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }
    fn period(&self) -> Nanos {
        SEC
    }
    fn on_review(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>) {
        self.in_review.push(ctx.budget());
        ctx.set_timer_in(TimerId(1), SEC / 2);
    }
    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, _: &Event) {
        self.in_event.push(ctx.budget());
    }
    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, _: TimerId) {
        self.in_timer.push(ctx.budget());
    }
    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, _: &OrderUpdate) {
        self.in_update.push(ctx.budget());
    }
}

fn trade(sec: u64) -> Event {
    Event::Trade(Trade {
        hdr: Header {
            ts_event: 1_790_953_200 * SEC + sec * SEC,
            ts_recv: 1_790_953_200 * SEC + sec * SEC,
            seq: sec,
            instrument: 0,
            provider: ProviderId::Synthetic,
        },
        px: px(1000),
        size: 1,
        flags: TradeFlags::NONE,
    })
}

fn run(id: u16, source: Option<&dyn BudgetSource>) -> Reader {
    let mut tier0 = Tier0::new(1);
    let refs = vec![RefInfo::default()];
    let mut r = CrossRunner::new(
        Reader {
            id,
            in_review: vec![],
            in_event: vec![],
            in_timer: vec![],
            in_update: vec![],
        },
        Members::from_ids([0]),
    );
    for sec in 0..4 {
        let ev = trade(sec);
        tier0.on_event(&ev);
        r.on_event(
            Market {
                tier0: &tier0,
                refs: &refs,
                budget: source,
            },
            None,
            None,
            &ev,
        );
    }
    // An order update reaches the strategy with no market: no budget there.
    let update = OrderUpdate {
        intent: IntentId {
            strategy: StrategyId(id),
            seq: 1,
        },
        order: None,
        state: OrderState::Cancelled,
        filled_qty: 0,
        avg_px: None,
        reject: None,
        ts: 0,
    };
    r.on_order_update(&tier0, None, None, &update);
    let s = r.strategy();
    Reader {
        id,
        in_review: s.in_review.clone(),
        in_event: s.in_event.clone(),
        in_timer: s.in_timer.clone(),
        in_update: s.in_update.clone(),
    }
}

#[test]
fn a_strategy_reads_its_budget_in_a_review_an_event_and_a_timer_and_a_strategy_without_one_reads_none()
 {
    let source = Fixed(view(50_000 * D, 12_000 * D));
    let r = run(1, Some(&source));
    assert!(!r.in_review.is_empty() && !r.in_event.is_empty() && !r.in_timer.is_empty());
    for seen in r.in_review.iter().chain(&r.in_event).chain(&r.in_timer) {
        assert_eq!(*seen, Some(view(50_000 * D, 12_000 * D)));
    }
    // Strategy 2 is not in the source's tree; and with no source at all there is nothing to read.
    let r = run(2, Some(&source));
    assert!(r.in_review.iter().chain(&r.in_event).all(Option::is_none));
    let r = run(1, None);
    assert!(!r.in_review.is_empty());
    assert!(
        r.in_review
            .iter()
            .chain(&r.in_event)
            .chain(&r.in_timer)
            .all(Option::is_none)
    );
}
