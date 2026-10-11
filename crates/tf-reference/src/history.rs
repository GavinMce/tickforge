//! Reference columns from one-minute history (E19-S04).
//!
//! Databento `ohlcv-1m` bars of the feed the strategies run on (XNAS.BASIC), read as a stream and kept as a few
//! numbers per symbol and session, become the columns the intraday strategies of `docs/research` read before the
//! open: the previous regular session's high, low and close, the ATR over 14 sessions in price units, the
//! volume baselines (first minute, first five minutes, premarket, cumulative volume at seven times of day) and
//! the state of an EMA(100) over regular-session hourly closes.
//!
//! Rules, stated and tested:
//! - **Sessions come from the calendar** (`tf-calendar`): the regular session is 09:30 to 16:00 New York time
//!   (a bar belongs to it when it starts inside), the premarket 04:00 to 09:30, whatever the offset from UTC that
//!   day. A date outside the calendar's table is an error, never a guess. Bars on a day the market was closed,
//!   overnight and in after-hours are counted and left out.
//! - **Point in time.** Only bars on or before the requested date are read; the as-of date is the last session
//!   with bars, and `prev_*` are that session's.
//! - **Short history is unknown, never zero.** Every session between the first and the as-of session must have
//!   bars in the file (a missing day is an error, not a smaller average). A volume average runs over the last 20
//!   sessions, a missing session of a symbol counting as zero volume, and needs the symbol to have traded in at
//!   least 10 of them. The ATR needs 15 sessions (14 true ranges, each against the session before it) all
//!   traded. The EMA needs 100 hourly closes.
//! - **Time-of-day volume skips early closes.** A session that ends at 13:00 has no volume at 14:00 or 15:30; the
//!   cumulative columns average over the normal sessions of the window only (the first-minute, first-five and
//!   premarket columns keep every session: the open and the premarket do not change shape).
//! - **The ATR** is the mean of the 14 true ranges `max(high - low, |high - prior close|, |low - prior close|)` of
//!   the regular sessions' own highs, lows and closes: the same number as Wilder's ATR(14) after its 14-bar seed.
//! - **The EMA** is `tf_engine::Ema` with period 100 and the simple-average seed over the regular-session hourly
//!   closes of the last 60 sessions, oldest first: each hour counted from 09:30 (09:30, 10:30 ... 15:30), a close
//!   being the last one-minute bar's close in it, an hour with no trade skipped (as the live bars do). Its state
//!   is exported exactly (`Ema::state`), so the live average can carry on from it.
//! - **A session's high and low are of capped bars.** The feed's own minute highs and lows carry off-market prints (a
//!   single report far from the market, with the bar's open and close normal): on 2 to 15 symbols-sessions in three the
//!   raw high or low is more than 1% from the consolidated daily one, up to 13% (ADR 0055). A bar's high is therefore
//!   capped at a fraction of its own body (the larger of open and close) above it, and its low floored the same way
//!   below the smaller: by default 0.5%. A real wick is kept up to that; the number of bars changed is reported. Zero
//!   sets the high and low to the body. Trade flags (E19-S08) are the real fix; until then the cap is.
//! - **Zero-share prints** are counted by the bars in highs and lows (ADR 0053); the cap limits their reach too.
//! - **The time-of-day profile** (E19-S53) is the volume in each quarter hour of the day, 04:00 to 16:00 (48 columns, the open's is the 23rd), and the
//!   normal absolute move over the hour from each regular-session quarter hour from 09:30 to 15:00 (23 columns). A quarter hour's volume is averaged
//!   as the cumulative columns are: the premarket's over every session of the window, the regular session's over the normal ones, a session that did not
//!   trade counting as zero, the symbol needing `min_days` sessions in which it traded. The move is the mean of `|P(t+60) - P(t)| / P(t)` over the
//!   normal sessions with a price at both ends, in hundred-thousandths (0.1 basis points), where `P(09:30)` is the open of the first regular bar and any other
//!   `P(t)` is the close of the last bar that started before `t`, carried from an earlier quarter hour if the symbol had no bar in the last one; at least
//!   `min_days` such sessions are needed. Unknown, never zero, when short.
//! - Integer arithmetic only; averages divide down.

