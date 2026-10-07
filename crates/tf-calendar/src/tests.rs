use super::*;

fn d(y: i32, m: u8, day: u8) -> Date {
    Date::new(y, m, day).expect("a real date")
}

const SEC: Nanos = NANOS_PER_SEC;

#[test]
fn dates_round_trip_through_days_and_weekdays_are_right() {
    for days in -800..60_000i64 {
        let date = Date::from_days(days);
        assert_eq!(date.days(), days, "{date}");
        assert!(Date::new(date.year, date.month, date.day).is_some());
    }
    assert_eq!(d(1970, 1, 1).days(), 0);
    assert_eq!(d(2000, 2, 29).days(), 11_016);
    assert_eq!(d(2026, 10, 2).weekday(), Weekday::Fri);
    assert_eq!(d(2024, 1, 1).weekday(), Weekday::Mon);
    assert_eq!(d(2028, 12, 31).weekday(), Weekday::Sun);
    assert!(Date::new(2025, 2, 29).is_none());
    assert!(Date::new(2024, 2, 29).is_some());
    assert!(Date::new(2100, 2, 29).is_none());
    assert!(Date::new(2025, 13, 1).is_none());
    assert!(Date::new(2025, 4, 31).is_none());
    assert!(Date::new(2025, 4, 0).is_none());
}

/// Python's zoneinfo wrote `tests/ny_midnights.txt`: the UTC second of New York midnight for every day of
/// 2024 to 2028 and the second of each daylight-saving change. The crate's own rule must agree with all of it.
#[test]
fn new_york_time_agrees_with_the_system_tz_database_for_five_years() {
    let text = include_str!("../tests/ny_midnights.txt");
    let cal = Calendar::us_equities();
    let (mut days, mut changes) = (0, 0);
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f[0] == "change" {
            let at: i64 = f[1].parse().unwrap();
            let off: i64 = f[2].parse().unwrap();
            // One second before the change the other offset, at it the new one.
            let before = if off == -14_400 { -18_000 } else { -14_400 };
            assert_eq!(offset_at(at - 1).unwrap(), before, "before {at}");
            assert_eq!(offset_at(at).unwrap(), off, "at {at}");
            changes += 1;
        } else {
            let p: Vec<i32> = f[0].split('-').map(|x| x.parse().unwrap()).collect();
            let date = d(p[0], p[1] as u8, p[2] as u8);
            let midnight: i64 = f[1].parse().unwrap();
            // The first instant of the day, and the last second of the day before.
            assert_eq!(
                cal.local(midnight as Nanos * SEC).unwrap(),
                (date, 0),
                "{date}"
            );
            assert_eq!(
                cal.local((midnight as Nanos - 1) * SEC + SEC - 1)
                    .unwrap()
                    .0,
                date.prev(),
                "{date}"
            );
            days += 1;
        }
    }
    assert_eq!((days, changes), (1827, 10));
}

#[test]
fn the_open_is_1330_utc_in_summer_and_1430_in_winter() {
    let cal = Calendar::us_equities();
    // Friday 2 October 2026: the real day measured in docs/DESIGN.md. Its official opening price
    // arrived at 13:30:00.3 UTC and its closing price at 20:00:00.1 UTC.
    let t = cal.times(d(2026, 10, 2)).unwrap().unwrap();
    assert_eq!(t.premarket, 1_790_928_000 * SEC); // 08:00 UTC
    assert_eq!(t.open, (1_790_928_000 + 5 * 3600 + 1800) * SEC); // 13:30 UTC
    assert_eq!(t.close, (1_790_928_000 + 12 * 3600) * SEC); // 20:00 UTC
    assert_eq!(t.after_hours_end, (1_790_928_000 + 16 * 3600) * SEC); // 00:00 UTC next day
    // Monday 2 November 2026, the day after daylight time ends.
    let w = cal.times(d(2026, 11, 2)).unwrap().unwrap();
    assert_eq!((w.open / SEC) % 86_400, 14 * 3600 + 1800);
    assert_eq!((w.close / SEC) % 86_400, 21 * 3600);
    assert_eq!((w.premarket / SEC) % 86_400, 9 * 3600);
}

