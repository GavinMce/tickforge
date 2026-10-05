//! One-second bars and rolling windows over them.
//!
//! [`RollingBars`] keeps the last 60 seconds of trading as 60 one-second bars in
//! a ring. Every rolling statistic (volume, trade count, high, low, price change
//! over the last 1, 5 or 60 seconds, or any length up to 60) is read from those
//! bars, so there is one structure per symbol and the answers are exact.
//!
//! # Semantics
//!
//! - Time is whole seconds: the bar for second `s` holds trades with
//!   `s * 1e9 <= ts < (s + 1) * 1e9`. "Now" is the latest second the structure
//!   has been told about, by a trade or by [`RollingBars::advance_to`]. The
//!   window of `k` seconds is the `k` seconds ending at "now", inclusive.
//! - Timestamps must be non-decreasing; use `ts_recv`, which is. A stale one is
//!   treated as falling in the current second rather than rewriting the past.
//! - Seconds with no trades still have a (empty) bar once tracking has started,
//!   so a quiet stretch reads as zero volume and no price change, not as missing.
//! - **Price change** over a window is the latest price minus the last trade
//!   price *before* the window began; if there was none (the window reaches back
//!   before the first trade), the first trade price inside the window. `None`
//!   until there has been a trade.
//! - A gap longer than 60 seconds is handled by resetting the ring in at most 60
//!   steps, whatever its length.

use tf_core::{NANOS_PER_SEC, Nanos, Px};

/// Seconds of history kept, and the longest window.
pub const WINDOW_SECS: usize = 60;
const NO_SEC: u64 = u64::MAX;

/// One second of trading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    /// The second this bar covers, in whole seconds since the epoch.
    pub sec: u64,
    /// Prices are meaningful only when `trades > 0`.
    pub open: Px,
    pub high: Px,
    pub low: Px,
    pub close: Px,
    pub volume: u64,
    pub trades: u32,
    /// The last trade price before this second began, if any.
    carry: Option<Px>,
}

impl Bar {
    const fn empty(sec: u64, carry: Option<Px>) -> Bar {
        Bar {
            sec,
            open: Px::ZERO,
            high: Px::ZERO,
            low: Px::ZERO,
            close: Px::ZERO,
            volume: 0,
            trades: 0,
            carry,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.trades == 0
    }
}

/// The last 60 one-second bars and the statistics over them. About 4 KB.
#[derive(Clone, Copy, Debug)]
pub struct RollingBars {
    bars: [Bar; WINDOW_SECS],
    /// The current second; `NO_SEC` until the first call.
    cur: u64,
    last_px: Option<Px>,
}

impl Default for RollingBars {
    fn default() -> Self {
        Self::new()
    }
}

impl RollingBars {
    pub const fn new() -> Self {
        RollingBars {
            bars: [Bar::empty(NO_SEC, None); WINDOW_SECS],
            cur: NO_SEC,
            last_px: None,
        }
    }

    /// Record a trade of `size` shares at `px` at time `ts`.
    pub fn on_trade(&mut self, ts: Nanos, px: Px, size: u32) {
        self.roll_to(ts / NANOS_PER_SEC);
        let bar = &mut self.bars[(self.cur % WINDOW_SECS as u64) as usize];
        if bar.trades == 0 {
            bar.open = px;
            bar.high = px;
            bar.low = px;
        } else {
            bar.high = bar.high.max(px);
            bar.low = bar.low.min(px);
        }
        bar.close = px;
        bar.volume += u64::from(size);
        bar.trades = bar.trades.saturating_add(1);
        self.last_px = Some(px);
    }

    /// Let time pass with no trade, so windows reflect a quiet stretch.
    pub fn advance_to(&mut self, ts: Nanos) {
        self.roll_to(ts / NANOS_PER_SEC);
    }

