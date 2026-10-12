//! The hourly bias from scripted closes: the series, the settle, the ties, the missing bars, and the live value equal to one offline pass.

use std::cell::RefCell;
use std::collections::BTreeMap;

use tf_core::{NANOS_PER_SEC, Nanos, Px};
use tf_engine::{Ema, Seed};
use tf_universe::{HistInfo, RefInfo};

use crate::hourly_bias::{BAR_SECS, Bias, Close, HourlyBias};
use crate::intent::Side;

const D: i64 = 1_000_000_000;
const SEC: u64 = NANOS_PER_SEC;
const HOUR_S: u64 = 3_600;
/// 09:30 New York on a summer day, as seconds since the epoch.
const OPEN_S: u64 = 1_780_000_000 - 1_780_000_000 % 900;
const OPEN: Nanos = OPEN_S * SEC;

fn dollars(x: i64) -> Px {
    Px::from_raw(x * D)
}

/// The state the snapshot would carry after `closes` hourly closes of the same price `x` dollars.
fn snapshot(x: i64, closes: u32) -> RefInfo {
    let mut e = Ema::new(100, Seed::Sma);
    for _ in 0..closes {
        e.update(x * D);
    }
    state_of(&e)
}

fn state_of(e: &Ema) -> RefInfo {
    let (s, c) = e
        .state()
        .map_or((None, None), |(s, c)| (Some(s), Some(i64::from(c))));
    RefInfo {
        hist: HistInfo {
            ema100h_state: s,
            ema100h_count: c,
            ..HistInfo::default()
        },
        ..RefInfo::default()
    }
}

/// A day of one name, id 7, from a snapshot of an average at $50.
fn day() -> HourlyBias {
    let mut b = HourlyBias::new();
    b.start_day(OPEN);
    assert!(b.add(7, &snapshot(50, 100)));
    b
}

/// The closes of the buckets by their end, hour 1 = 10:30.
fn closes(list: &[(u64, i64)]) -> impl Fn(u32, u64) -> Close {
    let m: BTreeMap<u64, i64> = list
        .iter()
        .map(|&(h, c)| (OPEN_S + h * HOUR_S - BAR_SECS, c))
        .collect();
    move |_, start| {
        m.get(&start)
            .map_or(Close::Missing, |&c| Close::Is(dollars(c)))
    }
}

#[test]
fn there_is_no_bias_before_the_first_bucket_has_ended() {
    let mut b = day();
    b.advance(OPEN + 3_599 * SEC, 0, closes(&[(1, 60)]));
    assert_eq!(b.bias(7), None);
    assert!(!b.allows(7, Side::Buy));
    b.advance(OPEN + 3_600 * SEC, 0, closes(&[(1, 60)]));
    assert_eq!(b.bias(7), Some(Bias::Up));
}

#[test]
fn a_bucket_is_folded_only_after_the_settle() {
    let mut b = day();
    let settle = 5 * SEC;
    b.advance(OPEN + 3_604 * SEC, settle, closes(&[(1, 60)]));
    assert_eq!(b.bias(7), None);
    b.advance(OPEN + 3_605 * SEC, settle, closes(&[(1, 60)]));
    assert_eq!(b.bias(7), Some(Bias::Up));
}

#[test]
fn a_close_under_the_average_is_down_and_over_is_up() {
    let mut b = day();
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 40)]));
    assert_eq!(b.bias(7), Some(Bias::Down));
    b.advance(OPEN + 2 * HOUR_S * SEC, 0, closes(&[(1, 40), (2, 70)]));
    assert_eq!(b.bias(7), Some(Bias::Up));
}

#[test]
fn a_close_equal_to_the_average_is_no_bias() {
    let mut b = day();
    // The average is $50.00 exactly, and a close of $50 moves it to $50 exactly.
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 50)]));
    assert_eq!(b.average(7), Some(50 * D));
    assert_eq!(b.bias(7), None);
}

#[test]
fn only_the_last_completed_close_decides() {
    let mut b = day();
    b.advance(
        OPEN + 3 * HOUR_S * SEC,
        0,
        closes(&[(1, 90), (2, 90), (3, 40)]),
    );
    assert_eq!(
        b.bias(7),
        Some(Bias::Down),
        "the average is still near $50 and the last close is $40"
    );
}

