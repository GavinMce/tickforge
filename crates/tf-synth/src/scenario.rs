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
}

const FOREVER: Nanos = Nanos::MAX;

fn quiet_phase(name: &'static str, duration: Nanos) -> Phase {
    Phase {
        name,
        duration,
        rate_permille: 1000,
        up_permille: 500,
        max_step_cents: 1,
        size_permille: 1000,
        spread_cents: 2,
    }
}

impl Scenario {
    /// Unbiased random walk at the base rate, forever.
    pub fn quiet() -> Self {
        Scenario {
            name: "quiet",
            phases: vec![quiet_phase("quiet", FOREVER)],
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
                },
                Phase {
                    name: "continuation",
                    duration: 40 * s,
                    rate_permille: 12_000,
                    up_permille: 700,
                    max_step_cents: 2,
                    size_permille: 3000,
                    spread_cents: 2,
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
                },
                Phase {
                    name: "fade",
                    duration: 90 * s,
                    rate_permille: 5000,
                    up_permille: 350,
                    max_step_cents: 2,
                    size_permille: 2000,
                    spread_cents: 3,
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
        }
    }
}