    fn roll_to(&mut self, sec: u64) {
        if self.cur == NO_SEC {
            self.cur = sec;
            self.bars[(sec % WINDOW_SECS as u64) as usize] = Bar::empty(sec, None);
            return;
        }
        if sec <= self.cur {
            return; // same second, or a stale timestamp: the current second
        }
        // Only the last 60 seconds can matter, however long the gap was.
        let from = (self.cur + 1).max(sec.saturating_sub(WINDOW_SECS as u64 - 1));
        for s in from..=sec {
            self.bars[(s % WINDOW_SECS as u64) as usize] = Bar::empty(s, self.last_px);
        }
        self.cur = sec;
    }

    /// The current second, or `None` before the first trade or `advance_to`.
    pub fn now_sec(&self) -> Option<u64> {
        (self.cur != NO_SEC).then_some(self.cur)
    }

    pub fn last_price(&self) -> Option<Px> {
        self.last_px
    }

    fn slot(&self, sec: u64) -> Option<&Bar> {
        let b = &self.bars[(sec % WINDOW_SECS as u64) as usize];
        (b.sec == sec).then_some(b)
    }

    /// The bars of the last `secs` seconds (at most 60), oldest first. Seconds
    /// from before tracking began are skipped.
    fn window(&self, secs: usize) -> impl Iterator<Item = &Bar> {
        let k = secs.min(WINDOW_SECS) as u64;
        let (first, last) = match (self.cur, k) {
            (NO_SEC, _) | (_, 0) => (1, 0), // empty range
            (cur, k) => (cur.saturating_sub(k - 1), cur),
        };
        (first..=last).filter_map(|s| self.slot(s))
    }

    /// The bar `n` seconds ago (0 is the current second), if tracked and
    /// within the last 60.
    pub fn bar_ago(&self, n: usize) -> Option<&Bar> {
        if n >= WINDOW_SECS || self.cur == NO_SEC {
            return None;
        }
        self.slot(self.cur.checked_sub(n as u64)?)
    }

    /// Shares traded in the `secs` seconds ending at second `now_sec`, which may be later
    /// than this symbol's last trade: a symbol that has stopped trading shows the quiet it
    /// has had, not the busy second it last saw.
    pub fn volume_asof(&self, now_sec: u64, secs: usize) -> u64 {
        let k = secs.min(WINDOW_SECS) as u64;
        (now_sec.saturating_add(1).saturating_sub(k)..=now_sec)
            .filter_map(|s| self.slot(s))
            .map(|b| b.volume)
            .sum()
    }

    /// Shares traded in the last `secs` seconds.
    pub fn volume(&self, secs: usize) -> u64 {
        self.window(secs).map(|b| b.volume).sum()
    }

    /// Trades in the last `secs` seconds.
    pub fn trades(&self, secs: usize) -> u64 {
        self.window(secs).map(|b| u64::from(b.trades)).sum()
    }

    pub fn high(&self, secs: usize) -> Option<Px> {
        self.window(secs)
            .filter(|b| !b.is_empty())
            .map(|b| b.high)
            .max()
    }

    pub fn low(&self, secs: usize) -> Option<Px> {
        self.window(secs)
            .filter(|b| !b.is_empty())
            .map(|b| b.low)
            .min()
    }

    /// The price the window started from: the last trade price before it, or
    /// the first trade inside it.
    fn reference(&self, secs: usize) -> Option<Px> {
        let mut bars = self.window(secs);
        let earliest = bars.next()?;
        if let Some(c) = earliest.carry {
            return Some(c);
        }
        std::iter::once(earliest)
            .chain(bars)
            .find(|b| !b.is_empty())
            .map(|b| b.open)
    }

    /// Latest price minus the window's starting price, in raw `Px` units.
    pub fn price_change(&self, secs: usize) -> Option<i64> {
        let last = self.last_px?;
        Some(last.raw() - self.reference(secs)?.raw())
    }

    /// Price change as permille of the starting price (rounded toward zero).
    pub fn price_change_permille(&self, secs: usize) -> Option<i64> {
        let last = self.last_px?;
        let start = self.reference(secs)?.raw();
        if start == 0 {
            return None;
        }
        let permille = i128::from(last.raw() - start) * 1000 / i128::from(start);
        i64::try_from(permille).ok()
    }
}

// Allocation-free by construction: a `Copy` type owns no heap data.
const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<RollingBars>();
    is_copy::<Bar>();
};
