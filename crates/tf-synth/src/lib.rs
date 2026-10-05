//! Deterministic synthetic provider.
//!
//! Same seed + same config = byte-identical event stream on every platform.
//! It speaks the same [`tf_provider::Provider`] contract as the real vendors,
//! including their limits (connection and symbol caps), and can inject the
//! failures an ingestor has to survive: drops, duplicates, reordering and
//! lost-while-disconnected data.

mod config;
mod provider;
mod rng;
mod scenario;
mod stream;
mod symbol_gen;

pub use config::{DEFAULT_SESSION_START, SymbolSpec, SynthConfig};
pub use provider::{Account, Faults, SynthOptions, SynthProvider};
pub use rng::SplitMix64;
pub use scenario::{Phase, PullbackKind, Scenario};
pub use stream::SynthStream;

#[cfg(test)]
mod tests;
