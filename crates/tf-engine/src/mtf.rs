//! Multi-timeframe bars: 1m, 5m, 15m, 1h and day, built live from trades.
//!
//! Tier 0 keeps 60 one-second bars, which cannot support an EMA of minute closes
//! or an opening range. [`MtfBars`] builds longer bars for a bounded set of tracked
//! symbols. Per symbol and timeframe it keeps the last [`BAR_DEPTH`] closed bars
//! and the bar still forming.
//!
//! Rules, all deliberate and tested:
//! - **Alignment** ([`Alignment`], one choice for the whole aggregator).
//!   - *Clock.* Bars of 1m to 1h start on multiples of their length since the Unix epoch (so an hourly
//!     bar is a UTC clock hour: 09:00 New York time in summer, 08:00 in winter). Day bars start at a
//!     configured offset into the UTC day (`day_open_offset_secs`). There is no time-zone database: the
//!     offset is fixed, so across a daylight-saving change the caller must change it.
//!   - *Session.* The calendar ([`tf_calendar`]) places every trade in the premarket (from 04:00), the regular
//!     session or after-hours (to 20:00). Hourly bars start at the beginning of their session and run an
//!     hour each, the last one cut short at the session's end: 09:30, 10:30 ... 14:30, then the 15:30 to 16:00
//!     stub (09:30 to 12:30 and the 12:30 to 13:00 stub on an early close). The day bar is the trading day, 04:00
//!     to 20:00 New York time, whatever the offset from UTC is that day. 1m, 5m and 15m bars are epoch-aligned as
//!     before (a session boundary is always a multiple of 15 minutes, so none straddles one). A trade outside
//!     every session, or on a day the calendar cannot answer for, is not placed in any bar and is counted
//!     ([`MtfBars::unplaced`]). Gap filling is a clock-alignment option and is ignored in session alignment.
//! - **Time.** Trades are placed by `ts_recv`, which is non-decreasing. A stale
//!   timestamp counts in the latest second seen.
//! - **Closing.** A bar closes when a trade arrives in a later interval, or when
//!   [`MtfBars::advance_to`] is told time has passed its end, so a quiet symbol's
//!   bar still closes on time. Every close is reported, in order. Time is looked at for every tracked
//!   symbol only when some forming bar is due, not on every trade, so the cost of a trade does not grow
//!   with the number of tracked symbols.
//! - **Empty intervals.** By default there is no bar for an interval with no trades.
//!   With [`MtfConfig::fill_gaps`] (clock alignment), flat bars (open = high = low = close = the previous
//!   close, no volume) fill the interval, up to [`BAR_DEPTH`] of them.
//! - **Corrections and cancels** are ignored. A closed bar is never rewound; the
//!   storage side can apply corrections after the fact.
//! - Volume and prices are exact integers; VWAP is rounded down.
//!
//! Integer arithmetic only, no clock, no allocation per event (each symbol's state
//! is `Copy` and boxed once when tracking starts). A symbol is about 40 KB, so the
//! tracked set is bounded: 1,000 symbols is about 40 MB.

use tf_calendar::Calendar;
use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};

/// Closed bars kept per symbol and timeframe.
pub const BAR_DEPTH: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Timeframe {
    M1,
    M5,
    M15,
    H1,
    Day,
}

impl Timeframe {
    pub const ALL: [Timeframe; 5] = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::H1,
        Timeframe::Day,
    ];

    pub const fn secs(self) -> u64 {
        match self {
            Timeframe::M1 => 60,
            Timeframe::M5 => 300,
            Timeframe::M15 => 900,
            Timeframe::H1 => 3600,
            Timeframe::Day => 86_400,
        }
    }

    pub const fn index(self) -> usize {
        self as usize
    }
}

/// How bars are aligned (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    /// Epoch-aligned bars; day bars start `day_open_offset_secs` after 00:00 UTC.
    Clock { day_open_offset_secs: u64 },
    /// Hourly and day bars follow the trading sessions of the calendar.
    Session,
}

impl Default for Alignment {
    fn default() -> Self {
        Alignment::Clock {
            day_open_offset_secs: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MtfConfig {
    pub alignment: Alignment,
    /// Fill empty intervals with flat bars (clock alignment only).
    pub fill_gaps: bool,
}

impl MtfConfig {
    /// Clock alignment with a day bar starting `day_open_offset_secs` after 00:00 UTC.
    pub fn clock(day_open_offset_secs: u64, fill_gaps: bool) -> MtfConfig {
        MtfConfig {
            alignment: Alignment::Clock {
                day_open_offset_secs,
            },
            fill_gaps,
        }
    }

    /// Session alignment from the calendar.
    pub fn session() -> MtfConfig {
        MtfConfig {
            alignment: Alignment::Session,
            fill_gaps: false,
        }
    }
}

/// The boundaries of one trading day in seconds since the epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DaySecs {
    pre: u64,
    open: u64,
    close: u64,
    end: u64,
}

/// Decides where the bar containing a second starts and ends, for a whole aggregator. In session
/// alignment it looks the day up in the calendar once and keeps it until a trade falls outside it.
#[derive(Clone, Debug)]
pub struct Placer {
    cfg: MtfConfig,
    day: Option<DaySecs>,
}

impl Placer {
    pub fn new(cfg: MtfConfig) -> Placer {
        Placer { cfg, day: None }
    }

    pub fn config(&self) -> &MtfConfig {
        &self.cfg
    }

    fn day_for(&mut self, sec: u64) -> Option<DaySecs> {
        if let Some(d) = self.day {
            if sec >= d.pre && sec < d.end {
                return Some(d);
            }
        }
        let ns = sec.checked_mul(NANOS_PER_SEC)?;
        let cal = Calendar::us_equities();
        let date = cal.date_of(ns).ok()?;
        let t = cal.times(date).ok()??;
        let d = DaySecs {
            pre: t.premarket / NANOS_PER_SEC,
            open: t.open / NANOS_PER_SEC,
            close: t.close / NANOS_PER_SEC,
            end: t.after_hours_end / NANOS_PER_SEC,
        };
        if sec >= d.pre && sec < d.end {
            self.day = Some(d);
            Some(d)
        } else {
            None
        }
    }

    /// The `[start, end)` seconds of the bar of timeframe `tf` that contains `sec`; `None` for a second
    /// that belongs to no bar (session alignment, outside every session).
    pub fn span(&mut self, tf: Timeframe, sec: u64) -> Option<(u64, u64)> {
        let len = tf.secs();
        match self.cfg.alignment {
            Alignment::Clock {
                day_open_offset_secs,
            } => {
                let start = match tf {
                    Timeframe::Day => {
                        let off = day_open_offset_secs % len;
                        // Before the first offset of the epoch day, fall back to the epoch day.
                        if sec < off {
                            0
                        } else {
                            (sec - off) / len * len + off
                        }
                    }
                    _ => sec / len * len,
                };
                Some((start, start + len))
            }
            Alignment::Session => {
                let d = self.day_for(sec)?;
                Some(match tf {
                    Timeframe::Day => (d.pre, d.end),
                    Timeframe::H1 => {
                        let (from, to) = if sec < d.open {
                            (d.pre, d.open)
                        } else if sec < d.close {
                            (d.open, d.close)
                        } else {
                            (d.close, d.end)
                        };
                        let start = from + (sec - from) / len * len;
                        (start, (start + len).min(to))
                    }
                    _ => {
                        let start = sec / len * len;
                        (start, start + len)
                    }
                })
            }
        }
    }
}

/// One bar. `start_sec` is the interval start, in seconds since the epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TfBar {
    pub start_sec: u64,
    pub open: Px,
    pub high: Px,
    pub low: Px,
    pub close: Px,
    pub volume: u64,
    pub trades: u32,
    /// Sum of price (raw) x shares.
    pub notional: u128,
}

impl TfBar {
    fn first(start_sec: u64, px: Px, size: u32) -> TfBar {
        TfBar {
            start_sec,
            open: px,
            high: px,
            low: px,
            close: px,
            volume: u64::from(size),
            trades: 1,
            notional: u128::try_from(px.raw()).unwrap_or(0) * u128::from(size),
        }
    }

