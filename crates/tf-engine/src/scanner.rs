//! The scanner: a cheap watch over every symbol for the start of a run.
//!
//! A symbol is a **hit** when, at the same moment, its volume over the last
//! [`BASE_SECS`] seconds is far above that symbol's own normal (a z-score against an
//! exponentially weighted baseline of the same measure), its price has risen at
//! least `min_change_permille` over `spike_secs`, and it passes the universe filters
//! (price range, spread, float). The scanner only reports; promoting a hit to Tier 1
//! and demoting it later is a separate policy with hysteresis (E07-S05).
//!
//! Design points, all tested:
//! - **Per-symbol baseline.** Every 10 s the symbol's 10 s volume is folded into an
//!   [`EwmaVar`], unless it already looks like a spike, so a run does not teach the
//!   baseline that runs are normal. A symbol is not scored until it has
//!   `baseline_samples` samples.
//! - **A floor under the deviation.** The z-score divides by the larger of the
//!   baseline's standard deviation and `std_floor_permille` of its mean, so a very
//!   steady symbol does not turn small wobbles into huge scores.
//! - **At most one evaluation per symbol per second**, on a trade of that symbol, using
//!   Tier 0's windows; no allocation.
//! - **Filters.** Price range and spread (a quote is required); float, from a table the
//!   caller fills (the float source is E14-S01). With `require_float` an unknown float
//!   fails the filter; without it, unknown passes.
//!
//! Integers only; time from events.

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};

use crate::{EwmaVar, Tier0};

