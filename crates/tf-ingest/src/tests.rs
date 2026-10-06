use std::time::Duration;

use tf_core::{Header, ProviderId, Px, Status, StatusKind, Trade, TradeFlags};
use tf_synth::SplitMix64;

use super::*;

fn hdr(instrument: u32, ts: u64) -> Header {
    Header {
        ts_event: ts,
        ts_recv: ts,
        seq: ts,
        instrument,
        provider: ProviderId::Databento,
    }
}

fn trade(instrument: u32, ts: u64) -> Event {
    Event::Trade(Trade {
        hdr: hdr(instrument, ts),
        px: Px::from_raw(ts as i64 * 10),
        size: 1,
        flags: TradeFlags::NONE,
    })
}

fn quote(instrument: u32, ts: u64) -> Event {
    Event::Quote(Quote {
        hdr: hdr(instrument, ts),
        bid_px: Px::from_raw(ts as i64 * 10 - 1),
        ask_px: Px::from_raw(ts as i64 * 10 + 1),
        bid_sz: 1,
        ask_sz: 1,
    })
}

fn halt(instrument: u32, ts: u64) -> Event {
    Event::Status(Status {
        hdr: hdr(instrument, ts),
        kind: StatusKind::TradingHalt,
        lo: Px::ZERO,
        hi: Px::ZERO,
    })
}

/// 100 messages: quotes stop at 50, trades at 90.
fn small() -> Config {
    Config {
        capacity: 100,
        quote_ceiling_permille: 500,
        trade_ceiling_permille: 900,
        instruments: 8,
    }
}

fn drain(c: &mut Consumer) -> Vec<Delivery> {
    std::iter::from_fn(|| c.try_recv()).collect()
}

fn ts(d: &Delivery) -> u64 {
    match d {
        Delivery::Event(e) => e.hdr().ts_recv,
        Delivery::Gap(g) => g.first_ts,
    }
}

#[test]
fn the_configuration_is_checked() {
    assert!(Config::default().validate().is_ok());
    let bad = |f: &dyn Fn(&mut Config)| {
        let mut c = small();
        f(&mut c);
        assert!(channel(c).is_err());
    };
    bad(&|c| c.capacity = 8);
    bad(&|c| c.quote_ceiling_permille = 0);
    bad(&|c| c.quote_ceiling_permille = 900);
    bad(&|c| c.trade_ceiling_permille = 1000);
    bad(&|c| c.trade_ceiling_permille = 400);
    assert_eq!(small().validate(), Ok(()));
    let (p, _) = channel(small()).unwrap();
    assert_eq!(p.config(), small());
}

#[test]
fn everything_arrives_in_order_when_the_consumer_keeps_up() {
    let (mut p, mut c) = channel(small()).unwrap();
    let mut sent = Vec::new();
    for i in 0..1_000u64 {
        let e = match i % 3 {
            0 => trade((i % 5) as u32, i),
            1 => quote((i % 5) as u32, i),
            _ => halt((i % 5) as u32, i),
        };
        assert_eq!(p.push(e), Admission::Queued);
        sent.push(Delivery::Event(e));
        assert_eq!(c.try_recv(), Some(sent[i as usize]));
    }
    let s = p.stats();
    assert_eq!(
        (s.offered, s.queued, s.delivered, s.conflated, s.max_depth),
        (1_000, 1_000, 1_000, 0, 1)
    );
    assert!(s.lossless());
    assert!(p.is_settled());
    assert_eq!(c.stats(), s, "both ends read the same counters");
    assert_eq!(c.try_recv(), None);
}

#[test]
fn quotes_conflate_per_symbol_once_the_ring_is_half_full_and_arrive_when_there_is_room() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        assert_eq!(p.push(trade(7, i)), Admission::Queued);
    }
    assert_eq!(c.depth(), 50);
    // At the ceiling: thirty quotes for three symbols all wait, ten each, only the latest kept.
    for i in 0..30u64 {
        assert_eq!(p.push(quote((i % 3) as u32, 100 + i)), Admission::Conflated);
    }
    assert_eq!(c.depth(), 50, "waiting quotes take no room on the ring");
    assert!(!p.is_settled());
    assert_eq!(p.stats().conflated, 30);
    // The consumer catches up; the feed goes quiet; a tick queues what waits, oldest symbol first.
    assert_eq!(drain(&mut c).len(), 50);
    p.tick();
    assert!(p.is_settled());
    let got = drain(&mut c);
    assert_eq!(got.len(), 3);
    let latest: Vec<Delivery> = (0..3u64)
        .map(|s| Delivery::Event(quote(s as u32, 100 + 27 + s)))
        .collect();
    assert_eq!(
        got, latest,
        "the newest quote of each symbol, in the order the symbols first waited"
    );
    assert_eq!((p.stats().flushed, p.stats().dropped_quotes), (3, 0));
}

