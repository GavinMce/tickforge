use tf_core::{
    Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade,
    TradeFlags,
};

use crate::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, RejectReason};
use crate::sim::{SimBroker, SimConfig, run_backtest};
use crate::strategy::{Ctx, Host, Request, Strategy, TimerId};

const MS: Nanos = 1_000_000;
const DAY: Nanos = 86_400 * NANOS_PER_SEC;

fn hdr(ts: Nanos) -> Header {
    Header {
        ts_event: ts,
        ts_recv: ts,
        seq: ts,
        instrument: 0,
        provider: ProviderId::Synthetic,
    }
}

fn quote(ts: Nanos, bid: i64, ask: i64, bid_sz: u32, ask_sz: u32) -> Event {
    Event::Quote(Quote {
        hdr: hdr(ts),
        bid_px: Px::from_cents(bid),
        ask_px: Px::from_cents(ask),
        bid_sz,
        ask_sz,
    })
}

fn trade(ts: Nanos, cents: i64) -> Event {
    Event::Trade(Trade {
        hdr: hdr(ts),
        px: Px::from_cents(cents),
        size: 1,
        flags: TradeFlags::NONE,
    })
}

fn status(ts: Nanos, kind: StatusKind) -> Event {
    Event::Status(Status {
        hdr: hdr(ts),
        kind,
        lo: Px::ZERO,
        hi: Px::ZERO,
    })
}

fn intent(seq: u64, ts: Nanos, side: Side, qty: u32, pricing: Pricing, tif: Tif) -> Intent {
    let opening = side != Side::Sell;
    let long = side == Side::Buy;
    let reference = pricing.reference_price().raw();
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq,
        },
        instrument: 0,
        side,
        qty,
        purpose: if opening {
            Purpose::Open
        } else {
            Purpose::Close
        },
        pricing,
        protect: opening.then(|| Protective {
            stop_trigger: Px::from_raw(if long { reference / 2 } else { reference * 2 }),
            stop_limit: None,
            take_profit: None,
        }),
        tif,
        ts,
        reason: 0,
    }
}

fn collar(cents: i64, permille: u32) -> Pricing {
    Pricing::Collar {
        reference: Px::from_cents(cents),
        collar_permille: permille,
    }
}

fn broker(latency_ms: u64, bps: u32) -> SimBroker {
    SimBroker::new(
        SimConfig {
            latency_ns: latency_ms * MS,
            borrow_bps_per_year: bps,
        },
        2,
    )
}

fn states(b: &mut SimBroker) -> Vec<(OrderState, u32)> {
    b.drain_updates()
        .iter()
        .map(|u| (u.state, u.filled_qty))
        .collect()
}

#[test]
fn a_marketable_buy_fills_at_the_ask_and_records_its_slippage() {
    let mut b = broker(50, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 500));
    // Reference 10.00, collar 2% -> may pay up to 10.20.
    b.submit(&intent(0, MS, Side::Buy, 100, collar(1000, 20), Tif::Ioc));
    assert_eq!(b.open_orders(), 1);
    b.on_event(&trade(60 * MS, 1005));
    let f = b.fills();
    assert_eq!(f.len(), 1);
    assert_eq!(
        (f[0].qty, f[0].px, f[0].ts),
        (100, Px::from_cents(1005), 51 * MS)
    );
    assert_eq!(
        f[0].slippage,
        Px::from_cents(5).raw(),
        "paid 5 cents over the reference"
    );
    assert_eq!(
        states(&mut b),
        [(OrderState::Accepted, 0), (OrderState::Filled, 100)]
    );
    assert_eq!((b.position(0), b.open_orders()), (100, 0));
}

#[test]
fn price_improvement_is_negative_slippage() {
    let mut b = broker(0, 0);
    b.on_event(&quote(0, 990, 995, 500, 500));
    b.submit(&intent(
        0,
        1,
        Side::Buy,
        10,
        Pricing::Limit(Px::from_cents(1000)),
        Tif::Ioc,
    ));
    b.on_event(&trade(2, 995));
    assert_eq!(
        b.fills()[0].px,
        Px::from_cents(995),
        "the ask, not the limit"
    );
    assert_eq!(b.fills()[0].slippage, -Px::from_cents(5).raw());
}

#[test]
fn the_order_meets_the_market_as_it_was_on_arrival_not_when_decided() {
    let mut b = broker(50, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 500));
    b.submit(&intent(
        0,
        10 * MS,
        Side::Buy,
        100,
        collar(1000, 20),
        Tif::Ioc,
    )); // arrives at 60 ms
    b.on_event(&quote(30 * MS, 1030, 1035, 500, 500)); // the ask runs away in flight: 10.35 > 10.20
    b.on_event(&quote(70 * MS, 1000, 1001, 500, 500)); // too late
    assert!(b.fills().is_empty());
    assert_eq!(
        states(&mut b),
        [(OrderState::Accepted, 0), (OrderState::Expired, 0)]
    );
}

#[test]
fn a_quote_arriving_at_the_arrival_instant_is_not_yet_visible() {
    let mut b = broker(50, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 500));
    b.submit(&intent(0, 0, Side::Buy, 100, collar(1000, 20), Tif::Ioc)); // arrives at 50 ms
    b.on_event(&quote(50 * MS, 1000, 1500, 500, 500)); // same instant: after the order
    assert_eq!(b.fills()[0].px, Px::from_cents(1005));
}

#[test]
fn fills_are_limited_to_the_size_shown_and_ioc_expires_the_rest() {
    let mut b = broker(0, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 60));
    b.submit(&intent(0, 1, Side::Buy, 100, collar(1000, 20), Tif::Ioc));
    b.on_event(&trade(2, 1005));
    assert_eq!(b.fills()[0].qty, 60);
    assert_eq!(
        states(&mut b),
        [
            (OrderState::Accepted, 0),
            (OrderState::PartiallyFilled, 60),
            (OrderState::Expired, 60)
        ]
    );
    assert_eq!(b.position(0), 60);
}

