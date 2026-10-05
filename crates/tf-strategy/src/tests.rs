use tf_core::Px;
use tf_synth::SplitMix64;

use crate::intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
use crate::lifecycle::{LifecycleError, Order, OrderId, OrderState, OrderUpdate, RejectReason};

fn px(cents: i64) -> Px {
    Px::from_cents(cents)
}

fn stop(trigger: i64) -> Protective {
    Protective {
        stop_trigger: px(trigger),
        stop_limit: None,
        take_profit: None,
    }
}

fn intent(side: Side, purpose: Purpose, pricing: Pricing, protect: Option<Protective>) -> Intent {
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq: 7,
        },
        instrument: 3,
        side,
        qty: 100,
        purpose,
        pricing,
        protect,
        tif: Tif::Day,
        ts: 1_000,
        reason: 0,
    }
}

fn long(protect: Option<Protective>) -> Intent {
    intent(Side::Buy, Purpose::Open, Pricing::Limit(px(1000)), protect)
}

fn short(protect: Option<Protective>) -> Intent {
    intent(
        Side::SellShort,
        Purpose::Open,
        Pricing::Limit(px(1000)),
        protect,
    )
}

#[test]
fn only_sensible_side_and_purpose_pairs_are_valid_and_protection_follows_purpose() {
    // (side, purpose, valid with protection, valid without)
    let table = [
        (
            Side::Buy,
            Purpose::Open,
            Ok(()),
            Err(IntentError::MissingProtection),
        ), // long entry
        (
            Side::SellShort,
            Purpose::Open,
            Ok(()),
            Err(IntentError::MissingProtection),
        ), // short entry
        (
            Side::Sell,
            Purpose::Open,
            Err(IntentError::SideAndPurpose),
            Err(IntentError::SideAndPurpose),
        ),
        (
            Side::Sell,
            Purpose::Close,
            Err(IntentError::UnexpectedProtection),
            Ok(()),
        ), // exit a long
        (
            Side::Buy,
            Purpose::Close,
            Err(IntentError::UnexpectedProtection),
            Ok(()),
        ), // cover a short
        (
            Side::SellShort,
            Purpose::Close,
            Err(IntentError::SideAndPurpose),
            Err(IntentError::SideAndPurpose),
        ),
    ];
    for (side, purpose, with, without) in table {
        // A stop that is right for the side, so only the pairing and presence are under test.
        let good_stop = if side.is_buy() { stop(900) } else { stop(1100) };
        let i = intent(side, purpose, Pricing::Limit(px(1000)), Some(good_stop));
        assert_eq!(i.validate(), with, "{side:?} {purpose:?} with protection");
        let i = intent(side, purpose, Pricing::Limit(px(1000)), None);
        assert_eq!(
            i.validate(),
            without,
            "{side:?} {purpose:?} without protection"
        );
    }
}

#[test]
fn stops_and_targets_must_be_on_the_right_side_of_the_entry() {
    let with = |p: Protective| (long(Some(p)).validate(), short(Some(p)).validate());
    // Long: stop below 10.00, target above. Short: the reverse. Equal is the wrong side.
    assert_eq!(with(stop(999)).0, Ok(()));
    assert_eq!(with(stop(1000)).0, Err(IntentError::StopOnWrongSide));
    assert_eq!(with(stop(1001)).0, Err(IntentError::StopOnWrongSide));
    assert_eq!(with(stop(1001)).1, Ok(()));
    assert_eq!(with(stop(1000)).1, Err(IntentError::StopOnWrongSide));
    assert_eq!(with(stop(999)).1, Err(IntentError::StopOnWrongSide));
    assert_eq!(
        with(stop(0)).0,
        Err(IntentError::StopOnWrongSide),
        "a stop at zero is not a stop"
    );

    let tgt = |s: i64, t: i64| Protective {
        take_profit: Some(px(t)),
        ..stop(s)
    };
    assert_eq!(long(Some(tgt(900, 1100))).validate(), Ok(()));
    assert_eq!(
        long(Some(tgt(900, 1000))).validate(),
        Err(IntentError::TargetOnWrongSide)
    );
    assert_eq!(
        long(Some(tgt(900, 950))).validate(),
        Err(IntentError::TargetOnWrongSide)
    );
    assert_eq!(short(Some(tgt(1100, 900))).validate(), Ok(()));
    assert_eq!(
        short(Some(tgt(1100, 1100))).validate(),
        Err(IntentError::TargetOnWrongSide)
    );

    // A stop-limit is no better than its trigger: below it for a long's sell-stop, above for a short's.
    let lim = |s: i64, l: i64| Protective {
        stop_limit: Some(px(l)),
        ..stop(s)
    };
    assert_eq!(long(Some(lim(900, 890))).validate(), Ok(()));
    assert_eq!(long(Some(lim(900, 900))).validate(), Ok(()));
    assert_eq!(
        long(Some(lim(900, 910))).validate(),
        Err(IntentError::StopLimitOnWrongSide)
    );
    assert_eq!(short(Some(lim(1100, 1110))).validate(), Ok(()));
    assert_eq!(
        short(Some(lim(1100, 1090))).validate(),
        Err(IntentError::StopLimitOnWrongSide)
    );
    assert_eq!(
        long(Some(lim(900, 0))).validate(),
        Err(IntentError::StopLimitOnWrongSide)
    );
}

