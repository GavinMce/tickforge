//! The US equity trading calendar: New York time, sessions, holidays and early closes.
//!
//! No time-zone database and no clock. New York's offset from UTC follows the US rule in force since 2007
//! (daylight time from 02:00 on the second Sunday of March to 02:00 on the first Sunday of November), so
//! the conversion is arithmetic and gives the same answer everywhere. Which days the market is closed is
//! data: a table of the exchange's published holidays, early closes and unscheduled closures for
//! [`FIRST_YEAR`] to [`LAST_YEAR`] (sources in ADR 0052). A question about a date outside the table is an
//! error, not a guess, so a stale table is noticed on the first day it matters.
//!
//! A day has four sessions: premarket from 04:00, the regular session from 09:30 to 16:00, after-hours to
//! 20:00. On an early close the regular session ends at 13:00 and after-hours at 17:00.

use tf_core::{NANOS_PER_SEC, Nanos};

/// First year the closure table covers: the start of the history the dataset holds (July 2024).
pub const FIRST_YEAR: i32 = 2024;
/// Last year the closure table covers: the end of the published calendars.
pub const LAST_YEAR: i32 = 2028;
/// The current US daylight-saving rule applies from this year; earlier years are refused.
const DST_RULE_FROM: i32 = 2007;

const HOUR: u32 = 3600;
const PRE_OPEN: u32 = 4 * HOUR;
const OPEN: u32 = 9 * HOUR + 30 * 60;
const CLOSE: u32 = 16 * HOUR;
const POST_CLOSE: u32 = 20 * HOUR;
const EARLY_CLOSE: u32 = 13 * HOUR;
const EARLY_POST_CLOSE: u32 = 17 * HOUR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalendarError {
    /// The closure table does not cover this year.
    OutsideTable(i32),
    /// The daylight-saving rule built in is the one in force from 2007.
    BeforeDstRule(i32),
}

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalendarError::OutsideTable(y) => write!(
                f,
                "the calendar covers {FIRST_YEAR} to {LAST_YEAR}, not {y}: extend the table"
            ),
            CalendarError::BeforeDstRule(y) => {
                write!(
                    f,
                    "the daylight-saving rule built in starts in {DST_RULE_FROM}, not {y}"
                )
            }
        }
    }
}

impl std::error::Error for CalendarError {}

/// Monday is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

/// A date in the proleptic Gregorian calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

impl std::fmt::Display for Date {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        _ => 28,
    }
}

impl Date {
    /// `None` for a date that does not exist.
    pub fn new(year: i32, month: u8, day: u8) -> Option<Date> {
        ((1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month))
            .then_some(Date { year, month, day })
    }

    /// Days since 1970-01-01.
    pub fn days(self) -> i64 {
        // Howard Hinnant's days_from_civil.
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn from_days(days: i64) -> Date {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Date { year, month, day }
    }

    pub fn weekday(self) -> Weekday {
        // 1970-01-01 was a Thursday.
        match (self.days() + 3).rem_euclid(7) {
            0 => Weekday::Mon,
            1 => Weekday::Tue,
            2 => Weekday::Wed,
            3 => Weekday::Thu,
            4 => Weekday::Fri,
            5 => Weekday::Sat,
            _ => Weekday::Sun,
        }
    }

    pub fn next(self) -> Date {
        Date::from_days(self.days() + 1)
    }

    pub fn prev(self) -> Date {
        Date::from_days(self.days() - 1)
    }

    /// The `n`th Sunday (1 is the first) of a month.
    fn nth_sunday(year: i32, month: u8, n: i64) -> Date {
        let first = Date {
            year,
            month,
            day: 1,
        };
        let to_sunday = (6 - first.days() - 3).rem_euclid(7);
        Date::from_days(first.days() + to_sunday + 7 * (n - 1))
    }
}

/// UTC seconds of the start and of the end of daylight time in a year.
fn dst_bounds(year: i32) -> (i64, i64) {
    let start = Date::nth_sunday(year, 3, 2).days() * 86_400 + 7 * 3600; // 02:00 EST
    let end = Date::nth_sunday(year, 11, 1).days() * 86_400 + 6 * 3600; // 02:00 EDT
    (start, end)
}

/// New York's offset from UTC, in seconds, at a UTC instant.
fn offset_at(utc_secs: i64) -> Result<i64, CalendarError> {
    let year = Date::from_days(utc_secs.div_euclid(86_400)).year;
    if year < DST_RULE_FROM {
        return Err(CalendarError::BeforeDstRule(year));
    }
    let (start, end) = dst_bounds(year);
    Ok(if (start..end).contains(&utc_secs) {
        -4 * 3600
    } else {
        -5 * 3600
    })
}

/// New York's offset from UTC at local `secs` into `date`: the time before a change is read as the
/// time before it, and the repeated hour in November as its first occurrence.
fn offset_for_local(date: Date, secs: u32) -> Result<i64, CalendarError> {
    if date.year < DST_RULE_FROM {
        return Err(CalendarError::BeforeDstRule(date.year));
    }
    let spring = Date::nth_sunday(date.year, 3, 2);
    let autumn = Date::nth_sunday(date.year, 11, 1);
    let edt = -4 * 3600;
    let est = -5 * 3600;
    Ok(if date == spring {
        if secs < 2 * HOUR { est } else { edt }
    } else if date == autumn {
        if secs < 2 * HOUR { edt } else { est }
    } else if date > spring && date < autumn {
        edt
    } else {
        est
    })
}

/// What kind of closure a table entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closure {
    /// A scheduled holiday: the market is closed.
    Holiday,
    /// An unscheduled closure (a national day of mourning): the market is closed.
    Unscheduled,
    /// A scheduled early close: the regular session ends at 13:00.
    EarlyClose,
}

