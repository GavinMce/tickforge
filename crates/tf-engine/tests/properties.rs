//! Property tests: random sessions, checked against brute-force references that
//! recompute every answer from the full list of trades.

use tf_core::{NANOS_PER_SEC, Nanos, Px};
use tf_engine::{Ewma, EwmaVar, RollingBars, WINDOW_SECS};
use tf_synth::SplitMix64;

#[derive(Clone, Copy)]
struct T {
    sec: u64,
    px: Px,
    size: u32,
}

/// The obvious implementation: keep every trade, scan them all per query.
#[derive(Default)]
struct Naive {
    trades: Vec<T>,
    cur: Option<u64>,
    start: Option<u64>,
}

impl Naive {
    fn touch(&mut self, sec: u64) -> u64 {
        let eff = self.cur.map_or(sec, |c| c.max(sec));
        self.start.get_or_insert(eff);
        self.cur = Some(eff);
        eff
    }

    fn on_trade(&mut self, ts: Nanos, px: Px, size: u32) {
        let sec = self.touch(ts / NANOS_PER_SEC);
        self.trades.push(T { sec, px, size });
    }

    fn advance_to(&mut self, ts: Nanos) {
        self.touch(ts / NANOS_PER_SEC);
    }

    fn first_sec(&self, secs: usize) -> Option<(u64, u64)> {
        let (cur, k) = (self.cur?, secs.min(WINDOW_SECS) as u64);
        (k > 0).then(|| (cur.saturating_sub(k - 1), cur))
    }

    fn in_window(&self, secs: usize) -> Vec<T> {
        match self.first_sec(secs) {
            Some((a, b)) => self
                .trades
                .iter()
                .filter(|t| (a..=b).contains(&t.sec))
                .copied()
                .collect(),
            None => Vec::new(),
        }
    }

    fn volume(&self, secs: usize) -> u64 {
        self.in_window(secs).iter().map(|t| u64::from(t.size)).sum()
    }

    fn count(&self, secs: usize) -> u64 {
        self.in_window(secs).len() as u64
    }

    fn high(&self, secs: usize) -> Option<Px> {
        self.in_window(secs).iter().map(|t| t.px).max()
    }

    fn low(&self, secs: usize) -> Option<Px> {
        self.in_window(secs).iter().map(|t| t.px).min()
    }

    /// Last trade before the window, else the first trade inside it.
    fn reference(&self, secs: usize) -> Option<Px> {
        let (first, _) = self.first_sec(secs)?;
        self.trades
            .iter()
            .rev()
            .find(|t| t.sec < first)
            .or_else(|| {
                self.in_window(secs)
                    .first()
                    .and_then(|w| self.trades.iter().find(|t| t.sec == w.sec))
            })
            .map(|t| t.px)
    }

    fn change(&self, secs: usize) -> Option<i64> {
        let last = self.trades.last()?.px;
        Some(last.raw() - self.reference(secs)?.raw())
    }

    fn change_permille(&self, secs: usize) -> Option<i64> {
        let last = self.trades.last()?.px;
        let start = self.reference(secs)?.raw();
        (start != 0).then(|| (i128::from(last.raw() - start) * 1000 / i128::from(start)) as i64)
    }

    /// OHLCV of the second `n` ago, if that second is within tracking and the ring.
    fn bar_ago(&self, n: usize) -> Option<BarView> {
        let (cur, start) = (self.cur?, self.start?);
        let sec = cur.checked_sub(n as u64)?;
        if n >= WINDOW_SECS || sec < start {
            return None;
        }
        let ts: Vec<&T> = self.trades.iter().filter(|t| t.sec == sec).collect();
        let ohlc = (!ts.is_empty()).then(|| {
            let px = |f: fn(&&T) -> Px| ts.iter().map(f).collect::<Vec<_>>();
            let p = px(|t| t.px);
            (
                p[0],
                *p.iter().max().unwrap(),
                *p.iter().min().unwrap(),
                *p.last().unwrap(),
            )
        });
        Some((
            sec,
            ohlc,
            ts.iter().map(|t| u64::from(t.size)).sum(),
            ts.len() as u32,
        ))
    }
}

/// (second, OHLC if any trade, volume, trades)
type BarView = (u64, Option<(Px, Px, Px, Px)>, u64, u32);

const KS: [usize; 9] = [0, 1, 2, 5, 17, 59, 60, 61, 500];

fn check(r: &RollingBars, n: &Naive, what: &str) {
    for k in KS {
        assert_eq!(r.volume(k), n.volume(k), "volume({k}) {what}");
        assert_eq!(r.trades(k), n.count(k), "trades({k}) {what}");
        assert_eq!(r.high(k), n.high(k), "high({k}) {what}");
        assert_eq!(r.low(k), n.low(k), "low({k}) {what}");
        assert_eq!(r.price_change(k), n.change(k), "price_change({k}) {what}");
        assert_eq!(
            r.price_change_permille(k),
            n.change_permille(k),
            "permille({k}) {what}"
        );
    }
    assert_eq!(r.now_sec(), n.cur);
    for age in [0, 1, 2, 7, 30, 59, 60, 100] {
        let got = r.bar_ago(age).map(|b| {
            let ohlc = (!b.is_empty()).then_some((b.open, b.high, b.low, b.close));
            (b.sec, ohlc, b.volume, b.trades)
        });
        assert_eq!(got, n.bar_ago(age), "bar_ago({age}) {what}");
    }
}

