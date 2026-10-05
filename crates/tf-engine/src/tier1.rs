//! Tier 1: rings and pullback features for the few symbols worth watching closely.
//!
//! Tier 0 keeps a handful of numbers for every symbol. A promoted symbol also gets
//! a [`Tier1Symbol`]: the last 256 trades and 64 quotes, and the last 256 seconds
//! of one-second bars, from which [`Tier1Symbol::features`] derives the
//! description of a pullback that the momentum strategy classifies on (DESIGN.md,
//! Strategy 1):
//!
//! - **depth**: how much of the impulse (swing low to swing high) the pullback has
//!   given back, in permille, at its deepest and right now;
//! - **volume ratio**: volume per second since the high against volume per second
//!   during the impulse (a healthy pullback dries up; a dangerous one does not);
//! - **higher lows**: how many consecutive complete 5 s buckets after the pullback
//!   low made a higher low than the one before;
//! - **tape speed**: trades per second in the last 5 s against the impulse, and the
//!   speed of the last 32 ticks;
//! - **spread and bid support**: latest and average over the quote ring.
//!
//! Integer arithmetic only, time from the events, and each symbol's state is a
//! `Copy` value (so it cannot own heap memory). [`Tier1`] allocates one box per
//! promotion, never per event, and refuses promotions past its bound, so memory
//! is capped. Which symbols to promote and when to demote them is a separate
//! policy (E07-S05); this module only provides the mechanism and the bound.
//!
//! [`Tier1Symbol::features`] scans up to 256 bars, so call it when a decision is
//! due (for example once a second), not on every event.

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};

/// Seconds of one-second bars kept.
pub const BAR_SECS: usize = 256;
/// Trades kept.
pub const TICK_RING: usize = 256;
/// Quotes kept.
pub const QUOTE_RING: usize = 64;
/// How many of the latest ticks the tape speed is measured over.
pub const SPEED_TICKS: usize = 32;
/// Width of the buckets the higher-low structure is read from.
pub const BUCKET_SECS: u64 = 5;
/// Window for the recent trade rate.
pub const RECENT_SECS: u64 = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    pub ts: Nanos,
    pub px: Px,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quote1 {
    pub ts: Nanos,
    pub bid: Px,
    pub ask: Px,
    pub bid_sz: u32,
    pub ask_sz: u32,
}

#[derive(Clone, Copy, Default)]
struct SecBar {
    sec: u64,
    used: bool,
    high: i64,
    low: i64,
    volume: u64,
    trades: u32,
}

/// What a pullback looks like right now. `None` fields have no data yet (for
/// example no seconds since the high, or no quotes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PullbackFeatures {
    pub impulse_low: Px,
    pub impulse_high: Px,
    pub pullback_low: Px,
    pub last: Px,
    /// Seconds from the swing low to the swing high, inclusive.
    pub impulse_secs: u32,
    pub secs_since_high: u32,
    /// Deepest give-back of the impulse so far, permille (above 1000 = through the start).
    pub depth_permille: i64,
    /// Give-back at the last price, permille (negative = above the high).
    pub retrace_now_permille: i64,
    /// Volume per second since the high / during the impulse, permille.
    pub volume_ratio_permille: Option<u64>,
    /// Consecutive higher lows in complete 5 s buckets after the pullback low.
    pub higher_lows: u32,
    /// Trades per second over the last 5 s, times 1000.
    pub recent_trades_per_sec_x1000: u64,
    /// That rate against the impulse's, permille.
    pub tape_ratio_permille: Option<u64>,
    /// Ticks per second over the last 32 ticks, times 1000 (0 with too few).
    pub tick_speed_x1000: u64,
    pub spread: Option<i64>,
    pub avg_spread: Option<i64>,
    /// Latest bid size as permille of bid + ask size.
    pub bid_support_permille: Option<u32>,
    pub avg_bid_support_permille: Option<u32>,
}

/// One symbol's rings and bars. About 20 KB.
#[derive(Clone, Copy)]
pub struct Tier1Symbol {
    bars: [SecBar; BAR_SECS],
    ticks: [Tick; TICK_RING],
    n_ticks: u64,
    quotes: [Quote1; QUOTE_RING],
    n_quotes: u64,
    now_sec: u64,
    last_px: i64,
}

