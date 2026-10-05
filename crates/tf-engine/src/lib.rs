//! Building blocks for the hot engine.
//!
//! Everything here is integer-only (no floats, no `rand`), takes time from the
//! timestamps it is given (never a clock), and owns no heap memory: the types
//! are `Copy`, which the compiler enforces, so a value of one cannot hold a
//! `Vec` or a `Box`, and updates and queries never allocate. The exceptions are
//! [`Tier0`], which allocates its arrays once at construction, and [`Tier1`], which
//! allocates one box when a symbol is promoted; neither allocates per event.

mod bars;
mod ewma;
mod tier0;
mod tier1;

pub use bars::{Bar, RollingBars, WINDOW_SECS};
pub use ewma::{Ewma, EwmaVar};
pub use tier0::{Level, SymbolState, Tier0};
pub use tier1::{
    BAR_SECS, PromoteError, PullbackFeatures, QUOTE_RING, Quote1, TICK_RING, Tick, Tier1,
    Tier1Symbol,
};