#[test]
fn the_days_of_the_two_changes_keep_their_session_times() {
    let cal = Calendar::us_equities();
    // Monday 9 March 2026 is the first day of daylight time (it began on Sunday 8 March).
    let a = cal.times(d(2026, 3, 9)).unwrap().unwrap();
    assert_eq!((a.open / SEC) % 86_400, 13 * 3600 + 1800);
    // Friday 6 March is the last winter day.
    let b = cal.times(d(2026, 3, 6)).unwrap().unwrap();
    assert_eq!((b.open / SEC) % 86_400, 14 * 3600 + 1800);
    // The changes themselves fall on Sundays, when the market is closed.
    assert!(cal.times(d(2026, 3, 8)).unwrap().is_none());
    assert!(cal.times(d(2026, 11, 1)).unwrap().is_none());
    // New York wall clock around the autumn change: 05:59:59 UTC on Sunday 1 November is 01:59:59 EDT
    // and 06:00:00 UTC is 01:00:00 EST.
    let s1 = d(2026, 11, 1).days() * 86_400;
    assert_eq!(
        cal.local(((s1 + 5 * 3600 + 3599) as Nanos) * SEC).unwrap(),
        (d(2026, 11, 1), 3599 + 3600)
    );
    assert_eq!(
        cal.local(((s1 + 6 * 3600) as Nanos) * SEC).unwrap(),
        (d(2026, 11, 1), 3600)
    );
}

#[test]
fn the_table_matches_the_published_calendars() {
    // From the press releases of 27 December 2021 (2024), 8 November 2024 (2025 to 2027) and
    // 23 December 2025 (2026 to 2028), and NYSE's notice of 9 January 2025.
    let cal = Calendar::us_equities();
    let got: Vec<String> = cal
        .closures()
        .map(|(date, c)| {
            format!(
                "{date} {}",
                match c {
                    Closure::Holiday => "H",
                    Closure::Unscheduled => "U",
                    Closure::EarlyClose => "E",
                }
            )
        })
        .collect();
    let want = [
        "2024-01-01 H",
        "2024-01-15 H",
        "2024-02-19 H",
        "2024-03-29 H",
        "2024-05-27 H",
        "2024-06-19 H",
        "2024-07-03 E",
        "2024-07-04 H",
        "2024-09-02 H",
        "2024-11-28 H",
        "2024-11-29 E",
        "2024-12-24 E",
        "2024-12-25 H",
        "2025-01-01 H",
        "2025-01-09 U",
        "2025-01-20 H",
        "2025-02-17 H",
        "2025-04-18 H",
        "2025-05-26 H",
        "2025-06-19 H",
        "2025-07-03 E",
        "2025-07-04 H",
        "2025-09-01 H",
        "2025-11-27 H",
        "2025-11-28 E",
        "2025-12-24 E",
        "2025-12-25 H",
        "2026-01-01 H",
        "2026-01-19 H",
        "2026-02-16 H",
        "2026-04-03 H",
        "2026-05-25 H",
        "2026-06-19 H",
        "2026-07-03 H",
        "2026-09-07 H",
        "2026-11-26 H",
        "2026-11-27 E",
        "2026-12-24 E",
        "2026-12-25 H",
        "2027-01-01 H",
        "2027-01-18 H",
        "2027-02-15 H",
        "2027-03-26 H",
        "2027-05-31 H",
        "2027-06-18 H",
        "2027-07-05 H",
        "2027-09-06 H",
        "2027-11-25 H",
        "2027-11-26 E",
        "2027-12-24 H",
        "2028-01-17 H",
        "2028-02-21 H",
        "2028-04-14 H",
        "2028-05-29 H",
        "2028-06-19 H",
        "2028-07-03 E",
        "2028-07-04 H",
        "2028-09-04 H",
        "2028-11-23 H",
        "2028-11-24 E",
        "2028-12-25 H",
    ];
    assert_eq!(got, want);
}

#[test]
fn the_table_is_sorted_and_no_closure_falls_on_a_weekend() {
    let cal = Calendar::us_equities();
    let all: Vec<(Date, Closure)> = cal.closures().collect();
    for w in all.windows(2) {
        assert!(w[0].0 < w[1].0, "{} before {}", w[0].0, w[1].0);
    }
    for (date, _) in &all {
        assert!(
            !matches!(date.weekday(), Weekday::Sat | Weekday::Sun),
            "{date}"
        );
    }
}

#[test]
fn holidays_weekends_and_unscheduled_closures_are_closed() {
    let cal = Calendar::us_equities();
    assert!(!cal.is_trading_day(d(2026, 11, 26)).unwrap()); // Thanksgiving
    assert!(!cal.is_trading_day(d(2026, 7, 3)).unwrap()); // Independence Day observed
    assert!(!cal.is_trading_day(d(2025, 1, 9)).unwrap()); // national day of mourning
    assert!(!cal.is_trading_day(d(2026, 10, 3)).unwrap()); // Saturday
    assert!(cal.is_trading_day(d(2026, 10, 2)).unwrap());
    assert!(cal.is_trading_day(d(2026, 7, 2)).unwrap());
    assert_eq!(cal.day_kind(d(2028, 1, 1)).unwrap(), DayKind::Closed); // Saturday: no observed holiday
    assert_eq!(cal.day_kind(d(2028, 1, 3)).unwrap(), DayKind::Normal);
    assert!(cal.times(d(2026, 11, 26)).unwrap().is_none());
}