#[test]
fn stops_are_checked_against_the_reference_and_targets_against_the_worst_price() {
    // A 5% collar around 10.00: a buy may fill anywhere up to 10.50, including at 10.00.
    let collar = Pricing::Collar {
        reference: px(1000),
        collar_permille: 50,
    };
    let buy = |p: Protective| intent(Side::Buy, Purpose::Open, collar, Some(p));
    assert_eq!(buy(stop(900)).limit_price(), px(1050));
    assert_eq!(buy(stop(900)).pricing.reference_price(), px(1000));

    // A stop at 10.20 would sit above a fill at 10.00 and fire on the fill itself.
    assert_eq!(
        buy(stop(1020)).validate(),
        Err(IntentError::StopOnWrongSide)
    );
    assert_eq!(
        buy(stop(1000)).validate(),
        Err(IntentError::StopOnWrongSide)
    );
    assert_eq!(buy(stop(999)).validate(), Ok(()));

    // A target must be a profit even at the worst allowed fill, 10.50.
    let tgt = |t: i64| Protective {
        take_profit: Some(px(t)),
        ..stop(900)
    };
    assert_eq!(
        buy(tgt(1040)).validate(),
        Err(IntentError::TargetOnWrongSide)
    );
    assert_eq!(
        buy(tgt(1050)).validate(),
        Err(IntentError::TargetOnWrongSide)
    );
    assert_eq!(buy(tgt(1051)).validate(), Ok(()));

    // Mirrored for a short, which may fill down to 9.50: the stop is above 10.00, the target below 9.50.
    let sell = |p: Protective| intent(Side::SellShort, Purpose::Open, collar, Some(p));
    let s = |t: i64, take: Option<i64>| Protective {
        take_profit: take.map(px),
        ..stop(t)
    };
    assert_eq!(sell(s(1001, None)).limit_price(), px(950));
    assert_eq!(
        sell(s(1000, None)).validate(),
        Err(IntentError::StopOnWrongSide)
    );
    assert_eq!(sell(s(1001, None)).validate(), Ok(()));
    assert_eq!(
        sell(s(1100, Some(960))).validate(),
        Err(IntentError::TargetOnWrongSide)
    );
    assert_eq!(sell(s(1100, Some(949))).validate(), Ok(()));
}

#[test]
fn malformed_quantities_prices_and_collars_are_refused() {
    let mut i = long(Some(stop(900)));
    i.qty = 0;
    assert_eq!(i.validate(), Err(IntentError::ZeroQty));
    for p in [0, -5] {
        let i = intent(
            Side::Buy,
            Purpose::Open,
            Pricing::Limit(px(p)),
            Some(stop(900)),
        );
        assert_eq!(i.validate(), Err(IntentError::BadPrice));
    }
    let collar = |c| Pricing::Collar {
        reference: px(1000),
        collar_permille: c,
    };
    assert_eq!(
        intent(Side::Buy, Purpose::Open, collar(999), Some(stop(900))).validate(),
        Ok(())
    );
    assert_eq!(
        intent(Side::Buy, Purpose::Open, collar(1000), Some(stop(900))).validate(),
        Err(IntentError::BadCollar)
    );
    assert_eq!(
        intent(Side::Buy, Purpose::Open, collar(0), Some(stop(900))).validate(),
        Ok(())
    );
    let zero_ref = Pricing::Collar {
        reference: px(0),
        collar_permille: 10,
    };
    assert_eq!(
        intent(Side::Buy, Purpose::Open, zero_ref, Some(stop(900))).validate(),
        Err(IntentError::BadPrice)
    );
}

