use std::io::Cursor;

use tf_calendar::{Calendar, Date};
use tf_universe::{CUMVOL_CHECKPOINTS, HISTORY_FEATURES, RefRow, StaticFeature};

use crate::history::*;
use crate::{date_days, date_text};

const D: i64 = 1_000_000_000;
const NANOS: u64 = 1_000_000_000;

fn date(s: &str) -> Date {
    Date::from_days(date_days(s).unwrap())
}

/// The UTC nanoseconds `minutes` after the 09:30 open of `d` (negative: before it).
fn at(d: &str, minutes: i64) -> u64 {
    let t = Calendar::us_equities().times(date(d)).unwrap().unwrap();
    (t.open as i64 + minutes * 60 * NANOS as i64) as u64
}

fn up_to(d: &str) -> i64 {
    date_days(d).unwrap()
}

fn small() -> HistoryParams {
    HistoryParams {
        sessions: 60,
        average_over: 2,
        min_days: 1,
    }
}

fn col(row: &HistoryRow, f: StaticFeature) -> Option<i64> {
    let i = HISTORY_FEATURES.iter().position(|x| *x == f).unwrap();
    row.values[i]
}

fn add(h: &mut MinuteHistory, ts: u64, sym: &str, high: i64, low: i64, close: i64, vol: u64) {
    // The body is the close alone, and no cap, unless the test says otherwise.
    h.add(ts, sym, close * D, high * D, low * D, close * D, vol)
        .unwrap();
}

#[test]
fn a_worked_example_gives_the_hand_columns() {
    let mut h = MinuteHistory::new(up_to("2026-10-02"), u32::MAX);
    // 30 September (Wed): a premarket bar and two regular ones.
    add(&mut h, at("2026-09-30", -330 + 1), "X", 90, 89, 90, 7);
    add(&mut h, at("2026-09-30", 0), "X", 102, 99, 101, 100);
    add(&mut h, at("2026-09-30", 10), "X", 103, 100, 102, 50);
    // 1 October: the open, and the last minute of the session.
    add(&mut h, at("2026-10-01", 0), "X", 105, 101, 104, 200);
    add(&mut h, at("2026-10-01", 389), "X", 106, 103, 105, 20);
    // 2 October.
    add(&mut h, at("2026-10-02", 0), "X", 108, 104, 107, 300);
    add(&mut h, at("2026-10-02", 3), "X", 109, 106, 108, 30);
    add(&mut h, at("2026-10-02", 59), "X", 110, 107, 109, 40);
    let (rows, rep) = h.build(small()).unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(
        (rep.as_of.as_str(), rep.sessions, rep.early_closes),
        ("2026-10-02", 3, 0)
    );
    // The previous session is 2 October: the highest high, the lowest low, the last bar's close.
    assert_eq!(col(r, StaticFeature::PrevHigh), Some(110 * D));
    assert_eq!(col(r, StaticFeature::PrevLow), Some(104 * D));
    assert_eq!(col(r, StaticFeature::PrevClose), Some(109 * D));
    // Volume averages run over the last two sessions (1 and 2 October).
    assert_eq!(col(r, StaticFeature::VolFirst1), Some((200 + 300) / 2));
    assert_eq!(col(r, StaticFeature::VolFirst5), Some((200 + 330) / 2));
    assert_eq!(
        col(r, StaticFeature::VolPremarket),
        Some(0),
        "traded in the window, no premarket volume"
    );
    // Cumulative: 1 October 200 throughout (its last-minute bar starts after the 15:30 checkpoint); 2 October 330
    // then 370 from the +59 bar.
    let cum = |f| col(r, f);
    assert_eq!(cum(StaticFeature::CumVol0935), Some((200 + 330) / 2));
    assert_eq!(cum(StaticFeature::CumVol1000), Some((200 + 330) / 2));
    assert_eq!(cum(StaticFeature::CumVol1030), Some((200 + 370) / 2));
    assert_eq!(cum(StaticFeature::CumVol1530), Some((200 + 370) / 2));
    // Three sessions are too few for the ATR (15) and three closes for the EMA (100).
    assert_eq!(col(r, StaticFeature::Atr14), None);
    assert_eq!(col(r, StaticFeature::Ema100hState), None);
    assert_eq!(col(r, StaticFeature::Ema100hCount), None);
}