#[test]
fn a_quotes_size_is_shared_by_every_order_and_used_once() {
    let mut b = broker(0, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 150));
    for s in 0..2 {
        b.submit(&intent(s, 1, Side::Buy, 100, collar(1000, 20), Tif::Ioc));
    }
    b.on_event(&trade(2, 1005));
    let q: Vec<u32> = b.fills().iter().map(|f| f.qty).collect();
    assert_eq!(q, [100, 50], "oldest first, then what is left");
    b.submit(&intent(2, 3, Side::Buy, 100, collar(1000, 20), Tif::Ioc));
    b.on_event(&trade(4, 1005));
    assert_eq!(
        b.fills().len(),
        2,
        "the quote is used up; the next order gets nothing"
    );
}

#[test]
fn a_day_order_rests_and_fills_when_a_later_quote_crosses() {
    let mut b = broker(0, 0);
    b.on_event(&quote(0, 1000, 1010, 500, 500));
    b.submit(&intent(
        0,
        1,
        Side::Buy,
        100,
        Pricing::Limit(Px::from_cents(1002)),
        Tif::Day,
    ));
    b.on_event(&trade(2, 1010));
    assert!(b.fills().is_empty());
    assert_eq!(b.open_orders(), 1);
    b.on_event(&quote(3, 998, 1002, 500, 40));
    b.on_event(&quote(4, 998, 1001, 500, 500));
    let fills: Vec<(u32, i64)> = b.fills().iter().map(|f| (f.qty, f.px.to_cents())).collect();
    assert_eq!(fills, [(40, 1002), (60, 1001)]);
    assert_eq!(b.open_orders(), 0);
}

#[test]
fn sells_hit_the_bid_and_a_short_sale_makes_the_position_negative() {
    let mut b = broker(0, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 500));
    // Sell at no less than 9.90.
    b.submit(&intent(
        0,
        1,
        Side::SellShort,
        100,
        Pricing::Limit(Px::from_cents(990)),
        Tif::Ioc,
    ));
    // Wants 10.20 or better: the bid is worse.
    b.submit(&intent(
        1,
        1,
        Side::SellShort,
        100,
        Pricing::Limit(Px::from_cents(1020)),
        Tif::Ioc,
    ));
    b.on_event(&trade(2, 1000));
    assert_eq!(b.fills().len(), 1);
    assert_eq!(
        (b.fills()[0].px, b.fills()[0].qty),
        (Px::from_cents(1000), 100)
    );
    assert_eq!(
        b.fills()[0].slippage,
        Px::from_cents(-10).raw(),
        "better than the 9.90 reference"
    );
    assert_eq!(b.position(0), -100);
}

#[test]
fn nothing_fills_while_halted_and_resting_orders_fill_on_resume() {
    let mut b = broker(0, 0);
    b.on_event(&quote(0, 1000, 1005, 500, 500));
    b.on_event(&status(1, StatusKind::TradingHalt));
    b.submit(&intent(0, 2, Side::Buy, 100, collar(1000, 20), Tif::Day));
    b.on_event(&quote(3, 1000, 1005, 500, 500));
    assert!(b.fills().is_empty());
    b.on_event(&status(4, StatusKind::TradingResume));
    assert_eq!(b.fills()[0].ts, 4);
}

#[test]
fn nothing_fills_before_there_is_a_quote_and_cancel_and_close_of_day_work() {
    let mut b = broker(10, 0);
    b.submit(&intent(0, 0, Side::Buy, 100, collar(1000, 20), Tif::Day));
    b.submit(&intent(1, 0, Side::Buy, 100, collar(1000, 20), Tif::Day));
    b.submit(&intent(2, 0, Side::Buy, 100, collar(1000, 20), Tif::Day));
    b.on_event(&trade(20 * MS, 1000));
    assert!(b.fills().is_empty());
    let id = |seq| IntentId {
        strategy: StrategyId(1),
        seq,
    };
    assert!(b.cancel(id(1), 21 * MS));
    assert!(!b.cancel(id(1), 22 * MS), "already finished");
    b.end_of_day(30 * MS);
    let mut s = states(&mut b);
    s.sort_by_key(|x| x.0 as u8);
    assert_eq!(s.iter().filter(|x| x.0 == OrderState::Cancelled).count(), 1);
    assert_eq!(s.iter().filter(|x| x.0 == OrderState::Expired).count(), 2);
    assert_eq!(b.open_orders(), 0);
}

#[test]
fn an_order_for_an_unknown_instrument_is_rejected() {
    let mut b = broker(0, 0);
    let mut i = intent(0, 5, Side::Buy, 10, collar(1000, 20), Tif::Ioc);
    i.instrument = 9;
    b.submit(&i);
    let u = b.drain_updates();
    assert_eq!(
        (u[0].state, u[0].reject, u[0].order),
        (OrderState::Rejected, Some(RejectReason::Broker), None)
    );
}

#[test]
fn borrow_cost_accrues_on_shorts_only_for_as_long_as_they_are_held() {
    // 1,000 shares short at $10, 10% a year, held exactly one day.
    let mut b = broker(0, 1000);
    b.on_event(&quote(0, 1000, 1001, 5000, 5000));
    b.on_event(&trade(0, 1000));
    b.submit(&intent(
        0,
        0,
        Side::SellShort,
        1000,
        Pricing::Limit(Px::from_cents(1000)),
        Tif::Ioc,
    ));
    b.on_event(&trade(1, 1000));
    assert_eq!(b.position(0), -1000);
    b.on_event(&trade(DAY, 1000));
    // 1,000 x $10 x 10% / 365 = $2.7397260..., rounded up (checked independently in Python).
    // The position was open from ns 0 to ns DAY, but the fill was at 0: one full day.
    assert_eq!(b.borrow_fee(0), 2_739_726_028);

    // Cover: the fee stops growing.
    b.submit(&intent(
        1,
        DAY,
        Side::Buy,
        1000,
        Pricing::Limit(Px::from_cents(1001)),
        Tif::Ioc,
    ));
    b.on_event(&trade(DAY + 1, 1000));
    assert_eq!(b.position(0), 0);
    let fee = b.borrow_fee(0);
    b.on_event(&trade(10 * DAY, 1000));
    assert_eq!(b.borrow_fee(0), fee);
}