#[test]
fn an_early_close_ends_the_session_at_1300_and_after_hours_at_1700() {
    let cal = Calendar::us_equities();
    let t = cal.times(d(2026, 11, 27)).unwrap().unwrap(); // the day after Thanksgiving, winter time
    assert_eq!((t.close / SEC) % 86_400, 18 * 3600); // 13:00 EST
    assert_eq!((t.after_hours_end / SEC) % 86_400, 22 * 3600); // 17:00 EST
    assert_eq!((t.open / SEC) % 86_400, 14 * 3600 + 1800);
    let s = cal.times(d(2025, 7, 3)).unwrap().unwrap(); // summer
    assert_eq!((s.close / SEC) % 86_400, 17 * 3600);
    assert_eq!(cal.day_kind(d(2025, 7, 3)).unwrap(), DayKind::EarlyClose);
}

#[test]
fn sessions_change_exactly_on_their_boundaries() {
    let cal = Calendar::us_equities();
    let t = cal.times(d(2026, 10, 2)).unwrap().unwrap();
    let at = |ts: Nanos| cal.session_at(ts).unwrap();
    assert_eq!(at(t.premarket - 1), Session::Closed);
    assert_eq!(at(t.premarket), Session::Premarket);
    assert_eq!(at(t.open - 1), Session::Premarket);
    assert_eq!(at(t.open), Session::Regular);
    assert_eq!(at(t.close - 1), Session::Regular);
    assert_eq!(at(t.close), Session::AfterHours);
    assert_eq!(at(t.after_hours_end - 1), Session::AfterHours);
    assert_eq!(at(t.after_hours_end), Session::Closed);
    // The real day's own records: the opening price at 13:30:00.3 UTC is regular, the closing
    // price at 20:00:00.1 UTC is after the bell.
    assert_eq!(at(1_790_947_800 * SEC + 315_761_590), Session::Regular);
    assert_eq!(at(1_790_971_200 * SEC + 144_954_473), Session::AfterHours);
    // A holiday and a weekend have no session at all.
    let h = d(2026, 11, 26).days() * 86_400 + 15 * 3600;
    assert_eq!(at(h as Nanos * SEC), Session::Closed);
    // On an early close the afternoon is after-hours.
    let e = cal.times(d(2026, 11, 27)).unwrap().unwrap();
    assert_eq!(at(e.close - 1), Session::Regular);
    assert_eq!(at(e.close), Session::AfterHours);
    assert_eq!(at(e.after_hours_end), Session::Closed);
}

#[test]
fn the_next_and_previous_trading_day_skip_weekends_and_holidays() {
    let cal = Calendar::us_equities();
    assert_eq!(
        cal.next_trading_day(d(2026, 10, 2)).unwrap(),
        d(2026, 10, 5)
    );
    assert_eq!(cal.next_trading_day(d(2026, 7, 2)).unwrap(), d(2026, 7, 6)); // 3 July is the holiday
    assert_eq!(
        cal.next_trading_day(d(2026, 11, 25)).unwrap(),
        d(2026, 11, 27)
    );
    assert_eq!(
        cal.prev_trading_day(d(2026, 10, 5)).unwrap(),
        d(2026, 10, 2)
    );
    assert_eq!(
        cal.prev_trading_day(d(2026, 1, 2)).unwrap(),
        d(2025, 12, 31)
    );
    assert_eq!(cal.prev_trading_day(d(2025, 1, 10)).unwrap(), d(2025, 1, 8)); // 9 January was closed
}