    fn flat(start_sec: u64, px: Px) -> TfBar {
        TfBar {
            start_sec,
            open: px,
            high: px,
            low: px,
            close: px,
            volume: 0,
            trades: 0,
            notional: 0,
        }
    }

    fn add(&mut self, px: Px, size: u32) {
        self.high = self.high.max(px);
        self.low = self.low.min(px);
        self.close = px;
        self.volume += u64::from(size);
        self.trades = self.trades.saturating_add(1);
        self.notional += u128::try_from(px.raw()).unwrap_or(0) * u128::from(size);
    }

    /// Volume-weighted average price, rounded down; `None` for a bar with no volume.
    pub fn vwap(&self) -> Option<Px> {
        if self.volume == 0 {
            return None;
        }
        i64::try_from(self.notional / u128::from(self.volume))
            .ok()
            .map(Px::from_raw)
    }
}

#[derive(Clone, Copy)]
struct Series {
    ring: [TfBar; BAR_DEPTH],
    /// Closed bars pushed so far (flat fills included).
    n: u64,
    forming: Option<TfBar>,
    /// Second at which the forming bar ends (the next interval's start, or the session's end for a stub).
    forming_end: u64,
}

impl Series {
    const fn new() -> Series {
        Series {
            ring: [TfBar {
                start_sec: 0,
                open: Px::ZERO,
                high: Px::ZERO,
                low: Px::ZERO,
                close: Px::ZERO,
                volume: 0,
                trades: 0,
                notional: 0,
            }; BAR_DEPTH],
            n: 0,
            forming: None,
            forming_end: 0,
        }
    }

    fn push(&mut self, bar: TfBar) {
        self.ring[(self.n % BAR_DEPTH as u64) as usize] = bar;
        self.n += 1;
    }

    fn latest(&self) -> Option<&TfBar> {
        (self.n > 0).then(|| &self.ring[((self.n - 1) % BAR_DEPTH as u64) as usize])
    }
}

/// One symbol's bars on every timeframe.
#[derive(Clone, Copy)]
pub struct SymbolBars {
    series: [Series; 5],
    now_sec: u64,
}

impl Default for SymbolBars {
    fn default() -> Self {
        Self::new()
    }
}

impl SymbolBars {
    pub const fn new() -> SymbolBars {
        SymbolBars {
            series: [Series::new(); 5],
            now_sec: 0,
        }
    }

    /// A trade. Every bar this closes (a flat filler included), in order, is appended to
    /// `out` with its timeframe. False if the placer puts the trade in no bar (session alignment,
    /// outside every session): nothing is kept then, but time still passes.
    pub fn on_trade(
        &mut self,
        placer: &mut Placer,
        ts: Nanos,
        px: Px,
        size: u32,
        out: &mut Vec<(Timeframe, TfBar)>,
    ) -> bool {
        let sec = (ts / NANOS_PER_SEC).max(self.now_sec);
        self.now_sec = sec;
        let fill = placer.cfg.fill_gaps && matches!(placer.cfg.alignment, Alignment::Clock { .. });
        let mut placed = true;
        for tf in Timeframe::ALL {
            let s = &mut self.series[tf.index()];
            let Some((start, end)) = placer.span(tf, sec) else {
                placed = false;
                // The trade is in no bar, but time has passed for the bars already forming.
                if let Some(f) = s.forming.filter(|_| sec >= s.forming_end) {
                    s.push(f);
                    s.forming = None;
                    out.push((tf, f));
                }
                continue;
            };
            match s.forming {
                Some(ref mut f) if f.start_sec == start => f.add(px, size),
                Some(f) => {
                    s.push(f);
                    out.push((tf, f));
                    s.forming = None;
                    Self::open_new(s, tf, fill, start, end, px, size, out);
                }
                None => Self::open_new(s, tf, fill, start, end, px, size, out),
            }
        }
        placed
    }

    #[allow(clippy::too_many_arguments)]
    fn open_new(
        s: &mut Series,
        tf: Timeframe,
        fill_gaps: bool,
        start: u64,
        end: u64,
        px: Px,
        size: u32,
        out: &mut Vec<(Timeframe, TfBar)>,
    ) {
        if fill_gaps {
            if let Some(last) = s.latest().copied() {
                let len = tf.secs();
                let mut at = last.start_sec + len;
                let mut filled = 0;
                // Only the last BAR_DEPTH intervals can matter, however long the gap.
                if start > at + len * BAR_DEPTH as u64 {
                    at = start - len * BAR_DEPTH as u64;
                }
                while at < start && filled < BAR_DEPTH {
                    let flat = TfBar::flat(at, last.close);
                    s.push(flat);
                    out.push((tf, flat));
                    at += len;
                    filled += 1;
                }
            }
        }
        s.forming = Some(TfBar::first(start, px, size));
        s.forming_end = end;
    }

    /// Time has reached `ts`: close any forming bar whose interval has ended.
    pub fn advance_to(&mut self, ts: Nanos, out: &mut Vec<(Timeframe, TfBar)>) {
        let sec = (ts / NANOS_PER_SEC).max(self.now_sec);
        self.now_sec = sec;
        for tf in Timeframe::ALL {
            let s = &mut self.series[tf.index()];
            if let Some(f) = s.forming {
                if sec >= s.forming_end {
                    s.push(f);
                    s.forming = None;
                    out.push((tf, f));
                }
            }
        }
    }

    /// The earliest second at which a forming bar ends, if any bar is forming.
    pub fn next_due(&self) -> Option<u64> {
        self.series
            .iter()
            .filter(|s| s.forming.is_some())
            .map(|s| s.forming_end)
            .min()
    }

    /// The `i`th most recent closed bar (0 = latest).
    pub fn closed(&self, tf: Timeframe, i: usize) -> Option<&TfBar> {
        let s = &self.series[tf.index()];
        (i < self.closed_len(tf))
            .then(|| &s.ring[((s.n - 1 - i as u64) % BAR_DEPTH as u64) as usize])
    }

    pub fn closed_len(&self, tf: Timeframe) -> usize {
        self.series[tf.index()].n.min(BAR_DEPTH as u64) as usize
    }

    /// The bar still forming, if any trade has arrived in the current interval.
    pub fn forming(&self, tf: Timeframe) -> Option<&TfBar> {
        self.series[tf.index()].forming.as_ref()
    }