use std::collections::HashMap;
use std::io::BufRead;

use tf_calendar::{Calendar, CalendarError, Date, SessionTimes};
use tf_engine::{Ema, Seed};
use tf_universe::{
    CUMVOL_CHECKPOINTS, HISTORY_FEATURES, Profile, QMOVE_COUNT, QVOL_COUNT, QVOL_FIRST_REGULAR,
    RefRow, StaticFeature, profile_features, valid_name,
};

use crate::bars::{ReadError, Symbology, date_days, date_text};

/// Hourly buckets of a regular session (09:30 to 16:00 is seven, the last a half hour).
const HOURS: usize = 7;
const NANOS: u64 = 1_000_000_000;
/// The first line of a kept day of sessions.
pub const SESSIONS_MAGIC: &str = "# tf minute sessions 2";
/// How far a bar's high may stand above its body, and its low below, in permille of the body's price.
pub const DEFAULT_WICK_CLIP_PERMILLE: u32 = 5;
/// Periods the columns are defined with.
pub const ATR_PERIOD: usize = 14;
pub const EMA_PERIOD: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryParams {
    /// Sessions of history used, the last this many up to the as-of session (the EMA runs over all of them).
    pub sessions: usize,
    /// Sessions the volume averages run over.
    pub average_over: usize,
    /// A symbol that traded in fewer of those has no volume average.
    pub min_days: usize,
}

impl Default for HistoryParams {
    fn default() -> Self {
        HistoryParams {
            sessions: 60,
            average_over: 20,
            min_days: 10,
        }
    }
}

/// Fields of a symbol's line in a kept day: the name, the flag, the high, low and close, the premarket and opening volumes, the seven cumulative
/// volumes, the seven hourly closes, then (E19-S53) the 48 quarter-hour volumes, the 26 closes of regular quarter hours and the open.
const FIELDS: usize = 15 + HOURS + QVOL_COUNT + QUARTERS + 1;
/// A quarter hour not seen.
const UNSEEN: u32 = u32::MAX;
/// Regular-session quarter hours.
const QUARTERS: usize = QVOL_COUNT - QVOL_FIRST_REGULAR;

#[derive(Clone, Copy, Debug)]
struct Session {
    has_regular: bool,
    high: i64,
    low: i64,
    /// Start of the latest regular bar seen and its close.
    close: Option<(u64, i64)>,
    pre: u64,
    first1: u64,
    first5: u64,
    cum: [u64; 7],
    hours: [Option<(u64, i64)>; HOURS],
    /// Volume in each quarter hour from 04:00 (E19-S53), saturating.
    qvol: [u32; QVOL_COUNT],
    /// For each regular-session quarter hour, the seconds after the open at which its last bar started and that bar's close
    /// (`UNSEEN` when it had none): the price at a quarter hour's end is the last close at or before it.
    qc_t: [u32; QUARTERS],
    qc: [i64; QUARTERS],
    /// Seconds after the open at which the first regular bar started and its open (`UNSEEN` for none): the price at 09:30.
    open_t: u32,
    open_px: i64,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            has_regular: false,
            high: 0,
            low: 0,
            close: None,
            pre: 0,
            first1: 0,
            first5: 0,
            cum: [0; 7],
            hours: [None; HOURS],
            qvol: [0; QVOL_COUNT],
            qc_t: [UNSEEN; QUARTERS],
            qc: [0; QUARTERS],
            open_t: UNSEEN,
            open_px: 0,
        }
    }
}

impl Session {
    /// The price at quarter-hour boundary `m` of the regular session (0 is 09:30, 26 is 16:00): the open's price for 0, then the close of the
    /// last bar that started before it. Unknown if the symbol had no bar yet.
    fn boundary(&self, m: usize) -> Option<i64> {
        if m == 0 {
            return (self.open_t != UNSEEN).then_some(self.open_px);
        }
        (0..m.min(QUARTERS))
            .rev()
            .find(|q| self.qc_t[*q] != UNSEEN)
            .map(|q| self.qc[q])
    }
}

#[derive(Clone, Copy, Debug)]
struct DayFacts {
    /// Whole seconds since the epoch.
    pre: u64,
    open: u64,
    close: u64,
    normal: bool,
    bars: u64,
}

