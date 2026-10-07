//! Session-by-session state (premarket, regular, after-hours) kept beside Tier 0.

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_engine::{SessionState, Sessions, Tier0};

const SEC: Nanos = 1_000_000_000;
const MIN: Nanos = 60 * SEC;

fn day(y: i32, m: u8, d: u8) -> SessionTimes {
    Calendar::us_equities()
        .times(Date::new(y, m, d).unwrap())
        .unwrap()
        .expect("a trading day")
}

fn trade(inst: u32, ts_recv: Nanos, px_cents: i64, size: u32) -> Event {
    Event::Trade(Trade {
        hdr: Header {
            ts_event: ts_recv.saturating_sub(7_000), // arrives a little after it happened
            ts_recv,
            seq: 0,
            instrument: inst,
            provider: ProviderId::Synthetic,
        },
        px: Px::from_cents(px_cents),
        size,
        flags: TradeFlags::NONE,
    })
}

fn tier0(t: SessionTimes) -> Tier0 {
    let mut t0 = Tier0::new(4);
    t0.set_day(t);
    t0
}

#[test]
fn a_trade_belongs_to_the_session_it_arrived_in_and_the_edges_go_to_the_later_one() {
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    let edges = [
        (t.premarket - 1, 1),       // before the day: kept nowhere
        (t.premarket, 2),           // premarket
        (t.open - 1, 3),            // premarket
        (t.open, 4),                // regular
        (t.close - 1, 5),           // regular
        (t.close, 6),               // after-hours
        (t.after_hours_end - 1, 7), // after-hours
        (t.after_hours_end, 8),     // after the day: kept nowhere
    ];
    for (ts, px) in edges {
        t0.on_event(&trade(0, ts, px, 100));
    }
    let s = t0.session(0).unwrap();
    assert_eq!(
        (s.premarket.volume, s.premarket.low, s.premarket.high),
        (200, Some(Px::from_cents(2)), Some(Px::from_cents(3)))
    );
    assert_eq!(
        (s.regular.volume, s.regular.low, s.regular.high),
        (200, Some(Px::from_cents(4)), Some(Px::from_cents(5)))
    );
    assert_eq!(
        (s.after_hours.volume, s.after_hours.low, s.after_hours.high),
        (200, Some(Px::from_cents(6)), Some(Px::from_cents(7)))
    );
    assert_eq!(s.open, Some((Px::from_cents(4), t.open)));
}

#[test]
fn tier0s_own_day_wide_fields_still_count_everything() {
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    t0.on_event(&trade(0, t.premarket + SEC, 900, 100)); // a premarket spike
    t0.on_event(&trade(0, t.open + SEC, 500, 100));
    let day_wide = t0.symbol(0).unwrap();
    assert_eq!(day_wide.day_high, Some(Px::from_cents(900)));
    assert_eq!(day_wide.volume, 200);
    let s = t0.session(0).unwrap();
    assert_eq!(s.regular.high, Some(Px::from_cents(500)));
    assert_eq!(s.regular.vwap(), Some(Px::from_cents(500)));
    assert_eq!(s.premarket.high, Some(Px::from_cents(900)));
}

#[test]
fn the_regular_vwap_is_anchored_at_the_open_and_rounds_down() {
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    t0.on_event(&trade(0, t.premarket + SEC, 1_000, 1_000)); // not in it
    t0.on_event(&trade(0, t.open + SEC, 1_000, 1)); // $10.00 x 1
    t0.on_event(&trade(0, t.open + 2 * SEC, 1_001, 2)); // $10.01 x 2
    let v = t0.session(0).unwrap().regular.vwap().unwrap();
    // (10.00 + 2 x 10.01) / 3 = 10.0066666..., rounded down to the billionth
    assert_eq!(v.raw(), 10_006_666_666);
    assert_eq!(t0.session(0).unwrap().regular.volume, 3);
}

#[test]
fn the_open_is_the_first_regular_trade_to_arrive_not_the_best_or_the_earliest_event_time() {
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    // The first to arrive carries a later event time than the second (a late report): still the open.
    let mut first = trade(0, t.open + 300_000_000, 427, 100);
    let mut second = trade(0, t.open + 400_000_000, 428, 100);
    if let (Event::Trade(a), Event::Trade(b)) = (&mut first, &mut second) {
        a.hdr.ts_event = t.open + 290_000_000;
        b.hdr.ts_event = t.open + 100_000_000;
    }
    t0.on_event(&first);
    t0.on_event(&second);
    assert_eq!(t0.session(0).unwrap().open.unwrap().0, Px::from_cents(427));
}