#[test]
fn the_last_25_minutes_start_at_1535_on_a_normal_day_and_1235_on_an_early_close() {
    let cal = Calendar::us_equities();
    let n = cal
        .minutes_before_close(d(2026, 10, 2), 25)
        .unwrap()
        .unwrap();
    assert_eq!((n / SEC) % 86_400, 19 * 3600 + 35 * 60); // 15:35 EDT
    let e = cal
        .minutes_before_close(d(2026, 11, 27), 25)
        .unwrap()
        .unwrap();
    assert_eq!((e / SEC) % 86_400, 17 * 3600 + 35 * 60); // 12:35 EST
    assert!(
        cal.minutes_before_close(d(2026, 11, 26), 25)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_date_outside_the_table_is_an_error_and_so_is_a_year_before_the_rule() {
    let cal = Calendar::us_equities();
    assert_eq!(
        cal.is_trading_day(d(2023, 12, 29)),
        Err(CalendarError::OutsideTable(2023))
    );
    assert_eq!(
        cal.day_kind(d(2029, 1, 2)),
        Err(CalendarError::OutsideTable(2029))
    );
    assert!(cal.times(d(2029, 6, 1)).is_err());
    // The last second of 2028 is inside; the first of 2029 is not.
    let last = d(2028, 12, 29).days() * 86_400 + 12 * 3600;
    assert!(cal.session_at(last as Nanos * SEC).is_ok());
    let next = d(2029, 1, 2).days() * 86_400 + 15 * 3600;
    assert_eq!(
        cal.session_at(next as Nanos * SEC),
        Err(CalendarError::OutsideTable(2029))
    );
    // 2006 used the old daylight-saving rule.
    assert_eq!(
        cal.date_of(1_150_000_000 * SEC),
        Err(CalendarError::BeforeDstRule(2006))
    );
    // next/prev stop at the table's edge rather than guessing.
    assert!(cal.next_trading_day(d(2028, 12, 29)).is_err());
    assert!(cal.prev_trading_day(d(2024, 1, 2)).is_err());
    assert!(
        CalendarError::OutsideTable(2030)
            .to_string()
            .contains("2024 to 2028")
    );
}

#[test]
fn the_sunday_rule_finds_the_right_sundays() {
    // Second Sunday of March and first Sunday of November, 2024 to 2028.
    let want = [
        (2024, "2024-03-10", "2024-11-03"),
        (2025, "2025-03-09", "2025-11-02"),
        (2026, "2026-03-08", "2026-11-01"),
        (2027, "2027-03-14", "2027-11-07"),
        (2028, "2028-03-12", "2028-11-05"),
    ];
    for (y, spring, autumn) in want {
        assert_eq!(Date::nth_sunday(y, 3, 2).to_string(), spring);
        assert_eq!(Date::nth_sunday(y, 11, 1).to_string(), autumn);
    }
}

#[test]
fn the_local_offset_changes_at_two_in_the_morning_on_the_day_of_each_change() {
    let est = -5 * 3600;
    let edt = -4 * 3600;
    let spring = d(2026, 3, 8);
    assert_eq!(offset_for_local(spring, 2 * HOUR - 1).unwrap(), est);
    assert_eq!(offset_for_local(spring, 2 * HOUR).unwrap(), edt);
    let autumn = d(2026, 11, 1);
    assert_eq!(offset_for_local(autumn, 2 * HOUR - 1).unwrap(), edt);
    assert_eq!(offset_for_local(autumn, 2 * HOUR).unwrap(), est);
    // The day before and the day after each change, and the middle of summer and of winter.
    assert_eq!(offset_for_local(d(2026, 3, 7), 12 * HOUR).unwrap(), est);
    assert_eq!(offset_for_local(d(2026, 3, 9), 0).unwrap(), edt);
    assert_eq!(offset_for_local(d(2026, 7, 1), 0).unwrap(), edt);
    assert_eq!(offset_for_local(d(2026, 10, 31), 23 * HOUR).unwrap(), edt);
    assert_eq!(offset_for_local(d(2026, 11, 2), 0).unwrap(), est);
    assert_eq!(offset_for_local(d(2026, 1, 15), 0).unwrap(), est);
}

#[test]
fn the_first_and_last_years_are_inside_the_table_and_2007_is_inside_the_rule() {
    let cal = Calendar::us_equities();
    assert_eq!(cal.is_trading_day(d(2024, 1, 2)), Ok(true));
    assert_eq!(cal.is_trading_day(d(2024, 1, 1)), Ok(false));
    assert_eq!(cal.is_trading_day(d(2028, 12, 29)), Ok(true));
    assert_eq!(cal.is_trading_day(d(2028, 12, 25)), Ok(false));
    // 1 July 2007 12:00 UTC.
    assert_eq!(cal.date_of(1_183_291_200 * SEC), Ok(d(2007, 7, 1)));
    assert_eq!(offset_for_local(d(2007, 7, 1), 0), Ok(-4 * 3600));
    assert_eq!(
        offset_for_local(d(2006, 7, 1), 0),
        Err(CalendarError::BeforeDstRule(2006))
    );
}
