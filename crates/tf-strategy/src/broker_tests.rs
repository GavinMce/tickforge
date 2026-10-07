use std::collections::BTreeMap;

use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote};

use crate::broker::{Broker, BrokerEvent, CancelOutcome, Kind, Submission, check_events};
use crate::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderId, OrderState};
use crate::sim::{FaultPlan, SimBroker, SimConfig};

const MS: Nanos = 1_000_000;

fn quote(ts: Nanos, instrument: u32, bid: i64, ask: i64, sz: u32) -> Event {
    Event::Quote(Quote {
        hdr: Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument,
            provider: ProviderId::Synthetic,
        },
        bid_px: Px::from_cents(bid),
        ask_px: Px::from_cents(ask),
        bid_sz: sz,
        ask_sz: sz,
    })
}

fn buy(seq: u64, ts: Nanos, qty: u32, limit_cents: i64, tif: Tif) -> Intent {
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq,
        },
        instrument: 0,
        side: Side::Buy,
        qty,
        purpose: Purpose::Open,
        pricing: Pricing::Limit(Px::from_cents(limit_cents)),
        protect: Some(Protective {
            stop_trigger: Px::from_cents(limit_cents / 2),
            stop_limit: None,
            take_profit: None,
        }),
        tif,
        ts,
        reason: 0,
    }
}

fn broker(latency_ms: u64) -> SimBroker {
    SimBroker::new(
        SimConfig {
            latency_ns: latency_ms * MS,
            borrow_bps_per_year: 0,
        },
        3,
    )
}

fn sizes(placed: &[(OrderId, u32)]) -> impl Fn(OrderId) -> Option<u32> + '_ {
    move |o| placed.iter().find(|p| p.0 == o).map(|p| p.1)
}

fn kinds(events: &[BrokerEvent]) -> Vec<(u64, Kind)> {
    events.iter().map(|e| (e.order.0, e.kind)).collect()
}

#[test]
fn an_accepted_order_is_acknowledged_when_it_arrives_and_filled_when_a_quote_crosses() {
    let mut b = broker(10);
    b.observe(&quote(0, 0, 9_990, 10_010, 100));
    assert_eq!(
        b.place(&buy(1, 100 * MS, 250, 10_010, Tif::Day), OrderId(42)),
        Submission::Accepted
    );
    assert!(
        b.take_events().is_empty(),
        "nothing at the venue until it arrives"
    );
    // 100 shares shown: the first fill is partial; a later quote gives the rest.
    b.observe(&quote(105 * MS, 0, 9_990, 10_010, 100));
    assert!(b.take_events().is_empty());
    b.observe(&quote(110 * MS, 0, 9_990, 10_010, 100));
    b.observe(&quote(120 * MS, 0, 9_990, 10_010, 100));
    b.observe(&quote(130 * MS, 0, 9_990, 10_010, 100));
    let ev = b.take_events();
    let px = Px::from_cents(10_010);
    assert_eq!(
        kinds(&ev),
        [
            (42, Kind::Ack),
            (42, Kind::Fill { qty: 100, px }),
            (42, Kind::Fill { qty: 100, px }),
            (42, Kind::Fill { qty: 50, px })
        ],
        "the broker's events carry the gateway's order id, and a complete fill ends the order by itself"
    );
    assert_eq!(ev[0].ts, 110 * MS);
    check_events(&ev, sizes(&[(OrderId(42), 250)])).unwrap();
    assert!(b.take_events().is_empty(), "drained");
    assert_eq!(b.open_orders(), 0);
}

#[test]
fn an_ioc_remainder_expires_and_a_day_order_expires_at_the_close() {
    let mut b = broker(0);
    b.observe(&quote(0, 0, 9_990, 10_010, 60));
    b.place(&buy(1, MS, 100, 10_010, Tif::Ioc), OrderId(1));
    b.place(&buy(2, MS, 100, 9_000, Tif::Day), OrderId(2));
    b.observe(&quote(2 * MS, 0, 9_990, 10_010, 60));
    let mut ev = b.take_events();
    b.close_day(5 * MS);
    ev.extend(b.take_events());
    let px = Px::from_cents(10_010);
    assert_eq!(
        kinds(&ev),
        [
            (1, Kind::Ack),
            (1, Kind::Fill { qty: 60, px }),
            (1, Kind::Close(OrderState::Expired)),
            (2, Kind::Ack),
            (2, Kind::Close(OrderState::Expired)),
        ]
    );
    check_events(&ev, sizes(&[(OrderId(1), 100), (OrderId(2), 100)])).unwrap();
}

