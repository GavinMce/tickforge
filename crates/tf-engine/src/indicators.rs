//! Indicators: EMA, SMA, VWAP (anchored, with bands, and rolling), RSI, ATR, rolling
//! extremes, rate of change and the opening range.
//!
//! The contract every indicator here keeps, so strategies can compose them:
//! - **Integers only.** Prices are raw `Px` units (1e-9 dollars); results that are
//!   ratios are in permille. No floats, so results are identical on every platform.
//! - **`Copy`, fixed storage.** Windows are const-generic arrays; nothing allocates,
//!   and the compiler enforces it (a `Copy` type owns no heap data).
//! - **Warm-up is explicit.** `value()` is `None` until the indicator has what it
//!   needs, and `is_ready()` says when its definition is fully met.
//! - **Time comes from the caller.** Indicators are fed samples in order. Only
//!   [`OpeningRange`] and [`Vwap::anchor`] take a time, and they take it as an argument.
//! - **Inputs are clamped** to +/- 2^40 raw units (about $1,100), like [`crate::Ewma`],
//!   which keeps every intermediate within `i128`; extreme values saturate, never
//!   panic.
//! - **Rounding** is stated per indicator. Recursive indicators keep a 16-bit
//!   fraction, so their error against exact arithmetic stays under one raw unit.
//!
//! Definitions follow the textbook ones (EMA `alpha = 2 / (n + 1)`, Wilder smoothing
//! `avg = (avg * (n - 1) + x) / n`), checked in the tests against an independent
//! exact-fraction computation in Python.

use crate::TfBar;
use tf_core::{NANOS_PER_SEC, Nanos};

const SHIFT: u32 = 16;
const LIMIT: i64 = 1 << 40;

fn clamp(x: i64) -> i64 {
    x.clamp(-LIMIT, LIMIT)
}

/// Divide, rounding half away from zero. `d` must be positive.
fn div_round(n: i128, d: i128) -> i128 {
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

fn unscale(v: i128) -> i64 {
    div_round(v, 1 << SHIFT) as i64
}

/// Simple moving average of the last `N` samples, rounded half away from zero.
#[derive(Clone, Copy, Debug)]
pub struct Sma<const N: usize> {
    ring: [i64; N],
    n: u64,
    sum: i128,
}

impl<const N: usize> Default for Sma<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Sma<N> {
    pub const fn new() -> Self {
        assert!(N > 0, "a window needs at least one sample");
        Sma {
            ring: [0; N],
            n: 0,
            sum: 0,
        }
    }

    pub fn update(&mut self, x: i64) {
        let x = clamp(x);
        let i = (self.n % N as u64) as usize;
        if self.n >= N as u64 {
            self.sum -= i128::from(self.ring[i]);
        }
        self.ring[i] = x;
        self.sum += i128::from(x);
        self.n += 1;
    }

    pub fn is_ready(&self) -> bool {
        self.n >= N as u64
    }

    /// The mean of the last `N` samples; `None` until `N` have arrived.
    pub fn value(&self) -> Option<i64> {
        self.is_ready()
            .then(|| div_round(self.sum, N as i128) as i64)
    }
}

/// How an [`Ema`] starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seed {
    /// The first `period` samples' simple average is the first value (the textbook
    /// definition). `value()` is `None` until then.
    Sma,
    /// The first sample is the first value. `value()` is available at once.
    FirstValue,
}

/// Exponential moving average with `alpha = 2 / (period + 1)`.
#[derive(Clone, Copy, Debug)]
pub struct Ema {
    period: u32,
    seed: Seed,
    scaled: i128,
    acc: i128,
    count: u32,
}

impl Ema {
    /// `period` is clamped to at least 1.
    pub const fn new(period: u32, seed: Seed) -> Ema {
        Ema {
            period: if period == 0 { 1 } else { period },
            seed,
            scaled: 0,
            acc: 0,
            count: 0,
        }
    }

    pub fn update(&mut self, x: i64) {
        let x = i128::from(clamp(x)) << SHIFT;
        let p = i128::from(self.period);
        match self.seed {
            Seed::FirstValue => {
                if self.count == 0 {
                    self.scaled = x;
                } else {
                    self.scaled += (x - self.scaled) * 2 / (p + 1);
                }
            }
            Seed::Sma if self.count < self.period => {
                self.acc += x;
                if self.count + 1 == self.period {
                    self.scaled = self.acc / p;
                }
            }
            Seed::Sma => self.scaled += (x - self.scaled) * 2 / (p + 1),
        }
        self.count = self.count.saturating_add(1);
    }

    /// `period` samples have been seen.
    pub fn is_ready(&self) -> bool {
        self.count >= self.period
    }

    /// The average, rounded to the nearest raw unit.
    pub fn value(&self) -> Option<i64> {
        let available = match self.seed {
            Seed::FirstValue => self.count > 0,
            Seed::Sma => self.is_ready(),
        };
        available.then(|| unscale(self.scaled))
    }
}

/// Highest and lowest of the last `N` samples. Updates are O(1); a query scans the
/// window, so keep `N` modest (hundreds, not millions).
#[derive(Clone, Copy, Debug)]
pub struct Extremes<const N: usize> {
    ring: [i64; N],
    n: u64,
}