impl Default for Tier1Symbol {
    fn default() -> Self {
        Self::new()
    }
}

impl Tier1Symbol {
    pub const fn new() -> Self {
        Tier1Symbol {
            bars: [SecBar {
                sec: 0,
                used: false,
                high: 0,
                low: 0,
                volume: 0,
                trades: 0,
            }; BAR_SECS],
            ticks: [Tick {
                ts: 0,
                px: Px::ZERO,
                size: 0,
            }; TICK_RING],
            n_ticks: 0,
            quotes: [Quote1 {
                ts: 0,
                bid: Px::ZERO,
                ask: Px::ZERO,
                bid_sz: 0,
                ask_sz: 0,
            }; QUOTE_RING],
            n_quotes: 0,
            now_sec: 0,
            last_px: 0,
        }
    }

    pub fn on_trade(&mut self, ts: Nanos, px: Px, size: u32) {
        // A stale timestamp counts in the current second.
        let sec = (ts / NANOS_PER_SEC).max(self.now_sec);
        self.now_sec = sec;
        let bar = &mut self.bars[(sec % BAR_SECS as u64) as usize];
        if !bar.used || bar.sec != sec {
            *bar = SecBar {
                sec,
                used: true,
                high: px.raw(),
                low: px.raw(),
                volume: 0,
                trades: 0,
            };
        }
        bar.high = bar.high.max(px.raw());
        bar.low = bar.low.min(px.raw());
        bar.volume += u64::from(size);
        bar.trades = bar.trades.saturating_add(1);
        self.ticks[(self.n_ticks % TICK_RING as u64) as usize] = Tick { ts, px, size };
        self.n_ticks += 1;
        self.last_px = px.raw();
    }

    pub fn on_quote(&mut self, q: Quote1) {
        self.quotes[(self.n_quotes % QUOTE_RING as u64) as usize] = q;
        self.n_quotes += 1;
    }

    /// Trades seen (and kept, up to the ring size).
    pub fn ticks_len(&self) -> usize {
        self.n_ticks.min(TICK_RING as u64) as usize
    }

    pub fn quotes_len(&self) -> usize {
        self.n_quotes.min(QUOTE_RING as u64) as usize
    }

    /// The `i`th most recent trade (0 = latest).
    pub fn tick_ago(&self, i: usize) -> Option<Tick> {
        (i < self.ticks_len())
            .then(|| self.ticks[((self.n_ticks - 1 - i as u64) % TICK_RING as u64) as usize])
    }

    /// The `i`th most recent quote (0 = latest).
    pub fn quote_ago(&self, i: usize) -> Option<Quote1> {
        (i < self.quotes_len())
            .then(|| self.quotes[((self.n_quotes - 1 - i as u64) % QUOTE_RING as u64) as usize])
    }

    /// Bars still inside the window, oldest first.
    fn bars_in_order(&self) -> impl Iterator<Item = &SecBar> {
        let from = self.now_sec.saturating_sub(BAR_SECS as u64 - 1);
        (from..=self.now_sec).filter_map(move |s| {
            let b = &self.bars[(s % BAR_SECS as u64) as usize];
            (b.used && b.sec == s).then_some(b)
        })
    }