#[test]
fn the_bias_allows_a_buy_with_up_and_a_sell_with_down_and_nothing_else() {
    let mut b = day();
    assert!(!b.allows(7, Side::Buy) && !b.allows(7, Side::Sell) && !b.allows(7, Side::SellShort));
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 60)]));
    assert!(b.allows(7, Side::Buy));
    assert!(!b.allows(7, Side::Sell) && !b.allows(7, Side::SellShort));
    let mut b = day();
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 40)]));
    assert!(!b.allows(7, Side::Buy));
    assert!(b.allows(7, Side::Sell) && b.allows(7, Side::SellShort));
}

#[test]
fn a_late_advance_folds_every_bucket_in_order_asking_for_the_bar_that_ends_with_each() {
    let mut b = day();
    let asked = RefCell::new(Vec::new());
    b.advance(OPEN + 7 * HOUR_S * SEC, 0, |_, start| {
        asked.borrow_mut().push(start);
        Close::Is(dollars(55))
    });
    let want: Vec<u64> = (1..=6).map(|h| OPEN_S + h * HOUR_S - BAR_SECS).collect();
    assert_eq!(
        *asked.borrow(),
        want,
        "10:30 to 15:30, the quarter hour before each"
    );
    // The stub to the close is not folded in the day.
    b.advance(OPEN + 8 * HOUR_S * SEC, 0, |_, _| {
        panic!("asked past 15:30")
    });
}

#[test]
fn a_missing_bar_breaks_the_name_for_the_day() {
    let mut b = day();
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 60)]));
    assert_eq!(b.bias(7), Some(Bias::Up));
    // 11:30 has no bar: the series is not the offline one from here on, whatever comes after.
    b.advance(OPEN + 3 * HOUR_S * SEC, 0, closes(&[(3, 70)]));
    assert_eq!(b.bias(7), None);
    assert!(!b.allows(7, Side::Buy));
}

#[test]
fn a_name_the_snapshot_has_no_state_for_has_no_bias() {
    let mut b = HourlyBias::new();
    b.start_day(OPEN);
    assert!(!b.add(1, &RefInfo::default()));
    assert!(!b.add(2, &snapshot(50, 99)), "short of the seed");
    // A state with a count that is short of the seed, or that is not a count at all, is refused too.
    let mut short = snapshot(50, 100);
    short.hist.ema100h_count = Some(99);
    assert!(!b.add(3, &short));
    short.hist.ema100h_count = Some(-1);
    assert!(!b.add(4, &short));
    short.hist.ema100h_count = Some(100);
    short.hist.ema100h_state = None;
    assert!(!b.add(5, &short), "a count without a state");
    assert!(b.is_empty());
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 60)]));
    assert_eq!(b.bias(1), None);
}

#[test]
fn a_new_day_starts_clean() {
    let mut b = day();
    b.advance(OPEN + 2 * HOUR_S * SEC, 0, closes(&[(1, 60), (2, 61)]));
    let next = OPEN + 86_400 * SEC;
    b.start_day(next);
    assert!(b.is_empty());
    assert_eq!(b.bias(7), None);
}

#[test]
fn the_state_carried_through_a_day_equals_one_pass_over_the_same_closes() {
    // Two hundred past closes of a drifting price, the snapshot taken after the first hundred and forty, and a day of six closes folded on it.
    let past: Vec<i64> = (0..140).map(|i| 40 * D + (i * 37 % 91) * D / 10).collect();
    let today: [i64; 6] = [
        43 * D,
        45 * D + 123_456_789,
        44 * D,
        41 * D + 7,
        46 * D,
        47 * D + 999,
    ];
    let mut head = Ema::new(100, Seed::Sma);
    past.iter().for_each(|&x| head.update(x));
    let mut b = HourlyBias::new();
    b.start_day(OPEN);
    assert!(b.add(7, &state_of(&head)));
    let m: BTreeMap<u64, i64> = today
        .iter()
        .enumerate()
        .map(|(i, &c)| (OPEN_S + (i as u64 + 1) * HOUR_S - BAR_SECS, c))
        .collect();
    b.advance(OPEN + 7 * HOUR_S * SEC, 0, |_, s| {
        m.get(&s)
            .map_or(Close::Missing, |&c| Close::Is(Px::from_raw(c)))
    });
    let mut whole = Ema::new(100, Seed::Sma);
    past.iter()
        .chain(today.iter())
        .for_each(|&x| whole.update(x));
    assert_eq!(b.average(7), whole.value());
    let want = if today[5] > whole.value().unwrap() {
        Bias::Up
    } else {
        Bias::Down
    };
    assert_eq!(b.bias(7), Some(want));
}