impl<const N: usize> Default for Extremes<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Extremes<N> {
    pub const fn new() -> Self {
        assert!(N > 0, "a window needs at least one sample");
        Extremes { ring: [0; N], n: 0 }
    }

    pub fn update(&mut self, x: i64) {
        self.ring[(self.n % N as u64) as usize] = clamp(x);
        self.n += 1;
    }

    pub fn is_ready(&self) -> bool {
        self.n >= N as u64
    }

    fn live(&self) -> &[i64] {
        &self.ring[..(self.n.min(N as u64)) as usize]
    }

    /// The highest of the samples seen, up to the last `N`; `None` before any.
    pub fn max(&self) -> Option<i64> {
        self.live().iter().copied().max()
    }

    pub fn min(&self) -> Option<i64> {
        self.live().iter().copied().min()
    }
}

/// Change over the last `N` samples, in permille of the sample `N` back, rounded
/// toward zero. Needs `N + 1` samples.
#[derive(Clone, Copy, Debug)]
pub struct RateOfChange<const N: usize> {
    ring: [i64; N],
    n: u64,
    last: Option<i64>,
}

impl<const N: usize> Default for RateOfChange<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RateOfChange<N> {
    pub const fn new() -> Self {
        assert!(N > 0, "a window needs at least one sample");
        RateOfChange {
            ring: [0; N],
            n: 0,
            last: None,
        }
    }

    pub fn update(&mut self, x: i64) {
        let x = clamp(x);
        let i = (self.n % N as u64) as usize;
        self.last = if self.n >= N as u64 && self.ring[i] != 0 {
            let old = i128::from(self.ring[i]);
            Some((i128::from(x - self.ring[i]) * 1000 / old) as i64)
        } else {
            None
        };
        self.ring[i] = x;
        self.n += 1;
    }

    pub fn is_ready(&self) -> bool {
        self.n > N as u64
    }

    /// `None` until `N + 1` samples, or if the earlier sample was zero.
    pub fn value(&self) -> Option<i64> {
        self.last
    }
}

/// Volume-weighted average price from an anchor, with standard-deviation bands.
///
/// `value()` is exact: total price x size over total size, rounded down. The
/// deviation is the volume-weighted standard deviation of trade prices around the
/// running VWAP (West's weighted update), kept in fixed point.
#[derive(Clone, Copy, Debug, Default)]
pub struct Vwap {
    volume: u128,
    notional: u128,
    mean16: i128,
    m2_16: i128,
}

impl Vwap {
    pub const fn new() -> Vwap {
        Vwap {
            volume: 0,
            notional: 0,
            mean16: 0,
            m2_16: 0,
        }
    }

    /// Start again from here (a new session, or an anchor such as a catalyst time).
    pub fn anchor(&mut self) {
        *self = Vwap::new();
    }

    /// A trade. A zero size is ignored.
    pub fn update(&mut self, px: i64, size: u32) {
        if size == 0 {
            return;
        }
        let px = clamp(px).max(0);
        let w = i128::from(size);
        self.notional += u128::try_from(px).unwrap_or(0) * u128::from(size);
        self.volume += u128::from(size);
        let x16 = i128::from(px) << SHIFT;
        let delta = x16 - self.mean16;
        self.mean16 += delta * w / self.volume as i128;
        let dx = x16 - self.mean16;
        let term = (delta.saturating_mul(dx) >> SHIFT).saturating_mul(w);
        self.m2_16 = self.m2_16.saturating_add(term);
    }

    pub fn volume(&self) -> u128 {
        self.volume
    }

    /// The VWAP, rounded down; `None` before any volume.
    pub fn value(&self) -> Option<i64> {
        (self.volume > 0)
            .then(|| i64::try_from(self.notional / self.volume).ok())
            .flatten()
    }

    /// The volume-weighted standard deviation of price, rounded to the nearest raw unit.
    pub fn std_dev(&self) -> Option<u64> {
        if self.volume == 0 {
            return None;
        }
        let var16 = (self.m2_16.max(0) as u128) / self.volume;
        // sqrt(var * 2^16) = std * 2^8
        Some((((var16.isqrt()) + (1 << 7)) >> (SHIFT / 2)) as u64)
    }

    /// `(low, high)` bands `k_permille` / 1000 standard deviations from the VWAP.
    pub fn bands(&self, k_permille: u32) -> Option<(i64, i64)> {
        let (v, s) = (self.value()?, self.std_dev()?);
        let off = (u128::from(s) * u128::from(k_permille) / 1000).min(i64::MAX as u128) as i64;
        Some((v.saturating_sub(off), v.saturating_add(off)))
    }
}

/// VWAP over the last `N` updates (trades or bars).
#[derive(Clone, Copy, Debug)]
pub struct RollingVwap<const N: usize> {
    ring: [(u128, u64); N],
    n: u64,
    notional: u128,
    volume: u128,
}

impl<const N: usize> Default for RollingVwap<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RollingVwap<N> {
    pub const fn new() -> Self {
        assert!(N > 0, "a window needs at least one sample");
        RollingVwap {
            ring: [(0, 0); N],
            n: 0,
            notional: 0,
            volume: 0,
        }
    }