    pub fn features(&self) -> Option<PullbackFeatures> {
        if self.n_ticks == 0 {
            return None;
        }
        // The swing high (latest occurrence of the highest price in the window).
        let (mut hi, mut sec_hi) = (i64::MIN, 0);
        for b in self.bars_in_order() {
            if b.high >= hi {
                (hi, sec_hi) = (b.high, b.sec);
            }
        }
        // The swing low before it.
        let (mut lo, mut sec_lo) = (i64::MAX, 0);
        for b in self.bars_in_order().filter(|b| b.sec <= sec_hi) {
            if b.low <= lo {
                (lo, sec_lo) = (b.low, b.sec);
            }
        }
        if hi <= lo {
            return None; // nothing has moved up yet
        }
        // The deepest point since the high.
        let (mut pb, mut sec_pb) = (i64::MAX, 0);
        for b in self.bars_in_order().filter(|b| b.sec >= sec_hi) {
            if b.low <= pb {
                (pb, sec_pb) = (b.low, b.sec);
            }
        }
        let range = i128::from(hi - lo);
        let permille = |gave_back: i64| (i128::from(gave_back) * 1000 / range) as i64;
        let impulse_secs = (sec_hi - sec_lo + 1) as u32;
        let secs_since_high = (self.now_sec - sec_hi) as u32;

        let (mut imp_vol, mut imp_trades, mut pb_vol) = (0u128, 0u128, 0u128);
        for b in self.bars_in_order() {
            if b.sec >= sec_lo && b.sec <= sec_hi {
                imp_vol += u128::from(b.volume);
                imp_trades += u128::from(b.trades);
            } else if b.sec > sec_hi {
                pb_vol += u128::from(b.volume);
            }
        }
        // (a / sa) / (b / sb) in permille, without losing precision to division order.
        let ratio = |a: u128, sa: u128, b: u128, sb: u128| -> Option<u64> {
            (sa > 0 && b > 0).then(|| (a * sb * 1000 / (b * sa)) as u64)
        };
        let imp_s = u128::from(impulse_secs);
        let pb_s = u128::from(secs_since_high);
        let volume_ratio_permille = ratio(pb_vol, pb_s, imp_vol, imp_s);

        let recent_from = self.now_sec.saturating_sub(RECENT_SECS - 1);
        let recent: u64 = self
            .bars_in_order()
            .filter(|b| b.sec >= recent_from)
            .map(|b| u64::from(b.trades))
            .sum();
        let recent_x1000 = recent * 1000 / RECENT_SECS;
        let tape_ratio_permille = (imp_trades > 0).then(|| {
            (u128::from(recent) * imp_s * 1000 / (imp_trades * u128::from(RECENT_SECS))) as u64
        });

        // Higher lows: complete buckets of 5 s starting at the pullback low's second.
        let mut higher_lows = 0;
        let mut prev: Option<i64> = None;
        let mut start = sec_pb;
        while start + BUCKET_SECS - 1 <= self.now_sec {
            let low = self
                .bars_in_order()
                .filter(|b| b.sec >= start && b.sec < start + BUCKET_SECS)
                .map(|b| b.low)
                .min();
            if let Some(l) = low {
                match prev {
                    Some(p) if l > p => higher_lows += 1,
                    Some(_) => break,
                    None => {}
                }
                prev = Some(l);
            }
            start += BUCKET_SECS;
        }

        // Tape speed over the latest ticks.
        let n = SPEED_TICKS.min(self.ticks_len());
        let tick_speed_x1000 = match (self.tick_ago(0), self.tick_ago(n.saturating_sub(1))) {
            (Some(new), Some(old)) if n >= 2 && new.ts > old.ts => {
                ((n as u128 - 1) * 1000 * u128::from(NANOS_PER_SEC) / u128::from(new.ts - old.ts))
                    as u64
            }
            _ => 0,
        };

        // Quote statistics.
        let (mut spread_sum, mut spread_n, mut sup_sum, mut sup_n) = (0i128, 0i128, 0u64, 0u64);
        let (mut spread, mut support) = (None, None);
        for i in 0..self.quotes_len() {
            let q = self.quote_ago(i).expect("within ring");
            let s = (q.ask.raw() > 0 && q.bid.raw() > 0 && q.ask >= q.bid)
                .then(|| q.ask.raw() - q.bid.raw());
            let b = (u64::from(q.bid_sz) + u64::from(q.ask_sz) > 0).then(|| {
                (u64::from(q.bid_sz) * 1000 / (u64::from(q.bid_sz) + u64::from(q.ask_sz))) as u32
            });
            if i == 0 {
                (spread, support) = (s, b);
            }
            if let Some(s) = s {
                spread_sum += i128::from(s);
                spread_n += 1;
            }
            if let Some(b) = b {
                sup_sum += u64::from(b);
                sup_n += 1;
            }
        }

        Some(PullbackFeatures {
            impulse_low: Px::from_raw(lo),
            impulse_high: Px::from_raw(hi),
            pullback_low: Px::from_raw(pb),
            last: Px::from_raw(self.last_px),
            impulse_secs,
            secs_since_high,
            depth_permille: permille(hi - pb),
            retrace_now_permille: permille(hi - self.last_px),
            volume_ratio_permille,
            higher_lows,
            recent_trades_per_sec_x1000: recent_x1000,
            tape_ratio_permille,
            tick_speed_x1000,
            spread,
            avg_spread: (spread_n > 0).then(|| (spread_sum / spread_n) as i64),
            bid_support_permille: support,
            avg_bid_support_permille: (sup_n > 0).then(|| (sup_sum / sup_n) as u32),
        })
    }
}