#[test]
fn each_cumulative_checkpoint_counts_the_bars_that_start_before_it_to_the_minute() {
    let mut h = MinuteHistory::new(up_to("2026-10-02"), u32::MAX);
    // Bars at the minutes either side of each checkpoint, with volumes that are distinct powers of two.
    let minutes = [
        0, 4, 5, 29, 30, 59, 60, 89, 90, 149, 150, 269, 270, 359, 360, 389,
    ];
    for (i, m) in minutes.iter().enumerate() {
        add(&mut h, at("2026-10-02", *m), "B", 10, 10, 10, 1 << i);
    }
    let p = HistoryParams {
        sessions: 1,
        average_over: 1,
        min_days: 1,
    };
    let (rows, _) = h.build(p).unwrap();
    let r = &rows[0];
    let want_minutes: Vec<u32> = CUMVOL_CHECKPOINTS.iter().map(|c| c.1).collect();
    assert_eq!(want_minutes, [5, 30, 60, 90, 150, 270, 360]);
    for (f, m) in CUMVOL_CHECKPOINTS {
        let want: u64 = minutes
            .iter()
            .enumerate()
            .filter(|(_, x)| **x < i64::from(m))
            .map(|(i, _)| 1u64 << i)
            .sum();
        assert_eq!(col(r, f), Some(want as i64), "{}", f.name());
    }
    // The first minute holds only the bar at 0, the first five minutes the bars at 0 and 4.
    assert_eq!(col(r, StaticFeature::VolFirst1), Some(1));
    assert_eq!(col(r, StaticFeature::VolFirst5), Some(1 + 2));
}

#[test]
fn the_sessions_follow_new_york_time_on_both_sides_of_a_daylight_saving_change() {
    // Friday 30 October (EDT, the open is 13:30 UTC) and Monday 2 November (EST, 14:30 UTC).
    let mut h = MinuteHistory::new(up_to("2026-11-02"), u32::MAX);
    let utc = |d: &str, hh: u64, mm: u64| {
        (date_days(d).unwrap() as u64 * 86_400 + hh * 3600 + mm * 60) * NANOS
    };
    add(&mut h, utc("2026-10-30", 13, 30), "D", 20, 20, 20, 11);
    add(&mut h, utc("2026-11-02", 14, 30), "D", 20, 20, 20, 13);
    // 13:30 UTC on the Monday is 08:30 New York time: premarket.
    add(&mut h, utc("2026-11-02", 13, 30), "D", 20, 20, 20, 5);
    // And 13:30 UTC on the Friday minus a minute is 09:29 EDT, premarket too.
    add(&mut h, utc("2026-10-30", 13, 29), "D", 20, 20, 20, 3);
    let (rows, rep) = h.build(small()).unwrap();
    assert_eq!(rep.sessions, 2);
    let r = &rows[0];
    assert_eq!(col(r, StaticFeature::VolFirst1), Some((11 + 13) / 2));
    assert_eq!(col(r, StaticFeature::VolPremarket), Some((3 + 5) / 2));
}

#[test]
fn an_early_close_is_left_out_of_the_time_of_day_columns_only() {
    // Wednesday 25 November, Thanksgiving (closed), Friday 27 November (closes at 13:00).
    let mut h = MinuteHistory::new(up_to("2026-11-27"), u32::MAX);
    add(&mut h, at("2026-11-25", 0), "E", 30, 30, 30, 100);
    add(&mut h, at("2026-11-25", 250), "E", 30, 30, 30, 1000);
    add(&mut h, at("2026-11-27", 0), "E", 30, 30, 30, 200);
    add(&mut h, at("2026-11-27", 120), "E", 30, 30, 30, 20);
    // After the 13:00 close (210 minutes after the open): after-hours, not used.
    add(&mut h, at("2026-11-27", 215), "E", 31, 31, 31, 9999);
    let (rows, rep) = h.build(small()).unwrap();
    assert_eq!(rep.sessions, 2, "Thanksgiving is not a missing session");
    assert_eq!(rep.early_closes, 1);
    assert_eq!(rep.bars_outside_sessions, 1);
    let r = &rows[0];
    assert_eq!(
        col(r, StaticFeature::VolFirst1),
        Some((100 + 200) / 2),
        "the open keeps both"
    );
    // Cumulative volume to 14:00 is the normal day's alone: 1100, not (1100 + 220) / 2.
    assert_eq!(col(r, StaticFeature::CumVol1400), Some(1100));
    assert_eq!(
        col(r, StaticFeature::CumVol1000),
        Some(100),
        "to 10:00 the normal day only has 100"
    );
    assert_eq!(
        col(r, StaticFeature::PrevClose),
        Some(30 * D),
        "the after-hours print is not the close"
    );
    assert_eq!(col(r, StaticFeature::PrevHigh), Some(30 * D));
}

