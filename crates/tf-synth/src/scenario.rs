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

/// A trading phase with the given pace and bias; add `..` fields for the rest.
fn active(
    name: &'static str,
    duration: Nanos,
    rate_permille: u32,
    up_permille: u32,
    max_step_cents: u32,
    size_permille: u32,
) -> Phase {
    Phase {
        name,
        duration,
        rate_permille,
        up_permille,
        max_step_cents,
        size_permille,
        ..BASE
    }
}

fn halt_phase(name: &'static str, duration: Nanos) -> Phase {
    Phase {
        halted: true,
        ..quiet_phase(name, duration)
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

    /// Volatility halt on the way up: a fast spike, a 60 s halt
    /// (`TradingHalt`, `TradingResume`), then a reopen 5% higher that keeps
    /// running.
    pub fn halt_up(lead_in: Nanos) -> Self {
        let s = NANOS_PER_SEC;
        Scenario {
            name: "halt_up",
            phases: vec![
                quiet_phase("lead_in", lead_in),
                active("spike", 15 * s, 10_000, 750, 2, 4000),
                halt_phase("halt", 60 * s),
                Phase {
                    gap_permille: 50,
                    ..active("reopen", 30 * s, 8000, 600, 2, 3000)
                },
                quiet_phase("after", FOREVER),
            ],
            ssr: false,
        }
    }

    /// Limit-up/limit-down: a +/-10% band (`LuldBand`) around the price, a
    /// push that pins at the upper limit, a 60 s limit-state halt, then a
    /// fresh band centred on the reopening price.
    pub fn luld(lead_in: Nanos) -> Self {
        let s = NANOS_PER_SEC;
        let banded = |p: Phase| Phase {
            luld_permille: 100,
            ..p
        };
        Scenario {
            name: "luld",
            phases: vec![
                banded(quiet_phase("lead_in", lead_in)),
                banded(active("push", 40 * s, 12_000, 800, 3, 4000)),
                halt_phase("limit_halt", 60 * s),
                banded(active("reopen", 60 * s, 3000, 500, 2, 2000)),
                quiet_phase("after", FOREVER),
            ],
            ssr: false,
        }
    }

    /// Short-sale restriction: a sell-off that takes the price 10% below its
    /// prior close, which fires `ShortSaleRestriction` on the first print
    /// through the trigger.
    pub fn ssr(lead_in: Nanos) -> Self {
        let s = NANOS_PER_SEC;
        Scenario {
            name: "ssr",
            phases: vec![
                quiet_phase("lead_in", lead_in),
                Phase {
                    spread_cents: 3,
                    ..active("selloff", 40 * s, 8000, 250, 2, 3000)
                },
                quiet_phase("after", FOREVER),
            ],
            ssr: true,
        }
    }

    /// Short squeeze: a ramp, a halt, and a reopen 30% higher with no prints
    /// in between, so a stop placed 10% above the pre-halt price fills far
    /// beyond itself. A blow-off and a fade follow.
    pub fn squeeze(lead_in: Nanos) -> Self {
        let s = NANOS_PER_SEC;
        Scenario {
            name: "squeeze",
            phases: vec![
                quiet_phase("lead_in", lead_in),
                active("ramp", 25 * s, 12_000, 800, 3, 4000),
                halt_phase("halt", 45 * s),
                Phase {
                    gap_permille: 300,
                    ..active("blowoff", 20 * s, 15_000, 700, 4, 5000)
                },
                Phase {
                    spread_cents: 4,
                    ..active("fade", 60 * s, 6000, 250, 3, 3000)
                },
                quiet_phase("after", FOREVER),
            ],
            ssr: false,
        }
    }

    /// Gap and go: the first print opens `gap_permille` above the prior
    /// close (the base price) and the stock keeps going, with a shallow flag
    /// in between. No halts or other statuses.
    pub fn gap_and_go(gap_permille: i32) -> Self {
        let s = NANOS_PER_SEC;
        Scenario {
            name: "gap_and_go",
            phases: vec![
                Phase {
                    gap_permille,
                    ..active("gap_and_go", 60 * s, 10_000, 700, 3, 4000)
                },
                active("flag", 45 * s, 3000, 400, 2, 2000),
                active("continuation", 60 * s, 8000, 650, 3, 3500),
                quiet_phase("after", FOREVER),
            ],
            ssr: false,
        }
    }

    /// A name that spikes `spikes` times, each followed by a pullback and a
    /// 90 s cool-down back to baseline volume: what Strategy 1's re-arm and
    /// cooldown logic has to cope with.
    pub fn multi_spike(spikes: u32, lead_in: Nanos) -> Self {
        let s = NANOS_PER_SEC;
        let mut phases = vec![quiet_phase("lead_in", lead_in)];
        for _ in 0..spikes {
            phases.push(active("spike", 20 * s, 12_000, 750, 3, 4000));
            phases.push(active("pullback", 25 * s, 4000, 350, 2, 1000));
            phases.push(quiet_phase("cooldown", 90 * s));
        }
        phases.push(quiet_phase("after", FOREVER));
        Scenario {
            name: "multi_spike",
            phases,
            ssr: false,
        }
    }
}