/// Tier 1 state for a bounded set of promoted symbols.
pub struct Tier1 {
    slots: Vec<Option<Box<Tier1Symbol>>>,
    live: usize,
    max: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromoteError {
    /// The instrument id is outside the id space.
    Unknown,
    AlreadyPromoted,
    /// `max` symbols are already promoted.
    Full,
}

impl Tier1 {
    /// Room for ids below `id_space`, at most `max` promoted at once.
    pub fn new(id_space: usize, max: usize) -> Tier1 {
        Tier1 {
            slots: (0..id_space).map(|_| None).collect(),
            live: 0,
            max,
        }
    }

    /// Start watching `id` closely. The one allocation Tier 1 makes for a symbol.
    pub fn promote(&mut self, id: InstrumentId) -> Result<(), PromoteError> {
        let slot = self
            .slots
            .get_mut(id as usize)
            .ok_or(PromoteError::Unknown)?;
        if slot.is_some() {
            return Err(PromoteError::AlreadyPromoted);
        }
        if self.live >= self.max {
            return Err(PromoteError::Full);
        }
        *slot = Some(Box::new(Tier1Symbol::new()));
        self.live += 1;
        Ok(())
    }

    /// Stop watching `id` and drop its history. Returns whether it was promoted.
    pub fn demote(&mut self, id: InstrumentId) -> bool {
        match self.slots.get_mut(id as usize).and_then(Option::take) {
            Some(_) => {
                self.live -= 1;
                true
            }
            None => false,
        }
    }

    pub fn live(&self) -> usize {
        self.live
    }

    pub fn symbol(&self, id: InstrumentId) -> Option<&Tier1Symbol> {
        self.slots.get(id as usize)?.as_deref()
    }

    /// Route an event to its symbol if promoted; others are ignored.
    pub fn on_event(&mut self, ev: &Event) {
        let Some(Some(s)) = self.slots.get_mut(ev.instrument() as usize) else {
            return;
        };
        match ev {
            Event::Trade(t) => s.on_trade(t.hdr.ts_recv, t.px, t.size),
            Event::Quote(q) => s.on_quote(Quote1 {
                ts: q.hdr.ts_recv,
                bid: q.bid_px,
                ask: q.ask_px,
                bid_sz: q.bid_sz,
                ask_sz: q.ask_sz,
            }),
            _ => {}
        }
    }
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<Tier1Symbol>();
    is_copy::<PullbackFeatures>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use tf_core::{Header, ProviderId, Quote, Trade, TradeFlags};
    use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

    const S: Nanos = NANOS_PER_SEC;
    const T0: Nanos = 1_000 * S;

    fn px(cents: i64) -> Px {
        Px::from_cents(cents)
    }

    fn q(bid: i64, ask: i64, bid_sz: u32, ask_sz: u32) -> Quote1 {
        Quote1 {
            ts: 0,
            bid: px(bid),
            ask: px(ask),
            bid_sz,
            ask_sz,
        }
    }

    /// The worked example: a 100.00 -> 110.00 impulse over six seconds (two trades a
    /// second, 100 shares a second), then a pullback to 106.00 at second 8 and a
    /// bounce of higher lows, one 20-share trade a second, ending at second 22.
    fn worked() -> Tier1Symbol {
        let mut t = Tier1Symbol::new();
        for (sec, cents) in [
            (0, 10000),
            (1, 10200),
            (2, 10400),
            (3, 10600),
            (4, 10800),
            (5, 11000),
        ] {
            t.on_trade(T0 + sec * S, px(cents), 50);
            t.on_trade(T0 + sec * S + S / 2, px(cents), 50);
        }
        let pullback = [
            (6, 10800),
            (7, 10700),
            (8, 10600),
            (9, 10700),
            (10, 10650),
            (11, 10750),
            (12, 10800),
            (13, 10650),
            (14, 10700),
            (15, 10750),
            (16, 10700),
            (17, 10800),
            (18, 10700),
            (19, 10750),
            (20, 10800),
            (21, 10750),
            (22, 10800),
        ];
        for (sec, cents) in pullback {
            t.on_trade(T0 + sec * S, px(cents), 20);
        }
        t.on_quote(q(10700, 10710, 300, 100));
        t.on_quote(q(10690, 10710, 100, 100));
        t.on_quote(q(10700, 10720, 200, 600));
        t
    }