#[test]
fn longs_pay_no_borrow_and_zero_rate_pays_none() {
    let mut b = broker(0, 1000);
    b.on_event(&quote(0, 1000, 1001, 5000, 5000));
    b.submit(&intent(0, 0, Side::Buy, 1000, collar(1001, 20), Tif::Ioc));
    b.on_event(&trade(DAY, 1001));
    assert_eq!(b.position(0), 1000);
    assert_eq!(b.borrow_fee(0), 0);

    let mut z = broker(0, 0);
    z.on_event(&quote(0, 1000, 1001, 5000, 5000));
    z.submit(&intent(
        0,
        0,
        Side::SellShort,
        1000,
        Pricing::Limit(Px::from_cents(1000)),
        Tif::Ioc,
    ));
    z.on_event(&trade(DAY, 1000));
    assert_eq!(z.position(0), -1000);
    assert_eq!(z.borrow_fee(0), 0);
}

#[test]
fn the_borrow_fee_follows_the_price_while_short() {
    // Price doubles after half a day: the second half costs twice as much per day.
    let mut b = broker(0, 1000);
    b.on_event(&quote(0, 1000, 1001, 5000, 5000));
    b.on_event(&trade(0, 1000));
    b.submit(&intent(
        0,
        0,
        Side::SellShort,
        1000,
        Pricing::Limit(Px::from_cents(1000)),
        Tif::Ioc,
    ));
    b.on_event(&trade(DAY / 2, 2000));
    b.on_event(&trade(DAY, 2000));
    // 1.5 days' worth at $10: 2,739,726,027.4 x 1.5 -> but the first half at $10, the second at $20:
    // (0.5 + 1.0) x 2,739,726,027.39 = 4,109,589,041.1, rounded up.
    assert_eq!(b.borrow_fee(0), 4_109_589_042);
}

// ---- with a strategy ----

struct OneShot {
    sent: bool,
    seen: Vec<(OrderState, u32)>,
}

impl Strategy for OneShot {
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }
    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
        if !self.sent && matches!(ev, Event::Quote(_)) {
            self.sent = true;
            let px = Px::from_cents(1000);
            ctx.submit(
                0,
                Request {
                    side: Side::Buy,
                    qty: 100,
                    purpose: Purpose::Open,
                    pricing: Pricing::Collar {
                        reference: px,
                        collar_permille: 20,
                    },
                    protect: Some(Protective {
                        stop_trigger: Px::from_cents(900),
                        stop_limit: None,
                        take_profit: None,
                    }),
                    tif: Tif::Ioc,
                    reason: 0,
                },
            )
            .unwrap();
        }
    }
    fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {}
    fn on_order_update(&mut self, _: &mut Ctx<'_>, u: &crate::OrderUpdate) {
        self.seen.push((u.state, u.filled_qty));
    }
}

fn backtest() -> (Vec<Intent>, Vec<crate::Fill>, Vec<(OrderState, u32)>) {
    let events = [
        quote(0, 1000, 1005, 500, 500),
        trade(10 * MS, 1005),
        quote(100 * MS, 1000, 1005, 500, 500),
    ];
    let mut host = Host::new(
        OneShot {
            sent: false,
            seen: Vec::new(),
        },
        2,
    );
    let mut b = broker(50, 0);
    let intents = run_backtest(&mut host, &mut b, events);
    (intents, b.fills().to_vec(), host.strategy().seen.clone())
}

#[test]
fn a_strategy_trades_against_the_sim_and_hears_back_in_event_order() {
    let (intents, fills, seen) = backtest();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].ts, 0);
    assert_eq!(
        (fills.len(), fills[0].ts),
        (1, 50 * MS),
        "arrived 50 ms after it was decided"
    );
    // The update reaches the strategy when the next event after it arrives (the 100 ms quote).
    assert_eq!(seen, [(OrderState::Accepted, 0), (OrderState::Filled, 100)]);
}

#[test]
fn the_same_stream_gives_the_same_fills() {
    assert_eq!(backtest(), backtest());
}

// ---- protective orders (E19-S05) ----

mod legs {
    use super::*;
    use crate::OrderId;
    use crate::broker::{Broker, Kind, Leg, Submission, check_events};
    use crate::testing::{BracketScenario, T0, bracket_scenarios};

    fn play(s: &BracketScenario, protective: bool) -> (SimBroker, Vec<crate::broker::BrokerEvent>) {
        let mut b = SimBroker::new(
            SimConfig {
                latency_ns: 0,
                borrow_bps_per_year: 0,
            },
            1,
        );
        if protective {
            b = b.with_protective_orders();
        }
        assert_eq!(b.place(&s.intent, OrderId(7)), Submission::Accepted);
        for e in &s.market {
            b.observe(e);
        }
        let events = b.take_events();
        (b, events)
    }