#[test]
fn a_cancel_reaches_a_working_order_or_one_still_in_flight_and_nothing_else() {
    let mut b = broker(50);
    b.observe(&quote(0, 0, 9_990, 10_010, 100));
    b.place(&buy(1, 0, 10, 9_000, Tif::Day), OrderId(1));
    b.place(&buy(2, 0, 10, 9_000, Tif::Day), OrderId(2));
    // Order 1 is cancelled while still in flight: the cancel wins, and it was never acknowledged.
    assert_eq!(
        b.cancel_order(OrderId(1), 10 * MS),
        CancelOutcome::Requested
    );
    b.observe(&quote(60 * MS, 0, 9_990, 10_010, 100));
    // Order 2 is working now.
    assert_eq!(
        b.cancel_order(OrderId(2), 70 * MS),
        CancelOutcome::Requested
    );
    assert_eq!(
        b.cancel_order(OrderId(2), 71 * MS),
        CancelOutcome::Finished,
        "already cancelled"
    );
    assert_eq!(
        b.cancel_order(OrderId(99), 72 * MS),
        CancelOutcome::Finished,
        "never placed"
    );
    let ev = b.take_events();
    assert_eq!(
        kinds(&ev),
        [
            (1, Kind::Close(OrderState::Cancelled)),
            (2, Kind::Ack),
            (2, Kind::Close(OrderState::Cancelled))
        ]
    );
    check_events(&ev, sizes(&[(OrderId(1), 10), (OrderId(2), 10)])).unwrap();
}

#[test]
fn an_order_that_filled_cannot_be_cancelled() {
    let mut b = broker(0);
    b.observe(&quote(0, 0, 9_990, 10_010, 100));
    b.place(&buy(1, MS, 10, 10_010, Tif::Day), OrderId(1));
    b.observe(&quote(2 * MS, 0, 9_990, 10_010, 100));
    assert_eq!(b.cancel_order(OrderId(1), 3 * MS), CancelOutcome::Finished);
}

#[test]
fn faults_are_injected_by_count_and_the_first_matching_rule_wins() {
    let plan = FaultPlan {
        refuse_instruments: vec![2],
        refuse_every: 4,
        rate_limit_every: 3,
        rate_limit_retry_ns: 5 * MS,
        unknown_every: 2,
        venue_reject_every: 5,
    };
    let mut b = broker(0).with_faults(plan);
    b.observe(&quote(0, 0, 9_990, 10_010, 1_000));
    let mut out = Vec::new();
    for n in 1..=12u64 {
        let mut i = buy(n, MS, 10, 10_010, Tif::Day);
        if n == 7 {
            i.instrument = 2;
        }
        if n == 8 {
            i.instrument = 1_000; // outside the id space
        }
        out.push(b.place(&i, OrderId(n)));
    }
    let refused = |m: &str, c: u16| Submission::Refused {
        code: c,
        message: m.to_owned(),
    };
    // Rules apply in this order: instruments, refuse_every, rate_limit_every, unknown_every,
    // venue_reject_every. So 4 and 12 are refused (before unknown_every 2 or rate_limit_every 3 get to
    // say anything), 6 is rate limited (before unknown_every), and 5 is accepted and then rejected by
    // the venue.
    let want = [
        Submission::Accepted, // 1
        Submission::Unknown,  // 2
        Submission::RateLimited {
            retry_after: 5 * MS,
        }, // 3
        refused("simulated refusal", 422), // 4
        Submission::Accepted, // 5
        Submission::RateLimited {
            retry_after: 5 * MS,
        }, // 6
        refused("simulated refusal", 422), // 7: instrument 2
        refused("unknown instrument", 400), // 8
        Submission::RateLimited {
            retry_after: 5 * MS,
        }, // 9
        Submission::Unknown,  // 10
        Submission::Accepted, // 11
        refused("simulated refusal", 422), // 12
    ];
    assert_eq!(out, want);
    b.observe(&quote(MS, 0, 9_990, 10_010, 1_000));
    let ev = b.take_events();
    // Placed at the venue: 1 (accepted), 5 (accepted, rejected on arrival), 11 (accepted); and of the
    // unknowns (2, 10) the first arrives, the second never does.
    let mut orders: Vec<u64> = ev.iter().map(|e| e.order.0).collect();
    orders.sort_unstable();
    orders.dedup();
    assert_eq!(orders, [1, 2, 5, 11], "{ev:?}");
    let by = |o: u64| {
        ev.iter()
            .filter(|e| e.order.0 == o)
            .map(|e| e.kind)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        by(5),
        [Kind::Close(OrderState::Rejected)],
        "rejected before any acknowledgement"
    );
    assert_eq!(by(1)[0], Kind::Ack);
    assert_eq!(by(2)[0], Kind::Ack, "an unknown placement that did arrive");
    let placed: Vec<(OrderId, u32)> = [1, 2, 5, 11].iter().map(|o| (OrderId(*o), 10)).collect();
    check_events(&ev, sizes(&placed)).unwrap();
}