    #[test]
    fn a_worked_pullback_gives_the_hand_figures() {
        let f = worked().features().unwrap();
        assert_eq!(
            (f.impulse_low, f.impulse_high, f.pullback_low, f.last),
            (px(10000), px(11000), px(10600), px(10800))
        );
        assert_eq!((f.impulse_secs, f.secs_since_high), (6, 17));
        assert_eq!(f.depth_permille, 400, "gave back 4 of 10");
        assert_eq!(f.retrace_now_permille, 200, "now 2 below the high");
        // 340 shares over 17 s = 20/s against 600 over 6 s = 100/s.
        assert_eq!(f.volume_ratio_permille, Some(200));
        // Buckets [8,13) low 106.00, [13,18) low 106.50, [18,23) low 107.00.
        assert_eq!(f.higher_lows, 2);
        // 5 trades in the last 5 s = 1/s; the impulse did 12 in 6 s = 2/s.
        assert_eq!(f.recent_trades_per_sec_x1000, 1000);
        assert_eq!(f.tape_ratio_permille, Some(500));
        // 29 ticks: the last 32 reach back to second 0, 28 intervals in 22 s.
        assert_eq!(f.tick_speed_x1000, 28 * 1000 / 22);
        assert_eq!(
            (f.spread, f.avg_spread),
            (Some(px(20).raw()), Some(5 * px(10).raw() / 3))
        );
        assert_eq!(
            (f.bid_support_permille, f.avg_bid_support_permille),
            (Some(250), Some(500))
        );
    }

    #[test]
    fn higher_lows_stop_at_the_first_lower_bucket() {
        let mut t = Tier1Symbol::new();
        for (sec, cents) in [(0, 10000), (1, 11000), (2, 10500)] {
            t.on_trade(T0 + sec * S, px(cents), 10);
        }
        // Buckets from the pullback low (second 2): [2,7) 105.00, [7,12) 105.50, [12,17) 105.20 (lower), [17,22) 106.00.
        for (sec, cents) in [(7, 10550), (12, 10520), (17, 10600), (21, 10650)] {
            t.on_trade(T0 + sec * S, px(cents), 10);
        }
        assert_eq!(
            t.features().unwrap().higher_lows,
            1,
            "the third bucket broke the run"
        );
    }

    #[test]
    fn an_equal_low_is_not_a_higher_low() {
        let mut t = Tier1Symbol::new();
        for (sec, cents) in [
            (0, 10000),
            (1, 11000),
            (2, 10500),
            (7, 10550),
            (12, 10550),
            (17, 10600),
        ] {
            t.on_trade(T0 + sec * S, px(cents), 10);
        }
        // Buckets [2,7) low 105.00, [7,12) 105.50 (higher), [12,17) 105.50 (equal: the run ends).
        assert_eq!(t.features().unwrap().higher_lows, 1);
    }

    #[test]
    fn no_features_before_an_impulse() {
        let mut t = Tier1Symbol::new();
        assert_eq!(t.features(), None);
        t.on_trade(T0, px(1000), 10);
        assert_eq!(t.features(), None, "one price: no range");
        for s in 1..20 {
            t.on_trade(T0 + s * S, px(1000), 10);
        }
        assert_eq!(t.features(), None, "flat");
        // Falling prices: the high is the first bar and there is no low before it.
        let mut d = Tier1Symbol::new();
        for s in 0..5 {
            d.on_trade(T0 + s * S, px(1000 - s as i64 * 10), 10);
        }
        assert_eq!(d.features(), None);
    }