#[test]
fn a_short_history_is_unknown_not_zero() {
    let mut h = MinuteHistory::new(up_to("2026-10-02"), u32::MAX);
    // Three sessions of data; symbol S traded on one of them only, T on all three.
    for d in ["2026-09-30", "2026-10-01", "2026-10-02"] {
        add(&mut h, at(d, 0), "T", 10, 10, 10, 50);
    }
    add(&mut h, at("2026-10-01", 0), "S", 10, 10, 10, 77);
    let p = HistoryParams {
        sessions: 60,
        average_over: 3,
        min_days: 2,
    };
    let (rows, _) = h.build(p).unwrap();
    let (s, t) = (&rows[0], &rows[1]);
    assert_eq!((s.symbol.as_str(), t.symbol.as_str()), ("S", "T"));
    // S traded on one session of the three: no volume column, and nothing for the session it missed.
    for f in [
        StaticFeature::VolFirst1,
        StaticFeature::VolFirst5,
        StaticFeature::VolPremarket,
        StaticFeature::CumVol0935,
        StaticFeature::PrevHigh,
        StaticFeature::PrevClose,
    ] {
        assert_eq!(col(s, f), None, "{}", f.name());
    }
    assert_eq!(col(t, StaticFeature::VolFirst1), Some(50));
    assert_eq!(col(t, StaticFeature::PrevClose), Some(10 * D));
}

/// Twenty consecutive sessions from Monday 7 September 2026 (Labor Day, 7 September, is a holiday: the
/// first is Tuesday 8 September).
fn sessions(n: usize) -> Vec<String> {
    let cal = Calendar::us_equities();
    let mut d = date("2026-09-08");
    let mut v = Vec::new();
    while v.len() < n {
        if cal.is_trading_day(d).unwrap() {
            v.push(date_text(d.days()));
        }
        d = d.next();
    }
    v
}

#[test]
fn the_atr_is_the_mean_of_fourteen_true_ranges_against_the_prior_session_and_needs_all_fifteen() {
    let days = sessions(20);
    let mut h = MinuteHistory::new(up_to(days.last().unwrap()), u32::MAX);
    // A deterministic walk: per session a high, low and close for symbol A; symbol B skips one of the last 15.
    let ohlc = |i: usize| -> (i64, i64, i64) {
        let base = 100 + (i as i64 * 7) % 13;
        (
            base + 3 + (i as i64 % 4),
            base - 2 - (i as i64 % 3),
            base + (i as i64 % 5) - 1,
        )
    };
    for (i, d) in days.iter().enumerate() {
        let (hi, lo, cl) = ohlc(i);
        // Two regular bars each: the high and low in the first, the close in the last.
        add(&mut h, at(d, 0), "A", hi, lo, hi.min(cl).max(lo), 10);
        add(&mut h, at(d, 100), "A", cl.max(lo), cl.min(hi), cl, 10);
        if i != 12 {
            add(&mut h, at(d, 0), "B", hi, lo, cl, 10);
        }
    }
    let (rows, _) = h.build(small()).unwrap();
    let (a, b) = (&rows[0], &rows[1]);
    // Independent recomputation from the sessions' highs, lows and closes.
    let mut sum = 0i64;
    for i in 20 - 14..20 {
        let (hi, lo, _) = ohlc(i);
        let pc = ohlc(i - 1).2;
        sum += (hi - lo).max((hi - pc).abs()).max((lo - pc).abs());
    }
    assert_eq!(col(a, StaticFeature::Atr14), Some(sum * D / 14));
    assert_eq!(
        col(b, StaticFeature::Atr14),
        None,
        "a session it did not trade: unknown, not a shorter average"
    );
    // A's previous session is the last, and its high and low are over its two bars.
    let (hi, lo, cl) = ohlc(19);
    assert_eq!(
        col(a, StaticFeature::PrevHigh),
        Some(hi.max(cl.max(lo)).max(hi.min(cl).max(lo)) * D)
    );
    assert_eq!(col(a, StaticFeature::PrevClose), Some(cl * D));
    let _ = lo;
}

