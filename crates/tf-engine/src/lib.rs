//! Building blocks for the hot engine.
//!
//! Everything here is integer-only (no floats, no `rand`), takes time from the
//! timestamps it is given (never a clock), and owns no heap memory: the types
//! are `Copy`, which the compiler enforces, so a value of one cannot hold a
//! `Vec` or a `Box`, and updates and queries never allocate. The exception is
//! [`Tier0`], which allocates its arrays once at construction and never again.

mod bars;
mod ewma;
mod tier0;

pub use bars::{Bar, RollingBars, WINDOW_SECS};
pub use ewma::{Ewma, EwmaVar};
pub use tier0::{Level, SymbolState, Tier0};
