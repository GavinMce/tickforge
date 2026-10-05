//! Multi-timeframe bars: 1m, 5m, 15m, 1h and day, built live from trades.
//!
//! Tier 0 keeps 60 one-second bars, which cannot support an EMA of minute closes
//! or an opening range. [`MtfBars`] builds longer bars for a bounded set of tracked
//! symbols. Per symbol and timeframe it keeps the last [`BAR_DEPTH`] closed bars
//! and the bar still forming.
//!
//! Rules, all deliberate and tested:
//! - **Alignment.** Bars of 1m to 1h start on multiples of their length since the
//!   Unix epoch. Day bars start at a configured offset into the UTC day
//!   ([`MtfConfig::day_open_offset_secs`], for example the 09:30 New York open as
//!   14:30 or 13:30 UTC). There is no time-zone database: the offset is fixed, so
//!   across a daylight-saving change the caller must change it.
//! - **Time.** Trades are placed by `ts_recv`, which is non-decreasing. A stale
//!   timestamp counts in the latest second seen.
//! - **Closing.** A bar closes when a trade arrives in a later interval, or when
//!   [`MtfBars::advance_to`] is told time has passed its end, so a quiet symbol's
//!   bar still closes on time. Every close is reported, in order.
//! - **Empty intervals.** By default there is no bar for an interval with no trades.
//!   With [`MtfConfig::fill_gaps`], flat bars (open = high = low = close = the previous
//!   close, no volume) fill the interval, up to [`BAR_DEPTH`] of them.
//! - **Corrections and cancels** are ignored. A closed bar is never rewound; the
//!   storage side can apply corrections after the fact.
//! - Volume and prices are exact integers; VWAP is rounded down.
//!
//! Integer arithmetic only, no clock, no allocation per event (each symbol's state
//! is `Copy` and boxed once when tracking starts). A symbol is about 40 KB, so the
//! tracked set is bounded: 1,000 symbols is about 40 MB.

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

    /// The start (in seconds) of the bar containing second `sec`.
    fn start_of(self, sec: u64, cfg: &MtfConfig) -> u64 {
        let len = self.secs();
        match self {
            Timeframe::Day => {
                let off = cfg.day_open_offset_secs % len;
                // Before the first offset of the epoch day, fall back to the epoch day.
                if sec < off {
                    return 0;
                }
                (sec - off) / len * len + off
            }
            _ => sec / len * len,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MtfConfig {
    /// Seconds after 00:00 UTC at which a day bar starts.
    pub day_open_offset_secs: u64,
    /// Fill empty intervals with flat bars.
    pub fill_gaps: bool,
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
    /// `out` with its timeframe.
    pub fn on_trade(
        &mut self,
        cfg: &MtfConfig,
        ts: Nanos,
        px: Px,
        size: u32,
        out: &mut Vec<(Timeframe, TfBar)>,
    ) {
        let sec = (ts / NANOS_PER_SEC).max(self.now_sec);
        self.now_sec = sec;
        for tf in Timeframe::ALL {
            let start = tf.start_of(sec, cfg);
            let s = &mut self.series[tf.index()];
            match s.forming {
                Some(ref mut f) if f.start_sec == start => f.add(px, size),
                Some(f) => {
                    s.push(f);
                    out.push((tf, f));
                    s.forming = None;
                    Self::open_new(s, tf, cfg, start, px, size, out);
                }
                None => Self::open_new(s, tf, cfg, start, px, size, out),
            }
        }
    }

    fn open_new(
        s: &mut Series,
        tf: Timeframe,
        cfg: &MtfConfig,
        start: u64,
        px: Px,
        size: u32,
        out: &mut Vec<(Timeframe, TfBar)>,
    ) {
        if cfg.fill_gaps {
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
    }

    /// Time has reached `ts`: close any forming bar whose interval has ended.
    pub fn advance_to(&mut self, ts: Nanos, out: &mut Vec<(Timeframe, TfBar)>) {
        let sec = (ts / NANOS_PER_SEC).max(self.now_sec);
        self.now_sec = sec;
        for tf in Timeframe::ALL {
            let s = &mut self.series[tf.index()];
            if let Some(f) = s.forming {
                if sec >= f.start_sec + tf.secs() {
                    s.push(f);
                    s.forming = None;
                    out.push((tf, f));
                }
            }
        }
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
    cfg: MtfConfig,
    slots: Vec<Option<Box<SymbolBars>>>,
    /// Tracked ids in ascending order, so closes are reported deterministically.
    tracked: Vec<InstrumentId>,
    max: usize,
    /// Reused between calls so closing bars does not allocate.
    scratch: Vec<(Timeframe, TfBar)>,
}

impl MtfBars {
    pub fn new(cfg: MtfConfig, id_space: usize, max_tracked: usize) -> MtfBars {
        MtfBars {
            cfg,
            slots: (0..id_space).map(|_| None).collect(),
            tracked: Vec::new(),
            max: max_tracked,
            scratch: Vec::new(),
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

    pub fn config(&self) -> &MtfConfig {
        &self.cfg
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
            s.on_trade(&self.cfg, ts, t.px, t.size, &mut self.scratch);
            out.extend(self.scratch.iter().map(|&(timeframe, bar)| BarClose {
                instrument: id,
                timeframe,
                bar,
            }));
        }
    }

    /// Time has reached `ts`: close what has ended, for every tracked symbol.
    pub fn advance_to(&mut self, ts: Nanos, out: &mut Vec<BarClose>) {
        for &id in &self.tracked {
            if let Some(Some(s)) = self.slots.get_mut(id as usize) {
                self.scratch.clear();
                s.advance_to(ts, &mut self.scratch);
                out.extend(self.scratch.iter().map(|&(timeframe, bar)| BarClose {
                    instrument: id,
                    timeframe,
                    bar,
                }));
            }
        }
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
            self.on_trade(cfg, ts, px, size, &mut out);
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
        let cfg = MtfConfig {
            day_open_offset_secs: 14 * 3600 + 30 * 60,
            fill_gaps: false,
        };
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
            let cfg = MtfConfig {
                day_open_offset_secs: 0,
                fill_gaps: fill,
            };
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
        let cfg = MtfConfig {
            day_open_offset_secs: 0,
            fill_gaps: true,
        };
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
        let mut m = MtfBars::new(
            MtfConfig {
                day_open_offset_secs: 0,
                fill_gaps: true,
            },
            1,
            1,
        );
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
                let cfg = MtfConfig {
                    day_open_offset_secs: off,
                    fill_gaps: fill,
                };
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
}