#[test]
fn the_ema_state_is_the_engines_and_needs_a_hundred_hourly_closes() {
    let days = sessions(20);
    let mut h = MinuteHistory::new(up_to(days.last().unwrap()), u32::MAX);
    // One bar in each of the seven hours of every session, closes on a ramp; a quiet symbol with one bar a day.
    let mut closes: Vec<i64> = Vec::new();
    for (i, d) in days.iter().enumerate() {
        for k in 0..7 {
            let c = 50 + (i * 7 + k) as i64 / 3;
            add(&mut h, at(d, k as i64 * 60 + 5), "R", c + 1, c - 1, c, 10);
            closes.push(c);
        }
        add(&mut h, at(d, 5), "Q", 9, 9, 9, 10);
    }
    let (rows, _) = h.build(small()).unwrap();
    let (q, r) = (&rows[0], &rows[1]);
    assert_eq!(r.symbol, "R");
    assert_eq!(col(r, StaticFeature::Ema100hCount), Some(140));
    // The same series through the engine's average, the state it exports.
    let mut e = tf_engine::Ema::new(100, tf_engine::Seed::Sma);
    for c in &closes {
        e.update(c * D);
    }
    let (state, count) = e.state().unwrap();
    assert_eq!(
        (
            col(r, StaticFeature::Ema100hState),
            col(r, StaticFeature::Ema100hCount)
        ),
        (Some(state), Some(i64::from(count)))
    );
    // A symbol with 20 closes has no state to carry.
    assert_eq!(col(q, StaticFeature::Ema100hState), None);
    // The state resumes: the average carried to the end equals the average of the whole series.
    let (scaled, n) = (state, count);
    let mut resumed = tf_engine::Ema::resume(100, tf_engine::Seed::Sma, scaled, n).unwrap();
    resumed.update(99 * D);
    e.update(99 * D);
    assert_eq!(resumed.value(), e.value());
}

#[test]
fn an_hour_with_no_trade_is_skipped_and_a_close_is_the_last_bar_of_its_hour() {
    let days = sessions(20);
    let mut h = MinuteHistory::new(up_to(days.last().unwrap()), u32::MAX);
    for d in &days {
        // Hours 0 and 2 only; in hour 0 the later bar (with the lower close) wins whatever order they come in.
        add(&mut h, at(d, 50), "H", 20, 10, 12, 1);
        add(&mut h, at(d, 10), "H", 30, 5, 25, 1);
        add(&mut h, at(d, 130), "H", 20, 10, 15, 1);
    }
    let (rows, _) = h.build(small()).unwrap();
    // 20 sessions x 2 hours = 40 closes: too few, but the count says how many hours were seen.
    assert_eq!(col(&rows[0], StaticFeature::Ema100hCount), None);
    let mut h = MinuteHistory::new(up_to(days.last().unwrap()), u32::MAX);
    for d in &days {
        // Every hour of the session, the later bar (the lower close) listed first.
        for k in [0, 1, 2, 3, 4, 5, 6] {
            add(&mut h, at(d, 60 * k + 25), "H", 20, 10, 12, 1);
            add(&mut h, at(d, 60 * k + 5), "H", 30, 5, 25, 1);
        }
    }
    let (rows, _) = h.build(small()).unwrap();
    // Every hour's close is 12 (the bar at +25 is the later), so the state is exactly 12 dollars.
    assert_eq!(col(&rows[0], StaticFeature::Ema100hCount), Some(140));
    assert_eq!(
        col(&rows[0], StaticFeature::Ema100hState),
        Some((12 * D) << 16)
    );
}