#[test]
fn a_collar_never_allows_a_worse_price_than_stated() {
    // Hand-checked rounding: reference 1001 raw, 50%.
    let c = Pricing::Collar {
        reference: Px::from_raw(1001),
        collar_permille: 500,
    };
    assert_eq!(
        c.worst_price(Side::Buy).raw(),
        1501,
        "1501.5 rounds down for a buyer"
    );
    assert_eq!(
        c.worst_price(Side::Sell).raw(),
        501,
        "500.5 rounds up for a seller"
    );
    assert_eq!(c.worst_price(Side::SellShort).raw(), 501);

    let mut rng = SplitMix64::new(12);
    for _ in 0..20_000 {
        let reference = rng.range(1, 5_000_000_000_000) as i64;
        let permille = rng.below(1000) as u32;
        let c = Pricing::Collar {
            reference: Px::from_raw(reference),
            collar_permille: permille,
        };
        let (buy, sell) = (
            c.worst_price(Side::Buy).raw(),
            c.worst_price(Side::Sell).raw(),
        );
        let (r, p) = (i128::from(reference), i128::from(permille));
        assert!(buy >= reference && sell <= reference && sell > 0);
        assert!(
            i128::from(buy) * 1000 <= r * (1000 + p),
            "buy bound looser than stated"
        );
        assert!(
            i128::from(sell) * 1000 >= r * (1000 - p),
            "sell bound looser than stated"
        );
        assert!(
            i128::from(buy) * 1000 > r * (1000 + p) - 1000,
            "buy bound more than 1 unit tighter"
        );
        assert!(
            i128::from(sell) * 1000 < r * (1000 - p) + 1000,
            "sell bound more than 1 unit tighter"
        );
    }
    assert_eq!(Pricing::Limit(px(5)).worst_price(Side::Buy), px(5));
}

#[test]
fn helpers_describe_the_intent() {
    let i = long(Some(stop(900)));
    assert_eq!((i.limit_price(), i.opens_long()), (px(1000), Some(true)));
    assert_eq!(i.notional_at_limit(), px(1000).raw() as u128 * 100);
    assert_eq!(short(Some(stop(1100))).opens_long(), Some(false));
    assert_eq!(
        intent(Side::Sell, Purpose::Close, Pricing::Limit(px(1000)), None).opens_long(),
        None
    );
}

