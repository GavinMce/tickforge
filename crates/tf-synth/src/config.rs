use tf_core::{NANOS_PER_SEC, Nanos, SymbolTable};

use crate::rng::SplitMix64;
use crate::scenario::{PullbackKind, Scenario};

/// 2026-01-05 14:30:00 UTC (09:30 ET), a regular-session open.
pub const DEFAULT_SESSION_START: Nanos = 1_767_623_400_000_000_000;

#[derive(Clone, Debug)]
pub struct SymbolSpec {
    pub symbol: String,
    pub base_px_cents: i64,
    /// Mean gap between trades in a phase whose rate multiplier is x1.
    pub base_interval_ns: Nanos,
    /// Emit a quote after every N trades.
    pub quote_every: u32,
    pub scenario: Scenario,
}

#[derive(Clone, Debug)]
pub struct SynthConfig {
    pub seed: u64,
    pub session_start: Nanos,
    pub duration: Nanos,
    pub symbols: Vec<SymbolSpec>,
}

impl SynthConfig {
    /// A reproducible universe of `n` symbols; roughly `runner_permille`/1000
    /// of them get a runner scenario (half healthy, half dangerous pullback),
    /// the rest random-walk quietly.
    pub fn universe(seed: u64, n: usize, duration: Nanos, runner_permille: u32) -> Self {
        let mut rng = SplitMix64::fork(seed, 0x00C0_FFEE);
        let symbols = (0..n)
            .map(|i| {
                let base_px_cents = rng.range(150, 2_000) as i64;
                let base_interval_ns = rng.range(200, 5_000) * 1_000_000;
                let scenario = if rng.permille(runner_permille) {
                    let kind = if rng.permille(500) {
                        PullbackKind::Healthy
                    } else {
                        PullbackKind::Dangerous
                    };
                    Scenario::runner(kind, rng.range(30, 300) * NANOS_PER_SEC)
                } else {
                    Scenario::quiet()
                };
                SymbolSpec {
                    symbol: format!("SYN{i:04}"),
                    base_px_cents,
                    base_interval_ns,
                    quote_every: 2,
                    scenario,
                }
            })
            .collect();
        SynthConfig {
            seed,
            session_start: DEFAULT_SESSION_START,
            duration,
            symbols,
        }
    }

    /// Symbol table whose ids line up with generator instrument ids.
    pub fn symbol_table(&self) -> SymbolTable {
        let mut t = SymbolTable::new();
        for s in &self.symbols {
            t.intern(&s.symbol);
        }
        t
    }
}