#[test]
fn only_bars_on_or_before_the_date_are_read_and_the_as_of_is_the_last_session_with_bars() {
    let mut h = MinuteHistory::new(up_to("2026-10-01"), u32::MAX);
    add(&mut h, at("2026-09-30", 0), "X", 10, 10, 10, 1);
    add(&mut h, at("2026-10-01", 0), "X", 11, 11, 11, 2);
    add(&mut h, at("2026-10-02", 0), "X", 99, 99, 99, 9999);
    let (rows, rep) = h.build(small()).unwrap();
    assert_eq!(rep.as_of, "2026-10-01");
    assert_eq!(rep.bars_after, 1);
    assert_eq!(col(&rows[0], StaticFeature::PrevClose), Some(11 * D));
    // A date with no bars at all before it is an error.
    let mut h = MinuteHistory::new(up_to("2026-09-01"), u32::MAX);
    add(&mut h, at("2026-10-02", 0), "X", 1, 1, 1, 1);
    assert_eq!(h.build(small()).unwrap_err(), HistoryError::NoBars);
}

#[test]
fn a_trading_day_missing_from_the_file_is_an_error_not_a_smaller_average() {
    let mut h = MinuteHistory::new(up_to("2026-10-05"), u32::MAX);
    add(&mut h, at("2026-10-01", 0), "X", 10, 10, 10, 1);
    add(&mut h, at("2026-10-05", 0), "X", 10, 10, 10, 1);
    // Friday 2 October is a trading day with no bar at all; the weekend is not.
    assert_eq!(
        h.build(small()).unwrap_err(),
        HistoryError::MissingSession("2026-10-02".to_owned())
    );
}

#[test]
fn bars_on_closed_days_and_overnight_are_counted_and_a_date_outside_the_table_is_an_error() {
    let mut h = MinuteHistory::new(up_to("2026-10-05"), u32::MAX);
    // Saturday 3 October at noon UTC; Friday 2 October at 03:00 New York time (07:00 UTC).
    h.add(
        (date_days("2026-10-03").unwrap() as u64 * 86_400 + 12 * 3600) * NANOS,
        "X",
        D,
        D,
        D,
        D,
        1,
    )
    .unwrap();
    h.add(
        (date_days("2026-10-02").unwrap() as u64 * 86_400 + 7 * 3600) * NANOS,
        "X",
        D,
        D,
        D,
        D,
        1,
    )
    .unwrap();
    add(&mut h, at("2026-10-02", 0), "X", 10, 10, 10, 1);
    let (_, rep) = h.build(small()).unwrap();
    assert_eq!(
        (
            rep.bars_closed_day,
            rep.bars_outside_sessions,
            rep.bars_read
        ),
        (1, 1, 3)
    );
    // 2031 is beyond the calendar's table: refused, not guessed (once the date is one that is to be read).
    let mut h = MinuteHistory::new(up_to("2032-01-01"), u32::MAX);
    let far = (date_days("2031-01-06").unwrap() as u64 * 86_400 + 15 * 3600) * NANOS;
    let e = h.add(far, "X", D, D, D, D, 1).unwrap_err();
    assert!(matches!(e, HistoryError::Calendar(_)), "{e}");
}

fn csv(rows: &[(u64, &str)]) -> String {
    let mut s = String::from(
        "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume,symbol\n",
    );
    for (ts, tail) in rows {
        s.push_str(&format!("{ts},33,93,1,{tail}\n"));
    }
    s
}