#[test]
fn a_rate_limited_or_refused_order_never_reaches_the_venue_and_resending_counts_as_a_new_placement()
{
    let plan = FaultPlan {
        rate_limit_every: 2,
        rate_limit_retry_ns: MS,
        ..FaultPlan::default()
    };
    let mut b = broker(0).with_faults(plan);
    b.observe(&quote(0, 0, 9_990, 10_010, 100));
    assert_eq!(
        b.place(&buy(1, MS, 10, 10_010, Tif::Day), OrderId(1)),
        Submission::Accepted
    );
    assert_eq!(
        b.place(&buy(2, MS, 10, 10_010, Tif::Day), OrderId(2)),
        Submission::RateLimited { retry_after: MS }
    );
    // Sending order 2 again is the third placement: it goes through.
    assert_eq!(
        b.place(&buy(2, 2 * MS, 10, 10_010, Tif::Day), OrderId(2)),
        Submission::Accepted
    );
    b.observe(&quote(3 * MS, 0, 9_990, 10_010, 100));
    let ev = b.take_events();
    assert_eq!(
        ev.iter()
            .filter(|e| e.order == OrderId(2) && e.kind == Kind::Ack)
            .count(),
        1
    );
}

#[test]
fn rules_that_are_off_do_nothing() {
    let mut b = broker(0).with_faults(FaultPlan::default());
    b.observe(&quote(0, 0, 9_990, 10_010, 100));
    for n in 1..=20 {
        assert_eq!(
            b.place(&buy(n, MS, 1, 10_010, Tif::Day), OrderId(n)),
            Submission::Accepted
        );
    }
}

fn ev(order: u64, ts: Nanos, kind: Kind) -> BrokerEvent {
    BrokerEvent {
        order: OrderId(order),
        ts,
        kind,
    }
}