/// What reading and building left out, for the report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryReport {
    pub bars_read: u64,
    /// Regular-session bars whose high or low was capped (see the module docs).
    pub bars_clipped: u64,
    /// Bars of a session after the requested date, never looked at.
    pub bars_after: u64,
    /// Bars on a day the market was closed, and bars in after-hours or overnight.
    pub bars_closed_day: u64,
    pub bars_outside_sessions: u64,
    pub bars_bad_symbol: u64,
    pub as_of: String,
    /// Sessions of history used, oldest first, and how many of them were early closes.
    pub first_session: String,
    pub sessions: usize,
    pub early_closes: usize,
    pub symbols: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HistoryError {
    NoBars,
    /// A trading day between the first and the as-of session has no bar at all.
    MissingSession(String),
    /// The calendar cannot answer for a bar's date (it is outside its table).
    Calendar(String),
    /// A kept day of sessions that cannot be read back, or does not fit this history.
    Sessions(String),
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::NoBars => {
                write!(f, "no regular-session bars on or before the requested date")
            }
            HistoryError::MissingSession(d) => write!(
                f,
                "the file has no bar at all on {d}, a trading day between its first and last: pull it again (a missing day would shrink every average)"
            ),
            HistoryError::Calendar(e) | HistoryError::Sessions(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HistoryError {}

/// The history columns of one symbol, in [`HISTORY_FEATURES`] order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub symbol: String,
    pub values: [Option<i64>; 16],
    /// The time-of-day profile (E19-S53).
    pub profile: Profile,
}

/// Accumulates one-minute bars; [`MinuteHistory::build`] makes the columns.
pub struct MinuteHistory {
    up_to: i64,
    clip_permille: u32,
    cal: Calendar,
    days: HashMap<i64, Option<DayFacts>>,
    symbols: HashMap<String, HashMap<i64, Session>>,
    report: HistoryReport,
}

impl MinuteHistory {
    /// Only bars dated on or before `up_to` (days since 1970-01-01) are used. `clip_permille` caps a bar's wick
    /// (see the module docs); [`DEFAULT_WICK_CLIP_PERMILLE`], or `u32::MAX` for none.
    pub fn new(up_to: i64, clip_permille: u32) -> MinuteHistory {
        MinuteHistory {
            up_to,
            clip_permille,
            cal: Calendar::us_equities(),
            days: HashMap::new(),
            symbols: HashMap::new(),
            report: HistoryReport::default(),
        }
    }

    fn facts(&mut self, date: Date) -> Result<Option<DayFacts>, CalendarError> {
        if let Some(f) = self.days.get(&date.days()) {
            return Ok(*f);
        }
        let f = self.cal.times(date)?.map(|t: SessionTimes| DayFacts {
            pre: t.premarket / NANOS,
            open: t.open / NANOS,
            close: t.close / NANOS,
            normal: t.close - t.open == 6 * 3600 * NANOS + 1800 * NANOS,
            bars: 0,
        });
        self.days.insert(date.days(), f);
        Ok(f)
    }

    /// One bar: its start (UTC nanoseconds), symbol and open, high, low and close (raw) and volume.
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &mut self,
        ts: u64,
        symbol: &str,
        open: i64,
        high: i64,
        low: i64,
        close: i64,
        volume: u64,
    ) -> Result<(), HistoryError> {
        self.report.bars_read += 1;
        let date = self
            .cal
            .date_of(ts)
            .map_err(|e| HistoryError::Calendar(format!("bar at {ts}: {e:?}")))?;
        if date.days() > self.up_to {
            self.report.bars_after += 1;
            return Ok(());
        }
        let Some(f) = self
            .facts(date)
            .map_err(|e| HistoryError::Calendar(format!("{}: {e:?}", date_text(date.days()))))?
        else {
            self.report.bars_closed_day += 1;
            return Ok(());
        };
        if !valid_name(symbol) {
            self.report.bars_bad_symbol += 1;
            return Ok(());
        }
        let sec = ts / NANOS;
        if sec < f.pre || sec >= f.close {
            // Overnight, or after-hours: not used by any column.
            self.report.bars_outside_sessions += 1;
            return Ok(());
        }
        if let Some(Some(day)) = self.days.get_mut(&date.days()) {
            day.bars += 1;
        }
        let s = self
            .symbols
            .entry(symbol.to_owned())
            .or_default()
            .entry(date.days())
            .or_default();
        if sec < f.open {
            s.pre += volume;
            let k = (((sec - f.pre) / 900) as usize).min(QVOL_FIRST_REGULAR - 1);
            s.qvol[k] = s.qvol[k].saturating_add(u32::try_from(volume).unwrap_or(u32::MAX));
            return Ok(());
        }
        // Cap the wick: an off-market print leaves the bar's open and close alone.
        let (orig_high, orig_low) = (high, low);
        let (body_hi, body_lo) = (open.max(close), open.min(close));
        let room = |p: i64| {
            i64::try_from(i128::from(p) * i128::from(self.clip_permille) / 1000).unwrap_or(i64::MAX)
        };
        let (high, low) = (
            high.min(body_hi.saturating_add(room(body_hi))),
            low.max(body_lo.saturating_sub(room(body_lo))),
        );
        if (high, low) != (orig_high, orig_low) {
            self.report.bars_clipped += 1;
        }
        if !s.has_regular {
            s.has_regular = true;
            s.high = high;
            s.low = low;
        } else {
            s.high = s.high.max(high);
            s.low = s.low.min(low);
        }
        if s.close.is_none_or(|(t, _)| ts >= t) {
            s.close = Some((ts, close));
        }
        let since = sec - f.open;
        let q = (since / 900) as usize;
        if q < QUARTERS {
            let k = QVOL_FIRST_REGULAR + q;
            s.qvol[k] = s.qvol[k].saturating_add(u32::try_from(volume).unwrap_or(u32::MAX));
            let t = since as u32;
            if s.qc_t[q] == UNSEEN || t >= s.qc_t[q] {
                s.qc_t[q] = t;
                s.qc[q] = close;
            }
        }
        if s.open_t == UNSEEN || (since as u32) < s.open_t {
            s.open_t = since as u32;
            s.open_px = open;
        }
        if since < 60 {
            s.first1 += volume;
        }
        if since < 300 {
            s.first5 += volume;
        }
        for (i, (_, minutes)) in CUMVOL_CHECKPOINTS.iter().enumerate() {
            if since < u64::from(*minutes) * 60 {
                s.cum[i] += volume;
            }
        }
        let k = (since / 3600) as usize;
        if k < HOURS && s.hours[k].is_none_or(|(t, _)| ts >= t) {
            s.hours[k] = Some((ts, close));
        }
        Ok(())
    }

    /// What reading left out so far, and the days (since 1970-01-01, oldest first) that have bars.
    pub fn report_and_days(&self) -> (HistoryReport, Vec<i64>) {
        let mut days: Vec<i64> = self
            .days
            .iter()
            .filter(|(_, f)| f.is_some_and(|f| f.bars > 0))
            .map(|(d, _)| *d)
            .collect();
        days.sort_unstable();
        (self.report.clone(), days)
    }

    /// One day's sessions of every symbol as text, to keep beside the other days: what [`MinuteHistory::add`] made
    /// of the day's bars, a few numbers per symbol, small against the bars they came from. `None` when the day has
    /// no bars. Read back with [`MinuteHistory::load_day`]. The wick cap it was built with is in the header.
    pub fn export_day(&self, day: i64) -> Option<String> {
        let f = self
            .days
            .get(&day)
            .copied()
            .flatten()
            .filter(|f| f.bars > 0)?;
        let mut names: Vec<&String> = self
            .symbols
            .iter()
            .filter(|(_, by_day)| by_day.contains_key(&day))
            .map(|(n, _)| n)
            .collect();
        names.sort();
        let mut out = format!(
            "{SESSIONS_MAGIC}\nday,{},normal,{},bars,{},clip,{}\n",
            date_text(day),
            u8::from(f.normal),
            f.bars,
            self.clip_permille
        );
        let cell = |v: Option<i64>| v.map_or(String::new(), |x| x.to_string());
        for n in names {
            let s = &self.symbols[n][&day];
            out.push_str(&format!(
                "{n},{},{},{},{},{},{},{}",
                u8::from(s.has_regular),
                s.high,
                s.low,
                cell(s.close.map(|c| c.1)),
                s.pre,
                s.first1,
                s.first5
            ));
            for c in s.cum {
                out.push_str(&format!(",{c}"));
            }
            for h in s.hours {
                out.push_str(&format!(",{}", cell(h.map(|x| x.1))));
            }
            for v in s.qvol {
                out.push_str(&format!(",{v}"));
            }
            for q in 0..QUARTERS {
                out.push_str(&format!(
                    ",{}",
                    cell((s.qc_t[q] != UNSEEN).then_some(s.qc[q]))
                ));
            }
            out.push_str(&format!(
                ",{}",
                cell((s.open_t != UNSEEN).then_some(s.open_px))
            ));
            out.push('\n');
        }
        Some(out)
    }

    /// Take in a day kept by [`MinuteHistory::export_day`]. Refused: text that is not a kept day, a day that has bars
    /// already, a day the market was closed, and a day made with another wick cap (the columns would mix two rules).
    pub fn load_day(&mut self, text: &str) -> Result<(), HistoryError> {
        let bad = |why: String| HistoryError::Sessions(why);
        let mut lines = text.lines();
        match lines.next() {
            Some(SESSIONS_MAGIC) => {}
            Some(l) if l.starts_with("# tf minute sessions ") => {
                return Err(bad(format!(
                    "a kept day of another version (`{l}`; this reads `{SESSIONS_MAGIC}`): fetch it again"
                )));
            }
            _ => {
                return Err(bad(format!(
                    "not a kept day of sessions: expected `{SESSIONS_MAGIC}`"
                )));
            }
        }
        let head: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
        let ["day", day, "normal", normal, "bars", bars, "clip", clip] = head[..] else {
            return Err(bad(
                "the second line is not `day,DATE,normal,N,bars,N,clip,N`".to_owned(),
            ));
        };
        let days = date_days(day).ok_or_else(|| bad(format!("`{day}` is not a date")))?;
        let bars: u64 = bars
            .parse()
            .map_err(|_| bad(format!("`{bars}` is not a count")))?;
        let clip: u32 = clip
            .parse()
            .map_err(|_| bad(format!("`{clip}` is not a wick cap")))?;
        if clip != self.clip_permille {
            return Err(bad(format!(
                "{day} was made with a wick cap of {clip} permille and this history uses {}",
                self.clip_permille
            )));
        }
        let facts = self
            .facts(Date::from_days(days))
            .map_err(|e| HistoryError::Calendar(format!("{day}: {e:?}")))?
            .ok_or_else(|| bad(format!("{day} is a day the market was closed")))?;
        if self
            .days
            .get(&days)
            .copied()
            .flatten()
            .is_some_and(|f| f.bars > 0)
        {
            return Err(bad(format!("{day} has bars already")));
        }
        if (normal == "1") != facts.normal || !matches!(normal, "0" | "1") {
            return Err(bad(format!(
                "{day}: the session length is not the calendar's"
            )));
        }
        let mut sessions = HashMap::new();
        for (i, line) in lines.enumerate() {
            let at = |why: String| bad(format!("{day} line {}: {why}", i + 3));
            let c: Vec<&str> = line.split(',').collect();
            if c.len() != FIELDS {
                return Err(at(format!("{} fields, expected {FIELDS}", c.len())));
            }
            if !valid_name(c[0]) {
                return Err(at(format!("`{}` is not a symbol", c[0])));
            }
            let num = |k: usize| {
                c[k].parse::<u64>()
                    .map_err(|_| at(format!("`{}` is not a number", c[k])))
            };
            let px = |k: usize| {
                c[k].parse::<i64>()
                    .map_err(|_| at(format!("`{}` is not a price", c[k])))
            };
            let opt = |k: usize| -> Result<Option<i64>, HistoryError> {
                if c[k].is_empty() {
                    Ok(None)
                } else {
                    px(k).map(Some)
                }
            };
            let mut s = Session {
                has_regular: match c[1] {
                    "0" => false,
                    "1" => true,
                    o => return Err(at(format!("`{o}` is not 0 or 1"))),
                },
                high: px(2)?,
                low: px(3)?,
                // The bar's own time is not kept: nothing is added to a kept day.
                close: opt(4)?.map(|v| (0, v)),
                pre: num(5)?,
                first1: num(6)?,
                first5: num(7)?,
                ..Session::default()
            };
            for (k, slot) in s.cum.iter_mut().enumerate() {
                *slot = num(8 + k)?;
            }
            for (k, slot) in s.hours.iter_mut().enumerate() {
                *slot = opt(15 + k)?.map(|v| (0, v));
            }
            for (k, slot) in s.qvol.iter_mut().enumerate() {
                *slot = c[15 + HOURS + k]
                    .parse::<u32>()
                    .map_err(|_| at(format!("`{}` is not a volume", c[15 + HOURS + k])))?;
            }
            for q in 0..QUARTERS {
                if let Some(v) = opt(15 + HOURS + QVOL_COUNT + q)? {
                    // The bar's own time is not kept either: nothing is added to a kept day.
                    s.qc_t[q] = 0;
                    s.qc[q] = v;
                }
            }
            if let Some(v) = opt(15 + HOURS + QVOL_COUNT + QUARTERS)? {
                s.open_t = 0;
                s.open_px = v;
            }
            if sessions.insert(c[0].to_owned(), s).is_some() {
                return Err(at(format!("{} twice", c[0])));
            }
        }
        for (name, s) in sessions {
            self.symbols.entry(name).or_default().insert(days, s);
        }
        if let Some(Some(f)) = self.days.get_mut(&days) {
            f.bars = bars;
        }
        Ok(())
    }

    /// The columns for every symbol, sorted by symbol.
    pub fn build(
        &self,
        p: HistoryParams,
    ) -> Result<(Vec<HistoryRow>, HistoryReport), HistoryError> {
        self.build_as_of(i64::MAX, p)
    }

    /// The columns as they were known after the session on or before `last_day` (days since 1970-01-01): bars of
    /// a later day are not looked at, so one history serves every day of a range, each as of the session before it
    /// (E19-S52). `NoBars` when there is no session on or before `last_day`.
    pub fn build_as_of(
        &self,
        last_day: i64,
        p: HistoryParams,
    ) -> Result<(Vec<HistoryRow>, HistoryReport), HistoryError> {
        let mut rep = self.report.clone();
        let with_data: Vec<i64> = {
            let mut v: Vec<i64> = self
                .days
                .iter()
                .filter(|(d, f)| **d <= last_day && f.is_some_and(|f| f.bars > 0))
                .map(|(d, _)| *d)
                .collect();
            v.sort_unstable();
            v
        };
        let (Some(&first), Some(&as_of)) = (with_data.first(), with_data.last()) else {
            return Err(HistoryError::NoBars);
        };
        // Every trading day from the first to the last must have bars.
        let mut days: Vec<i64> = Vec::new();
        let mut d = Date::from_days(first);
        while d.days() <= as_of {
            if self
                .cal
                .is_trading_day(d)
                .map_err(|e| HistoryError::Calendar(format!("{e:?}")))?
            {
                if self
                    .days
                    .get(&d.days())
                    .and_then(|f| *f)
                    .is_none_or(|f| f.bars == 0)
                {
                    return Err(HistoryError::MissingSession(date_text(d.days())));
                }
                days.push(d.days());
            }
            d = d.next();
        }
        let keep = p.sessions.max(1).min(days.len());
        let days = &days[days.len() - keep..];
        let normal = |d: i64| self.days.get(&d).and_then(|f| *f).is_some_and(|f| f.normal);
        let avg_days: Vec<i64> = days[days.len().saturating_sub(p.average_over.max(1))..].to_vec();
        let avg_normal: Vec<i64> = avg_days.iter().copied().filter(|d| normal(*d)).collect();
        rep.as_of = date_text(as_of);
        rep.first_session = date_text(days[0]);
        rep.sessions = days.len();
        rep.early_closes = days.iter().filter(|d| !normal(**d)).count();

        let mut names: Vec<&String> = self
            .symbols
            .iter()
            .filter(|(_, by_day)| by_day.keys().any(|d| *d <= as_of))
            .map(|(n, _)| n)
            .collect();
        names.sort();
        rep.symbols = names.len();
        let mut rows = Vec::with_capacity(names.len());
        for name in names {
            let by_day = &self.symbols[name];
            let mut v = [None::<i64>; 16];
            let regular = |d: i64| by_day.get(&d).filter(|s| s.has_regular);
            // prev_high, prev_low, prev_close
            if let Some(s) = regular(as_of) {
                v[0] = Some(s.high);
                v[1] = Some(s.low);
                v[2] = s.close.map(|c| c.1);
            }
            // atr14: 15 sessions, all traded.
            if days.len() > ATR_PERIOD {
                let tail = &days[days.len() - ATR_PERIOD - 1..];
                let sess: Vec<Option<&Session>> = tail.iter().map(|d| regular(*d)).collect();
                if sess.iter().all(Option::is_some) {
                    let mut sum: i128 = 0;
                    for w in sess.windows(2) {
                        let (prev, cur) = (w[0].unwrap(), w[1].unwrap());
                        let pc = i128::from(prev.close.map_or(0, |c| c.1));
                        let (h, l) = (i128::from(cur.high), i128::from(cur.low));
                        sum += (h - l).max((h - pc).abs()).max((l - pc).abs());
                    }
                    v[3] = i64::try_from(sum / ATR_PERIOD as i128).ok();
                }
            }
            // ema100h over the hourly closes of every session kept.
            let mut ema = Ema::new(EMA_PERIOD, Seed::Sma);
            for d in days {
                if let Some(s) = regular(*d) {
                    for h in s.hours.iter().flatten() {
                        ema.update(h.1);
                    }
                }
            }
            if let Some((state, count)) = ema.state() {
                v[4] = Some(state);
                v[5] = Some(i64::from(count));
            }
            // Volume averages.
            let traded = |ds: &[i64]| ds.iter().filter(|d| regular(**d).is_some()).count();
            let average = |ds: &[i64], f: &dyn Fn(&Session) -> u64| -> Option<i64> {
                if ds.is_empty() || traded(ds) < p.min_days {
                    return None;
                }
                let sum: u128 = ds
                    .iter()
                    .filter_map(|d| by_day.get(d))
                    .map(|s| u128::from(f(s)))
                    .sum();
                i64::try_from(sum / ds.len() as u128).ok()
            };
            v[6] = average(&avg_days, &|s| s.first1);
            v[7] = average(&avg_days, &|s| s.first5);
            v[8] = average(&avg_days, &|s| s.pre);
            for i in 0..7 {
                v[9 + i] = average(&avg_normal, &|s| s.cum[i]);
            }
            // The time-of-day profile: volume per quarter hour, the premarket over every session of the window and the regular session over
            // the normal ones (as the cumulative columns), and the mean absolute 60-minute move over the normal sessions that traded at
            // both ends of it.
            let mut profile = Profile::default();
            for k in 0..QVOL_COUNT {
                let days = if k < QVOL_FIRST_REGULAR {
                    &avg_days
                } else {
                    &avg_normal
                };
                profile.set_vol_quarter(k, average(days, &|s| u64::from(s.qvol[k])));
            }
            for j in 0..QMOVE_COUNT {
                let (mut n, mut sum) = (0usize, 0u128);
                for d in &avg_normal {
                    let Some(s) = regular(*d) else { continue };
                    let (Some(a), Some(b)) = (s.boundary(j), s.boundary(j + 4)) else {
                        continue;
                    };
                    if a <= 0 {
                        continue;
                    }
                    n += 1;
                    sum += (i128::from(b) - i128::from(a)).unsigned_abs() * 100_000
                        / u128::from(a.unsigned_abs());
                }
                if n >= p.min_days.max(1) {
                    profile.set_move_quarter(j, i64::try_from(sum / n as u128).ok());
                }
            }
            rows.push(HistoryRow {
                symbol: name.clone(),
                values: v,
                profile,
            });
        }
        Ok((rows, rep))
    }
}