#[test]
fn the_csv_reader_is_strict_and_streams_into_the_history() {
    let ts = at("2026-10-02", 0);
    let good = csv(&[
        (
            ts,
            &format!("{},{},{},{},500,ABC", 10 * D, 11 * D, 9 * D, 10 * D),
        ),
        (
            ts + 60 * NANOS,
            &format!("{},{},{},{},300,ABC", 10 * D, 12 * D, 10 * D, 12 * D),
        ),
    ]);
    let mut h = MinuteHistory::new(up_to("2026-10-02"), u32::MAX);
    read_minute_bars(Cursor::new(good.as_bytes()), &mut h).unwrap();
    let (rows, rep) = h.build(small()).unwrap();
    assert_eq!(rep.bars_read, 2);
    assert_eq!(col(&rows[0], StaticFeature::PrevHigh), Some(12 * D));
    assert_eq!(col(&rows[0], StaticFeature::PrevLow), Some(9 * D));
    assert_eq!(col(&rows[0], StaticFeature::PrevClose), Some(12 * D));
    assert_eq!(col(&rows[0], StaticFeature::VolFirst1), Some(500));
    assert_eq!(col(&rows[0], StaticFeature::VolFirst5), Some(800));

    let read = |text: &str| {
        let mut h = MinuteHistory::new(up_to("2026-10-02"), u32::MAX);
        read_minute_bars(Cursor::new(text.as_bytes()), &mut h)
            .unwrap_err()
            .to_string()
    };
    assert!(read("ts_event,rtype\n").contains("expected the header"));
    let bad = |tail: &str| read(&csv(&[(ts, tail)]));
    assert!(bad("1,2,3").contains("fields"), "too few fields");
    assert!(
        bad(&format!("{},{},{},{},5,ABC", 10 * D, 9 * D, 9 * D, 10 * D)).contains("do not contain"),
        "high below the open"
    );
    assert!(
        bad(&format!("10.0,{},{},{},5,ABC", 11 * D, 9 * D, 10 * D)).contains("not a number"),
        "a pretty price"
    );
    assert!(read(&format!("ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume,symbol\n{ts},35,93,1,{D},{D},{D},{D},1,ABC\n")).contains("rtype 35"));
    assert!(read(&format!("ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume,symbol\n{},33,93,1,{D},{D},{D},{D},1,ABC\n", ts + 1)).contains("on a minute"));
}

#[test]
fn history_columns_merge_into_rows_and_leave_the_rest_alone() {
    let mut h = MinuteHistory::new(up_to("2026-10-02"), u32::MAX);
    add(&mut h, at("2026-10-02", 0), "AAA", 10, 9, 10, 40);
    add(&mut h, at("2026-10-02", 0), "ZZZ", 10, 9, 10, 40);
    let (hist, _) = h.build(small()).unwrap();
    let mut rows = vec![
        RefRow {
            symbol: "AAA".into(),
            price: Some(10 * D),
            ..RefRow::default()
        },
        RefRow {
            symbol: "BBB".into(),
            price: Some(5 * D),
            ..RefRow::default()
        },
    ];
    let (matched, history_only) = merge_history(&mut rows, &hist);
    assert_eq!((matched, history_only), (1, 1));
    assert_eq!(rows[0].prev_high, Some(10 * D));
    assert_eq!(rows[0].price, Some(10 * D), "what was there stays");
    assert_eq!(rows[1].prev_high, None, "no history: unknown");
    assert_eq!(rows[1].price, Some(5 * D));
}

#[test]
fn a_bars_wick_is_capped_at_a_fraction_of_its_body_and_the_bars_changed_are_counted() {
    let day = "2026-10-02";
    let build = |clip: u32| {
        let mut h = MinuteHistory::new(up_to(day), clip);
        // open 100, close 101: the body is 100 to 101. One bar has a real wick (0.3% over), one an off-market print
        // (high 110, low 90) with the open and close unchanged, one is inside its body.
        for (m, hi, lo) in [
            (0, 101_300_000_000i64, 99_800_000_000i64),
            (1, 110 * D, 90 * D),
            (2, 101 * D, 100 * D),
        ] {
            h.add(at(day, m), "C", 100 * D, hi, lo, 101 * D, 10)
                .unwrap();
        }
        let p = HistoryParams {
            sessions: 1,
            average_over: 1,
            min_days: 1,
        };
        h.build(p).unwrap()
    };
    // Default 0.5%: the high may stand 0.5% above the body's top (101) and the low 0.5% below its bottom (100).
    let (rows, rep) = build(DEFAULT_WICK_CLIP_PERMILLE);
    let r = &rows[0];
    assert_eq!(
        col(r, StaticFeature::PrevHigh),
        Some(101_505_000_000),
        "110 is cut to 101 + 0.5%"
    );
    assert_eq!(
        col(r, StaticFeature::PrevLow),
        Some(99_500_000_000),
        "90 is cut to 100 - 0.5%"
    );
    assert_eq!(rep.bars_clipped, 1, "only the off-market bar was changed");
    // The 0.3% wick is real and kept when the cap is 0.5%, and cut when it is 0.1%.
    let (rows, rep) = build(1);
    assert_eq!(
        col(&rows[0], StaticFeature::PrevHigh),
        Some(101_101_000_000)
    );
    assert_eq!(rep.bars_clipped, 2);
    // Zero is the body alone; no cap is the raw bars.
    let (rows, _) = build(0);
    assert_eq!(col(&rows[0], StaticFeature::PrevHigh), Some(101 * D));
    assert_eq!(col(&rows[0], StaticFeature::PrevLow), Some(100 * D));
    let (rows, rep) = build(u32::MAX);
    assert_eq!(col(&rows[0], StaticFeature::PrevHigh), Some(110 * D));
    assert_eq!(col(&rows[0], StaticFeature::PrevLow), Some(90 * D));
    assert_eq!(rep.bars_clipped, 0);
}