    /// An update with its total price x size and size.
    pub fn update(&mut self, notional: u128, volume: u64) {
        let i = (self.n % N as u64) as usize;
        if self.n >= N as u64 {
            self.notional -= self.ring[i].0;
            self.volume -= u128::from(self.ring[i].1);
        }
        self.ring[i] = (notional, volume);
        self.notional += notional;
        self.volume += u128::from(volume);
        self.n += 1;
    }

    pub fn update_trade(&mut self, px: i64, size: u32) {
        self.update(
            u128::try_from(clamp(px)).unwrap_or(0) * u128::from(size),
            size.into(),
        );
    }

    pub fn update_bar(&mut self, bar: &TfBar) {
        self.update(bar.notional, bar.volume);
    }

    pub fn is_ready(&self) -> bool {
        self.n >= N as u64
    }

    /// The VWAP of the last `N` updates, rounded down; `None` until `N` have arrived
    /// or if they carried no volume.
    pub fn value(&self) -> Option<i64> {
        (self.is_ready() && self.volume > 0)
            .then(|| i64::try_from(self.notional / self.volume).ok())
            .flatten()
    }
}

/// Wilder's relative strength index, in permille (0 to 1000).
///
/// Average gain and loss are seeded with the simple average of the first `period`
/// changes, then smoothed `avg = (avg * (period - 1) + x) / period`. If neither gain
/// nor loss has occurred, the value is 500.
#[derive(Clone, Copy, Debug)]
pub struct Rsi {
    period: u32,
    prev: Option<i64>,
    gain: i128,
    loss: i128,
    changes: u32,
}

impl Rsi {
    pub const fn new(period: u32) -> Rsi {
        Rsi {
            period: if period == 0 { 1 } else { period },
            prev: None,
            gain: 0,
            loss: 0,
            changes: 0,
        }
    }

    pub fn update(&mut self, x: i64) {
        let x = clamp(x);
        if let Some(p) = self.prev {
            let d = x - p;
            let (g, l) = (
                i128::from(d.max(0)) << SHIFT,
                i128::from(-d.min(0)) << SHIFT,
            );
            let n = i128::from(self.period);
            self.changes = self.changes.saturating_add(1);
            if self.changes <= self.period {
                self.gain += g;
                self.loss += l;
                if self.changes == self.period {
                    self.gain /= n;
                    self.loss /= n;
                }
            } else {
                self.gain = (self.gain * (n - 1) + g) / n;
                self.loss = (self.loss * (n - 1) + l) / n;
            }
        }
        self.prev = Some(x);
    }

    /// `period` changes (`period + 1` samples) have been seen.
    pub fn is_ready(&self) -> bool {
        self.changes >= self.period
    }

    pub fn value(&self) -> Option<i64> {
        if !self.is_ready() {
            return None;
        }
        let total = self.gain + self.loss;
        Some(if total == 0 {
            500
        } else {
            div_round(self.gain * 1000, total) as i64
        })
    }
}

/// Wilder's average true range from bars, in raw price units.
///
/// The first bar's true range is high - low (there is no previous close); later
/// ones are `max(high - low, |high - prev close|, |low - prev close|)`. Seeded with
/// the simple average of the first `period` true ranges, then Wilder-smoothed.
#[derive(Clone, Copy, Debug)]
pub struct Atr {
    period: u32,
    prev_close: Option<i64>,
    avg: i128,
    bars: u32,
}

impl Atr {
    pub const fn new(period: u32) -> Atr {
        Atr {
            period: if period == 0 { 1 } else { period },
            prev_close: None,
            avg: 0,
            bars: 0,
        }
    }

    pub fn update(&mut self, high: i64, low: i64, close: i64) {
        let (h, l, c) = (clamp(high), clamp(low), clamp(close));
        let tr = match self.prev_close {
            None => h - l,
            Some(pc) => (h - l).max((h - pc).abs()).max((l - pc).abs()),
        };
        let tr = i128::from(tr.max(0)) << SHIFT;
        let n = i128::from(self.period);
        self.bars = self.bars.saturating_add(1);
        if self.bars <= self.period {
            self.avg += tr;
            if self.bars == self.period {
                self.avg /= n;
            }
        } else {
            self.avg = (self.avg * (n - 1) + tr) / n;
        }
        self.prev_close = Some(c);
    }

    pub fn update_bar(&mut self, bar: &TfBar) {
        self.update(bar.high.raw(), bar.low.raw(), bar.close.raw());
    }

    pub fn is_ready(&self) -> bool {
        self.bars >= self.period
    }

    pub fn value(&self) -> Option<i64> {
        self.is_ready().then(|| unscale(self.avg))
    }
}

/// The high and low over a fixed window after a start time (the opening range).
///
/// Samples before the start or at/after the end are ignored. The range is final once
/// time has reached the end ([`OpeningRange::is_complete`]); a breakout is only
/// reported then.
#[derive(Clone, Copy, Debug)]
pub struct OpeningRange {
    start: Nanos,
    end: Nanos,
    high: i64,
    low: i64,
    any: bool,
}