use Closure::{EarlyClose as E, Holiday as H, Unscheduled as U};

/// The published closures, ascending by date. Sources are in ADR 0052.
const TABLE: &[(i32, u8, u8, Closure)] = &[
    (2024, 1, 1, H),
    (2024, 1, 15, H),
    (2024, 2, 19, H),
    (2024, 3, 29, H),
    (2024, 5, 27, H),
    (2024, 6, 19, H),
    (2024, 7, 3, E),
    (2024, 7, 4, H),
    (2024, 9, 2, H),
    (2024, 11, 28, H),
    (2024, 11, 29, E),
    (2024, 12, 24, E),
    (2024, 12, 25, H),
    (2025, 1, 1, H),
    (2025, 1, 9, U),
    (2025, 1, 20, H),
    (2025, 2, 17, H),
    (2025, 4, 18, H),
    (2025, 5, 26, H),
    (2025, 6, 19, H),
    (2025, 7, 3, E),
    (2025, 7, 4, H),
    (2025, 9, 1, H),
    (2025, 11, 27, H),
    (2025, 11, 28, E),
    (2025, 12, 24, E),
    (2025, 12, 25, H),
    (2026, 1, 1, H),
    (2026, 1, 19, H),
    (2026, 2, 16, H),
    (2026, 4, 3, H),
    (2026, 5, 25, H),
    (2026, 6, 19, H),
    (2026, 7, 3, H),
    (2026, 9, 7, H),
    (2026, 11, 26, H),
    (2026, 11, 27, E),
    (2026, 12, 24, E),
    (2026, 12, 25, H),
    (2027, 1, 1, H),
    (2027, 1, 18, H),
    (2027, 2, 15, H),
    (2027, 3, 26, H),
    (2027, 5, 31, H),
    (2027, 6, 18, H),
    (2027, 7, 5, H),
    (2027, 9, 6, H),
    (2027, 11, 25, H),
    (2027, 11, 26, E),
    (2027, 12, 24, H),
    (2028, 1, 17, H),
    (2028, 2, 21, H),
    (2028, 4, 14, H),
    (2028, 5, 29, H),
    (2028, 6, 19, H),
    (2028, 7, 3, E),
    (2028, 7, 4, H),
    (2028, 9, 4, H),
    (2028, 11, 23, H),
    (2028, 11, 24, E),
    (2028, 12, 25, H),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    Closed,
    Premarket,
    Regular,
    AfterHours,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DayKind {
    /// A weekend, a holiday or an unscheduled closure.
    Closed,
    Normal,
    /// The regular session ends at 13:00 and after-hours at 17:00.
    EarlyClose,
}

/// The session boundaries of one trading day, as UTC nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTimes {
    pub premarket: Nanos,
    pub open: Nanos,
    pub close: Nanos,
    pub after_hours_end: Nanos,
}

/// The US equity trading calendar.
#[derive(Clone, Copy, Debug, Default)]
pub struct Calendar;

fn utc_nanos(date: Date, secs: u32) -> Result<Nanos, CalendarError> {
    let off = offset_for_local(date, secs)?;
    let utc = date.days() * 86_400 + i64::from(secs) - off;
    Ok(utc as Nanos * NANOS_PER_SEC)
}