#[test]
fn the_windows_from_the_open_end_exactly_at_one_five_and_fifteen_minutes() {
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    let at = |d: Nanos| t.open + d;
    let evs = [
        (at(0), 100, 10),             // in all windows
        (at(MIN - 1), 101, 20),       // last ns of the first minute
        (at(MIN), 102, 40),           // first ns of the second minute: not in the first minute
        (at(5 * MIN - 1), 103, 80),   // last ns of 5 minutes
        (at(5 * MIN), 104, 160),      // not in 5 minutes, in 15
        (at(15 * MIN - 1), 105, 320), // last ns of 15 minutes
        (at(15 * MIN), 106, 640),     // in none
    ];
    for (ts, px, size) in evs {
        t0.on_event(&trade(0, ts, px, size));
    }
    let s = t0.session(0).unwrap();
    assert_eq!(s.first_minute_volume, 10 + 20);
    assert_eq!(s.first_5m_volume, 10 + 20 + 40 + 80);
    assert_eq!(
        (s.range_5m.low, s.range_5m.high),
        (Some(Px::from_cents(100)), Some(Px::from_cents(103)))
    );
    assert_eq!(
        (s.range_15m.low, s.range_15m.high),
        (Some(Px::from_cents(100)), Some(Px::from_cents(105)))
    );
    assert_eq!(s.regular.volume, 10 + 20 + 40 + 80 + 160 + 320 + 640);
}

#[test]
fn an_early_close_day_and_a_winter_day_use_their_own_boundaries() {
    // Friday 27 November 2026: the regular session ends at 13:00 New York time, after-hours at 17:00.
    let t = day(2026, 11, 27);
    let mut t0 = tier0(t);
    t0.on_event(&trade(0, t.close - 1, 100, 10));
    t0.on_event(&trade(0, t.close, 101, 20));
    t0.on_event(&trade(0, t.after_hours_end, 102, 40));
    let s = t0.session(0).unwrap();
    assert_eq!((s.regular.volume, s.after_hours.volume), (10, 20));
    // Monday 2 November 2026 (winter time): the open is 14:30 UTC.
    let w = day(2026, 11, 2);
    let mut t1 = tier0(w);
    let utc_1430 = (w.open / SEC) % 86_400;
    assert_eq!(utc_1430, 14 * 3600 + 1800);
    t1.on_event(&trade(0, w.open - 1, 100, 5));
    t1.on_event(&trade(0, w.open, 100, 7));
    let s = t1.session(0).unwrap();
    assert_eq!((s.premarket.volume, s.regular.volume), (5, 7));
}

#[test]
fn nothing_is_kept_until_a_day_is_set_and_a_new_day_or_a_reset_clears_it() {
    let t = day(2026, 10, 2);
    let mut t0 = Tier0::new(4);
    assert!(t0.day().is_none());
    t0.on_event(&trade(0, t.open + SEC, 100, 100));
    assert_eq!(t0.session(0).unwrap(), &SessionState::default());
    assert_eq!(
        t0.symbol(0).unwrap().volume,
        100,
        "Tier 0 itself still counts it"
    );
    t0.set_day(t);
    t0.on_event(&trade(0, t.open + SEC, 100, 100));
    assert_eq!(t0.session(0).unwrap().regular.volume, 100);
    assert_eq!(t0.day(), Some(&t));
    // A new day: state cleared, the boundaries replaced.
    let next = day(2026, 10, 5);
    t0.set_day(next);
    assert_eq!(t0.session(0).unwrap(), &SessionState::default());
    t0.on_event(&trade(0, next.open + SEC, 100, 3));
    assert_eq!(t0.session(0).unwrap().regular.volume, 3);
    // reset_day forgets the day as well.
    t0.reset_day();
    assert!(t0.day().is_none());
    t0.on_event(&trade(0, next.open + 2 * SEC, 100, 3));
    assert_eq!(t0.session(0).unwrap(), &SessionState::default());
}

#[test]
fn quotes_and_other_events_and_unknown_instruments_change_nothing() {
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    t0.on_event(&Event::Quote(Quote {
        hdr: Header {
            ts_event: t.open,
            ts_recv: t.open,
            seq: 0,
            instrument: 0,
            provider: ProviderId::Synthetic,
        },
        bid_px: Px::from_cents(100),
        ask_px: Px::from_cents(101),
        bid_sz: 1,
        ask_sz: 1,
    }));
    assert_eq!(t0.session(0).unwrap(), &SessionState::default());
    t0.on_event(&trade(99, t.open + SEC, 100, 1)); // outside the table
    assert!(t0.session(99).is_none());
    assert_eq!(t0.unknown_events(), 1);
    for i in 0..4 {
        assert_eq!(t0.session(i).unwrap(), &SessionState::default());
    }
}