#[test]
fn the_atr_uses_the_capped_highs_and_lows() {
    let days = sessions(20);
    let run = |clip: u32| {
        let mut h = MinuteHistory::new(up_to(days.last().unwrap()), clip);
        for d in &days {
            // Every session: a normal bar, and one off-market print 20 dollars above it.
            h.add(at(d, 0), "A", 100 * D, 101 * D, 99 * D, 100 * D, 10)
                .unwrap();
            h.add(at(d, 1), "A", 100 * D, 120 * D, 99 * D, 100 * D, 10)
                .unwrap();
        }
        let (rows, _) = h.build(small()).unwrap();
        col(&rows[0], StaticFeature::Atr14).unwrap()
    };
    // Capped: a body of 100 allows 99.5 to 100.5 whatever the bar's wicks (101 and 99 too), true range 1.0.
    assert_eq!(run(DEFAULT_WICK_CLIP_PERMILLE), D);
    // Raw: 99 to 120, true range 21.
    assert_eq!(run(u32::MAX), 21 * D);
}

#[test]
fn the_atr_needs_exactly_fifteen_sessions_and_counts_gaps_in_both_directions() {
    let atr = |n: usize, closes: &dyn Fn(usize) -> (i64, i64, i64)| {
        let days = sessions(n);
        let mut h = MinuteHistory::new(up_to(days.last().unwrap()), u32::MAX);
        for (i, d) in days.iter().enumerate() {
            let (hi, lo, cl) = closes(i);
            add(&mut h, at(d, 0), "G", hi, lo, cl, 10);
        }
        let (rows, _) = h.build(small()).unwrap();
        col(&rows[0], StaticFeature::Atr14)
    };
    let flat = |_: usize| (101, 99, 100);
    assert_eq!(
        atr(14, &flat),
        None,
        "fourteen sessions have thirteen true ranges"
    );
    assert_eq!(
        atr(15, &flat),
        Some(2 * D),
        "fifteen have fourteen, each high - low = 2"
    );
    // A gap up: the true range is high - prior close, not high - low; alternate gaps up and down.
    let gappy = |i: usize| match i % 2 {
        0 => (101, 100, 100), // closes at 100, range 1
        _ => (111, 110, 110), // opens 10 above the prior close, range 1: true range is 111 - 100 = 11
    };
    // Sessions 1..15 (the last 14 true ranges of 15): odd sessions are the gap up (11), even the gap down
    // (101 - prior close 110 = -9, low 100: |100 - 110| = 10).
    let want_sum: i64 = (1..15).map(|i| if i % 2 == 1 { 11 } else { 10 }).sum();
    assert_eq!(atr(15, &gappy), Some(want_sum * D / 14));
}