/// Read a Databento `ohlcv-1m` CSV with fixed-point prices and integer timestamps, the symbol mapped in
/// (`pretty_px=false`, `pretty_ts=false`, `map_symbols=true`), as a stream into `h`. A line that does not parse
/// is an error: a skipped bar would change an average.
pub fn read_minute_bars(input: impl BufRead, h: &mut MinuteHistory) -> Result<(), ReadError> {
    read_bars_with(input, None, h)
}

/// The same for a CSV without symbols (`map_symbols=false`, the only form Databento gives for every symbol at
/// once): each bar's instrument number is named by `names` (a `symbology.resolve` answer for the same dataset and
/// day), and a bar whose number it does not name is counted under the names not listable.
pub fn read_minute_bars_by_id(
    input: impl BufRead,
    names: &Symbology,
    h: &mut MinuteHistory,
) -> Result<(), ReadError> {
    read_bars_with(input, Some(names), h)
}

fn read_bars_with(
    input: impl BufRead,
    names: Option<&Symbology>,
    h: &mut MinuteHistory,
) -> Result<(), ReadError> {
    const HEADER: &str =
        "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume,symbol";
    const HEADER_BY_ID: &str =
        "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume";
    let want = if names.is_some() {
        HEADER_BY_ID
    } else {
        HEADER
    };
    let mut lines = input.lines().enumerate();
    match lines.next() {
        Some((_, Ok(l))) if l == want => {}
        _ => {
            return Err(ReadError {
                line: 1,
                why: if names.is_some() {
                    format!(
                        "expected the header `{want}` (fixed-point prices, integer times, no symbols)"
                    )
                } else {
                    format!(
                        "expected the header `{want}` (fixed-point prices, integer times, symbols mapped)"
                    )
                },
            });
        }
    }
    let fields = if names.is_some() { 9 } else { 10 };
    for (i, line) in lines {
        let err = |why: String| ReadError { line: i + 1, why };
        let line = line.map_err(|e| err(e.to_string()))?;
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != fields {
            return Err(err(format!("{} fields, expected {fields}", c.len())));
        }
        let n = |k: usize| {
            c[k].parse::<u64>()
                .map_err(|_| err(format!("`{}` is not a number", c[k])))
        };
        let px =
            |k: usize| i64::try_from(n(k)?).map_err(|_| err(format!("`{}` is too large", c[k])));
        if n(1)? != 33 {
            return Err(err(format!("rtype {} is not a one-minute bar (33)", c[1])));
        }
        let ts = n(0)?;
        if ts % (60 * NANOS) != 0 {
            return Err(err("a one-minute bar starts on a minute".to_owned()));
        }
        let (open, high, low, close) = (px(4)?, px(5)?, px(6)?, px(7)?);
        if high < open.max(close) || low > open.min(close) {
            return Err(err(
                "the bar's high and low do not contain its open and close".to_owned(),
            ));
        }
        let symbol = match names {
            None => c[9],
            Some(names) => {
                let id =
                    u32::try_from(n(3)?).map_err(|_| err(format!("`{}` is too large", c[3])))?;
                let day = i64::try_from(ts / (86_400 * NANOS)).unwrap_or(i64::MAX);
                names.symbol(id, day).unwrap_or("")
            }
        };
        h.add(ts, symbol, open, high, low, close, n(8)?)
            .map_err(|e| err(e.to_string()))?;
    }
    Ok(())
}

/// Put the history columns into rows built from daily bars. Symbols the history has and the rows lack are
/// counted, not added; rows without history keep unknown cells. Returns `(matched, history_only)`.
pub fn merge_history(rows: &mut [RefRow], hist: &[HistoryRow]) -> (usize, usize) {
    let mut matched = 0;
    let by: HashMap<&str, &HistoryRow> = hist.iter().map(|h| (h.symbol.as_str(), h)).collect();
    for r in rows.iter_mut() {
        if let Some(h) = by.get(r.symbol.as_str()) {
            matched += 1;
            for (f, v) in HISTORY_FEATURES.iter().zip(h.values) {
                r.set_num(*f, v);
            }
            r.profile = h.profile;
        }
    }
    (matched, hist.len() - matched)
}

/// The columns [`merge_history`] fills in.
pub const HISTORY_COLUMNS: [StaticFeature; 16] = HISTORY_FEATURES;

/// The columns of the time-of-day profile [`merge_history`] fills in as well (E19-S53).
pub fn profile_columns() -> impl Iterator<Item = StaticFeature> {
    profile_features()
}