    /// Closed bars in total, including those that have left the ring.
    pub fn closed_total(&self, tf: Timeframe) -> u64 {
        self.series[tf.index()].n
    }
}

/// A bar that has just closed. It carries the bar itself: when a gap is filled, several
/// bars close at once and the latest one in the series is not the one being reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarClose {
    pub instrument: InstrumentId,
    pub timeframe: Timeframe,
    pub bar: TfBar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackError {
    Unknown,
    AlreadyTracked,
    Full,
}

/// Bars for a bounded set of tracked symbols.
pub struct MtfBars {
    placer: Placer,
    slots: Vec<Option<Box<SymbolBars>>>,
    /// Tracked ids in ascending order, so closes are reported deterministically.
    tracked: Vec<InstrumentId>,
    max: usize,
    /// Reused between calls so closing bars does not allocate.
    scratch: Vec<(Timeframe, TfBar)>,
    /// The latest second any event has shown; a stale timestamp counts in it.
    now_sec: u64,
    /// No forming bar ends before this second (`u64::MAX` when none is forming), so time need not be
    /// offered to every tracked symbol on every trade.
    next_due: u64,
    /// Trades that session alignment put in no bar.
    unplaced: u64,
}

impl MtfBars {
    pub fn new(cfg: MtfConfig, id_space: usize, max_tracked: usize) -> MtfBars {
        MtfBars {
            placer: Placer::new(cfg),
            slots: (0..id_space).map(|_| None).collect(),
            tracked: Vec::new(),
            max: max_tracked,
            scratch: Vec::new(),
            now_sec: 0,
            next_due: u64::MAX,
            unplaced: 0,
        }
    }

    /// Start building bars for `id`. The one allocation made for a symbol.
    pub fn track(&mut self, id: InstrumentId) -> Result<(), TrackError> {
        let slot = self.slots.get_mut(id as usize).ok_or(TrackError::Unknown)?;
        if slot.is_some() {
            return Err(TrackError::AlreadyTracked);
        }
        if self.tracked.len() >= self.max {
            return Err(TrackError::Full);
        }
        *slot = Some(Box::new(SymbolBars::new()));
        let at = self.tracked.partition_point(|&t| t < id);
        self.tracked.insert(at, id);
        Ok(())
    }

    pub fn untrack(&mut self, id: InstrumentId) -> bool {
        match self.slots.get_mut(id as usize).and_then(Option::take) {
            Some(_) => {
                self.tracked.retain(|&t| t != id);
                true
            }
            None => false,
        }
    }

    pub fn tracked(&self) -> usize {
        self.tracked.len()
    }

    /// Tracked ids, ascending.
    pub fn tracked_ids(&self) -> &[InstrumentId] {
        &self.tracked
    }

    pub fn max_tracked(&self) -> usize {
        self.max
    }

    pub fn config(&self) -> &MtfConfig {
        self.placer.config()
    }

    /// Trades of tracked symbols that session alignment put in no bar (outside every session, or on a day
    /// the calendar does not cover).
    pub fn unplaced(&self) -> u64 {
        self.unplaced
    }

    pub fn symbol(&self, id: InstrumentId) -> Option<&SymbolBars> {
        self.slots.get(id as usize)?.as_deref()
    }

    /// Feed a market event. Bars that closed are appended to `out`, in time order
    /// (symbols that merely ran out of time first, in id order, then the symbol that
    /// traded). Only trades build bars.
    pub fn on_event(&mut self, ev: &Event, out: &mut Vec<BarClose>) {
        let Event::Trade(t) = ev else { return };
        let ts = t.hdr.ts_recv;
        self.advance_to(ts, out);
        let id = t.hdr.instrument;
        if let Some(Some(s)) = self.slots.get_mut(id as usize) {
            self.scratch.clear();
            // A stale timestamp counts in the latest second seen by any symbol.
            let at = ts.max(self.now_sec.saturating_mul(NANOS_PER_SEC));
            if !s.on_trade(&mut self.placer, at, t.px, t.size, &mut self.scratch) {
                self.unplaced += 1;
            }
            if let Some(due) = s.next_due() {
                self.next_due = self.next_due.min(due);
            }
            out.extend(self.scratch.iter().map(|&(timeframe, bar)| BarClose {
                instrument: id,
                timeframe,
                bar,
            }));
        }
    }

    /// Time has reached `ts`: close what has ended, for every tracked symbol.
    pub fn advance_to(&mut self, ts: Nanos, out: &mut Vec<BarClose>) {
        let sec = ts / NANOS_PER_SEC;
        self.now_sec = self.now_sec.max(sec);
        if self.now_sec < self.next_due {
            return;
        }
        let mut due = u64::MAX;
        for &id in &self.tracked {
            if let Some(Some(s)) = self.slots.get_mut(id as usize) {
                self.scratch.clear();
                s.advance_to(
                    self.now_sec.saturating_mul(NANOS_PER_SEC),
                    &mut self.scratch,
                );
                out.extend(self.scratch.iter().map(|&(timeframe, bar)| BarClose {
                    instrument: id,
                    timeframe,
                    bar,
                }));
                if let Some(d) = s.next_due() {
                    due = due.min(d);
                }
            }
        }
        self.next_due = due;
    }
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<SymbolBars>();
    is_copy::<TfBar>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use tf_core::{Header, ProviderId, Trade, TradeFlags};
    use tf_synth::{Scenario, SplitMix64, SymbolSpec, SynthConfig, SynthStream};

    const S: Nanos = NANOS_PER_SEC;

    /// Test helpers that return what closed instead of filling a buffer.
    trait Quick {
        fn trade(
            &mut self,
            cfg: &MtfConfig,
            ts: Nanos,
            px: Px,
            size: u32,
        ) -> Vec<(Timeframe, TfBar)>;
        fn advance(&mut self, ts: Nanos) -> Vec<(Timeframe, TfBar)>;
    }

    impl Quick for SymbolBars {
        fn trade(
            &mut self,
            cfg: &MtfConfig,
            ts: Nanos,
            px: Px,
            size: u32,
        ) -> Vec<(Timeframe, TfBar)> {
            let mut out = Vec::new();
            self.on_trade(&mut Placer::new(*cfg), ts, px, size, &mut out);
            out
        }
        fn advance(&mut self, ts: Nanos) -> Vec<(Timeframe, TfBar)> {
            let mut out = Vec::new();
            self.advance_to(ts, &mut out);
            out
        }
    }
    /// 2026-01-05 00:00:00 UTC, a Monday; a multiple of a day.
    const DAY0: u64 = 1_767_571_200;

    fn px(cents: i64) -> Px {
        Px::from_cents(cents)
    }

    fn trade_ev(inst: u32, ts: Nanos, cents: i64, size: u32) -> Event {
        Event::Trade(Trade {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument: inst,
                provider: ProviderId::Synthetic,
            },
            px: px(cents),
            size,
            flags: TradeFlags::NONE,
        })
    }

    fn at(sec: u64) -> Nanos {
        sec * S
    }