#[test]
fn a_quote_that_fits_replaces_the_one_that_was_waiting() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        p.push(trade(7, i));
    }
    assert_eq!(p.push(quote(1, 100)), Admission::Conflated);
    // The consumer takes ten: room under the quote ceiling, but not yet below the low-water mark.
    for _ in 0..10 {
        c.try_recv().unwrap();
    }
    assert_eq!(c.depth(), 40);
    assert_eq!(p.push(quote(1, 101)), Admission::Queued);
    assert!(
        p.is_settled(),
        "the waiting one was superseded, not left behind"
    );
    let got = drain(&mut c);
    assert_eq!(got.len(), 41);
    assert_eq!(got.last(), Some(&Delivery::Event(quote(1, 101))));
    assert!(
        !got.contains(&Delivery::Event(quote(1, 100))),
        "the old quote is gone for good"
    );
    assert_eq!(p.stats().conflated, 2, "one quote waited and was replaced");
}

#[test]
fn with_plenty_of_room_a_waiting_quote_goes_out_first_and_the_new_one_after_it() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        p.push(trade(7, i));
    }
    assert_eq!(p.push(quote(1, 100)), Admission::Conflated);
    assert_eq!(drain(&mut c).len(), 50);
    assert_eq!(p.push(quote(1, 101)), Admission::Queued);
    assert!(p.is_settled());
    assert_eq!(
        drain(&mut c),
        [
            Delivery::Event(quote(1, 100)),
            Delivery::Event(quote(1, 101))
        ],
        "nothing lost, in order"
    );
}

#[test]
fn a_waiting_quote_goes_ahead_of_the_next_event_of_its_symbol() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        p.push(trade(7, i));
    }
    assert_eq!(p.push(quote(1, 100)), Admission::Conflated);
    assert_eq!(p.push(trade(1, 101)), Admission::Queued);
    assert_eq!(p.push(halt(1, 102)), Admission::Queued);
    let got = drain(&mut c);
    assert_eq!(got.len(), 50 + 3);
    assert_eq!(
        &got[50..],
        &[
            Delivery::Event(quote(1, 100)),
            Delivery::Event(trade(1, 101)),
            Delivery::Event(halt(1, 102))
        ]
    );
    assert!(p.is_settled());
}

#[test]
fn trades_queue_up_to_their_ceiling_and_are_dropped_after_it_with_a_marker_in_the_stream() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..120u64 {
        let a = p.push(trade((i % 4) as u32, 1_000 + i));
        assert_eq!(
            a,
            if i < 90 {
                Admission::Queued
            } else {
                Admission::Dropped
            },
            "{i}"
        );
    }
    let s = p.stats();
    assert_eq!(
        (s.queued, s.dropped_trades, s.offered, s.gaps),
        (90, 30, 120, 0)
    );
    assert!(!s.lossless());
    // The marker waits for room; it is delivered before the next event that follows the break.
    let first = drain(&mut c);
    assert_eq!(first.len(), 90);
    assert!(
        first
            .iter()
            .all(|d| matches!(d, Delivery::Event(Event::Trade(_))))
    );
    assert_eq!(p.push(trade(0, 2_000)), Admission::Queued);
    let after = drain(&mut c);
    assert_eq!(
        after,
        [
            Delivery::Gap(Gap {
                lost: Lost::Trades,
                count: 30,
                first_ts: 1_090,
                last_ts: 1_119
            }),
            Delivery::Event(trade(0, 2_000)),
        ]
    );
    assert_eq!(p.stats().gaps, 1);
    assert!(p.is_settled());
    // Offered trades are all accounted for.
    let s = p.stats();
    assert_eq!(s.queued - s.gaps + s.dropped_trades, s.offered);
}