    #[test]
    fn every_scripted_scenario_gives_exactly_the_expected_events_and_a_lawful_sequence() {
        for s in bracket_scenarios() {
            let (b, events) = play(&s, true);
            let kinds: Vec<Kind> = events.iter().map(|e| e.kind).collect();
            assert_eq!(kinds, s.expected, "{}", s.name);
            assert!(events.iter().all(|e| e.order == OrderId(7)));
            check_events(&events, |_| Some(s.intent.qty))
                .unwrap_or_else(|f| panic!("{}: {f:?}", s.name));
            assert_eq!(b.position(0), s.position, "{}", s.name);
            // The position the fills add up to is the position.
            let net: i64 = b
                .fills()
                .iter()
                .map(|f| {
                    if f.side.is_buy() {
                        i64::from(f.qty)
                    } else {
                        -i64::from(f.qty)
                    }
                })
                .sum();
            assert_eq!(net, s.position, "{}", s.name);
        }
    }

    #[test]
    fn without_asking_for_them_protective_orders_do_nothing() {
        // The same market, legs off: the entry fills and the position stays, whatever the price does.
        for s in bracket_scenarios() {
            let (b, events) = play(&s, false);
            let kinds: Vec<Kind> = events.iter().map(|e| e.kind).collect();
            let entry: Vec<Kind> = s
                .expected
                .iter()
                .copied()
                .filter(|k| !matches!(k, Kind::LegFill { .. }))
                .collect();
            assert_eq!(kinds, entry, "{}", s.name);
            let filled: i64 = entry
                .iter()
                .map(|k| match k {
                    Kind::Fill { qty, .. } => i64::from(*qty),
                    _ => 0,
                })
                .sum();
            assert_eq!(b.position(0).abs(), filled);
        }
    }