/// Seconds of volume that are scored, and the spacing of baseline samples.
pub const BASE_SECS: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScannerConfig {
    /// Window for the price spike, seconds (1 to 60).
    pub spike_secs: u32,
    /// Smallest rise over that window, permille. Deliberately low: the volume gates do the
    /// discriminating, and a stock priced near $20 can run hard on less than 3% in ten seconds
    /// (measured on the synthetic universe: 5 permille misses none and costs no false positives;
    /// 30 misses a quarter of the runners).
    pub min_change_permille: i64,
    /// Smallest z-score, times 1000 (8000 = 8 standard deviations).
    pub min_z_milli: i64,
    /// Smallest 10 s volume in shares, whatever the z-score says.
    pub min_volume: u64,
    /// Baseline samples needed before a symbol is scored.
    pub baseline_samples: u32,
    /// Smoothing of the baseline, permille (50 remembers about 20 samples).
    pub baseline_alpha_permille: u32,
    /// The deviation used is at least this permille of the baseline mean.
    pub std_floor_permille: u32,
    pub min_price: Px,
    pub max_price: Px,
    /// Widest acceptable spread, permille of the ask.
    pub max_spread_permille: u32,
    /// Largest float in shares, if there is a limit.
    pub max_float: Option<u64>,
    /// Treat an unknown float as failing the filter.
    pub require_float: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScannerError(pub &'static str);

impl Default for ScannerConfig {
    fn default() -> Self {
        ScannerConfig {
            spike_secs: 10,
            min_change_permille: 5,
            min_z_milli: 8_000,
            min_volume: 5_000,
            baseline_samples: 6,
            baseline_alpha_permille: 50,
            std_floor_permille: 150,
            min_price: Px::from_cents(100),
            max_price: Px::from_cents(2_000),
            max_spread_permille: 50,
            max_float: Some(20_000_000),
            require_float: false,
        }
    }
}

impl ScannerConfig {
    pub fn validate(&self) -> Result<(), ScannerError> {
        let bad = |m| Err(ScannerError(m));
        if self.spike_secs == 0 || self.spike_secs > 60 {
            return bad("spike_secs must be 1 to 60");
        }
        if self.min_z_milli <= 0 || self.min_change_permille <= 0 {
            return bad("min_z_milli and min_change_permille must be positive");
        }
        if self.baseline_samples == 0 || self.baseline_alpha_permille == 0 {
            return bad("baseline_samples and baseline_alpha_permille must be positive");
        }
        if self.min_price.raw() <= 0 || self.max_price < self.min_price {
            return bad("price filter must be positive and ordered");
        }
        Ok(())
    }
}

/// A symbol that qualifies right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub instrument: InstrumentId,
    pub ts: Nanos,
    /// Volume z-score, times 1000.
    pub z_milli: i64,
    /// Price change over the spike window, permille.
    pub change_permille: i64,
    /// Shares in the last 10 s.
    pub volume: u64,
}

#[derive(Clone, Copy)]
struct Sym {
    base: EwmaVar,
    samples: u32,
    next_sample_sec: u64,
    last_eval_sec: u64,
    seen: bool,
}

pub struct Scanner {
    cfg: ScannerConfig,
    syms: Vec<Sym>,
    floats: Vec<Option<u64>>,
}

impl Scanner {
    pub fn new(cfg: ScannerConfig, id_space: usize) -> Result<Scanner, ScannerError> {
        cfg.validate()?;
        let sym = Sym {
            base: EwmaVar::new(cfg.baseline_alpha_permille),
            samples: 0,
            next_sample_sec: 0,
            last_eval_sec: 0,
            seen: false,
        };
        Ok(Scanner {
            cfg,
            syms: vec![sym; id_space],
            floats: vec![None; id_space],
        })
    }

    pub fn config(&self) -> &ScannerConfig {
        &self.cfg
    }

    /// Record the float (shares outstanding available to trade) of `id`.
    pub fn set_float(&mut self, id: InstrumentId, shares: u64) {
        if let Some(f) = self.floats.get_mut(id as usize) {
            *f = Some(shares);
        }
    }

    /// The z-score (times 1000) of `volume` against `id`'s baseline, if it is scored yet.
    pub fn z_milli(&self, id: InstrumentId, volume: u64) -> Option<i64> {
        let s = self.syms.get(id as usize)?;
        if s.samples < self.cfg.baseline_samples {
            return None;
        }
        let mean = s.base.mean()?;
        let floor =
            (i128::from(mean.max(0)) * i128::from(self.cfg.std_floor_permille) / 1000).max(1);
        let std = i128::from(s.base.std_dev()?).max(floor);
        let z = (i128::from(volume) - i128::from(mean)) * 1000 / std;
        Some(z.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
    }

    fn passes_filters(&self, tier0: &Tier0, id: InstrumentId) -> bool {
        let Some(st) = tier0.symbol(id) else {
            return false;
        };
        let (Some(last), Some((ask, _))) = (st.last_px, st.ask) else {
            return false;
        };
        if last < self.cfg.min_price || last > self.cfg.max_price || ask.raw() <= 0 {
            return false;
        }
        let spread = st.spread().unwrap_or(i64::MAX);
        if i128::from(spread) * 1000
            > i128::from(ask.raw()) * i128::from(self.cfg.max_spread_permille)
        {
            return false;
        }
        match (
            self.cfg.max_float,
            self.floats.get(id as usize).copied().flatten(),
        ) {
            (Some(limit), Some(f)) => f <= limit,
            (Some(_), None) => !self.cfg.require_float,
            (None, _) => true,
        }
    }

    /// Feed an event that Tier 0 has already absorbed. A hit is appended to `out` at
    /// most once per symbol per second.
    pub fn on_event(&mut self, tier0: &Tier0, ev: &Event, out: &mut Vec<Hit>) {
        let Event::Trade(t) = ev else { return };
        let id = t.hdr.instrument;
        let Some(windows) = tier0.windows(id) else {
            return;
        };
        let ts = t.hdr.ts_recv;
        let sec = ts / NANOS_PER_SEC;
        let Some(s) = self.syms.get(id as usize) else {
            return;
        };
        let (mut seen, mut next_sample) = (s.seen, s.next_sample_sec);
        let volume = windows.volume(BASE_SECS);
        if !seen {
            seen = true;
            next_sample = sec + BASE_SECS as u64;
        }
        let z = self.z_milli(id, volume);
        // Fold a baseline sample in every 10 s, unless it already looks like the start of a run.
        let mut fold = false;
        if sec >= next_sample {
            fold = z.is_none_or(|z| z < self.cfg.min_z_milli / 2);
            next_sample = sec + BASE_SECS as u64;
        }
        let evaluate = sec > s.last_eval_sec;
        let s = &mut self.syms[id as usize];
        s.seen = seen;
        s.next_sample_sec = next_sample;
        if fold {
            s.base.update(i64::try_from(volume).unwrap_or(i64::MAX));
            s.samples = s.samples.saturating_add(1);
        }
        if !evaluate {
            return;
        }
        s.last_eval_sec = sec;
        let (Some(z), Some(change)) = (
            z,
            windows.price_change_permille(self.cfg.spike_secs as usize),
        ) else {
            return;
        };
        if z >= self.cfg.min_z_milli
            && change >= self.cfg.min_change_permille
            && volume >= self.cfg.min_volume
            && self.passes_filters(tier0, id)
        {
            out.push(Hit {
                instrument: id,
                ts,
                z_milli: z,
                change_permille: change,
                volume,
            });
        }
    }
}