    #[test]
    fn a_worked_example_gives_the_hand_bars() {
        let cfg = MtfConfig::default();
        let mut b = SymbolBars::new();
        // 12:00:10 .. 12:02:40 relative to a day start; three minutes.
        let t0 = DAY0 + 12 * 3600;
        let trades = [
            (10, 1000, 100),
            (30, 1020, 200),
            (59, 990, 100),
            (60, 1005, 50),
            (119, 1010, 50),
            (160, 1100, 10),
        ];
        let mut closes = Vec::new();
        for (dt, c, size) in trades {
            let closed = b.trade(&cfg, at(t0 + dt), px(c), size);
            for (tf, bar) in &closed {
                assert_eq!(
                    b.closed(*tf, 0),
                    Some(bar),
                    "a reported close is the bar now in the series"
                );
            }
            closes.push((
                dt,
                closed.iter().any(|c| c.0 == Timeframe::M1),
                closed.iter().any(|c| c.0 == Timeframe::M5),
            ));
        }
        // The minute closes when the first trade of the next one arrives (dt 60 and dt 160).
        assert_eq!(
            closes
                .iter()
                .filter(|c| c.1)
                .map(|c| c.0)
                .collect::<Vec<_>>(),
            [60, 160]
        );
        assert!(
            closes.iter().all(|c| !c.2),
            "the 5-minute bar is still open"
        );
        let m = b.closed(Timeframe::M1, 1).unwrap();
        assert_eq!(
            *m,
            TfBar {
                start_sec: t0,
                open: px(1000),
                high: px(1020),
                low: px(990),
                close: px(990),
                volume: 400,
                trades: 3,
                notional: (1000 * 100 + 1020 * 200 + 990 * 100) as u128 * 10_000_000,
            }
        );
        assert_eq!(
            m.vwap(),
            Some(Px::from_raw(
                (1000i64 * 100 + 1020 * 200 + 990 * 100) * 10_000_000 / 400
            ))
        );
        let second = b.closed(Timeframe::M1, 0).unwrap();
        assert_eq!(
            (
                second.start_sec,
                second.open,
                second.close,
                second.volume,
                second.trades
            ),
            (t0 + 60, px(1005), px(1010), 100, 2)
        );
        let f = b.forming(Timeframe::M1).unwrap();
        assert_eq!((f.start_sec, f.open, f.volume), (t0 + 120, px(1100), 10));
        // The 5-minute and hour bars hold everything so far.
        let f5 = b.forming(Timeframe::M5).unwrap();
        assert_eq!(
            (f5.open, f5.high, f5.low, f5.close, f5.volume, f5.trades),
            (px(1000), px(1100), px(990), px(1100), 510, 6)
        );
        assert_eq!(b.closed_len(Timeframe::M5), 0);
    }

    #[test]
    fn vwap_rounds_down() {
        let mut b = SymbolBars::new();
        let cfg = MtfConfig::default();
        b.trade(&cfg, at(DAY0), px(1000), 1);
        b.trade(&cfg, at(DAY0 + 1), px(1001), 2);
        // (10.00 x 1 + 10.01 x 2) / 3 = 10.006666666...
        assert_eq!(
            b.forming(Timeframe::M1).unwrap().vwap(),
            Some(Px::from_raw(10_006_666_666))
        );
    }

    #[test]
    fn alignment_is_to_the_epoch_and_the_day_to_the_configured_open() {
        let cfg = MtfConfig::clock(14 * 3600 + 30 * 60, false);
        let mut b = SymbolBars::new();
        b.trade(&cfg, at(DAY0 + 15 * 3600 + 7 * 60 + 13), px(1000), 1);
        assert_eq!(
            b.forming(Timeframe::M1).unwrap().start_sec,
            DAY0 + 15 * 3600 + 7 * 60
        );
        assert_eq!(
            b.forming(Timeframe::M5).unwrap().start_sec,
            DAY0 + 15 * 3600 + 5 * 60
        );
        assert_eq!(
            b.forming(Timeframe::M15).unwrap().start_sec,
            DAY0 + 15 * 3600
        );
        assert_eq!(
            b.forming(Timeframe::H1).unwrap().start_sec,
            DAY0 + 15 * 3600
        );
        assert_eq!(
            b.forming(Timeframe::Day).unwrap().start_sec,
            DAY0 + 14 * 3600 + 30 * 60
        );
        // Before the open, the trade belongs to the previous day's bar.
        let mut c = SymbolBars::new();
        c.trade(&cfg, at(DAY0 + 9 * 3600), px(1000), 1);
        assert_eq!(
            c.forming(Timeframe::Day).unwrap().start_sec,
            DAY0 - 86_400 + 14 * 3600 + 30 * 60
        );
        // At the open exactly, a new one.
        c.trade(&cfg, at(DAY0 + 14 * 3600 + 30 * 60), px(1000), 1);
        assert_eq!(c.closed_total(Timeframe::Day), 1);
    }

    #[test]
    fn a_quiet_symbols_bar_closes_when_time_passes_and_only_once() {
        let cfg = MtfConfig::default();
        let mut b = SymbolBars::new();
        let t0 = DAY0 + 600;
        b.trade(&cfg, at(t0 + 5), px(1000), 10);
        assert!(b.advance(at(t0 + 59)).is_empty());
        let c = b.advance(at(t0 + 60));
        assert!(c.iter().any(|x| x.0 == Timeframe::M1) && !c.iter().any(|x| x.0 == Timeframe::M5));
        assert!(b.advance(at(t0 + 61)).is_empty(), "already closed");
        assert_eq!(b.closed_total(Timeframe::M1), 1);
        assert!(b.forming(Timeframe::M1).is_none());
        // 600 s later the 5-minute and 15-minute... the 5 minute bar [t0-0.., +300) closed at 300.
        let c = b.advance(at(t0 + 600));
        assert!(c.iter().any(|x| x.0 == Timeframe::M5));
    }

    #[test]
    fn empty_intervals_are_skipped_or_filled_flat() {
        let t0 = DAY0 + 3600;
        let run = |fill: bool| {
            let cfg = MtfConfig::clock(0, fill);
            let mut b = SymbolBars::new();
            b.trade(&cfg, at(t0 + 10), px(1000), 10);
            b.trade(&cfg, at(t0 + 4 * 60 + 20), px(1050), 20); // minutes 1, 2, 3 had no trades
            b
        };
        let skip = run(false);
        assert_eq!(skip.closed_len(Timeframe::M1), 1);
        let fill = run(true);
        assert_eq!(fill.closed_len(Timeframe::M1), 4);
        let starts: Vec<u64> = (0..4)
            .rev()
            .map(|i| fill.closed(Timeframe::M1, i).unwrap().start_sec)
            .collect();
        assert_eq!(starts, [t0, t0 + 60, t0 + 120, t0 + 180]);
        let flat = fill.closed(Timeframe::M1, 1).unwrap();
        assert_eq!(
            (
                flat.open,
                flat.high,
                flat.low,
                flat.close,
                flat.volume,
                flat.trades
            ),
            (px(1000), px(1000), px(1000), px(1000), 0, 0)
        );
        assert_eq!(flat.vwap(), None);
        // A huge gap only fills the last BAR_DEPTH intervals.
        let cfg = MtfConfig::clock(0, true);
        let mut b = SymbolBars::new();
        b.trade(&cfg, at(t0), px(1000), 1);
        b.trade(&cfg, at(t0 + 100 * 3600), px(1000), 1);
        assert_eq!(b.closed_len(Timeframe::M1), BAR_DEPTH);
        assert_eq!(
            b.closed(Timeframe::M1, 0).unwrap().start_sec,
            t0 + 100 * 3600 - 60
        );
    }