/// A random session: mostly sub-second gaps, some multi-second, a few gaps
/// longer than the window; occasional stale timestamps and idle time passing.
fn session(seed: u64, steps: usize) {
    let mut rng = SplitMix64::new(seed);
    let mut r = RollingBars::new();
    let mut n = Naive::default();
    check(&r, &n, "empty");

    let mut ts: Nanos = 1_767_623_400 * NANOS_PER_SEC + rng.below(NANOS_PER_SEC);
    let mut px: i64 = 500;
    for step in 0..steps {
        ts += match rng.below(100) {
            0..=69 => rng.below(400_000_000),
            70..=94 => rng.range(400_000_000, 8 * NANOS_PER_SEC),
            95..=97 => rng.range(30 * NANOS_PER_SEC, 61 * NANOS_PER_SEC),
            _ => rng.range(61 * NANOS_PER_SEC, 400 * NANOS_PER_SEC),
        };
        if rng.below(100) < 12 {
            r.advance_to(ts);
            n.advance_to(ts);
        } else {
            px = (px + rng.below(11) as i64 - 5).max(1);
            let size = rng.range(1, 2000) as u32;
            // 3% of the time the timestamp is stale: up to 5 s in the past.
            let t = if rng.below(100) < 3 {
                ts.saturating_sub(rng.below(5 * NANOS_PER_SEC))
            } else {
                ts
            };
            r.on_trade(t, Px::from_cents(px), size);
            n.on_trade(t, Px::from_cents(px), size);
        }
        check(&r, &n, &format!("seed {seed} step {step}"));
    }
}

#[test]
fn rolling_bars_match_the_brute_force_reference() {
    for seed in 0..120 {
        session(seed, 250);
    }
}

#[test]
fn a_long_session_that_wraps_the_ring_many_times() {
    session(9_999, 4000);
}

#[test]
fn a_trade_after_a_huge_gap_still_sees_a_flat_window() {
    let mut r = RollingBars::new();
    r.on_trade(1000 * NANOS_PER_SEC, Px::from_cents(500), 100);
    r.advance_to(1000 * NANOS_PER_SEC + 10_000 * NANOS_PER_SEC);
    assert_eq!((r.volume(60), r.trades(60), r.high(60)), (0, 0, None));
    assert_eq!(
        r.price_change(60),
        Some(0),
        "no trades in the window: flat, not missing"
    );
    r.on_trade(
        1000 * NANOS_PER_SEC + 10_000 * NANOS_PER_SEC,
        Px::from_cents(550),
        10,
    );
    assert_eq!(r.price_change(60), Some(Px::from_cents(50).raw()));
    assert_eq!(r.price_change_permille(60), Some(100));
}

/// Reference EWMA in f64 (tests may use floats; the engine may not).
fn ewma_ref(alpha: f64, xs: &[i64]) -> f64 {
    let mut m = xs[0] as f64;
    for &x in &xs[1..] {
        m += alpha * (x as f64 - m);
    }
    m
}

fn ewvar_ref(alpha: f64, xs: &[i64]) -> (f64, f64) {
    let (mut m, mut v) = (xs[0] as f64, 0.0);
    for &x in &xs[1..] {
        let d = x as f64 - m;
        m += alpha * d;
        v = (1.0 - alpha) * (v + alpha * d * d);
    }
    (m, v)
}

#[test]
fn ewma_tracks_a_floating_point_reference() {
    let mut rng = SplitMix64::new(77);
    for alpha_permille in [1u32, 10, 50, 100, 333, 900, 1000] {
        let alpha = f64::from(alpha_permille) / 1000.0;
        for scale in [100u64, 10_000, 1_000_000_000] {
            let xs: Vec<i64> = (0..2000)
                .map(|_| rng.below(scale) as i64 - (scale / 4) as i64)
                .collect();
            let (mut e, mut v) = (Ewma::new(alpha_permille), EwmaVar::new(alpha_permille));
            for (i, &x) in xs.iter().enumerate() {
                e.update(x);
                v.update(x);
                if i % 97 == 0 || i == xs.len() - 1 {
                    let want = ewma_ref(alpha, &xs[..=i]);
                    let got = e.value().unwrap() as f64;
                    // Rounding to an integer is the only error that matters.
                    assert!(
                        (got - want).abs() <= 1.0,
                        "ewma a={alpha_permille} scale={scale} i={i}: {got} vs {want}"
                    );

                    let (wm, wv) = ewvar_ref(alpha, &xs[..=i]);
                    assert!((v.mean().unwrap() as f64 - wm).abs() <= 1.0);
                    let gv = v.variance().unwrap() as f64;
                    assert!(
                        (gv - wv).abs() <= 1.0 + wv * 1e-6,
                        "var a={alpha_permille} scale={scale} i={i}: {gv} vs {wv}"
                    );
                    let gs = v.std_dev().unwrap() as f64;
                    assert!(
                        (gs - wv.sqrt()).abs() <= 1.0 + wv.sqrt() * 1e-6,
                        "std {gs} vs {}",
                        wv.sqrt()
                    );
                }
            }
        }
    }
}

#[test]
fn state_is_plain_data() {
    assert!(
        std::mem::size_of::<RollingBars>() < 8 * 1024,
        "{} bytes",
        std::mem::size_of::<RollingBars>()
    );
}