#[test]
fn the_checker_refuses_what_a_broker_may_not_say() {
    let px = Px::from_cents(100);
    let size = |_: OrderId| Some(10);
    let bad = |events: Vec<BrokerEvent>, want: &str| {
        let e = check_events(&events, size).unwrap_err();
        assert!(e.why.contains(want), "{want}: {e:?}");
    };
    check_events(
        &[
            ev(1, 1, Kind::Ack),
            ev(1, 2, Kind::Fill { qty: 4, px }),
            ev(1, 3, Kind::Fill { qty: 6, px }),
        ],
        size,
    )
    .unwrap();
    check_events(&[ev(1, 1, Kind::Close(OrderState::Rejected))], size).unwrap();
    check_events(
        &[
            ev(1, 1, Kind::Ack),
            ev(1, 1, Kind::Close(OrderState::Cancelled)),
        ],
        size,
    )
    .unwrap();
    check_events(&[ev(1, 1, Kind::Close(OrderState::Cancelled))], size).unwrap();
    bad(vec![ev(1, 1, Kind::Ack), ev(1, 2, Kind::Ack)], "twice");
    bad(
        vec![ev(1, 1, Kind::Fill { qty: 1, px })],
        "before the acknowledgement",
    );
    bad(
        vec![ev(1, 1, Kind::Ack), ev(1, 2, Kind::Fill { qty: 0, px })],
        "no shares",
    );
    bad(
        vec![ev(1, 1, Kind::Ack), ev(1, 2, Kind::Fill { qty: 11, px })],
        "more than was asked",
    );
    bad(
        vec![
            ev(1, 1, Kind::Ack),
            ev(1, 2, Kind::Fill { qty: 5, px }),
            ev(1, 3, Kind::Fill { qty: 6, px }),
        ],
        "more than was asked",
    );
    bad(
        vec![
            ev(1, 1, Kind::Ack),
            ev(1, 2, Kind::Close(OrderState::Rejected)),
        ],
        "after it was acknowledged",
    );
    bad(
        vec![ev(1, 1, Kind::Close(OrderState::Expired))],
        "before it was acknowledged",
    );
    bad(
        vec![
            ev(1, 1, Kind::Ack),
            ev(1, 2, Kind::Close(OrderState::Cancelled)),
            ev(1, 3, Kind::Ack),
        ],
        "after the order ended",
    );
    bad(
        vec![
            ev(1, 1, Kind::Ack),
            ev(1, 2, Kind::Fill { qty: 10, px }),
            ev(1, 3, Kind::Close(OrderState::Cancelled)),
        ],
        "after the order ended",
    );
    bad(vec![ev(1, 5, Kind::Ack), ev(2, 4, Kind::Ack)], "backwards");
    bad(
        vec![ev(1, 1, Kind::Close(OrderState::Filled))],
        "cancelled, expired or rejected only",
    );
    assert!(
        check_events(
            &[ev(9, 1, Kind::Ack), ev(9, 2, Kind::Fill { qty: 1, px })],
            |_| None
        )
        .unwrap_err()
        .why
        .contains("nobody placed")
    );
    // Orders interleave freely.
    check_events(
        &[
            ev(1, 1, Kind::Ack),
            ev(2, 1, Kind::Ack),
            ev(2, 2, Kind::Fill { qty: 10, px }),
            ev(1, 3, Kind::Close(OrderState::Expired)),
        ],
        size,
    )
    .unwrap();
}