    #[test]
    fn closes_carry_their_own_bar_even_when_a_gap_is_filled() {
        let mut m = MtfBars::new(MtfConfig::clock(0, true), 1, 1);
        m.track(0).unwrap();
        let mut out = Vec::new();
        let t0 = DAY0 + 600;
        m.on_event(&trade_ev(0, at(t0 + 10), 1000, 5), &mut out);
        m.on_event(&trade_ev(0, at(t0 + 4 * 60 + 5), 1050, 7), &mut out);
        let m1: Vec<&BarClose> = out
            .iter()
            .filter(|c| c.timeframe == Timeframe::M1)
            .collect();
        // The real bar first, then three flat fillers; each close reports its own bar.
        assert_eq!(m1.len(), 4);
        assert_eq!((m1[0].bar.volume, m1[0].bar.start_sec), (5, t0));
        for (k, c) in m1[1..].iter().enumerate() {
            assert_eq!(
                (c.bar.start_sec, c.bar.volume, c.bar.close),
                (t0 + 60 * (k as u64 + 1), 0, px(1000))
            );
        }
        // The newest closed bar in the series is the last filler, not the real bar.
        assert_eq!(
            m.symbol(0).unwrap().closed(Timeframe::M1, 0),
            Some(&m1[3].bar)
        );
    }

    #[test]
    fn a_late_trade_counts_in_the_current_interval() {
        let cfg = MtfConfig::default();
        let mut b = SymbolBars::new();
        let t0 = DAY0 + 600;
        b.trade(&cfg, at(t0 + 70), px(1000), 10);
        let closed = b.trade(&cfg, at(t0 + 5), px(1100), 10); // stale
        assert!(closed.is_empty());
        let f = b.forming(Timeframe::M1).unwrap();
        assert_eq!(
            (f.start_sec, f.high, f.volume, f.trades),
            (t0 + 60, px(1100), 20, 2)
        );
    }

    #[test]
    fn only_tracked_symbols_build_bars_and_tracking_is_bounded() {
        let mut m = MtfBars::new(MtfConfig::default(), 5, 2);
        assert_eq!(m.track(9), Err(TrackError::Unknown));
        m.track(3).unwrap();
        assert_eq!(m.track(3), Err(TrackError::AlreadyTracked));
        m.track(1).unwrap();
        assert_eq!(m.track(2), Err(TrackError::Full));
        let mut out = Vec::new();
        m.on_event(&trade_ev(0, at(DAY0), 1000, 1), &mut out); // untracked
        m.on_event(&trade_ev(1, at(DAY0), 1000, 1), &mut out);
        assert!(m.symbol(0).is_none());
        assert_eq!(
            m.symbol(1).unwrap().forming(Timeframe::M1).unwrap().volume,
            1
        );
        assert!(m.untrack(1));
        assert!(!m.untrack(1));
        assert_eq!(m.tracked(), 1);
        m.track(2).unwrap();
        assert!(
            m.symbol(2).unwrap().forming(Timeframe::M1).is_none(),
            "a fresh start"
        );
    }

    #[test]
    fn closes_are_reported_in_time_order_with_quiet_symbols_first() {
        let mut m = MtfBars::new(MtfConfig::default(), 4, 4);
        for id in [2, 0, 3] {
            m.track(id).unwrap();
        }
        let mut out = Vec::new();
        let t0 = DAY0 + 600;
        for id in [0, 2, 3] {
            m.on_event(&trade_ev(id, at(t0 + 10), 1000, 1), &mut out);
        }
        assert!(out.is_empty());
        // Symbol 3 trades in the next minute: time passing closes the bars of 0, 2 and 3 (id order).
        m.on_event(&trade_ev(3, at(t0 + 65), 1000, 1), &mut out);
        let got: Vec<(u32, Timeframe)> = out.iter().map(|c| (c.instrument, c.timeframe)).collect();
        assert_eq!(
            got,
            [(0, Timeframe::M1), (2, Timeframe::M1), (3, Timeframe::M1)]
        );
        out.clear();
        m.on_event(&trade_ev(3, at(t0 + 125), 1000, 1), &mut out);
        let got: Vec<(u32, Timeframe)> = out.iter().map(|c| (c.instrument, c.timeframe)).collect();
        assert_eq!(
            got,
            [(3, Timeframe::M1)],
            "0 and 2 had no forming bar left to close"
        );
    }

    #[test]
    fn quotes_corrections_and_cancels_do_not_touch_bars() {
        use tf_core::{CancelError, CancelErrorKind, Correction};
        let mut m = MtfBars::new(MtfConfig::default(), 1, 1);
        m.track(0).unwrap();
        let mut out = Vec::new();
        m.on_event(&trade_ev(0, at(DAY0), 1000, 10), &mut out);
        let hdr = Header {
            ts_event: at(DAY0),
            ts_recv: at(DAY0 + 1),
            seq: 1,
            instrument: 0,
            provider: ProviderId::Synthetic,
        };
        m.on_event(
            &Event::Correction(Correction {
                hdr,
                orig_px: px(1000),
                orig_size: 10,
                px: px(1100),
                size: 5,
            }),
            &mut out,
        );
        m.on_event(
            &Event::CancelError(CancelError {
                hdr,
                kind: CancelErrorKind::Cancel,
                px: px(1000),
                size: 10,
            }),
            &mut out,
        );
        let f = m.symbol(0).unwrap().forming(Timeframe::M1).unwrap();
        assert_eq!((f.volume, f.high, f.trades), (10, px(1000), 1));
    }

    #[test]
    fn a_symbols_state_is_a_known_size() {
        // The budget in the module docs: about 40 KB a symbol.
        let n = std::mem::size_of::<SymbolBars>();
        assert!((30_000..60_000).contains(&n), "{n} bytes");
    }

    // ---- against a brute-force reference ----

    fn reference_start(tf: Timeframe, sec: u64, off: u64) -> u64 {
        let len = tf.secs();
        if tf == Timeframe::Day {
            (sec - off) / len * len + off
        } else {
            sec / len * len
        }
    }

    /// Group raw trades by interval directly, with no streaming state.
    fn reference(trades: &[(u64, i64, u32)], tf: Timeframe, off: u64, fill: bool) -> Vec<TfBar> {
        let mut out: Vec<TfBar> = Vec::new();
        for &(sec, cents, size) in trades {
            let start = reference_start(tf, sec, off);
            match out.last_mut() {
                Some(b) if b.start_sec == start => b.add(px(cents), size),
                _ => {
                    if fill {
                        if let Some(prev) = out.last().copied() {
                            let mut s = prev.start_sec + tf.secs();
                            while s < start {
                                out.push(TfBar::flat(s, prev.close));
                                s += tf.secs();
                            }
                        }
                    }
                    out.push(TfBar::first(start, px(cents), size));
                }
            }
        }
        out
    }

