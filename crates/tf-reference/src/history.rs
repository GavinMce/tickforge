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
//! - Integer arithmetic only; averages divide down.

use std::collections::HashMap;
use std::io::BufRead;

use tf_calendar::{Calendar, CalendarError, Date, SessionTimes};
use tf_engine::{Ema, Seed};
use tf_universe::{CUMVOL_CHECKPOINTS, HISTORY_FEATURES, RefRow, StaticFeature, valid_name};

use crate::bars::{ReadError, date_text};

/// Hourly buckets of a regular session (09:30 to 16:00 is seven, the last a half hour).
const HOURS: usize = 7;
const NANOS: u64 = 1_000_000_000;
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

#[derive(Clone, Copy, Debug, Default)]
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
            HistoryError::Calendar(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HistoryError {}

/// The history columns of one symbol, in [`HISTORY_FEATURES`] order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    pub symbol: String,
    pub values: [Option<i64>; 16],
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

    /// The columns for every symbol, sorted by symbol.
    pub fn build(
        &self,
        p: HistoryParams,
    ) -> Result<(Vec<HistoryRow>, HistoryReport), HistoryError> {
        let mut rep = self.report.clone();
        let with_data: Vec<i64> = {
            let mut v: Vec<i64> = self
                .days
                .iter()
                .filter(|(_, f)| f.is_some_and(|f| f.bars > 0))
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

        let mut names: Vec<&String> = self.symbols.keys().collect();
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
            rows.push(HistoryRow {
                symbol: name.clone(),
                values: v,
            });
        }
        Ok((rows, rep))
    }
}

/// Read a Databento `ohlcv-1m` CSV with fixed-point prices and integer timestamps, the symbol mapped in
/// (`pretty_px=false`, `pretty_ts=false`, `map_symbols=true`), as a stream into `h`. A line that does not parse
/// is an error: a skipped bar would change an average.
pub fn read_minute_bars(input: impl BufRead, h: &mut MinuteHistory) -> Result<(), ReadError> {
    const HEADER: &str =
        "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume,symbol";
    let mut lines = input.lines().enumerate();
    match lines.next() {
        Some((_, Ok(l))) if l == HEADER => {}
        _ => {
            return Err(ReadError {
                line: 1,
                why: format!(
                    "expected the header `{HEADER}` (fixed-point prices, integer times, symbols mapped)"
                ),
            });
        }
    }
    for (i, line) in lines {
        let err = |why: String| ReadError { line: i + 1, why };
        let line = line.map_err(|e| err(e.to_string()))?;
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != 10 {
            return Err(err(format!("{} fields, expected 10", c.len())));
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
        h.add(ts, c[9], open, high, low, close, n(8)?)
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
        }
    }
    (matched, hist.len() - matched)
}

/// The columns [`merge_history`] fills in.
pub const HISTORY_COLUMNS: [StaticFeature; 16] = HISTORY_FEATURES;