#[test]
fn a_gap_marker_does_not_wait_for_another_event_when_the_feed_ticks() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..200u64 {
        p.push(trade(1, i));
    }
    // Ten halts: the first sends the trade marker ahead of itself, and the ring is full with the
    // ninth, so the last has no place and becomes a marker of its own.
    for i in 0..10u64 {
        p.push(halt(1, 1_000 + i));
    }
    assert_eq!(c.depth(), 100, "full");
    p.tick();
    assert!(!p.is_settled(), "no room at all for the control marker");
    let first = drain(&mut c);
    assert_eq!(first.len(), 100);
    assert_eq!(
        first[90],
        Delivery::Gap(Gap {
            lost: Lost::Trades,
            count: 110,
            first_ts: 90,
            last_ts: 199
        })
    );
    p.tick();
    assert!(p.is_settled());
    assert_eq!(
        drain(&mut c),
        [Delivery::Gap(Gap {
            lost: Lost::Control,
            count: 1,
            first_ts: 1_009,
            last_ts: 1_009
        })]
    );
}

#[test]
fn a_marker_takes_the_room_there_is_when_the_feed_ticks() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..95u64 {
        p.push(trade(1, i));
    }
    assert!(!p.is_settled());
    p.tick();
    assert!(p.is_settled());
    let got = drain(&mut c);
    assert_eq!(got.len(), 91);
    assert_eq!(
        got[90],
        Delivery::Gap(Gap {
            lost: Lost::Trades,
            count: 5,
            first_ts: 90,
            last_ts: 94
        })
    );
}

#[test]
fn control_events_get_the_last_of_the_room_and_only_a_full_ring_drops_one() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..90u64 {
        assert_eq!(p.push(trade(1, i)), Admission::Queued);
    }
    assert_eq!(p.push(trade(1, 90)), Admission::Dropped);
    // Ten places are left, for the control events and the trade gap marker that goes ahead of them.
    let mut queued = 0;
    for i in 0..20u64 {
        if p.push(halt(2, 200 + i)) == Admission::Queued {
            queued += 1;
        }
    }
    assert_eq!(queued, 9, "ten places, one taken by the trade gap marker");
    let s = p.stats();
    assert_eq!((s.dropped_control, s.dropped_trades), (11, 1));
    let got = drain(&mut c);
    assert_eq!(got.len(), 100);
    assert!(
        matches!(
            got[90],
            Delivery::Gap(Gap {
                lost: Lost::Trades,
                count: 1,
                ..
            })
        ),
        "{:?}",
        got[90]
    );
    // Control losses are marked too, once there is room.
    p.tick();
    let g = drain(&mut c);
    assert_eq!(
        g,
        [Delivery::Gap(Gap {
            lost: Lost::Control,
            count: 11,
            first_ts: 209,
            last_ts: 219
        })]
    );
}

#[test]
fn a_waiting_quote_with_no_room_before_a_trade_of_its_symbol_is_counted_lost() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        p.push(trade(7, i));
    }
    assert_eq!(p.push(quote(1, 100)), Admission::Conflated);
    for i in 0..40u64 {
        p.push(trade(7, 200 + i)); // up to the trade ceiling
    }
    assert_eq!(c.depth(), 90);
    assert_eq!(p.push(trade(1, 300)), Admission::Dropped);
    let s = p.stats();
    assert_eq!((s.dropped_quotes, s.dropped_trades), (1, 1));
    assert_eq!(drain(&mut c).len(), 90);
}

#[test]
fn instruments_the_table_does_not_cover_are_never_conflated() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        p.push(trade(7, i));
    }
    assert_eq!(p.push(quote(99, 100)), Admission::Dropped);
    assert_eq!(p.stats().dropped_quotes, 1);
    assert_eq!(p.stats().conflated, 0);
    // Below the ceiling they are queued like any other.
    assert_eq!(drain(&mut c).len(), 50);
    assert_eq!(p.push(quote(99, 101)), Admission::Queued);
}

