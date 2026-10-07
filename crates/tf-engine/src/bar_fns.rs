//! Indicators as pure functions over closed bars (E19-S03).
//!
//! The streaming indicators in [`crate::indicators`] are fed one bar at a time by whoever owns them, and
//! warm up from the bars they see. A cross-sectional strategy reviews thousands of symbols at a time and
//! does not keep an indicator per symbol; it asks for the value over the bars the shared aggregator
//! holds. These functions give it that: each takes closed bars, oldest first, and returns what the
//! streaming indicator of the same name would show after being fed those bars one by one.
//!
//! Rules, stated and tested:
//! - **Equal to the streaming indicator.** Each runs the streaming indicator over the bars, so the
//!   definition (seed, Wilder smoothing, warm-up, rounding, the clamp at about $1,100) is one definition.
//!   Tests compare them on random series and on the closes a real aggregator builds, including a real day
//!   (`tf-bench`, example `real_bars`).
//! - **Bars with no trades are skipped.** A flat filler bar (gap filling) carries no information and is
//!   not a sample, as in `TrendLong`.
//! - **Only what is given.** A function over the bars of a [`SymbolBars`] sees the last
//!   [`crate::BAR_DEPTH`] closed bars of the timeframe, so for a recursive indicator (EMA, RSI, ATR) the
//!   value equals the streaming one only while the series has not outgrown the ring; past that it is the
//!   indicator warmed up on the retained bars. A caller that needs a longer memory (the EMA(100) of hourly
//!   closes) seeds from history instead (E19-S04).
//! - **Warm-up is `None`**, as the streaming value is.
//! - **The VWAP is exact from the bars' notional**, so it also holds above the streaming clamp.

use crate::indicators::{Atr, Ema, Rsi, Seed};
use crate::mtf::TfBar;

fn samples<'a>(bars: impl IntoIterator<Item = &'a TfBar>) -> impl Iterator<Item = &'a TfBar> {
    bars.into_iter().filter(|b| b.trades > 0)
}

/// The EMA of the closes (raw price units), `None` until the seed is met.
pub fn ema<'a>(bars: impl IntoIterator<Item = &'a TfBar>, period: u32, seed: Seed) -> Option<i64> {
    let mut e = Ema::new(period, seed);
    for b in samples(bars) {
        e.update(b.close.raw());
    }
    e.value()
}

/// Wilder's RSI of the closes, in permille (0 to 1000).
pub fn rsi<'a>(bars: impl IntoIterator<Item = &'a TfBar>, period: u32) -> Option<i64> {
    let mut r = Rsi::new(period);
    for b in samples(bars) {
        r.update(b.close.raw());
    }
    r.value()
}

/// Wilder's average true range (raw price units).
pub fn atr<'a>(bars: impl IntoIterator<Item = &'a TfBar>, period: u32) -> Option<i64> {
    let mut a = Atr::new(period);
    for b in samples(bars) {
        a.update_bar(b);
    }
    a.value()
}