    #[test]
    fn every_timeframe_matches_a_brute_force_grouping_of_the_trades() {
        for seed in 0..40u64 {
            for fill in [false, true] {
                let mut rng = SplitMix64::new(seed);
                let off = rng.next_u64() % 86_400;
                let cfg = MtfConfig::clock(off, fill);
                let mut sec = DAY0 + 1000 + rng.next_u64() % 5000;
                let mut trades = Vec::new();
                let mut b = SymbolBars::new();
                for _ in 0..400 {
                    // Mostly dense, sometimes a gap of several intervals (kept under BAR_DEPTH).
                    sec += if rng.next_u64() % 10 == 0 {
                        rng.next_u64() % 1500
                    } else {
                        rng.next_u64() % 20
                    };
                    let (cents, size) = (
                        100 + (rng.next_u64() % 900) as i64,
                        1 + (rng.next_u64() % 300) as u32,
                    );
                    trades.push((sec, cents, size));
                    b.trade(&cfg, at(sec), px(cents), size);
                }
                b.advance(at(sec + 3 * 86_400));
                for tf in Timeframe::ALL {
                    let want = reference(&trades, tf, off, fill);
                    let keep = want.len().min(BAR_DEPTH);
                    assert_eq!(
                        b.closed_total(tf),
                        want.len() as u64,
                        "seed {seed} fill {fill} {tf:?}"
                    );
                    assert_eq!(b.closed_len(tf), keep);
                    for i in 0..keep {
                        assert_eq!(
                            b.closed(tf, i),
                            Some(&want[want.len() - 1 - i]),
                            "seed {seed} fill {fill} {tf:?} bar {i}"
                        );
                    }
                    assert!(b.forming(tf).is_none());
                }
            }
        }
    }

    #[test]
    fn larger_bars_are_the_merge_of_the_smaller_ones() {
        let cfg = MtfConfig::default();
        let mut rng = SplitMix64::new(11);
        let mut b = SymbolBars::new();
        let mut sec = DAY0;
        for _ in 0..3000 {
            sec += rng.next_u64() % 8;
            b.trade(
                &cfg,
                at(sec),
                px(100 + (rng.next_u64() % 500) as i64),
                1 + (rng.next_u64() % 50) as u32,
            );
        }
        // Every closed 5-minute bar in the ring is made of closed or forming 1-minute bars.
        let m1: Vec<TfBar> = (0..b.closed_len(Timeframe::M1))
            .rev()
            .filter_map(|i| b.closed(Timeframe::M1, i).copied())
            .collect();
        for i in 0..b.closed_len(Timeframe::M5) {
            let five = b.closed(Timeframe::M5, i).unwrap();
            let parts: Vec<&TfBar> = m1
                .iter()
                .filter(|m| m.start_sec >= five.start_sec && m.start_sec < five.start_sec + 300)
                .collect();
            if parts.len() < 5 {
                continue; // the oldest 5-minute bars reach beyond the 1-minute ring
            }
            assert_eq!(five.open, parts[0].open);
            assert_eq!(five.close, parts.last().unwrap().close);
            assert_eq!(five.high, parts.iter().map(|p| p.high).max().unwrap());
            assert_eq!(five.low, parts.iter().map(|p| p.low).min().unwrap());
            assert_eq!(five.volume, parts.iter().map(|p| p.volume).sum::<u64>());
            assert_eq!(five.trades, parts.iter().map(|p| p.trades).sum::<u32>());
            assert_eq!(
                five.notional,
                parts.iter().map(|p| p.notional).sum::<u128>()
            );
        }
    }

    // ---- reproducibility ----

    fn synth_events() -> Vec<Event> {
        let cfg = SynthConfig {
            seed: 8,
            session_start: tf_synth::DEFAULT_SESSION_START,
            duration: 1500 * S,
            symbols: vec![
                SymbolSpec {
                    symbol: "A".into(),
                    base_px_cents: 500,
                    base_interval_ns: 300_000_000,
                    quote_every: 2,
                    scenario: Scenario::runner(tf_synth::PullbackKind::Healthy, 60 * S),
                    news: Vec::new(),
                },
                SymbolSpec {
                    symbol: "B".into(),
                    base_px_cents: 1000,
                    base_interval_ns: 2_000_000_000,
                    quote_every: 2,
                    scenario: Scenario::quiet(),
                    news: Vec::new(),
                },
            ],
        };
        SynthStream::new(&cfg).collect()
    }

    fn build(events: &[Event]) -> (MtfBars, Vec<BarClose>) {
        let mut m = MtfBars::new(MtfConfig::default(), 2, 2);
        m.track(0).unwrap();
        m.track(1).unwrap();
        let mut out = Vec::new();
        for ev in events {
            m.on_event(ev, &mut out);
        }
        (m, out)
    }

    fn digest(m: &MtfBars, closes: &[BarClose]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: u128| {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        };
        for c in closes {
            mix(u128::from(c.instrument));
            mix(c.timeframe.index() as u128);
        }
        for id in 0..2 {
            let s = m.symbol(id).unwrap();
            for tf in Timeframe::ALL {
                for i in 0..s.closed_len(tf) {
                    let b = s.closed(tf, i).unwrap();
                    mix(u128::from(b.start_sec));
                    mix(b.open.raw() as u128);
                    mix(b.high.raw() as u128);
                    mix(b.low.raw() as u128);
                    mix(b.close.raw() as u128);
                    mix(u128::from(b.volume));
                    mix(u128::from(b.trades));
                    mix(b.notional);
                }
            }
        }
        h
    }

    #[test]
    fn a_stream_and_its_encoded_replay_give_identical_bars_and_a_pinned_hash() {
        let events = synth_events();
        // The "tape": every event encoded and decoded again, as a replay would.
        let replayed: Vec<Event> = events
            .iter()
            .map(|e| {
                let mut buf = Vec::new();
                e.encode(&mut buf);
                Event::decode(&buf).expect("decodes").0
            })
            .collect();
        assert_eq!(replayed, events);
        let (a, ca) = build(&events);
        let (b, cb) = build(&replayed);
        assert_eq!(ca, cb);
        assert_eq!(digest(&a, &ca), digest(&b, &cb));
        assert!(ca.len() > 30, "{} closes", ca.len());
        assert_eq!(
            digest(&a, &ca),
            0x2234_f04b_92d9_8905,
            "bar digest {:#x}",
            digest(&a, &ca)
        );
    }

    // ---- session alignment (E19-S03) ----

    use tf_calendar::{Calendar, Date};

    /// Boundaries of a trading day in seconds since the epoch: premarket, open, close, after-hours end.
    fn day(y: i32, m: u8, d: u8) -> (u64, u64, u64, u64) {
        let t = Calendar::us_equities()
            .times(Date::new(y, m, d).unwrap())
            .unwrap()
            .unwrap();
        (
            t.premarket / S,
            t.open / S,
            t.close / S,
            t.after_hours_end / S,
        )
    }

    fn session_bars() -> MtfBars {
        MtfBars::new(MtfConfig::session(), 4, 4)
    }

    fn feed(m: &mut MtfBars, inst: u32, sec: u64, cents: i64, size: u32) -> Vec<BarClose> {
        let mut out = Vec::new();
        m.on_event(&trade_ev(inst, at(sec), cents, size), &mut out);
        out
    }