#[test]
fn streams_keep_each_symbols_order_account_for_every_drop_and_settle_on_the_latest_quote() {
    for seed in 0..60u64 {
        let mut rng = SplitMix64::new(0xD0_0000 + seed);
        let cfg = Config {
            capacity: 64,
            quote_ceiling_permille: 500,
            trade_ceiling_permille: 900,
            instruments: 6,
        };
        let (mut p, mut c) = channel(cfg).unwrap();
        let mut offered_trades = 0u64;
        let mut last_quote: [Option<Event>; 6] = [None; 6];
        let mut delivered: Vec<Delivery> = Vec::new();
        let mut t = 0u64;
        for _ in 0..2_000 {
            t += 1;
            let sym = (rng.next_u64() % 6) as u32;
            let e = match rng.next_u64() % 100 {
                0..=59 => {
                    let q = quote(sym, t);
                    last_quote[sym as usize] = Some(q);
                    q
                }
                60..=94 => {
                    offered_trades += 1;
                    trade(sym, t)
                }
                _ => halt(sym, t),
            };
            p.push(e);
            if rng.next_u64() % 100 < 12 {
                for _ in 0..(rng.next_u64() % 40) {
                    if let Some(d) = c.try_recv() {
                        delivered.push(d);
                    }
                }
            }
            if rng.next_u64() % 50 == 0 {
                p.tick();
            }
        }
        // Let it all through.
        for _ in 0..10 {
            p.tick();
            delivered.extend(drain(&mut c));
        }
        assert!(p.is_settled(), "seed {seed}");
        let s = p.stats();
        assert!(s.max_depth <= 64, "seed {seed}: {}", s.max_depth);
        // Per symbol, time never goes backwards.
        let mut last = [0u64; 6];
        let (mut trades, mut gap_trades) = (0u64, 0u64);
        for d in &delivered {
            match d {
                Delivery::Event(e) => {
                    let i = e.hdr().instrument as usize;
                    assert!(
                        e.hdr().ts_recv > last[i],
                        "seed {seed}: symbol {i} went backwards"
                    );
                    last[i] = e.hdr().ts_recv;
                    trades += u64::from(matches!(e, Event::Trade(_)));
                }
                Delivery::Gap(g) if g.lost == Lost::Trades => gap_trades += g.count,
                Delivery::Gap(_) => {}
            }
        }
        // Every trade is either delivered or counted, and the markers say exactly how many.
        assert_eq!(trades + s.dropped_trades, offered_trades, "seed {seed}");
        assert_eq!(
            gap_trades, s.dropped_trades,
            "seed {seed}: the markers add up to the drops"
        );
        // And where no quote was lost, the last quote of each symbol is the last one offered.
        if s.dropped_quotes == 0 {
            for sym in 0..6u32 {
                let got = delivered.iter().rev().find_map(|d| match d {
                    Delivery::Event(e @ Event::Quote(_)) if e.hdr().instrument == sym => Some(*e),
                    _ => None,
                });
                assert_eq!(got, last_quote[sym as usize], "seed {seed} symbol {sym}");
            }
        }
        assert_eq!(s.queued, delivered.len() as u64, "seed {seed}");
    }
}

#[test]
fn a_producer_thread_and_a_consumer_thread_lose_nothing_that_fits() {
    let cfg = Config {
        capacity: 1 << 20,
        ..Config::default()
    };
    let (mut p, mut c) = channel(cfg).unwrap();
    let n = 600_000u64;
    let consumer = std::thread::spawn(move || {
        let mut next = 0u64;
        let mut got = 0u64;
        while got < n {
            match c.recv_timeout(Duration::from_secs(10)) {
                Some(Delivery::Event(e)) => {
                    assert_eq!(e.hdr().ts_recv, next, "in order");
                    next += 1;
                    got += 1;
                }
                other => panic!("{other:?} after {got}"),
            }
        }
        c
    });
    for i in 0..n {
        assert_eq!(p.push(trade((i % 100) as u32, i)), Admission::Queued);
    }
    let c = consumer.join().unwrap();
    let s = c.stats();
    assert_eq!((s.offered, s.delivered, s.dropped_trades), (n, n, 0));
    assert!(s.lossless());
}

#[test]
fn the_consumer_waits_politely_and_knows_when_the_feed_is_gone() {
    let (mut p, mut c) = channel(small()).unwrap();
    assert_eq!(c.try_recv(), None);
    assert_eq!(c.recv_timeout(Duration::from_millis(5)), None);
    p.push(trade(1, 1));
    assert_eq!(
        c.recv_timeout(Duration::from_millis(5)),
        Some(Delivery::Event(trade(1, 1)))
    );
    drop(p);
    assert_eq!(c.try_recv(), None);
    assert_eq!(c.recv_timeout(Duration::from_millis(5)), None);
    // A message queued before the producer went away is still delivered.
    let (mut p, mut c) = channel(small()).unwrap();
    p.push(trade(1, 9));
    drop(p);
    assert_eq!(ts(&c.try_recv().unwrap()), 9);
}

#[test]
fn a_consumer_that_is_gone_makes_the_feed_drop_not_block() {
    let (mut p, c) = channel(small()).unwrap();
    drop(c);
    assert_eq!(p.push(trade(1, 1)), Admission::Dropped);
    assert_eq!(p.push(halt(1, 2)), Admission::Dropped);
    assert_eq!(p.stats().dropped_trades + p.stats().dropped_control, 2);
}