impl Calendar {
    pub fn us_equities() -> Calendar {
        Calendar
    }

    /// The date in New York at a UTC instant, and the seconds into that day.
    pub fn local(&self, ts: Nanos) -> Result<(Date, u32), CalendarError> {
        let utc = (ts / NANOS_PER_SEC) as i64;
        let local = utc + offset_at(utc)?;
        let days = local.div_euclid(86_400);
        Ok((Date::from_days(days), local.rem_euclid(86_400) as u32))
    }

    /// The date in New York at a UTC instant.
    pub fn date_of(&self, ts: Nanos) -> Result<Date, CalendarError> {
        Ok(self.local(ts)?.0)
    }

    /// The closure-table entry for a date, if any.
    pub fn closure(&self, date: Date) -> Result<Option<Closure>, CalendarError> {
        if !(FIRST_YEAR..=LAST_YEAR).contains(&date.year) {
            return Err(CalendarError::OutsideTable(date.year));
        }
        Ok(TABLE
            .binary_search_by_key(&(date.year, date.month, date.day), |&(y, m, d, _)| {
                (y, m, d)
            })
            .ok()
            .map(|i| TABLE[i].3))
    }

    pub fn day_kind(&self, date: Date) -> Result<DayKind, CalendarError> {
        let entry = self.closure(date)?;
        Ok(match (date.weekday(), entry) {
            (Weekday::Sat | Weekday::Sun, _) => DayKind::Closed,
            (_, Some(Closure::Holiday | Closure::Unscheduled)) => DayKind::Closed,
            (_, Some(Closure::EarlyClose)) => DayKind::EarlyClose,
            _ => DayKind::Normal,
        })
    }

    pub fn is_trading_day(&self, date: Date) -> Result<bool, CalendarError> {
        Ok(self.day_kind(date)? != DayKind::Closed)
    }

    /// The session boundaries of a date; `None` when the market is closed all day.
    pub fn times(&self, date: Date) -> Result<Option<SessionTimes>, CalendarError> {
        let (close, post) = match self.day_kind(date)? {
            DayKind::Closed => return Ok(None),
            DayKind::Normal => (CLOSE, POST_CLOSE),
            DayKind::EarlyClose => (EARLY_CLOSE, EARLY_POST_CLOSE),
        };
        Ok(Some(SessionTimes {
            premarket: utc_nanos(date, PRE_OPEN)?,
            open: utc_nanos(date, OPEN)?,
            close: utc_nanos(date, close)?,
            after_hours_end: utc_nanos(date, post)?,
        }))
    }

    /// The session at a UTC instant. The start of a session belongs to it and its end to the next.
    pub fn session_at(&self, ts: Nanos) -> Result<Session, CalendarError> {
        let date = self.date_of(ts)?;
        let Some(t) = self.times(date)? else {
            return Ok(Session::Closed);
        };
        Ok(if ts < t.premarket {
            Session::Closed
        } else if ts < t.open {
            Session::Premarket
        } else if ts < t.close {
            Session::Regular
        } else if ts < t.after_hours_end {
            Session::AfterHours
        } else {
            Session::Closed
        })
    }

    /// The first trading day after `date`.
    pub fn next_trading_day(&self, date: Date) -> Result<Date, CalendarError> {
        let mut d = date.next();
        while !self.is_trading_day(d)? {
            d = d.next();
        }
        Ok(d)
    }

    /// The last trading day before `date`.
    pub fn prev_trading_day(&self, date: Date) -> Result<Date, CalendarError> {
        let mut d = date.prev();
        while !self.is_trading_day(d)? {
            d = d.prev();
        }
        Ok(d)
    }

    /// The instant `minutes` before the regular close of a trading day, for rules about the last
    /// minutes of the session. `None` on a closed day.
    pub fn minutes_before_close(
        &self,
        date: Date,
        minutes: u32,
    ) -> Result<Option<Nanos>, CalendarError> {
        Ok(self
            .times(date)?
            .map(|t| t.close - u64::from(minutes) * 60 * NANOS_PER_SEC))
    }

    /// The closures in the table, ascending, for a caller that wants to list or check them.
    pub fn closures(&self) -> impl Iterator<Item = (Date, Closure)> {
        TABLE.iter().map(|&(y, m, d, c)| {
            (
                Date {
                    year: y,
                    month: m,
                    day: d,
                },
                c,
            )
        })
    }
}

#[cfg(test)]
mod tests;
