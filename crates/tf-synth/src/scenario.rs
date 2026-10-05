//! Scripted price/volume behaviour for one symbol.
//!
//! A scenario is a list of phases. Everything is integer maths (no `ln`, no
//! floats) so the stream is bit-exact on every platform.

use tf_core::{NANOS_PER_SEC, Nanos};

#[derive(Clone, Debug)]
pub struct Phase {
    pub name: &'static str,
    pub duration: Nanos,
    /// Trade-rate multiplier x1000 relative to the symbol's base rate.
    pub rate_permille: u32,
    /// P(step is up | step is non-zero), in permille.
    pub up_permille: u32,
    /// Largest price step per trade, in cents.
    pub max_step_cents: u32,
    /// Trade-size multiplier x1000.
    pub size_permille: u32,
    /// Quoted spread, in cents.
    pub spread_cents: u32,
    /// Trading halt. `TradingHalt` is emitted when the phase starts, nothing
    /// trades or quotes until it ends, and `TradingResume` is emitted when the
    /// next phase starts. The rate, direction, size and spread fields are
    /// ignored.
    pub halted: bool,
    /// On entering the phase the price jumps by this many permille of the
    /// current price (negative is down), with no trades in between. After a
    /// halt this is the reopening gap.
    pub gap_permille: i32,
    /// LULD band half-width as permille of the price when the phase is
    /// entered (0 is no band). A `LuldBand` status is emitted on entry and
    /// trades are held inside the band, so a hot impulse pins at the limit.
    pub luld_permille: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullbackKind {
    /// Shallow retrace on light volume, then continuation to new highs.
    Healthy,
    /// Deep retrace on heavy volume with a wide spread, then a fade.
    Dangerous,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: &'static str,
    pub phases: Vec<Phase>,
    /// Emit `ShortSaleRestriction` the first time the price trades 10% or more
    /// below its starting price (the prior close), as the SSR rule does.
    pub ssr: bool,
}

const FOREVER: Nanos = Nanos::MAX;

/// An unbiased random walk with every special behaviour off. Phases fill in
/// what differs with `..BASE`, so a new knob defaults to off everywhere.
const BASE: Phase = Phase {
    name: "",
    duration: 0,
    rate_permille: 1000,
    up_permille: 500,
    max_step_cents: 1,
    size_permille: 1000,
    spread_cents: 2,
    halted: false,
    gap_permille: 0,
    luld_permille: 0,
};

fn quiet_phase(name: &'static str, duration: Nanos) -> Phase {
    Phase {
        name,
        duration,
        ..BASE
    }
}

impl Scenario {
    /// Unbiased random walk at the base rate, forever.
    pub fn quiet() -> Self {
        Scenario {
            name: "quiet",
            phases: vec![quiet_phase("quiet", FOREVER)],
            ssr: false,
        }
    }

    /// Low-float runner: quiet lead-in, volume+price impulse, pullback, then
    /// either continuation (healthy) or fade (dangerous), then quiet again.
    pub fn runner(kind: PullbackKind, lead_in: Nanos) -> Self {
        let s = NANOS_PER_SEC;
        let impulse = Phase {
            name: "impulse",
            duration: 30 * s,
            rate_permille: 15_000,
            up_permille: 750,
            max_step_cents: 3,
            size_permille: 4000,
            spread_cents: 2,
            ..BASE
        };
        let phases = match kind {
            PullbackKind::Healthy => vec![
                quiet_phase("lead_in", lead_in),
                impulse,
                Phase {
                    name: "pullback",
                    duration: 40 * s,
                    rate_permille: 4000,
                    up_permille: 300,
                    max_step_cents: 2,
                    size_permille: 1000,
                    spread_cents: 2,
                    ..BASE
                },
                Phase {
                    name: "continuation",
                    duration: 40 * s,
                    rate_permille: 12_000,
                    up_permille: 700,
                    max_step_cents: 2,
                    size_permille: 3000,
                    spread_cents: 2,
                    ..BASE
                },
                quiet_phase("after", FOREVER),
            ],
            PullbackKind::Dangerous => vec![
                quiet_phase("lead_in", lead_in),
                impulse,
                Phase {
                    name: "pullback",
                    duration: 40 * s,
                    rate_permille: 10_000,
                    up_permille: 250,
                    max_step_cents: 3,
                    size_permille: 3000,
                    spread_cents: 4,
                    ..BASE
                },
                Phase {
                    name: "fade",
                    duration: 90 * s,
                    rate_permille: 5000,
                    up_permille: 350,
                    max_step_cents: 2,
                    size_permille: 2000,
                    spread_cents: 3,
                    ..BASE
                },
                quiet_phase("after", FOREVER),
            ],
        };
        Scenario {
            name: match kind {
                PullbackKind::Healthy => "runner_healthy_pullback",
                PullbackKind::Dangerous => "runner_dangerous_pullback",
            },
            phases,
            ssr: false,
        }
    }
}