#[test]
fn the_transition_table_is_exactly_this() {
    use OrderState::*;
    let allowed = [
        (Pending, Accepted),
        (Pending, Rejected),
        (Pending, Cancelled),
        (Accepted, PartiallyFilled),
        (Accepted, Filled),
        (Accepted, Cancelled),
        (Accepted, Expired),
        (PartiallyFilled, PartiallyFilled),
        (PartiallyFilled, Filled),
        (PartiallyFilled, Cancelled),
        (PartiallyFilled, Expired),
    ];
    for from in OrderState::ALL {
        for to in OrderState::ALL {
            assert_eq!(
                from.can_transition_to(to),
                allowed.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
        if from.is_terminal() {
            assert!(
                OrderState::ALL.iter().all(|&t| !from.can_transition_to(t)),
                "{from:?} is terminal"
            );
        } else {
            // Every live state can still reach a terminal one.
            assert!(
                OrderState::ALL
                    .iter()
                    .any(|&t| from.can_transition_to(t) && (t.is_terminal() || t != from))
            );
        }
    }
    assert_eq!(
        OrderState::ALL.iter().filter(|s| s.is_terminal()).count(),
        4
    );
}

fn order(qty: u32) -> Order {
    let mut i = long(Some(stop(900)));
    i.qty = qty;
    Order::new(OrderId(1), i)
}

#[test]
fn fills_move_an_order_through_partial_to_filled_with_a_weighted_average() {
    let mut o = order(400);
    assert_eq!(
        (o.state(), o.filled_qty(), o.remaining(), o.avg_px()),
        (OrderState::Pending, 0, 400, None)
    );
    o.transition(OrderState::Accepted).unwrap();
    o.fill(100, px(1000)).unwrap();
    assert_eq!(
        (o.state(), o.filled_qty(), o.remaining()),
        (OrderState::PartiallyFilled, 100, 300)
    );
    o.fill(300, px(1040)).unwrap();
    assert_eq!((o.state(), o.remaining()), (OrderState::Filled, 0));
    assert_eq!(
        o.avg_px(),
        Some(px(1030)),
        "(100 x 10.00 + 300 x 10.40) / 400"
    );

    let u = o.update(5_000);
    assert_eq!(
        (u.state, u.filled_qty, u.avg_px, u.order, u.reject, u.ts),
        (
            OrderState::Filled,
            400,
            Some(px(1030)),
            Some(OrderId(1)),
            None,
            5_000
        )
    );
    assert_eq!(u.intent, o.intent.id);
}

#[test]
fn impossible_operations_are_refused_and_change_nothing() {
    let mut o = order(100);
    assert_eq!(
        o.fill(10, px(1000)),
        Err(LifecycleError::NotWorking(OrderState::Pending)),
        "a fill before the acknowledgement"
    );
    o.transition(OrderState::Accepted).unwrap();
    assert_eq!(o.fill(0, px(1000)), Err(LifecycleError::BadFill));
    assert_eq!(o.fill(10, px(0)), Err(LifecycleError::BadFill));
    assert_eq!(
        o.fill(101, px(1000)),
        Err(LifecycleError::Overfill {
            remaining: 100,
            fill: 101
        })
    );
    assert_eq!(
        o.transition(OrderState::Filled),
        Err(LifecycleError::BadTransition {
            from: OrderState::Accepted,
            to: OrderState::Filled
        }),
        "fills go through fill()"
    );
    assert_eq!(
        o.transition(OrderState::Pending),
        Err(LifecycleError::BadTransition {
            from: OrderState::Accepted,
            to: OrderState::Pending
        })
    );
    assert_eq!(
        (o.state(), o.filled_qty(), o.avg_px()),
        (OrderState::Accepted, 0, None)
    );

    o.fill(60, px(1000)).unwrap();
    o.transition(OrderState::Cancelled).unwrap();
    assert_eq!(
        (o.state(), o.filled_qty()),
        (OrderState::Cancelled, 60),
        "a cancel keeps what filled"
    );
    assert_eq!(
        o.fill(10, px(1000)),
        Err(LifecycleError::NotWorking(OrderState::Cancelled))
    );
    assert_eq!(
        o.transition(OrderState::Expired),
        Err(LifecycleError::BadTransition {
            from: OrderState::Cancelled,
            to: OrderState::Expired
        })
    );
}

#[test]
fn a_broker_rejection_and_a_gateway_rejection_are_distinct_updates() {
    let mut o = order(100);
    o.transition(OrderState::Rejected).unwrap();
    let u = o.update(9);
    assert_eq!(
        (u.state, u.order.is_some(), u.filled_qty),
        (OrderState::Rejected, true, 0)
    );

    let id = IntentId {
        strategy: StrategyId(2),
        seq: 1,
    };
    let r = OrderUpdate::rejected(id, RejectReason::NotShortable, 77);
    assert_eq!(
        (r.state, r.order, r.reject, r.ts, r.intent),
        (
            OrderState::Rejected,
            None,
            Some(RejectReason::NotShortable),
            77,
            id
        )
    );
}

#[test]
fn random_operation_sequences_never_break_an_orders_invariants() {
    let mut rng = SplitMix64::new(99);
    for case in 0..3000 {
        let qty = rng.range(1, 500) as u32;
        let mut o = order(qty);
        let (mut lo, mut hi) = (i64::MAX, i64::MIN);
        for _ in 0..30 {
            let before = o;
            let result = match rng.below(4) {
                0 => o.transition(OrderState::ALL[rng.below(7) as usize]),
                _ => {
                    let (q, p) = (rng.range(0, 200) as u32, rng.range(0, 3000) as i64);
                    let r = o.fill(q, px(p));
                    if r.is_ok() {
                        (lo, hi) = (lo.min(px(p).raw()), hi.max(px(p).raw()));
                    }
                    r
                }
            };
            if result.is_err() {
                assert_eq!(
                    o, before,
                    "case {case}: a refused operation changed the order"
                );
            }
            assert!(o.filled_qty() <= qty, "case {case}: overfilled");
            assert_eq!(
                o.state() == OrderState::Filled,
                o.filled_qty() == qty,
                "case {case}: Filled iff fully filled"
            );
            if before.state().is_terminal() {
                assert_eq!(
                    o.state(),
                    before.state(),
                    "case {case}: left a terminal state"
                );
            }
            if let Some(avg) = o.avg_px() {
                assert!(
                    lo <= avg.raw() && avg.raw() <= hi,
                    "case {case}: average outside the fills' range"
                );
            } else {
                assert_eq!(o.filled_qty(), 0);
            }
        }
    }
}