#[test]
fn the_averages_need_the_minimum_sessions_exactly_and_the_session_limit_bounds_the_ema() {
    let days = sessions(20);
    let up = up_to(days.last().unwrap());
    // Symbol M trades on exactly 10 of the last 20 sessions, N on 9.
    let mut h = MinuteHistory::new(up, u32::MAX);
    for (i, d) in days.iter().enumerate() {
        add(&mut h, at(d, 0), "Z", 10, 10, 10, 1);
        if i >= 10 {
            add(&mut h, at(d, 0), "M", 10, 10, 10, 100);
        }
        if i >= 11 {
            add(&mut h, at(d, 0), "N", 10, 10, 10, 100);
        }
    }
    let p = HistoryParams {
        sessions: 60,
        average_over: 20,
        min_days: 10,
    };
    let (rows, _) = h.build(p).unwrap();
    let get = |s: &str| rows.iter().find(|r| r.symbol == s).unwrap();
    assert_eq!(
        col(get("M"), StaticFeature::VolFirst1),
        Some(10 * 100 / 20),
        "ten sessions of twenty is enough, and the others count as zero"
    );
    assert_eq!(col(get("N"), StaticFeature::VolFirst1), None, "nine is not");
    // The average runs over the sessions it is told to: the last 5 here, all of them traded by Z.
    let p5 = HistoryParams {
        average_over: 5,
        min_days: 1,
        ..p
    };
    let (rows5, _) = h.build(p5).unwrap();
    assert_eq!(
        col(
            rows5.iter().find(|r| r.symbol == "N").unwrap(),
            StaticFeature::VolFirst1
        ),
        Some(100)
    );
    // `sessions` limits how much history the EMA sees: 10 sessions of 7 hourly closes is 70, too few for a state.
    let mut h = MinuteHistory::new(up, u32::MAX);
    for d in &days {
        for k in 0..7 {
            add(&mut h, at(d, k * 60 + 5), "E", 10, 10, 10, 1);
        }
    }
    let ema_count = |sessions: usize| {
        let (rows, rep) = h.build(HistoryParams { sessions, ..p }).unwrap();
        (col(&rows[0], StaticFeature::Ema100hCount), rep.sessions)
    };
    assert_eq!(ema_count(20), (Some(140), 20));
    assert_eq!(ema_count(15), (Some(105), 15));
    assert_eq!(ema_count(14), (None, 14), "14 x 7 = 98 closes");
}

#[test]
fn a_bar_on_a_boundary_second_belongs_to_the_session_that_starts_there() {
    let day = "2026-10-02";
    let t = Calendar::us_equities().times(date(day)).unwrap().unwrap();
    let mut h = MinuteHistory::new(up_to(day), u32::MAX);
    // The first premarket minute (04:00), the last premarket minute (09:29), the open (09:30), the last regular
    // minute (15:59), the first after-hours minute (16:00) and the minute before the premarket (03:59).
    for (ts, v) in [
        (t.premarket - 60 * NANOS, 1u64),
        (t.premarket, 2),
        (t.open - 60 * NANOS, 4),
        (t.open, 8),
        (t.close - 60 * NANOS, 16),
        (t.close, 32),
    ] {
        // The last regular minute closes at 3, the after-hours one at 5: the session's close is the 3.
        let px = if ts == t.close {
            5 * D
        } else if ts == t.close - 60 * NANOS {
            3 * D
        } else {
            D
        };
        h.add(ts, "T", px, px, px, px, v).unwrap();
    }
    let p = HistoryParams {
        sessions: 1,
        average_over: 1,
        min_days: 1,
    };
    let (rows, rep) = h.build(p).unwrap();
    let r = &rows[0];
    assert_eq!(col(r, StaticFeature::VolPremarket), Some(2 + 4));
    assert_eq!(col(r, StaticFeature::VolFirst1), Some(8));
    assert_eq!(
        col(r, StaticFeature::CumVol1530),
        Some(8),
        "15:59 starts after the 15:30 checkpoint"
    );
    assert_eq!(
        col(r, StaticFeature::PrevClose),
        Some(3 * D),
        "the 15:59 bar is the last of the session, 16:00 is not"
    );
    assert_eq!(
        rep.bars_outside_sessions, 2,
        "03:59 and 16:00 are in neither"
    );
}

#[test]
fn an_hour_ends_on_the_hour_the_last_minute_of_one_and_the_first_of_the_next_are_two_hours() {
    let days = sessions(20);
    let mut h = MinuteHistory::new(up_to(days.last().unwrap()), u32::MAX);
    // Minutes 59 and 60, 119 and 120 ... after the open: hour 0 has one bar, hours 1 to 5 two each, hour 6 one.
    for d in &days {
        for m in [59, 60, 119, 120, 179, 180, 239, 240, 299, 300, 359, 360] {
            add(&mut h, at(d, m), "H", 10, 10, 10, 1);
        }
    }
    let (rows, _) = h.build(small()).unwrap();
    assert_eq!(col(&rows[0], StaticFeature::Ema100hCount), Some(20 * 7));
}