    #[test]
    fn a_leg_fill_is_a_fill_with_its_leg_and_the_slippage_against_its_trigger() {
        let s = &bracket_scenarios()[0];
        let (b, _) = play(s, true);
        let f = b.fills();
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].leg, f[0].side), (None, Side::Buy));
        let l = f[1];
        assert_eq!(
            (l.leg, l.side, l.qty, l.px, l.intent, l.order),
            (
                Some(Leg::Stop),
                Side::Sell,
                100,
                Px::from_cents(940),
                s.intent.id,
                OrderId(7)
            )
        );
        // Stopped at 9.40 against a 9.50 trigger: 0.10 a share worse.
        assert_eq!(l.slippage, Px::from_cents(10).raw());
        assert_eq!(l.ts, T0 + 4 * 1_000_000_000);
        // A target that fills better than its price improves on it: sold at 11.02 against 11.00.
        let (b, _) = play(&bracket_scenarios()[1], true);
        assert_eq!(b.fills()[1].slippage, -Px::from_cents(2).raw());
    }

    fn sim(latency: u64) -> SimBroker {
        SimBroker::new(
            SimConfig {
                latency_ns: latency,
                borrow_bps_per_year: 0,
            },
            1,
        )
        .with_protective_orders()
    }

    fn halt(sec: u64, kind: tf_core::StatusKind) -> Event {
        Event::Status(tf_core::Status {
            hdr: Header {
                ts_event: T0 + sec * 1_000_000_000,
                ts_recv: T0 + sec * 1_000_000_000,
                seq: sec,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            kind,
            lo: Px::ZERO,
            hi: Px::ZERO,
        })
    }

    #[test]
    fn nothing_fills_while_halted_and_the_stop_waits_for_the_resume() {
        let s = &bracket_scenarios()[0];
        let mut b = sim(0);
        b.place(&s.intent, OrderId(7));
        // Entry, then a halt; the gap and the trade through the stop happen in the halt's shadow (a trade report
        // during a halt is a late print), and the stop fills only once trading resumes.
        b.observe(&s.market[0]);
        b.observe(&halt(2, tf_core::StatusKind::TradingHalt));
        b.observe(&s.market[1]);
        b.observe(&s.market[2]);
        assert_eq!(b.position(0), 100, "halted: no exit");
        b.observe(&halt(5, tf_core::StatusKind::TradingResume));
        assert_eq!(b.position(0), 0);
        let kinds: Vec<Kind> = b.take_events().iter().map(|e| e.kind).collect();
        assert_eq!(kinds, s.expected);
    }

    #[test]
    fn a_leg_never_takes_the_position_past_flat_and_legs_end_with_the_day() {
        let s = &bracket_scenarios()[0];
        let mut b = sim(0);
        b.place(&s.intent, OrderId(7));
        b.observe(&s.market[0]);
        assert_eq!(b.position(0), 100);
        // Another order sells the whole position (the broker would refuse it while the legs stand; here it
        // happens): the legs then have nothing to sell and a stop through the market must not go short.
        let mut sell = s.intent;
        sell.id.seq = 2;
        sell.side = Side::Sell;
        sell.purpose = Purpose::Close;
        sell.protect = None;
        sell.pricing = Pricing::Limit(Px::from_cents(900));
        b.place(&sell, OrderId(8));
        b.observe(&quote_at(2, 999, 1000));
        assert_eq!(b.position(0), 0);
        b.observe(&quote_at(3, 940, 941));
        b.observe(&trade_at(4, 941));
        assert_eq!(b.position(0), 0, "the stop found nothing to sell");
        // A fresh position's legs end with the day: after it, a trade through the stop does nothing.
        let mut b = sim(0);
        b.place(&s.intent, OrderId(7));
        b.observe(&s.market[0]);
        b.end_of_day(T0 + 2 * 1_000_000_000);
        b.observe(&s.market[1]);
        b.observe(&s.market[2]);
        assert_eq!(b.position(0), 100);
    }

    fn quote_at(sec: u64, bid: i64, ask: i64) -> Event {
        Event::Quote(tf_core::Quote {
            hdr: Header {
                ts_event: T0 + sec * 1_000_000_000,
                ts_recv: T0 + sec * 1_000_000_000,
                seq: sec,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            bid_px: Px::from_cents(bid),
            ask_px: Px::from_cents(ask),
            bid_sz: 500,
            ask_sz: 500,
        })
    }

    fn trade_at(sec: u64, px: i64) -> Event {
        Event::Trade(tf_core::Trade {
            hdr: Header {
                ts_event: T0 + sec * 1_000_000_000,
                ts_recv: T0 + sec * 1_000_000_000,
                seq: sec,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(px),
            size: 10,
            flags: tf_core::TradeFlags::NONE,
        })
    }

    #[test]
    fn latency_delays_the_entry_and_the_legs_follow_it() {
        let s = &bracket_scenarios()[0];
        let mut b = sim(2 * 1_000_000_000);
        b.place(&s.intent, OrderId(7));
        // The entry reaches the venue at +2 s, so the +1 s quote is not what it sees... it sees the +3 s one.
        for e in &s.market {
            b.observe(e);
        }
        let kinds: Vec<Kind> = b.take_events().iter().map(|e| e.kind).collect();
        // At +2 s the book is 9.99/10.00 (from +1 s): the order fills on arrival, then the gap and the stop.
        assert_eq!(kinds, s.expected);
        assert_eq!(b.fills()[0].ts, T0 + 2 * 1_000_000_000);
    }

    // ---- extended hours ----

    /// A copy of the first scenario's intent decided `hours` from the regular-session instant `T0` (11:00 New York).
    fn at(hours: i64, mut i: crate::Intent) -> crate::Intent {
        i.ts = (T0 as i64 + hours * 3_600 * 1_000_000_000) as u64;
        i
    }

    #[test]
    fn extended_hours_orders_must_be_plain_limit_orders_day_or_good_til_cancelled() {
        use crate::session_rules::ExtendedHoursRefusal as R;
        let bracket = bracket_scenarios()[0].intent;
        let mut plain = bracket;
        plain.protect = None;
        plain.purpose = Purpose::Open;
        let mut ioc = plain;
        ioc.tif = crate::Tif::Ioc;
        let mut gtc = plain;
        gtc.tif = crate::Tif::Gtc;
        let refused = |i: crate::Intent| match sim(0).place(&i, OrderId(1)) {
            Submission::Refused { code, message } => Some((code, message)),
            Submission::Accepted => None,
            o => panic!("{o:?}"),
        };
        // 08:00 (premarket) and 17:00 New York time (after-hours); 11:00 is the regular session.
        for hours in [-3, 6] {
            let (code, message) = refused(at(hours, bracket)).expect("a bracket is refused");
            assert_eq!(
                (code, message.as_str()),
                (422, R::ProtectiveOrders.message()),
                "{hours}"
            );
            let (code, message) = refused(at(hours, ioc)).expect("an IOC is refused");
            assert_eq!(
                (code, message.as_str()),
                (422, R::TimeInForce.message()),
                "{hours}"
            );
            assert!(
                refused(at(hours, plain)).is_none(),
                "a day limit order is fine at {hours}"
            );
            assert!(
                refused(at(hours, gtc)).is_none(),
                "and so is a good-til-cancelled one"
            );
        }
        // The same orders are all accepted in the regular session.
        for i in [bracket, plain, ioc, gtc] {
            assert!(refused(at(0, i)).is_none());
        }
        // The boundaries: 09:29:59 is the premarket, 09:30:00 the regular session; 15:59:59 and 16:00:00 likewise.
        let open = tf_calendar::Calendar::us_equities()
            .times(tf_calendar::Date::new(2026, 10, 2).unwrap())
            .unwrap()
            .unwrap();
        let mut i = bracket;
        i.ts = open.open - 1;
        assert!(refused(i).is_some());
        i.ts = open.open;
        assert!(refused(i).is_none());
        i.ts = open.close - 1;
        assert!(refused(i).is_none());
        i.ts = open.close;
        assert!(refused(i).is_some());
        i.ts = open.after_hours_end;
        assert!(
            refused(i).is_none(),
            "after 20:00 the market is closed, not extended hours"
        );
        // An instant the calendar cannot place is the regular session (and a Saturday is no session at all).
        i.ts = 100 * 1_000_000_000;
        assert!(refused(i).is_none());
        i.ts = (tf_calendar::Date::new(2026, 10, 3).unwrap().days() as u64 * 86_400 + 12 * 3600)
            * 1_000_000_000;
        assert!(refused(i).is_none());
    }

    #[test]
    fn the_old_submit_path_refuses_the_same_orders_and_a_good_til_cancelled_one_outlives_the_day() {
        let bracket = bracket_scenarios()[0].intent;
        let mut b = sim(0);
        b.submit(&at(-3, bracket));
        let u = b.drain_updates();
        assert_eq!(u.len(), 1);
        assert_eq!(
            (u[0].state, u[0].reject),
            (OrderState::Rejected, Some(crate::RejectReason::Broker))
        );
        assert_eq!(b.open_orders(), 0);
        // A day order resting far from the market expires at the end of the day; a good-til-cancelled one stays.
        let mut rest = bracket;
        rest.protect = None;
        rest.pricing = Pricing::Limit(Px::from_cents(100));
        let mut gtc = rest;
        gtc.id.seq = 2;
        gtc.tif = crate::Tif::Gtc;
        let mut b = sim(0);
        b.submit(&rest);
        b.submit(&gtc);
        b.observe(&quote_at(1, 999, 1000));
        assert_eq!(b.open_orders(), 2);
        b.end_of_day(T0 + 2 * 1_000_000_000);
        assert_eq!(
            b.open_orders(),
            1,
            "the good-til-cancelled order is still working"
        );
        let states: Vec<_> = b
            .drain_updates()
            .iter()
            .map(|u| (u.intent.seq, u.state))
            .collect();
        assert!(states.contains(&(1, OrderState::Expired)));
        assert!(!states.contains(&(2, OrderState::Expired)));
    }

    fn after_entry(i: crate::Intent, bid_ask: (i64, i64)) -> SimBroker {
        let mut b = sim(0);
        b.place(&i, OrderId(7));
        b.observe(&quote_at(1, bid_ask.0, bid_ask.1));
        assert_eq!(b.position(0).abs(), 100, "the entry filled");
        b.take_events();
        b
    }

    fn legs_of(b: &mut SimBroker) -> Vec<(Leg, u32, Px)> {
        b.take_events()
            .iter()
            .filter_map(|e| match e.kind {
                Kind::LegFill { leg, qty, px } => Some((leg, qty, px)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_trade_exactly_at_the_trigger_arms_the_stop_for_a_long_and_for_a_short() {
        let long = bracket_scenarios()[0].intent; // buy at 10.00, stop 9.50
        let mut b = after_entry(long, (999, 1000));
        b.observe(&quote_at(2, 940, 941));
        b.observe(&trade_at(3, 951));
        assert!(legs_of(&mut b).is_empty(), "one tick above the trigger");
        b.observe(&trade_at(4, 950));
        assert_eq!(legs_of(&mut b), [(Leg::Stop, 100, Px::from_cents(940))]);
        let short = bracket_scenarios()[4].intent; // sell short at 10.00, stop 10.50
        let mut b = after_entry(short, (1000, 1001));
        b.observe(&quote_at(2, 1059, 1060));
        b.observe(&trade_at(3, 1049));
        assert!(legs_of(&mut b).is_empty());
        b.observe(&trade_at(4, 1050));
        assert_eq!(legs_of(&mut b), [(Leg::Stop, 100, Px::from_cents(1060))]);
        // The buy at 10.60 against a 10.50 trigger is 0.10 worse a share.
        assert_eq!(b.fills().last().unwrap().slippage, Px::from_cents(10).raw());
    }

    #[test]
    fn a_target_fills_at_exactly_its_price_for_a_long_and_a_short_and_not_a_tick_short() {
        let long = bracket_scenarios()[0].intent; // target 11.00
        let mut b = after_entry(long, (999, 1000));
        b.observe(&quote_at(2, 1099, 1100));
        assert!(legs_of(&mut b).is_empty());
        b.observe(&quote_at(3, 1100, 1101));
        assert_eq!(legs_of(&mut b), [(Leg::Target, 100, Px::from_cents(1100))]);
        let short = bracket_scenarios()[4].intent; // target 9.00
        let mut b = after_entry(short, (1000, 1001));
        b.observe(&quote_at(2, 900, 901));
        assert!(legs_of(&mut b).is_empty());
        b.observe(&quote_at(3, 899, 900));
        assert_eq!(legs_of(&mut b), [(Leg::Target, 100, Px::from_cents(900))]);
    }

    #[test]
    fn a_triggered_stop_waits_for_a_bid_and_a_short_stop_limit_waits_for_its_price() {
        let long = bracket_scenarios()[0].intent;
        let mut b = after_entry(long, (999, 1000));
        // The stop is armed with no bid in the book: nothing to sell to.
        b.observe(&quote_at(2, 0, 941));
        b.observe(&trade_at(3, 945));
        assert!(legs_of(&mut b).is_empty());
        assert_eq!(b.position(0), 100);
        b.observe(&quote_at(4, 940, 941));
        assert_eq!(legs_of(&mut b), [(Leg::Stop, 100, Px::from_cents(940))]);
        // A short's stop-limit: trigger 10.50, limit 10.60. At 10.65 it does not fill; at 10.60 exactly it does.
        let mut short = bracket_scenarios()[4].intent;
        short.protect = Some(crate::Protective {
            stop_limit: Some(Px::from_cents(1060)),
            ..short.protect.unwrap()
        });
        let mut b = after_entry(short, (1000, 1001));
        b.observe(&quote_at(2, 1064, 1065));
        b.observe(&trade_at(3, 1055));
        assert!(legs_of(&mut b).is_empty());
        b.observe(&quote_at(4, 1059, 1060));
        assert_eq!(legs_of(&mut b), [(Leg::Stop, 100, Px::from_cents(1060))]);
        assert_eq!(b.position(0), 0);
    }

    #[test]
    fn a_trade_in_another_instrument_does_not_arm_this_ones_stop() {
        let long = bracket_scenarios()[0].intent;
        let mut b = SimBroker::new(
            SimConfig {
                latency_ns: 0,
                borrow_bps_per_year: 0,
            },
            2,
        )
        .with_protective_orders();
        b.place(&long, OrderId(7));
        b.observe(&quote_at(1, 999, 1000));
        b.take_events();
        // A trade of instrument 1 at a price through instrument 0's stop.
        let other = Event::Trade(tf_core::Trade {
            hdr: Header {
                ts_event: T0 + 2 * 1_000_000_000,
                ts_recv: T0 + 2 * 1_000_000_000,
                seq: 2,
                instrument: 1,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(900),
            size: 10,
            flags: tf_core::TradeFlags::NONE,
        });
        b.observe(&other);
        b.observe(&quote_at(3, 940, 941));
        b.observe(&trade_at(3, 960));
        assert_eq!(
            b.position(0),
            100,
            "instrument 0's own trade (9.60) is above its stop"
        );
    }

    fn open_more(b: &mut SimBroker, seq: u64, qty: u32) {
        // An opening order with no protective orders (the simulator does not validate): 50 more shares.
        let mut i = bracket_scenarios()[0].intent;
        i.id.seq = seq;
        i.qty = qty;
        i.protect = None;
        b.place(&i, OrderId(seq));
    }

    #[test]
    fn the_legs_sell_only_what_they_protect_and_only_what_is_held() {
        let long = bracket_scenarios()[0].intent;
        // More shares are held than the legs protect: 150 held, 100 protected. The stop sells 100.
        let mut b = after_entry(long, (999, 1000));
        open_more(&mut b, 2, 50);
        b.observe(&quote_at(2, 999, 1000));
        assert_eq!(b.position(0), 150);
        b.observe(&quote_at(3, 940, 941));
        b.observe(&trade_at(3, 945));
        assert_eq!(legs_of(&mut b), [(Leg::Stop, 100, Px::from_cents(940))]);
        assert_eq!(b.position(0), 50, "the 50 nobody protected are still held");
        // The legs' own fills count against what they protect: a target that takes 30 first leaves 70 for the rest
        // (not 120, though 120 shares are held).
        let mut b = after_entry(long, (999, 1000));
        open_more(&mut b, 2, 50);
        b.observe(&quote_at(2, 999, 1000));
        let mut q = quote_at(3, 1105, 1106);
        if let Event::Quote(x) = &mut q {
            x.bid_sz = 30;
        }
        b.observe(&q);
        b.observe(&quote_at(4, 1105, 1106));
        assert_eq!(
            legs_of(&mut b),
            [
                (Leg::Target, 30, Px::from_cents(1105)),
                (Leg::Target, 70, Px::from_cents(1105))
            ]
        );
        assert_eq!(b.position(0), 50);
        // Fewer shares are held than protected (another order sold 40): the stop sells the 60 that are left.
        let mut b = after_entry(long, (999, 1000));
        let mut sell = long;
        sell.id.seq = 3;
        sell.side = Side::Sell;
        sell.purpose = Purpose::Close;
        sell.protect = None;
        sell.qty = 40;
        sell.pricing = Pricing::Limit(Px::from_cents(900));
        b.place(&sell, OrderId(8));
        b.observe(&quote_at(2, 999, 1000));
        assert_eq!(b.position(0), 60);
        b.observe(&quote_at(3, 940, 941));
        b.observe(&trade_at(3, 945));
        assert_eq!(legs_of(&mut b), [(Leg::Stop, 60, Px::from_cents(940))]);
        assert_eq!(b.position(0), 0);
    }

    #[test]
    fn two_protected_orders_share_what_one_quote_shows() {
        // Two entries of 60 shares each, both with a target at 11.00; the bid shows 100 shares at 11.05: the targets
        // take 100 between them (60 and 40), not 120.
        let mut a = bracket_scenarios()[0].intent;
        a.qty = 60;
        let mut c = a;
        c.id.seq = 2;
        let mut b = sim(0);
        b.place(&a, OrderId(7));
        b.place(&c, OrderId(8));
        b.observe(&quote_at(1, 999, 1000));
        assert_eq!(b.position(0), 120);
        b.take_events();
        let mut q = quote_at(2, 1105, 1106);
        if let Event::Quote(x) = &mut q {
            x.bid_sz = 100;
        }
        b.observe(&q);
        let got: Vec<u32> = legs_of(&mut b).iter().map(|l| l.1).collect();
        assert_eq!(got, [60, 40]);
        assert_eq!(b.position(0), 20);
    }
}

// ---- short sales (E19-S06) ----

mod shorts {
    use super::*;
    use crate::OrderId;
    use crate::broker::{Broker, Submission};
    use crate::sim::{Borrow, SHORT_REFUSED_CODE};
    use crate::testing::{T0, bracket_scenarios};
    use tf_core::{Status, StatusKind};

    const S: u64 = 1_000_000_000;

    /// A short sale of 100 at 10.00 with the stop and target of the scenarios.
    fn short() -> crate::Intent {
        bracket_scenarios()[4].intent
    }

    fn q(sec: u64, bid: i64, ask: i64) -> Event {
        Event::Quote(tf_core::Quote {
            hdr: Header {
                ts_event: T0 + sec * S,
                ts_recv: T0 + sec * S,
                seq: sec,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            bid_px: Px::from_cents(bid),
            ask_px: Px::from_cents(ask),
            bid_sz: 500,
            ask_sz: 500,
        })
    }

    fn status(sec: u64, kind: StatusKind) -> Event {
        Event::Status(Status {
            hdr: Header {
                ts_event: T0 + sec * S,
                ts_recv: T0 + sec * S,
                seq: sec,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            kind,
            lo: Px::ZERO,
            hi: Px::ZERO,
        })
    }

    fn broker(bps: u32) -> SimBroker {
        SimBroker::new(
            SimConfig {
                latency_ns: 0,
                borrow_bps_per_year: bps,
            },
            1,
        )
    }

    fn long_sell() -> crate::Intent {
        let mut i = short();
        i.side = Side::Sell;
        i.purpose = Purpose::Close;
        i.protect = None;
        i
    }

    #[test]
    fn while_the_restriction_lasts_a_short_sale_does_not_fill_and_it_fills_when_it_is_lifted() {
        let mut b = broker(0);
        b.observe(&q(0, 999, 1000));
        b.observe(&status(1, StatusKind::ShortSaleRestriction));
        let mut i = short();
        i.ts = T0 + 2 * S;
        assert_eq!(b.place(&i, OrderId(7)), Submission::Accepted);
        // The bid is at the limit the sale asks for: with no restriction it would fill at the bid. It does not.
        b.observe(&q(3, 1000, 1001));
        b.observe(&q(4, 1010, 1011));
        assert_eq!(b.position(0), 0);
        assert_eq!(b.open_orders(), 1, "the order rests");
        // The restriction ends: the order is tried against the market as it stands, and fills at the bid.
        b.observe(&status(5, StatusKind::ShortSaleRestrictionLifted));
        assert_eq!(b.position(0), -100);
        assert_eq!(b.fills()[0].px, Px::from_cents(1010));
        // And it can start again: a second short is held back.
        b.observe(&status(6, StatusKind::ShortSaleRestriction));
        let mut j = short();
        j.id.seq = 2;
        j.ts = T0 + 7 * S;
        b.place(&j, OrderId(8));
        b.observe(&q(8, 1010, 1011));
        assert_eq!(b.position(0), -100);
        // An immediate-or-cancel short sale under the restriction expires unfilled.
        let mut k = short();
        k.id.seq = 3;
        k.tif = crate::Tif::Ioc;
        k.ts = T0 + 9 * S;
        b.place(&k, OrderId(9));
        b.observe(&q(10, 1010, 1011));
        assert!(b.take_events().iter().any(|e| e.order == OrderId(9)
            && matches!(e.kind, crate::broker::Kind::Close(OrderState::Expired))));
    }

    #[test]
    fn the_restriction_is_per_instrument_and_does_not_hold_back_a_sale_of_shares_held() {
        let mut b = SimBroker::new(
            SimConfig {
                latency_ns: 0,
                borrow_bps_per_year: 0,
            },
            2,
        );
        // Long 100 in instrument 0 first, then the restriction starts on instrument 0 only.
        let mut buy = short();
        buy.side = Side::Buy;
        buy.pricing = Pricing::Limit(Px::from_cents(1002));
        b.place(&buy, OrderId(1));
        b.observe(&q(1, 999, 1000));
        assert_eq!(b.position(0), 100);
        b.observe(&status(2, StatusKind::ShortSaleRestriction));
        // Selling what is held is not a short sale: it fills.
        let mut sell = long_sell();
        sell.id.seq = 5;
        sell.ts = T0 + 3 * S;
        sell.pricing = Pricing::Limit(Px::from_cents(990));
        b.place(&sell, OrderId(2));
        b.observe(&q(4, 999, 1000));
        assert_eq!(b.position(0), 0);
        // A short sale in instrument 1, which has no restriction, fills.
        let mut other = short();
        other.id.seq = 6;
        other.instrument = 1;
        other.ts = T0 + 5 * S;
        b.place(&other, OrderId(3));
        let mut q1 = q(6, 1000, 1001);
        if let Event::Quote(x) = &mut q1 {
            x.hdr.instrument = 1;
        }
        b.observe(&q1);
        assert_eq!(b.position(1), -100);
    }

    #[test]
    fn a_short_sale_of_a_name_that_is_not_easy_to_borrow_is_refused_as_the_broker_refuses_it() {
        let table = vec![Borrow::Hard];
        for (kind, why) in [
            (Borrow::Hard, "hard to borrow"),
            (Borrow::NotShortable, "cannot be sold short"),
            (Borrow::Unknown, "is not known"),
        ] {
            let mut b = broker(0).with_borrow_table(vec![kind]);
            match b.place(&short(), OrderId(7)) {
                Submission::Refused { code, message } => {
                    assert_eq!(code, SHORT_REFUSED_CODE);
                    assert_eq!(code, 403, "the status the broker answers with");
                    assert!(message.contains(why), "{message}");
                }
                o => panic!("{kind:?}: {o:?}"),
            }
            assert_eq!(b.open_orders(), 0);
            // The old path refuses it too, as a rejection.
            let mut b = broker(0).with_borrow_table(vec![kind]);
            b.submit(&short());
            assert_eq!(b.drain_updates()[0].state, OrderState::Rejected);
        }
        let _ = table;
        // An easy-to-borrow name is taken; a sale of shares held and a purchase are never asked about borrowing.
        let mut b = broker(0).with_borrow_table(vec![Borrow::Easy]);
        assert_eq!(b.place(&short(), OrderId(7)), Submission::Accepted);
        let mut b = broker(0).with_borrow_table(vec![Borrow::Hard]);
        let mut buy = short();
        buy.side = Side::Buy;
        assert_eq!(b.place(&buy, OrderId(7)), Submission::Accepted);
        assert_eq!(b.place(&long_sell(), OrderId(8)), Submission::Accepted);
        // An instrument the table does not reach is not known.
        let mut b = SimBroker::new(
            SimConfig {
                latency_ns: 0,
                borrow_bps_per_year: 0,
            },
            3,
        )
        .with_borrow_table(vec![Borrow::Easy]);
        let mut i = short();
        i.instrument = 2;
        assert!(matches!(
            b.place(&i, OrderId(1)),
            Submission::Refused { .. }
        ));
        // Without the rules, nothing about borrowing is asked: the short is accepted, as before.
        let mut b = broker(0);
        assert_eq!(b.place(&short(), OrderId(7)), Submission::Accepted);
    }

    #[test]
    fn the_borrow_fee_follows_the_broker_none_on_easy_to_borrow_names() {
        // Short 100 at 10.00 for a day, at 500 bps a year (5%): about $0.137 without the broker's rules; nothing with
        // them on an easy-to-borrow name.
        let fee = |table: Option<Vec<Borrow>>| {
            let mut b = broker(500);
            if let Some(t) = table {
                b = b.with_borrow_table(t);
            }
            b.place(&short(), OrderId(7));
            b.observe(&q(1, 1000, 1001));
            assert_eq!(b.position(0), -100);
            b.observe(&Event::Trade(tf_core::Trade {
                hdr: Header {
                    ts_event: T0 + 2 * S,
                    ts_recv: T0 + 2 * S,
                    seq: 2,
                    instrument: 0,
                    provider: ProviderId::Synthetic,
                },
                px: Px::from_cents(1000),
                size: 10,
                flags: tf_core::TradeFlags::NONE,
            }));
            b.observe(&q(2 + 86_400, 1000, 1001));
            b.borrow_fee(0)
        };
        let charged = fee(None);
        // Worked independently: the mark is the quote midpoint (10.005) for the first second, then the last trade
        // (10.00) for 86,400 s: (100 x 10.005 x 1 + 100 x 10.00 x 86,400) x 5% / (365 x 86,400) = 0.13698788765...,
        // in raw units (1e-9 dollars) rounded up.
        assert_eq!(charged, 136_987_888);
        assert_eq!(fee(Some(vec![Borrow::Easy])), 0);
    }
}