/// The volume-weighted average price of the bars (raw price units, rounded down), anchored at the first
/// bar given: pass the bars from the anchor on. `None` for no volume.
pub fn vwap<'a>(bars: impl IntoIterator<Item = &'a TfBar>) -> Option<i64> {
    let (mut notional, mut volume) = (0u128, 0u128);
    for b in samples(bars) {
        notional += b.notional;
        volume += u128::from(b.volume);
    }
    (volume > 0)
        .then(|| i64::try_from(notional / volume).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicators::Vwap;
    use crate::mtf::{BarClose, MtfBars, MtfConfig, Timeframe};
    use tf_core::{Event, Header, NANOS_PER_SEC, ProviderId, Px, Trade, TradeFlags};

    const S: u64 = NANOS_PER_SEC;
    const T0: u64 = 1_767_571_200 + 3600;

    fn bar(close: i64, high: i64, low: i64, volume: u64, trades: u32) -> TfBar {
        TfBar {
            start_sec: 0,
            open: Px::from_cents(close),
            high: Px::from_cents(high),
            low: Px::from_cents(low),
            close: Px::from_cents(close),
            volume,
            trades,
            notional: u128::try_from(Px::from_cents(close).raw()).unwrap() * u128::from(volume),
        }
    }

    fn trade(sec: u64, cents: i64, size: u32) -> Event {
        Event::Trade(Trade {
            hdr: Header {
                ts_event: sec * S,
                ts_recv: sec * S,
                seq: sec,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(cents),
            size,
            flags: TradeFlags::NONE,
        })
    }

    #[test]
    fn hand_worked_values() {
        let c = |v| Px::from_cents(v).raw();
        // EMA(3) seeded by the average of the first three closes (10, 20, 30 dollars) is 20; the next
        // close of 40 moves it by 2 / (3 + 1) of the difference, to 30.
        let bars = [
            bar(1000, 1000, 1000, 1, 1),
            bar(2000, 2000, 2000, 1, 1),
            bar(3000, 3000, 3000, 1, 1),
        ];
        assert_eq!(ema(&bars, 3, Seed::Sma), Some(c(2000)));
        assert_eq!(ema(&bars[..2], 3, Seed::Sma), None, "not warm");
        assert_eq!(ema(&bars[..1], 3, Seed::FirstValue), Some(c(1000)));
        let mut more = bars.to_vec();
        more.push(bar(4000, 4000, 4000, 1, 1));
        assert_eq!(ema(&more, 3, Seed::Sma), Some(c(3000)));
        // RSI: only gains is 1000; only losses is 0; no movement is 500.
        let up: Vec<_> = (1..=5).map(|i| bar(1000 + i * 10, 0, 0, 1, 1)).collect();
        assert_eq!(rsi(&up, 4), Some(1000));
        let down: Vec<_> = (1..=5).map(|i| bar(1000 - i * 10, 0, 0, 1, 1)).collect();
        assert_eq!(rsi(&down, 4), Some(0));
        let flat: Vec<_> = (0..5).map(|_| bar(1000, 0, 0, 1, 1)).collect();
        assert_eq!(rsi(&flat, 4), Some(500));
        assert_eq!(rsi(&up[..4], 4), None, "needs period + 1 closes");
        // ATR(2): true ranges 2.00 (high - low), then max(1.50, |10.50 - 9.00|, |9.00 - 9.00|) = 1.50;
        // the seed is their average, 1.75.
        let a = [bar(900, 1100, 900, 5, 1), bar(1050, 1050, 900, 5, 1)];
        assert_eq!(atr(&a, 2), Some(c(175)));
        assert_eq!(atr(&a[..1], 2), None);
        // VWAP: (10.00 x 1 + 20.00 x 3) / 4 = 17.50, from the bars' notional.
        let v = [bar(1000, 0, 0, 1, 1), bar(2000, 0, 0, 3, 2)];
        assert_eq!(vwap(&v), Some(c(1750)));
        assert_eq!(
            vwap(&v[1..]),
            Some(c(2000)),
            "anchored at the first bar given"
        );
        assert_eq!(vwap(&[bar(1000, 0, 0, 0, 0)]), None);
        assert_eq!(
            vwap(&[bar(1000, 0, 0, 1, 1)]),
            Some(c(1000)),
            "one share is volume"
        );
    }

    #[test]
    fn a_bar_with_no_trades_is_not_a_sample() {
        let mut with_gap = vec![
            bar(1000, 1000, 1000, 1, 1),
            bar(1010, 1010, 1010, 1, 1),
            bar(1020, 1020, 1020, 1, 1),
        ];
        let plain = with_gap.clone();
        with_gap.insert(1, bar(1000, 1000, 1000, 0, 0));
        with_gap.push(bar(1020, 1020, 1020, 0, 0));
        assert_eq!(ema(&with_gap, 3, Seed::Sma), ema(&plain, 3, Seed::Sma));
        assert_eq!(rsi(&with_gap, 2), rsi(&plain, 2));
        assert_eq!(atr(&with_gap, 2), atr(&plain, 2));
        assert_eq!(vwap(&with_gap), vwap(&plain));
    }

    /// Random trades through a real aggregator; the streaming indicators fed from its closes one by one
    /// against the functions over the bars it holds.
    fn compare_on(seed: u64, minutes: u64, fill: bool) {
        let mut x = seed;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let cfg = MtfConfig::clock(0, fill);
        let mut m = MtfBars::new(cfg, 1, 1);
        m.track(0).unwrap();
        let mut closes: Vec<BarClose> = Vec::new();
        let mut px = 5000i64;
        let mut vw = Vwap::new();
        let mut sec = T0;
        let end = T0 + minutes * 60;
        while sec < end {
            // Quiet stretches happen, so there are gaps for the filler to fill.
            sec += 1 + next() % if next() % 40 == 0 { 400 } else { 12 };
            px = (px + (next() % 21) as i64 - 10).clamp(100, 20_000);
            let size = 1 + (next() % 500) as u32;
            let ev = trade(sec, px, size);
            m.on_event(&ev, &mut closes);
            vw.update(Px::from_cents(px).raw(), size);
        }
        // Let time pass so every bar is closed: all the trades are in closed bars.
        m.advance_to((end + 3 * 3600) * S, &mut closes);
        let s = m.symbol(0).unwrap();
        assert!(
            s.closed_len(Timeframe::M1) < crate::BAR_DEPTH,
            "the series fits the ring"
        );
        let bars: Vec<&TfBar> = (0..s.closed_len(Timeframe::M1))
            .rev()
            .map(|i| s.closed(Timeframe::M1, i).unwrap())
            .collect();
        assert!(bars.len() > 30, "{} bars", bars.len());
        for period in [1, 2, 5, 14, 20] {
            let (mut e_sma, mut e_first) = (
                Ema::new(period, Seed::Sma),
                Ema::new(period, Seed::FirstValue),
            );
            let (mut r, mut a) = (Rsi::new(period), Atr::new(period));
            // The streaming side is fed from the close reports, as a strategy's on_bar would.
            for c in closes
                .iter()
                .filter(|c| c.timeframe == Timeframe::M1 && c.bar.trades > 0)
            {
                e_sma.update(c.bar.close.raw());
                e_first.update(c.bar.close.raw());
                r.update(c.bar.close.raw());
                a.update_bar(&c.bar);
            }
            let it = || bars.iter().copied();
            assert_eq!(
                ema(it(), period, Seed::Sma),
                e_sma.value(),
                "ema sma {period} seed {seed}"
            );
            assert_eq!(
                ema(it(), period, Seed::FirstValue),
                e_first.value(),
                "ema first {period}"
            );
            assert_eq!(rsi(it(), period), r.value(), "rsi {period} seed {seed}");
            assert_eq!(atr(it(), period), a.value(), "atr {period} seed {seed}");
        }
        assert_eq!(vwap(bars.iter().copied()), vw.value(), "vwap seed {seed}");
    }

    #[test]
    fn they_equal_the_streaming_indicators_on_random_series() {
        for seed in 1..=40u64 {
            compare_on(seed * 0x9e37_79b9, 90, false);
            compare_on(seed * 0x2545_f491, 90, true);
        }
    }

    #[test]
    fn past_the_ring_they_are_the_indicator_warmed_up_on_what_is_kept() {
        let mut m = MtfBars::new(MtfConfig::default(), 1, 1);
        m.track(0).unwrap();
        let mut closes = Vec::new();
        for k in 0..300u64 {
            // A curve (an exactly straight line would hide the difference: the average of the first
            // closes seeds an EMA exactly where its steady lag puts it).
            m.on_event(
                &trade(T0 + k * 60, 5000 + (k * k / 9) as i64, 10),
                &mut closes,
            );
        }
        let s = m.symbol(0).unwrap();
        assert_eq!(s.closed_len(Timeframe::M1), crate::BAR_DEPTH);
        let kept: Vec<&TfBar> = (0..crate::BAR_DEPTH)
            .rev()
            .map(|i| s.closed(Timeframe::M1, i).unwrap())
            .collect();
        let mut on_kept = Ema::new(100, Seed::Sma);
        for b in &kept {
            on_kept.update(b.close.raw());
        }
        assert_eq!(ema(kept.iter().copied(), 100, Seed::Sma), on_kept.value());
        // The full history gives another number: that is the limit stated in the module docs.
        let mut full = Ema::new(100, Seed::Sma);
        for c in closes.iter().filter(|c| c.timeframe == Timeframe::M1) {
            full.update(c.bar.close.raw());
        }
        assert_ne!(full.value(), on_kept.value());
    }
}