/// The two ways of driving the simulated broker give the same market outcome: the legacy
/// `submit`/`drain_updates` pair and the `Broker` interface, over a long pseudo-random session.
#[test]
fn the_broker_interface_and_the_original_api_make_the_same_fills() {
    let mut x: u64 = 0xDEAD_BEEF_1234_5678;
    let mut rnd = move |m: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % m
    };
    let (mut old, mut new) = (broker(7), broker(7));
    let mut placed: Vec<(OrderId, u32)> = Vec::new();
    let mut events: Vec<BrokerEvent> = Vec::new();
    let mut updates = Vec::new();
    let mut seq = 0u64;
    for step in 0..4_000u64 {
        let ts = step * 3 * MS;
        let mid = 10_000 + (rnd(41) as i64 - 20);
        let q = quote(ts, 0, mid - 5, mid + 5, 50 + rnd(100) as u32);
        old.on_event(&q);
        new.observe(&q);
        if rnd(4) == 0 {
            let tif = if rnd(2) == 0 { Tif::Ioc } else { Tif::Day };
            let i = buy(seq, ts, 1 + rnd(200) as u32, mid + rnd(15) as i64 - 3, tif);
            old.submit(&i);
            let order = OrderId(seq);
            assert_eq!(new.place(&i, order), Submission::Accepted);
            placed.push((order, i.qty));
            seq += 1;
        }
        if rnd(11) == 0 && seq > 0 {
            let k = rnd(seq);
            let a = old.cancel(
                IntentId {
                    strategy: StrategyId(1),
                    seq: k,
                },
                ts,
            );
            let b = new.cancel_order(OrderId(k), ts) == CancelOutcome::Requested;
            assert_eq!(a, b, "step {step}");
        }
        updates.extend(old.drain_updates());
        events.extend(new.take_events());
    }
    old.end_of_day(20_000 * MS);
    new.close_day(20_000 * MS);
    updates.extend(old.drain_updates());
    events.extend(new.take_events());
    assert_eq!(old.fills(), new.fills());
    assert!(old.fills().len() > 100, "{} fills", old.fills().len());
    check_events(&events, sizes(&placed)).unwrap();
    // The events say what the fills say, and what the updates say about how each order ended.
    let from_events: Vec<(u64, u32, i64, Nanos)> = events
        .iter()
        .filter_map(|e| match e.kind {
            Kind::Fill { qty, px } => Some((e.order.0, qty, px.raw(), e.ts)),
            _ => None,
        })
        .collect();
    let from_fills: Vec<(u64, u32, i64, Nanos)> = old
        .fills()
        .iter()
        .map(|f| (f.order.0, f.qty, f.px.raw(), f.ts))
        .collect();
    assert_eq!(from_events, from_fills);
    let mut end_by_events: BTreeMap<u64, OrderState> = BTreeMap::new();
    let mut filled: BTreeMap<u64, u32> = BTreeMap::new();
    for e in &events {
        match e.kind {
            Kind::Close(s) => {
                end_by_events.insert(e.order.0, s);
            }
            Kind::Fill { qty, .. } => *filled.entry(e.order.0).or_default() += qty,
            _ => {}
        }
    }
    let mut end_by_updates: BTreeMap<u64, OrderState> = BTreeMap::new();
    for u in &updates {
        if u.state.is_terminal() && u.state != OrderState::Filled {
            end_by_updates.insert(u.intent.seq, u.state);
        }
    }
    assert_eq!(end_by_events, end_by_updates);
    assert!(
        end_by_events.values().any(|s| *s == OrderState::Cancelled)
            && end_by_events.values().any(|s| *s == OrderState::Expired)
    );
    // Every order ends exactly once: filled completely, or closed.
    for (o, qty) in &placed {
        let done =
            filled.get(&o.0).copied().unwrap_or(0) == *qty || end_by_events.contains_key(&o.0);
        assert!(done, "order {} neither filled nor closed", o.0);
    }
}

#[test]
fn a_run_that_never_uses_the_broker_interface_keeps_no_event_log() {
    let mut b = broker(0);
    b.observe(&quote(0, 0, 9_990, 10_010, 100));
    b.submit(&buy(1, MS, 10, 10_010, Tif::Day));
    b.observe(&quote(2 * MS, 0, 9_990, 10_010, 100));
    assert_eq!(b.fills().len(), 1);
    assert!(b.take_events().is_empty());
}

#[test]
fn a_protective_leg_may_fill_after_its_parent_is_complete_but_not_before_it_was_acknowledged() {
    use crate::broker::Leg;
    let leg = |ts, qty| {
        ev(
            1,
            ts,
            Kind::LegFill {
                leg: Leg::Stop,
                qty,
                px: Px::from_cents(950),
            },
        )
    };
    let sz = |_: OrderId| Some(100);
    let fill = Kind::Fill {
        qty: 100,
        px: Px::from_cents(1000),
    };
    // Acknowledged, filled in full (the parent has ended), then the stop fills: lawful.
    let ok = [ev(1, 1, Kind::Ack), ev(1, 2, fill), leg(3, 100), leg(4, 10)];
    assert_eq!(check_events(&ok, sz), Ok(()));
    // A leg of an order never acknowledged.
    let bad = [leg(1, 100)];
    let f = check_events(&bad, sz).unwrap_err();
    assert_eq!(f.index, 0);
    assert!(
        f.why.contains("before the parent was acknowledged"),
        "{}",
        f.why
    );
    // A leg fill of no shares, and time going backwards, are still refused.
    let zero = [ev(1, 1, Kind::Ack), leg(2, 0)];
    assert!(
        check_events(&zero, sz)
            .unwrap_err()
            .why
            .contains("no shares")
    );
    let back = [ev(1, 5, Kind::Ack), ev(1, 2, fill)];
    assert!(
        check_events(&back, sz)
            .unwrap_err()
            .why
            .contains("backwards")
    );
}