    fn starts(m: &MtfBars, id: u32, tf: Timeframe) -> Vec<u64> {
        let s = m.symbol(id).unwrap();
        (0..s.closed_len(tf))
            .rev()
            .map(|i| s.closed(tf, i).unwrap().start_sec)
            .collect()
    }

    #[test]
    fn the_hand_computed_days_start_where_the_new_york_clock_says() {
        // The calendar is checked on its own (tf-calendar); this pins the UTC seconds the bars rely on.
        let days = |y, m, d| Date::new(y, m, d).unwrap().days() as u64 * 86_400;
        // Summer (UTC-4): the open is 13:30 UTC, premarket 08:00, close 20:00, after-hours end 24:00.
        let (pre, open, close, end) = day(2026, 10, 2);
        assert_eq!(
            (pre, open, close, end),
            (
                days(2026, 10, 2) + 8 * 3600,
                days(2026, 10, 2) + 13 * 3600 + 1800,
                days(2026, 10, 2) + 20 * 3600,
                days(2026, 10, 3)
            )
        );
        // Winter (UTC-5): 14:30 UTC.
        let (pre, open, close, end) = day(2026, 11, 2);
        assert_eq!(
            (pre, open, close, end),
            (
                days(2026, 11, 2) + 9 * 3600,
                days(2026, 11, 2) + 14 * 3600 + 1800,
                days(2026, 11, 2) + 21 * 3600,
                days(2026, 11, 3) + 3600
            )
        );
    }

    #[test]
    fn regular_session_hours_start_at_the_open_and_the_last_is_a_half_hour_stub() {
        for (y, m, d) in [(2026, 10, 2), (2026, 11, 2)] {
            let (_, open, close, _) = day(y, m, d);
            let mut b = session_bars();
            b.track(0).unwrap();
            // A trade in every hour of the session, the first at the open and the last in the stub.
            for k in 0..7 {
                feed(&mut b, 0, open + k * 3600 + 5, 1000 + k as i64, 10);
            }
            // Nothing has closed the stub yet; the forming hour is the one starting at 15:30 New York.
            assert_eq!(
                b.symbol(0)
                    .unwrap()
                    .forming(Timeframe::H1)
                    .unwrap()
                    .start_sec,
                close - 1800
            );
            let want: Vec<u64> = (0..6).map(|k| open + k * 3600).collect();
            assert_eq!(starts(&b, 0, Timeframe::H1), want, "{y}-{m}-{d}");
            // Time reaching the close closes the stub, at 16:00 and not at 16:30.
            let mut out = Vec::new();
            b.advance_to(at(close - 1), &mut out);
            assert!(out.iter().all(|c| c.timeframe != Timeframe::H1));
            b.advance_to(at(close), &mut out);
            let stub: Vec<_> = out
                .iter()
                .filter(|c| c.timeframe == Timeframe::H1)
                .collect();
            assert_eq!(stub.len(), 1);
            assert_eq!(stub[0].bar.start_sec, close - 1800);
            assert_eq!(b.symbol(0).unwrap().closed_len(Timeframe::H1), 7);
        }
    }

    #[test]
    fn the_first_bar_is_at_half_past_nine_on_both_sides_of_each_daylight_saving_change() {
        // Fridays and Mondays around 8 March 2026 (spring) and 1 November 2026 (autumn), with the open's
        // UTC time written out by hand: 14:30 in winter, 13:30 in summer.
        let days = |y, m, d| Date::new(y, m, d).unwrap().days() as u64 * 86_400;
        let cases = [
            (2026, 3, 6, 14 * 3600 + 1800),
            (2026, 3, 9, 13 * 3600 + 1800),
            (2026, 10, 30, 13 * 3600 + 1800),
            (2026, 11, 2, 14 * 3600 + 1800),
        ];
        for (y, m, d, off) in cases {
            let want = days(y, m, d) + off;
            let mut b = session_bars();
            b.track(0).unwrap();
            feed(&mut b, 0, want, 1000, 1);
            let s = b.symbol(0).unwrap();
            assert_eq!(
                s.forming(Timeframe::H1).unwrap().start_sec,
                want,
                "{y}-{m}-{d}"
            );
            assert_eq!(s.forming(Timeframe::M15).unwrap().start_sec, want);
            // One second earlier is the premarket's last hour, which ends at the open.
            let mut b = session_bars();
            b.track(0).unwrap();
            feed(&mut b, 0, want - 1, 1000, 1);
            assert_eq!(
                b.symbol(0)
                    .unwrap()
                    .forming(Timeframe::H1)
                    .unwrap()
                    .start_sec,
                want - 1800,
                "premarket 09:00 to 09:30"
            );
        }
    }

    #[test]
    fn an_early_close_ends_the_stub_at_one_and_after_hours_at_five() {
        // Friday 27 November 2026: closes at 13:00 (18:00 UTC), after-hours to 17:00 (22:00 UTC).
        let (_, open, close, end) = day(2026, 11, 27);
        assert_eq!((close - open, end - close), (3 * 3600 + 1800, 4 * 3600));
        let mut b = session_bars();
        b.track(0).unwrap();
        for k in 0..4 {
            feed(&mut b, 0, open + k * 3600, 1000, 1);
        }
        assert_eq!(
            starts(&b, 0, Timeframe::H1),
            [open, open + 3600, open + 7200]
        );
        let stub = b
            .symbol(0)
            .unwrap()
            .forming(Timeframe::H1)
            .unwrap()
            .start_sec;
        assert_eq!(stub, close - 1800, "12:30 to 13:00");
        // An after-hours trade at 13:00 starts the after-hours hour; the stub is closed by it.
        let out = feed(&mut b, 0, close, 1001, 1);
        assert!(
            out.iter()
                .any(|c| c.timeframe == Timeframe::H1 && c.bar.start_sec == stub)
        );
        assert_eq!(
            b.symbol(0)
                .unwrap()
                .forming(Timeframe::H1)
                .unwrap()
                .start_sec,
            close
        );
        // The day bar ends at 17:00.
        let mut out = Vec::new();
        b.advance_to(at(end - 1), &mut out);
        assert!(out.iter().all(|c| c.timeframe != Timeframe::Day));
        b.advance_to(at(end), &mut out);
        assert!(out.iter().any(|c| c.timeframe == Timeframe::Day));
    }

    #[test]
    fn premarket_and_after_hours_hours_start_at_their_own_session_start() {
        let (pre, open, close, end) = day(2026, 10, 2);
        let mut b = session_bars();
        b.track(0).unwrap();
        for sec in [pre, pre + 3600, open - 1, close, close + 3600, end - 1] {
            feed(&mut b, 0, sec, 1000, 1);
        }
        let h = starts(&b, 0, Timeframe::H1);
        // Closed: 04:00, 05:00, the 09:00 stub to 09:30 (hour 5 of the premarket), then 16:00 and 17:00.
        assert_eq!(h, [pre, pre + 3600, pre + 5 * 3600, close, close + 3600]);
        assert_eq!(
            b.symbol(0)
                .unwrap()
                .forming(Timeframe::H1)
                .unwrap()
                .start_sec,
            close + 3 * 3600,
            "19:00 to 20:00"
        );
    }

