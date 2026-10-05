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