impl OpeningRange {
    /// A window of `window_secs` starting at `start_sec` (seconds since the epoch).
    pub const fn new(start_sec: u64, window_secs: u64) -> OpeningRange {
        OpeningRange {
            start: start_sec.saturating_mul(NANOS_PER_SEC),
            end: start_sec
                .saturating_add(window_secs)
                .saturating_mul(NANOS_PER_SEC),
            high: i64::MIN,
            low: i64::MAX,
            any: false,
        }
    }

    /// A trade at `ts` (event time, nanoseconds).
    pub fn update(&mut self, ts: Nanos, px: i64) {
        self.update_range(ts, px, px);
    }

    /// A bar starting at `ts` with this high and low.
    pub fn update_range(&mut self, ts: Nanos, high: i64, low: i64) {
        if ts < self.start || ts >= self.end {
            return;
        }
        self.high = self.high.max(clamp(high));
        self.low = self.low.min(clamp(low));
        self.any = true;
    }

    pub fn update_bar(&mut self, bar: &TfBar) {
        self.update_range(
            bar.start_sec.saturating_mul(NANOS_PER_SEC),
            bar.high.raw(),
            bar.low.raw(),
        );
    }

    /// Time has reached the end of the window.
    pub fn is_complete(&self, now: Nanos) -> bool {
        now >= self.end
    }

    /// `(low, high)` so far; `None` before any sample in the window.
    pub fn range(&self) -> Option<(i64, i64)> {
        self.any.then_some((self.low, self.high))
    }