    #[test]
    fn the_day_bar_is_the_whole_trading_day_whatever_the_offset_from_utc() {
        for (y, m, d) in [(2026, 10, 2), (2026, 11, 2)] {
            let (pre, open, close, end) = day(y, m, d);
            let mut b = session_bars();
            b.track(0).unwrap();
            for sec in [pre, open, close, end - 1] {
                feed(&mut b, 0, sec, 1000, 1);
            }
            let f = b.symbol(0).unwrap().forming(Timeframe::Day).unwrap();
            assert_eq!((f.start_sec, f.trades), (pre, 4), "{y}-{m}-{d}");
            // The next trading day's premarket opens a new day bar and closes this one.
            let (npre, ..) = day(y, m, if d == 2 { 5 } else { 3 });
            let out = feed(&mut b, 0, npre, 1000, 1);
            let c: Vec<_> = out
                .iter()
                .filter(|c| c.timeframe == Timeframe::Day)
                .collect();
            assert_eq!(c.len(), 1);
            assert_eq!((c[0].bar.start_sec, c[0].bar.trades), (pre, 4));
        }
    }

    #[test]
    fn a_trade_that_belongs_to_no_session_is_counted_and_builds_nothing() {
        let (pre, _, _, end) = day(2026, 10, 2);
        let mut b = session_bars();
        b.track(0).unwrap();
        let weekend = Date::new(2026, 10, 3).unwrap().days() as u64 * 86_400 + 15 * 3600;
        let far = Date::new(2031, 1, 6).unwrap().days() as u64 * 86_400 + 15 * 3600;
        // In time order: before the premarket, after after-hours, overnight, a Saturday, a year outside the
        // calendar's table.
        for sec in [pre - 1, end, end + 3 * 3600, weekend, far] {
            assert!(feed(&mut b, 0, sec, 1000, 1).is_empty());
        }
        assert_eq!(b.unplaced(), 5);
        assert_eq!(b.symbol(0).unwrap().closed_total(Timeframe::M1), 0);
        assert!(b.symbol(0).unwrap().forming(Timeframe::M1).is_none());

        // The boundary seconds themselves are placed: the start of a session belongs to it, its end to
        // the next, and after-hours' end to nothing.
        let mut b = session_bars();
        b.track(0).unwrap();
        feed(&mut b, 0, pre - 1, 1000, 1);
        assert_eq!(b.unplaced(), 1);
        feed(&mut b, 0, pre, 1000, 1);
        feed(&mut b, 0, end - 1, 1000, 1);
        assert_eq!(b.unplaced(), 1);
        assert!(b.symbol(0).unwrap().forming(Timeframe::M1).is_some());
        // A trade outside every session builds nothing but lets time close what is forming.
        let out = feed(&mut b, 0, end, 1000, 1);
        assert_eq!(b.unplaced(), 2);
        for tf in [Timeframe::M1, Timeframe::H1, Timeframe::Day] {
            assert!(
                out.iter().any(|c| c.timeframe == tf),
                "{tf:?} closes at the end"
            );
        }
        assert!(b.symbol(0).unwrap().forming(Timeframe::Day).is_none());
    }

    #[test]
    fn minutes_stay_on_the_epoch_grid_in_session_alignment_and_gap_filling_is_off() {
        let (_, open, ..) = day(2026, 10, 2);
        let mut cfg = MtfConfig::session();
        cfg.fill_gaps = true;
        let mut b = MtfBars::new(cfg, 1, 1);
        b.track(0).unwrap();
        feed(&mut b, 0, open + 5, 1000, 1);
        feed(&mut b, 0, open + 4 * 60 + 5, 1000, 1);
        let s = b.symbol(0).unwrap();
        assert_eq!(s.forming(Timeframe::M5).unwrap().start_sec, open);
        assert_eq!(s.forming(Timeframe::M15).unwrap().start_sec, open);
        assert_eq!(
            s.closed_len(Timeframe::M1),
            1,
            "minutes 1 to 3 are not filled"
        );
    }

    #[test]
    fn time_is_offered_to_symbols_only_when_a_bar_is_due() {
        // A quiet symbol's minute bar closes at the first event at or after its end, exactly once, even
        // though thousands of trades by another symbol arrive in between.
        let mut b = MtfBars::new(MtfConfig::default(), 2, 2);
        b.track(0).unwrap();
        b.track(1).unwrap();
        let t0 = DAY0 + 3600;
        feed(&mut b, 0, t0 + 5, 1000, 1);
        let mut closed_at = None;
        for k in 0..200u64 {
            let sec = t0 + 6 + k;
            let out = feed(&mut b, 1, sec, 2000, 1);
            if out
                .iter()
                .any(|c| c.instrument == 0 && c.timeframe == Timeframe::M1)
            {
                assert!(closed_at.is_none(), "closed twice");
                closed_at = Some(sec);
            }
        }
        assert_eq!(closed_at, Some(t0 + 60));
    }

    #[test]
    fn a_stale_timestamp_counts_in_the_latest_second_any_event_has_shown() {
        let mut b = MtfBars::new(MtfConfig::default(), 2, 2);
        b.track(1).unwrap();
        let t0 = DAY0 + 3600;
        feed(&mut b, 1, t0 + 130, 1000, 1);
        // A symbol tracked after that, whose first trade arrives stamped before it: it is placed in the
        // minute the engine has reached, not in the one it names.
        b.track(0).unwrap();
        feed(&mut b, 0, t0 + 20, 1000, 1);
        assert_eq!(
            b.symbol(0)
                .unwrap()
                .forming(Timeframe::M1)
                .unwrap()
                .start_sec,
            t0 + 120
        );
    }

    #[test]
    fn a_cached_day_does_not_answer_for_a_second_before_it_starts() {
        let (pre, open, _, end) = day(2026, 10, 2);
        let mut p = Placer::new(MtfConfig::session());
        assert!(
            p.span(Timeframe::M1, open).is_some(),
            "the day is now cached"
        );
        // Asked out of order, a second before the premarket belongs to no bar, though the cache is warm.
        assert_eq!(p.span(Timeframe::H1, pre - 1), None);
        assert_eq!(p.span(Timeframe::H1, pre), Some((pre, pre + 3600)));
        assert_eq!(p.span(Timeframe::H1, end), None);
        assert_eq!(p.span(Timeframe::Day, end - 1), Some((pre, end)));
    }

    #[test]
    fn a_trade_in_no_session_closes_the_bars_whose_time_has_come() {
        let (_, _, _, end) = day(2026, 10, 2);
        let mut p = Placer::new(MtfConfig::session());
        let mut b = SymbolBars::new();
        let mut out = Vec::new();
        assert!(b.on_trade(&mut p, at(end - 1), px(1000), 1, &mut out));
        out.clear();
        // After-hours ended at `end`: a trade a second later is placed nowhere, and the minute, hour and
        // day bars that ended with the session close with it, exactly when time reaches `end`.
        assert!(!b.on_trade(&mut p, at(end), px(1000), 1, &mut out));
        let tfs: Vec<Timeframe> = out.iter().map(|c| c.0).collect();
        assert!(
            tfs.contains(&Timeframe::M1)
                && tfs.contains(&Timeframe::H1)
                && tfs.contains(&Timeframe::Day),
            "{tfs:?}"
        );
    }
}