    #[test]
    fn missing_data_is_none_not_zero() {
        let mut t = Tier1Symbol::new();
        for (s, c) in [(0, 1000), (1, 1100)] {
            t.on_trade(T0 + s * S, px(c), 10);
        }
        let f = t.features().unwrap();
        assert_eq!(f.secs_since_high, 0);
        assert_eq!(
            f.volume_ratio_permille, None,
            "no seconds since the high yet"
        );
        assert_eq!(
            (f.spread, f.avg_spread, f.bid_support_permille),
            (None, None, None)
        );
        assert_eq!(
            f.tick_speed_x1000, 1000,
            "two ticks 1 s apart: one interval per second"
        );
        // A crossed or empty quote has no spread; zero sizes have no support.
        t.on_quote(q(1010, 1000, 0, 0));
        let f = t.features().unwrap();
        assert_eq!((f.spread, f.bid_support_permille), (None, None));
    }

    #[test]
    fn the_rings_keep_the_latest_and_wrap() {
        let mut t = Tier1Symbol::new();
        for i in 0..300u64 {
            t.on_trade(T0 + i * (S / 10), px(1000 + i as i64), i as u32);
        }
        assert_eq!(t.ticks_len(), TICK_RING);
        assert_eq!(t.tick_ago(0).unwrap().size, 299);
        assert_eq!(
            t.tick_ago(255).unwrap().size,
            44,
            "the oldest kept is the 45th"
        );
        assert_eq!(t.tick_ago(256), None);
        for i in 0..100 {
            t.on_quote(q(1000, 1001, i, 1));
        }
        assert_eq!(t.quotes_len(), QUOTE_RING);
        assert_eq!(t.quote_ago(0).unwrap().bid_sz, 99);
        assert_eq!(t.quote_ago(63).unwrap().bid_sz, 36);
        assert_eq!(t.quote_ago(64), None);
    }

    #[test]
    fn history_older_than_the_window_is_forgotten() {
        let mut t = Tier1Symbol::new();
        t.on_trade(T0, px(100), 10); // a very low print...
        t.on_trade(T0 + 1000 * S, px(5000), 10); // ...a long time ago by the time of these
        t.on_trade(T0 + 1001 * S, px(5100), 10);
        let f = t.features().unwrap();
        assert_eq!(
            f.impulse_low,
            px(5000),
            "the old low is out of the 256 s window"
        );
    }

    #[test]
    fn a_stale_timestamp_counts_in_the_current_second() {
        let mut t = Tier1Symbol::new();
        t.on_trade(T0 + 10 * S, px(1000), 10);
        t.on_trade(T0 + 3 * S, px(1100), 10); // late
        let f = t.features().unwrap();
        assert_eq!((f.impulse_high, f.impulse_low), (px(1100), px(1000)));
        assert_eq!(f.secs_since_high, 0);
    }

    #[test]
    fn promotion_is_bounded_and_demotion_frees_a_place() {
        let mut t = Tier1::new(10, 2);
        assert_eq!(t.promote(10), Err(PromoteError::Unknown));
        t.promote(3).unwrap();
        assert_eq!(t.promote(3), Err(PromoteError::AlreadyPromoted));
        t.promote(5).unwrap();
        assert_eq!(t.promote(7), Err(PromoteError::Full));
        assert_eq!(t.live(), 2);
        assert!(t.demote(3));
        assert!(!t.demote(3));
        assert_eq!(t.live(), 1);
        t.promote(7).unwrap();
        assert!(t.symbol(3).is_none() && t.symbol(7).is_some());
    }

    fn trade_ev(inst: u32, ts: Nanos, cents: i64) -> Event {
        Event::Trade(Trade {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument: inst,
                provider: ProviderId::Synthetic,
            },
            px: px(cents),
            size: 5,
            flags: TradeFlags::NONE,
        })
    }