    /// Above the range (`Some(true)`), below it (`Some(false)`), or neither / not complete (`None`).
    pub fn breakout(&self, now: Nanos, px: i64) -> Option<bool> {
        let (lo, hi) = self.range().filter(|_| self.is_complete(now))?;
        if px > hi {
            Some(true)
        } else if px < lo {
            Some(false)
        } else {
            None
        }
    }
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<Sma<20>>();
    is_copy::<Ema>();
    is_copy::<Extremes<20>>();
    is_copy::<RateOfChange<20>>();
    is_copy::<Vwap>();
    is_copy::<RollingVwap<20>>();
    is_copy::<Rsi>();
    is_copy::<Atr>();
    is_copy::<OpeningRange>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MtfBars, MtfConfig, Timeframe};
    use tf_core::{Event, Px};
    use tf_synth::{PullbackKind, Scenario, SplitMix64, SymbolSpec, SynthConfig, SynthStream};

    const CENT: i64 = 10_000_000;

    // Generated by an independent exact-fraction (Python) computation. NONE = not ready.
    const NONE: i64 = i64::MIN;
    const SERIES_CENTS: [i64; 40] = [
        4997, 5015, 5028, 5019, 5022, 5009, 5024, 5037, 5030, 5033, 5016, 5010, 5028, 5036, 5038,
        5050, 5056, 5068, 5066, 5055, 5048, 5062, 5055, 5040, 5027, 5035, 5043, 5046, 5065, 5069,
        5089, 5096, 5095, 5095, 5091, 5090, 5105, 5085, 5083, 5092,
    ];
    const HIGH_CENTS: [i64; 40] = [
        5000, 5027, 5031, 5027, 5033, 5018, 5027, 5050, 5042, 5047, 5017, 5018, 5034, 5048, 5039,
        5061, 5068, 5079, 5067, 5067, 5054, 5077, 5070, 5048, 5033, 5040, 5051, 5052, 5066, 5077,
        5096, 5103, 5096, 5104, 5095, 5102, 5117, 5087, 5094, 5104,
    ];
    const LOW_CENTS: [i64; 40] = [
        4990, 5005, 5023, 5007, 5009, 4996, 5021, 5036, 5016, 5030, 5005, 5007, 5018, 5033, 5030,
        5047, 5054, 5055, 5064, 5047, 5037, 5051, 5046, 5028, 5021, 5023, 5037, 5040, 5056, 5061,
        5080, 5094, 5081, 5088, 5080, 5078, 5103, 5074, 5077, 5089,
    ];
    const CLOSE_CENTS: [i64; 40] = [
        4997, 5015, 5028, 5019, 5022, 5009, 5024, 5037, 5030, 5033, 5016, 5010, 5028, 5036, 5038,
        5050, 5056, 5068, 5066, 5055, 5048, 5062, 5055, 5040, 5027, 5035, 5043, 5046, 5065, 5069,
        5089, 5096, 5095, 5095, 5091, 5090, 5105, 5085, 5083, 5092,
    ];
    const EMA5_SMA_SEED_RAW: [i64; 40] = [
        NONE,
        NONE,
        NONE,
        NONE,
        50162000000,
        50138000000,
        50172000000,
        50238000000,
        50258666667,
        50282444444,
        50241629630,
        50194419753,
        50222946502,
        50268631001,
        50305754001,
        50370502667,
        50433668445,
        50515778963,
        50563852642,
        50559235095,
        50532823397,
        50561882264,
        50557921510,
        50505281006,
        50426854004,
        50401236003,
        50410824002,
        50427216001,
        50501477334,
        50564318223,
        50672878815,
        50768585877,
        50829057251,
        50869371501,
        50882914334,
        50888609556,
        50942406371,
        50911604247,
        50884402831,
        50896268554,
    ];
    const EMA5_FIRST_SEED_RAW: [i64; 40] = [
        49970000000,
        50030000000,
        50113333333,
        50138888889,
        50165925926,
        50140617284,
        50173744856,
        50239163237,
        50259442158,
        50282961439,
        50241974293,
        50194649528,
        50223099686,
        50268733124,
        50305822082,
        50370548055,
        50433698703,
        50515799136,
        50563866090,
        50559244060,
        50532829373,
        50561886249,
        50557924166,
        50505282777,
        50426855185,
        50401236790,
        50410824527,
        50427216351,
        50501477567,
        50564318378,
        50672878919,
        50768585946,
        50829057297,
        50869371532,
        50882914354,
        50888609570,
        50942406380,
        50911604253,
        50884402835,
        50896268557,
    ];
    const RSI14_PERMILLE_X1: [i64; 40] = [
        NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, 641,
        671, 685, 711, 700, 644, 611, 650, 616, 551, 501, 529, 556, 566, 624, 635, 685, 701, 695,
        695, 672, 666, 708, 599, 590, 620,
    ];
    const ATR14_RAW: [i64; 40] = [
        NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, 212142857,
        203418367, 205317055, 203508694, 206115216, 194249844, 194660569, 193613386, 200498144,
        203319705, 208082583, 206790970, 204163044, 201008540, 195222216, 195563486, 193023237,
        198521578, 194341465, 191174217, 188947488, 186165524, 190010844, 195724355, 203886901,
        201466408, 202075950,
    ];

    fn raw(c: i64) -> i64 {
        c * CENT
    }

    fn close_to(got: Option<i64>, want: i64, tol: i64, what: &str, i: usize) {
        if want == NONE {
            assert_eq!(got, None, "{what} at {i} should not be ready");
        } else {
            let g = got.unwrap_or_else(|| panic!("{what} at {i} should be ready"));
            assert!(
                (g - want).abs() <= tol,
                "{what} at {i}: got {g}, want {want}"
            );
        }
    }

    // ---- SMA ----

    #[test]
    fn sma_is_the_mean_of_the_last_n_rounded_half_away_from_zero() {
        let mut s = Sma::<3>::new();
        assert_eq!(s.value(), None);
        s.update(1);
        s.update(2);
        assert!(!s.is_ready() && s.value().is_none());
        s.update(4); // 7 / 3 = 2.33
        assert_eq!(s.value(), Some(2));
        s.update(4); // (2 + 4 + 4) / 3 = 3.33
        assert_eq!(s.value(), Some(3));
        s.update(5); // 13 / 3 = 4.33
        s.update(5); // 14 / 3 = 4.67
        assert_eq!(s.value(), Some(5));
        let mut n = Sma::<2>::new();
        n.update(-3);
        n.update(0); // -1.5 rounds away from zero
        assert_eq!(n.value(), Some(-2));
        let mut h = Sma::<2>::new();
        h.update(3);
        h.update(0); // +1.5
        assert_eq!(h.value(), Some(2));
    }

    #[test]
    fn sma_matches_a_brute_force_mean() {
        let mut rng = SplitMix64::new(1);
        let mut s = Sma::<7>::new();
        let mut all = Vec::new();
        for _ in 0..500 {
            let x = (rng.next_u64() % 2_000_000) as i64 - 1_000_000;
            s.update(x);
            all.push(x);
            if all.len() >= 7 {
                let sum: i128 = all[all.len() - 7..].iter().map(|&v| i128::from(v)).sum();
                assert_eq!(s.value(), Some(div_round(sum, 7) as i64));
            } else {
                assert_eq!(s.value(), None);
            }
        }
    }

    // ---- EMA ----

    #[test]
    fn ema_matches_the_independent_exact_computation_for_both_seeds() {
        let mut sma = Ema::new(5, Seed::Sma);
        let mut first = Ema::new(5, Seed::FirstValue);
        for (i, &c) in SERIES_CENTS.iter().enumerate() {
            sma.update(raw(c));
            first.update(raw(c));
            close_to(sma.value(), EMA5_SMA_SEED_RAW[i], 1, "ema(5, sma seed)", i);
            close_to(
                first.value(),
                EMA5_FIRST_SEED_RAW[i],
                1,
                "ema(5, first value)",
                i,
            );
            assert_eq!(sma.is_ready(), i >= 4);
            assert_eq!(first.is_ready(), i >= 4);
        }
    }

    #[test]
    fn ema_edge_cases() {
        let mut one = Ema::new(1, Seed::Sma);
        for x in [5, -7, 100] {
            one.update(x);
            assert_eq!(one.value(), Some(x), "period 1 follows the input");
        }
        assert!(Ema::new(0, Seed::Sma).value().is_none());
        let mut f = Ema::new(10, Seed::FirstValue);
        f.update(100);
        assert_eq!((f.value(), f.is_ready()), (Some(100), false));
        let mut c = Ema::new(3, Seed::Sma);
        for _ in 0..50 {
            c.update(1234);
        }
        assert_eq!(c.value(), Some(1234), "a constant series stays constant");
    }

    #[test]
    fn ema_tracks_a_floating_point_reference_closely_on_long_random_series() {
        for (period, seed) in [(3u32, Seed::FirstValue), (20, Seed::Sma), (200, Seed::Sma)] {
            let mut rng = SplitMix64::new(u64::from(period));
            let mut e = Ema::new(period, seed);
            let a = 2.0 / (f64::from(period) + 1.0);
            let (mut r, mut acc, mut n) = (0.0f64, 0.0f64, 0u32);
            for _ in 0..5000 {
                let x = 40_000_000_000i64 + (rng.next_u64() % 4_000_000_000) as i64;
                e.update(x);
                let xf = x as f64;
                match seed {
                    Seed::FirstValue => r = if n == 0 { xf } else { r + a * (xf - r) },
                    Seed::Sma => {
                        if n < period {
                            acc += xf;
                            if n + 1 == period {
                                r = acc / f64::from(period);
                            }
                        } else {
                            r += a * (xf - r);
                        }
                    }
                }
                n += 1;
                if let Some(v) = e.value() {
                    assert!((v as f64 - r).abs() < 2.0, "period {period}: {v} vs {r}");
                }
            }
        }
    }

    // ---- RSI ----

    #[test]
    fn rsi_matches_the_independent_wilder_computation() {
        let mut r = Rsi::new(14);
        for (i, &c) in SERIES_CENTS.iter().enumerate() {
            r.update(raw(c));
            close_to(r.value(), RSI14_PERMILLE_X1[i], 1, "rsi(14)", i);
            assert_eq!(r.is_ready(), i >= 14);
        }
    }

    #[test]
    fn rsi_extremes_and_no_movement() {
        let (mut up, mut down, mut flat) = (Rsi::new(5), Rsi::new(5), Rsi::new(5));
        for i in 0..20 {
            up.update(100 + i * 3);
            down.update(100 - i * 3);
            flat.update(100);
        }
        assert_eq!(
            (up.value(), down.value(), flat.value()),
            (Some(1000), Some(0), Some(500))
        );
        let mut early = Rsi::new(5);
        for x in [1, 2, 3, 4, 5] {
            early.update(x);
        }
        assert_eq!(early.value(), None, "five samples are four changes");
        early.update(6);
        assert_eq!(early.value(), Some(1000));
    }

    // ---- ATR ----

    #[test]
    fn atr_matches_the_independent_wilder_computation() {
        let mut a = Atr::new(14);
        for i in 0..SERIES_CENTS.len() {
            a.update(raw(HIGH_CENTS[i]), raw(LOW_CENTS[i]), raw(CLOSE_CENTS[i]));
            close_to(a.value(), ATR14_RAW[i], 1, "atr(14)", i);
        }
    }

    #[test]
    fn atr_counts_gaps_between_bars_in_the_true_range() {
        let mut a = Atr::new(2);
        a.update(110, 100, 105); // first: high - low = 10
        a.update(130, 125, 128); // gap up: |130 - 105| = 25 beats 5
        assert_eq!(
            a.value(),
            Some(18),
            "(10 + 25) / 2 = 17.5, half away from zero"
        );
        a.update(120, 118, 119); // gap down from 128: |118 - 128| = 10 beats 2
        assert_eq!(a.value(), Some(14), "(17.5 + 10) / 2 = 13.75");
        let mut bar = Atr::new(1);
        let b = TfBar {
            high: Px::from_raw(30),
            low: Px::from_raw(10),
            close: Px::from_raw(20),
            ..TfBar::default()
        };
        bar.update_bar(&b);
        assert_eq!(bar.value(), Some(20));
    }

    // ---- extremes and rate of change ----

    #[test]
    fn extremes_match_a_brute_force_scan() {
        let mut rng = SplitMix64::new(3);
        let mut e = Extremes::<9>::new();
        let mut all = Vec::new();
        assert_eq!((e.min(), e.max()), (None, None));
        for _ in 0..300 {
            let x = (rng.next_u64() % 1000) as i64 - 500;
            e.update(x);
            all.push(x);
            let w = &all[all.len().saturating_sub(9)..];
            assert_eq!(
                (e.min(), e.max()),
                (w.iter().copied().min(), w.iter().copied().max())
            );
            assert_eq!(e.is_ready(), all.len() >= 9);
        }
    }

    #[test]
    fn rate_of_change_is_permille_over_n_samples() {
        let mut r = RateOfChange::<3>::new();
        for x in [100, 110, 120] {
            r.update(x);
            assert_eq!(r.value(), None);
        }
        r.update(130); // against 100
        assert_eq!((r.value(), r.is_ready()), (Some(300), true));
        r.update(60); // against 110: -45.45%, toward zero
        assert_eq!(r.value(), Some(-454));
        let mut z = RateOfChange::<1>::new();
        z.update(0);
        z.update(5);
        assert_eq!(z.value(), None, "no ratio against zero");
    }

    // ---- VWAP ----

    fn trades(seed: u64, n: usize) -> Vec<(i64, u32)> {
        let mut rng = SplitMix64::new(seed);
        (0..n)
            .map(|_| {
                (
                    raw(100 + (rng.next_u64() % 400) as i64),
                    1 + (rng.next_u64() % 500) as u32,
                )
            })
            .collect()
    }

    #[test]
    fn vwap_is_exact_and_rounds_down() {
        let mut v = Vwap::new();
        assert_eq!((v.value(), v.std_dev()), (None, None));
        v.update(1_000_000_000, 1);
        v.update(1_000_000_001, 2); // (1e9 + 2 * (1e9 + 1)) / 3 = 1e9 + 0.67
        assert_eq!(v.value(), Some(1_000_000_000));
        v.update(5, 0);
        assert_eq!(v.volume(), 3, "a zero size is ignored");
        let mut empty = Vwap::new();
        empty.update(5, 0);
        assert_eq!(
            (empty.value(), empty.volume()),
            (None, 0),
            "a zero size first is ignored too"
        );
        for seed in 0..10 {
            let t = trades(seed, 200);
            let mut v = Vwap::new();
            for &(p, s) in &t {
                v.update(p, s);
            }
            let (n, d) = t.iter().fold((0i128, 0i128), |(n, d), &(p, s)| {
                (n + i128::from(p) * i128::from(s), d + i128::from(s))
            });
            assert_eq!(v.value(), Some((n / d) as i64));
        }
    }

    #[test]
    fn vwap_deviation_matches_a_floating_point_reference() {
        for seed in 0..10 {
            let t = trades(seed, 300);
            let mut v = Vwap::new();
            for &(p, s) in &t {
                v.update(p, s);
            }
            let w: f64 = t.iter().map(|&(_, s)| f64::from(s)).sum();
            let mean = t.iter().map(|&(p, s)| p as f64 * f64::from(s)).sum::<f64>() / w;
            let var = t
                .iter()
                .map(|&(p, s)| f64::from(s) * (p as f64 - mean).powi(2))
                .sum::<f64>()
                / w;
            let got = v.std_dev().unwrap() as f64;
            assert!(
                (got - var.sqrt()).abs() <= 2.0 + var.sqrt() * 1e-6,
                "seed {seed}: {got} vs {}",
                var.sqrt()
            );
        }
    }

    #[test]
    fn vwap_bands_are_symmetric_and_anchoring_starts_over() {
        let mut v = Vwap::new();
        for &(p, s) in &trades(1, 100) {
            v.update(p, s);
        }
        let (lo, hi) = v.bands(2000).unwrap();
        let mid = v.value().unwrap();
        assert_eq!(hi - mid, mid - lo);
        assert_eq!(hi - mid, (v.std_dev().unwrap() * 2) as i64);
        assert_eq!(v.bands(0), Some((mid, mid)));
        v.anchor();
        assert_eq!((v.value(), v.volume(), v.bands(1000)), (None, 0, None));
        v.update(raw(50), 10);
        assert_eq!(
            (v.value(), v.std_dev()),
            (Some(raw(50)), Some(0)),
            "one price has no spread"
        );
    }

    #[test]
    fn rolling_vwap_covers_exactly_the_last_n_updates() {
        let t = trades(4, 100);
        let mut r = RollingVwap::<10>::new();
        for (i, &(p, s)) in t.iter().enumerate() {
            r.update_trade(p, s);
            if i >= 9 {
                let w = &t[i - 9..=i];
                let n: i128 = w.iter().map(|&(p, s)| i128::from(p) * i128::from(s)).sum();
                let d: i128 = w.iter().map(|&(_, s)| i128::from(s)).sum();
                assert_eq!(r.value(), Some((n / d) as i64), "at {i}");
            } else {
                assert_eq!(r.value(), None);
            }
        }
        let mut idle = RollingVwap::<2>::new();
        idle.update(0, 0);
        idle.update(0, 0);
        assert_eq!(idle.value(), None, "no volume, no price");
    }

    #[test]
    fn rolling_vwap_takes_bars() {
        let b = |notional: u128, volume: u64| TfBar {
            notional,
            volume,
            ..TfBar::default()
        };
        let mut r = RollingVwap::<2>::new();
        r.update_bar(&b(1000, 10));
        r.update_bar(&b(3000, 10));
        r.update_bar(&b(5000, 10)); // the first has left
        assert_eq!(r.value(), Some(400));
    }

    // ---- opening range ----

    #[test]
    fn the_opening_range_takes_only_its_window_and_reports_breakouts_when_complete() {
        // 14:30:00 UTC for 5 minutes.
        let start = 1_767_571_200 + 14 * 3600 + 30 * 60;
        let mut o = OpeningRange::new(start, 300);
        let at = |s: u64| s * NANOS_PER_SEC;
        o.update(at(start - 1), raw(900)); // before
        assert_eq!(o.range(), None);
        o.update(at(start), raw(100));
        o.update(at(start + 120), raw(130));
        o.update(at(start + 299), raw(90));
        o.update(at(start + 300), raw(500)); // at the end: outside
        assert_eq!(o.range(), Some((raw(90), raw(130))));
        assert_eq!(o.breakout(at(start + 299), raw(200)), None, "not complete");
        assert!(!o.is_complete(at(start + 299)) && o.is_complete(at(start + 300)));
        let now = at(start + 400);
        assert_eq!(o.breakout(now, raw(131)), Some(true));
        assert_eq!(o.breakout(now, raw(89)), Some(false));
        assert_eq!(
            o.breakout(now, raw(130)),
            None,
            "the high itself is not a breakout"
        );
        assert_eq!(o.breakout(now, raw(90)), None);
        let mut b = OpeningRange::new(start, 300);
        b.update_bar(&TfBar {
            start_sec: start + 60,
            high: Px::from_raw(7),
            low: Px::from_raw(3),
            ..TfBar::default()
        });
        b.update_bar(&TfBar {
            start_sec: start + 300,
            high: Px::from_raw(99),
            low: Px::from_raw(1),
            ..TfBar::default()
        });
        assert_eq!(b.range(), Some((3, 7)));
        assert_eq!(
            OpeningRange::new(start, 300).breakout(at(start + 400), 1),
            None,
            "an empty range has no breakout"
        );
    }

    // ---- robustness ----

    #[test]
    fn extreme_inputs_saturate_and_never_panic() {
        let big = [i64::MAX, i64::MIN, 0, i64::MAX, i64::MIN + 1];
        let (mut sma, mut ema, mut rsi, mut atr) = (
            Sma::<3>::new(),
            Ema::new(4, Seed::Sma),
            Rsi::new(3),
            Atr::new(3),
        );
        let (mut roc, mut ext, mut rv) = (
            RateOfChange::<2>::new(),
            Extremes::<3>::new(),
            RollingVwap::<3>::new(),
        );
        let mut v = Vwap::new();
        for _ in 0..50 {
            for &x in &big {
                sma.update(x);
                ema.update(x);
                rsi.update(x);
                atr.update(x, x, x);
                roc.update(x);
                ext.update(x);
                rv.update_trade(x, u32::MAX);
                v.update(x, u32::MAX);
            }
        }
        assert!(sma.value().is_some() && ema.value().is_some() && rsi.value().is_some());
        let _ = (
            atr.value(),
            roc.value(),
            ext.max(),
            rv.value(),
            v.value(),
            v.std_dev(),
            v.bands(u32::MAX),
        );
    }

    // ---- reproducibility ----

    fn synth_bars() -> (Vec<Event>, MtfBars) {
        let cfg = SynthConfig {
            seed: 12,
            session_start: tf_synth::DEFAULT_SESSION_START,
            duration: 1800 * NANOS_PER_SEC,
            symbols: vec![SymbolSpec {
                symbol: "RUN".into(),
                base_px_cents: 500,
                base_interval_ns: 300_000_000,
                quote_every: 2,
                scenario: Scenario::runner(PullbackKind::Healthy, 120 * NANOS_PER_SEC),
                news: Vec::new(),
            }],
        };
        let events: Vec<Event> = SynthStream::new(&cfg).collect();
        let mut m = MtfBars::new(MtfConfig::default(), 1, 1);
        m.track(0).unwrap();
        (events, m)
    }

    #[test]
    fn indicators_over_a_synthetic_session_are_reproducible_and_pinned() {
        let run = || {
            let (events, mut m) = synth_bars();
            let (mut ema, mut sma, mut rsi, mut atr) = (
                Ema::new(9, Seed::Sma),
                Sma::<5>::new(),
                Rsi::new(7),
                Atr::new(7),
            );
            let (mut vwap, mut rv, mut ext, mut roc) = (
                Vwap::new(),
                RollingVwap::<5>::new(),
                Extremes::<5>::new(),
                RateOfChange::<3>::new(),
            );
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            let mut mix = |v: Option<i64>| {
                for b in v.map_or(i128::MIN, i128::from).to_le_bytes() {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x100_0000_01b3);
                }
            };
            let mut out = Vec::new();
            let mut bars_seen = 0;
            for ev in &events {
                if let Event::Trade(t) = ev {
                    vwap.update(t.px.raw(), t.size);
                }
                out.clear();
                m.on_event(ev, &mut out);
                for c in &out {
                    if c.timeframe == Timeframe::M1 {
                        let bar = *m
                            .symbol(c.instrument)
                            .unwrap()
                            .closed(Timeframe::M1, 0)
                            .unwrap();
                        let close = bar.close.raw();
                        ema.update(close);
                        sma.update(close);
                        rsi.update(close);
                        atr.update_bar(&bar);
                        rv.update_bar(&bar);
                        ext.update(close);
                        roc.update(close);
                        bars_seen += 1;
                        for v in [
                            ema.value(),
                            sma.value(),
                            rsi.value(),
                            atr.value(),
                            rv.value(),
                            ext.max(),
                            ext.min(),
                            roc.value(),
                            vwap.value(),
                        ] {
                            mix(v);
                        }
                        mix(vwap.std_dev().map(|s| s as i64));
                    }
                }
            }
            (h, bars_seen)
        };
        let (a, n) = run();
        assert_eq!((a, n), run());
        assert!(n >= 20, "{n} minute bars");
        assert_eq!(
            a, 0xdc64_5052_83ab_02c0,
            "indicator digest {a:#x} over {n} bars"
        );
    }
}