#[test]
fn the_message_is_small() {
    assert!(
        std::mem::size_of::<Delivery>() <= 80,
        "{}",
        std::mem::size_of::<Delivery>()
    );
}

#[test]
fn waiting_symbols_are_listed_once_however_long_the_pressure_lasts() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..50 {
        p.push(trade(7, i));
    }
    // The ring oscillates around its quote ceiling: a quote waits, the consumer takes one message,
    // the next quote for the symbol goes straight on and supersedes the waiting one, the consumer
    // takes another and a trade brings the ring back to the ceiling. Over and over, for one symbol.
    for round in 0..500u64 {
        assert_eq!(c.depth(), 50, "round {round}");
        assert_eq!(
            p.push(quote(1, 1_000 + round * 3)),
            Admission::Conflated,
            "round {round}"
        );
        c.try_recv().unwrap();
        assert_eq!(
            p.push(quote(1, 1_001 + round * 3)),
            Admission::Queued,
            "round {round}"
        );
        assert!(p.is_settled(), "the waiting quote was superseded");
        c.try_recv().unwrap();
        p.push(trade(7, 1_002 + round * 3));
        assert!(
            p.dirty.len() <= 1,
            "round {round}: {} entries",
            p.dirty.len()
        );
    }
}

#[test]
fn a_lost_trade_and_a_lost_control_event_are_marked_apart_even_when_both_wait() {
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..90u64 {
        p.push(trade(1, i));
    }
    for i in 0..10u64 {
        assert_eq!(p.push(halt(2, 100 + i)), Admission::Queued);
    }
    assert_eq!(c.depth(), 100);
    assert_eq!(p.push(trade(1, 200)), Admission::Dropped);
    assert_eq!(p.push(halt(2, 201)), Admission::Dropped);
    assert_eq!(p.push(trade(1, 202)), Admission::Dropped);
    assert_eq!(drain(&mut c).len(), 100);
    p.tick();
    let mut got = drain(&mut c);
    got.sort_by_key(|d| match d {
        Delivery::Gap(g) => g.count,
        _ => 0,
    });
    assert_eq!(
        got,
        [
            Delivery::Gap(Gap {
                lost: Lost::Control,
                count: 1,
                first_ts: 201,
                last_ts: 201
            }),
            Delivery::Gap(Gap {
                lost: Lost::Trades,
                count: 2,
                first_ts: 200,
                last_ts: 202
            }),
        ]
    );
}

#[test]
fn a_skip_the_gateway_reports_is_a_marker_in_order_apart_from_trades_and_control_losses() {
    let (mut p, mut c) = channel(small()).unwrap();
    p.push(trade(1, 10));
    p.note_skip(11);
    p.note_skip(12);
    p.push(trade(1, 13));
    assert_eq!(
        drain(&mut c),
        [
            Delivery::Event(trade(1, 10)),
            Delivery::Gap(Gap {
                lost: Lost::Skipped,
                count: 1,
                first_ts: 11,
                last_ts: 11
            }),
            Delivery::Gap(Gap {
                lost: Lost::Skipped,
                count: 1,
                first_ts: 12,
                last_ts: 12
            }),
            Delivery::Event(trade(1, 13)),
        ],
        "each notice is a marker at once when there is room"
    );
    assert_eq!(p.stats().gaps, 2);
    // With no room the notices gather into one marker, kept apart from the losses of trades and of control
    // events, and are delivered when there is room.
    let (mut p, mut c) = channel(small()).unwrap();
    for i in 0..200u64 {
        p.push(trade(1, i));
    }
    for i in 0..10u64 {
        p.push(halt(1, 1_000 + i));
    }
    assert_eq!(c.depth(), 100, "full");
    p.note_skip(2_000);
    p.note_skip(2_001);
    assert!(!p.is_settled());
    assert_eq!(drain(&mut c).len(), 100);
    p.tick();
    assert!(p.is_settled());
    let rest = drain(&mut c);
    assert!(
        rest.contains(&Delivery::Gap(Gap {
            lost: Lost::Skipped,
            count: 2,
            first_ts: 2_000,
            last_ts: 2_001
        })),
        "{rest:?}"
    );
    assert!(
        rest.iter()
            .any(|d| matches!(d, Delivery::Gap(g) if g.lost == Lost::Control))
    );
    assert_eq!(
        rest.iter()
            .filter(|d| matches!(d, Delivery::Gap(g) if g.lost == Lost::Trades))
            .count(),
        0
    );
}