#[test]
fn names_are_followed_apart() {
    let mut b = HourlyBias::new();
    b.start_day(OPEN);
    assert!(b.add(1, &snapshot(50, 100)) && b.add(2, &snapshot(80, 100)));
    b.advance(OPEN + HOUR_S * SEC, 0, |id, start| {
        if start == OPEN_S + HOUR_S - BAR_SECS {
            Close::Is(dollars(if id == 1 { 60 } else { 70 }))
        } else {
            Close::Missing
        }
    });
    assert_eq!(b.bias(1), Some(Bias::Up));
    assert_eq!(b.bias(2), Some(Bias::Down));
    b.forget(1);
    assert_eq!(b.bias(1), None);
}

#[test]
fn a_bar_that_has_not_closed_waits_for_the_next_call_and_the_others_go_on() {
    let mut b = HourlyBias::new();
    b.start_day(OPEN);
    assert!(b.add(1, &snapshot(50, 100)) && b.add(2, &snapshot(50, 100)));
    let ready = RefCell::new(false);
    let close = |id: u32, start: u64| {
        if id == 1 && !*ready.borrow() {
            Close::NotYet
        } else if start == OPEN_S + HOUR_S - BAR_SECS {
            Close::Is(dollars(60))
        } else {
            Close::Missing
        }
    };
    b.advance(OPEN + HOUR_S * SEC, 0, close);
    assert_eq!(b.bias(1), None, "waiting, not broken");
    assert_eq!(b.bias(2), Some(Bias::Up));
    *ready.borrow_mut() = true;
    b.advance(OPEN + HOUR_S * SEC, 0, close);
    assert_eq!(b.bias(1), Some(Bias::Up));
}

/// A day of one name, id 7, from a snapshot of an average at $50 and a previous close of `prev` dollars, carrying it into the morning.
fn carrying_day(prev: Option<i64>) -> HourlyBias {
    let mut b = HourlyBias::new().carrying_prior(true);
    b.start_day(OPEN);
    let mut r = snapshot(50, 100);
    r.hist.prev_close = prev.map(|p| p * D);
    assert!(b.add(7, &r));
    b
}

#[test]
fn carrying_the_previous_close_gives_a_bias_before_the_first_bucket_ends() {
    assert_eq!(carrying_day(Some(60)).bias(7), Some(Bias::Up));
    assert_eq!(carrying_day(Some(40)).bias(7), Some(Bias::Down));
    assert_eq!(carrying_day(Some(50)).bias(7), None, "a tie");
    assert_eq!(carrying_day(None).bias(7), None, "no previous close");
    assert_eq!(carrying_day(Some(0)).bias(7), None, "not a price");
    assert!(carrying_day(Some(60)).allows(7, Side::Buy));
    assert!(carrying_day(Some(40)).allows(7, Side::SellShort));
}

#[test]
fn without_carrying_the_previous_close_is_not_read() {
    let mut b = HourlyBias::new();
    b.start_day(OPEN);
    let mut r = snapshot(50, 100);
    r.hist.prev_close = Some(60 * D);
    assert!(b.add(7, &r));
    assert_eq!(b.bias(7), None);
}

#[test]
fn the_first_close_of_the_day_replaces_the_previous_close() {
    let mut b = carrying_day(Some(60));
    b.advance(OPEN + HOUR_S * SEC, 0, closes(&[(1, 40)]));
    assert_eq!(b.bias(7), Some(Bias::Down));
    // A bucket that is waited for leaves the previous close standing.
    let mut b = carrying_day(Some(60));
    b.advance(OPEN + HOUR_S * SEC, 0, |_, _| Close::NotYet);
    assert_eq!(b.bias(7), Some(Bias::Up));
}

#[test]
fn a_broken_name_has_no_bias_whatever_the_previous_close() {
    let mut b = carrying_day(Some(60));
    b.advance(OPEN + HOUR_S * SEC, 0, |_, _| Close::Missing);
    assert_eq!(b.bias(7), None);
}