/// A SplitMix64, so the stream does not depend on a crate.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[test]
fn random_streams_agree_with_a_plain_recomputation() {
    for (seed, d) in [
        (1u64, day(2026, 10, 2)),
        (2, day(2026, 11, 27)),
        (3, day(2026, 3, 9)),
        (4, day(2025, 7, 3)),
    ] {
        let mut rng = Rng(seed);
        let mut events = Vec::new();
        let mut ts = d.premarket - 10 * MIN;
        let end = d.after_hours_end + 10 * MIN;
        while ts < end {
            // Dense around the open and the close so the windows and edges are exercised.
            ts += rng.next() % (3 * SEC);
            let inst = (rng.next() % 4) as u32;
            let px = 1_000 + (rng.next() % 500) as i64;
            let size = 1 + (rng.next() % 300) as u32;
            events.push(trade(inst, ts, px, size));
        }
        let mut t0 = tier0(d);
        for e in &events {
            t0.on_event(e);
        }
        for inst in 0..4u32 {
            let mine: Vec<(Nanos, i64, u32)> = events
                .iter()
                .filter_map(|e| match e {
                    Event::Trade(t) if t.hdr.instrument == inst => {
                        Some((t.hdr.ts_recv, t.px.to_cents(), t.size))
                    }
                    _ => None,
                })
                .collect();
            let pick = |lo: Nanos, hi: Nanos| {
                mine.iter()
                    .filter(move |&&(ts, _, _)| ts >= lo && ts < hi)
                    .copied()
                    .collect::<Vec<_>>()
            };
            let vol = |v: &[(Nanos, i64, u32)]| v.iter().map(|x| u64::from(x.2)).sum::<u64>();
            let hi = |v: &[(Nanos, i64, u32)]| v.iter().map(|x| Px::from_cents(x.1)).max();
            let lo = |v: &[(Nanos, i64, u32)]| v.iter().map(|x| Px::from_cents(x.1)).min();
            let s = t0.session(inst).unwrap();
            let (pm, rg, ah) = (
                pick(d.premarket, d.open),
                pick(d.open, d.close),
                pick(d.close, d.after_hours_end),
            );
            assert_eq!(
                (s.premarket.volume, s.premarket.high, s.premarket.low),
                (vol(&pm), hi(&pm), lo(&pm)),
                "seed {seed} premarket"
            );
            assert_eq!(
                (s.regular.volume, s.regular.high, s.regular.low),
                (vol(&rg), hi(&rg), lo(&rg)),
                "seed {seed} regular"
            );
            assert_eq!(
                (s.after_hours.volume, s.after_hours.high, s.after_hours.low),
                (vol(&ah), hi(&ah), lo(&ah)),
                "seed {seed} after-hours"
            );
            let w1 = pick(d.open, d.open + MIN);
            let w5 = pick(d.open, d.open + 5 * MIN);
            let w15 = pick(d.open, d.open + 15 * MIN);
            assert_eq!(s.first_minute_volume, vol(&w1));
            assert_eq!(s.first_5m_volume, vol(&w5));
            assert_eq!((s.range_5m.high, s.range_5m.low), (hi(&w5), lo(&w5)));
            assert_eq!((s.range_15m.high, s.range_15m.low), (hi(&w15), lo(&w15)));
            assert_eq!(s.open.map(|o| o.0), rg.first().map(|x| Px::from_cents(x.1)));
            let notional: u128 = rg
                .iter()
                .map(|x| Px::from_cents(x.1).raw() as u128 * u128::from(x.2))
                .sum();
            assert_eq!(s.regular.notional, notional);
        }
    }
}

#[test]
fn the_state_is_copy_and_a_few_hundred_bytes_and_events_never_reallocate() {
    // 256 bytes or so a symbol: 13,000 symbols are a few megabytes.
    assert!(
        std::mem::size_of::<SessionState>() <= 320,
        "{}",
        std::mem::size_of::<SessionState>()
    );
    fn is_copy<T: Copy>() {}
    is_copy::<SessionState>();
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    let before = format!("{:p}", t0.session(0).unwrap());
    for i in 0..50_000u64 {
        t0.on_event(&trade(
            (i % 4) as u32,
            t.premarket + i * 400_000,
            500 + (i % 9) as i64,
            10,
        ));
    }
    assert_eq!(
        before,
        format!("{:p}", t0.session(0).unwrap()),
        "the array never moved"
    );
}

#[test]
fn an_empty_span_has_no_vwap_and_a_single_share_is_its_own_price() {
    assert_eq!(SessionState::default().regular.vwap(), None);
    let t = day(2026, 10, 2);
    let mut t0 = tier0(t);
    t0.on_event(&trade(0, t.open + SEC, 1_234, 1));
    assert_eq!(
        t0.session(0).unwrap().regular.vwap(),
        Some(Px::from_cents(1_234))
    );
}

#[test]
fn the_sessions_array_counts_trades_for_instruments_it_does_not_have() {
    let t = day(2026, 10, 2);
    let mut s = Sessions::new(2);
    s.set_day(t);
    s.on_event(&trade(5, t.open + SEC, 100, 1));
    s.on_event(&trade(1, t.open + SEC, 100, 1));
    assert_eq!(s.unknown_events(), 1);
    assert_eq!(s.state(1).unwrap().regular.volume, 1);
    assert!(s.state(5).is_none());
}