    #[test]
    fn events_reach_only_promoted_symbols_and_demotion_drops_history() {
        let mut t = Tier1::new(4, 4);
        t.promote(1).unwrap();
        t.on_event(&trade_ev(0, T0, 1000)); // not promoted
        t.on_event(&trade_ev(1, T0, 1000));
        t.on_event(&Event::Quote(Quote {
            hdr: Header {
                ts_event: T0,
                ts_recv: T0,
                seq: 1,
                instrument: 1,
                provider: ProviderId::Synthetic,
            },
            bid_px: px(999),
            ask_px: px(1001),
            bid_sz: 1,
            ask_sz: 2,
        }));
        t.on_event(&trade_ev(9, T0, 1000)); // outside the id space: ignored
        assert!(t.symbol(0).is_none());
        let s = t.symbol(1).unwrap();
        assert_eq!((s.ticks_len(), s.quotes_len()), (1, 1));
        t.demote(1);
        t.promote(1).unwrap();
        assert_eq!(t.symbol(1).unwrap().ticks_len(), 0, "a fresh start");
    }

    // ---- against the synthetic scenarios ----

    fn run_scenario(kind: PullbackKind) -> Vec<(Nanos, PullbackFeatures)> {
        let cfg = SynthConfig {
            seed: 3,
            session_start: tf_synth::DEFAULT_SESSION_START,
            duration: 160 * S,
            symbols: vec![SymbolSpec {
                symbol: "RUN".into(),
                base_px_cents: 500,
                base_interval_ns: 300_000_000,
                quote_every: 2,
                scenario: Scenario::runner(kind, 10 * S),
                news: Vec::new(),
            }],
        };
        let mut t1 = Tier1::new(1, 1);
        t1.promote(0).unwrap();
        let (mut out, mut next) = (Vec::new(), 0);
        for ev in SynthStream::new(&cfg) {
            t1.on_event(&ev);
            let ts = ev.ts_recv();
            if ts >= next {
                next = ts - ts % S + S;
                if let Some(f) = t1.symbol(0).unwrap().features() {
                    out.push((ts, f));
                }
            }
        }
        out
    }

    fn at(v: &[(Nanos, PullbackFeatures)], secs_after_first: u64) -> PullbackFeatures {
        let start = v[0].0;
        v.iter()
            .find(|(t, _)| *t >= start + secs_after_first * S)
            .unwrap()
            .1
    }

    #[test]
    fn a_dangerous_pullback_looks_different_from_a_healthy_one() {
        let healthy = run_scenario(PullbackKind::Healthy);
        let danger = run_scenario(PullbackKind::Dangerous);
        // The pullback phase runs 40 s after the 30 s impulse; look near its end.
        let (h, d) = (at(&healthy, 60), at(&danger, 60));
        // Measured: healthy gives back ~12% of the impulse on ~6% of its volume rate;
        // dangerous gives back ~43% on ~50%. Thresholds sit well between.
        assert!(h.depth_permille < 300 && h.volume_ratio_permille.unwrap() < 200);
        assert!(d.depth_permille > 350 && d.volume_ratio_permille.unwrap() > 300);
        assert!(
            h.depth_permille < d.depth_permille,
            "healthy {} vs dangerous {}",
            h.depth_permille,
            d.depth_permille
        );
        assert!(
            h.volume_ratio_permille < d.volume_ratio_permille,
            "healthy {:?} vs dangerous {:?}",
            h.volume_ratio_permille,
            d.volume_ratio_permille
        );
    }

    #[test]
    fn features_are_reproducible_and_pinned() {
        let a = run_scenario(PullbackKind::Healthy);
        assert_eq!(a, run_scenario(PullbackKind::Healthy));
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: i128| {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        };
        for (ts, f) in &a {
            mix(*ts as i128);
            mix(f.impulse_low.raw().into());
            mix(f.impulse_high.raw().into());
            mix(f.pullback_low.raw().into());
            mix(f.depth_permille.into());
            mix(f.retrace_now_permille.into());
            mix(f.volume_ratio_permille.map_or(-1, i128::from));
            mix(f.higher_lows.into());
            mix(f.recent_trades_per_sec_x1000.into());
            mix(f.tape_ratio_permille.map_or(-1, i128::from));
            mix(f.tick_speed_x1000.into());
            mix(f.spread.map_or(-1, i128::from));
            mix(f.avg_spread.map_or(-1, i128::from));
            mix(f.bid_support_permille.map_or(-1, i128::from));
            mix(f.avg_bid_support_permille.map_or(-1, i128::from));
        }
        assert_eq!(a.len(), 159);
        assert_eq!(h, 0xb0ff_96c3_c759_e468, "features hash {h:#x}");
    }
}
