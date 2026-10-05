//! Building blocks for the hot engine.
//!
//! Everything here is integer-only (no floats, no `rand`), takes time from the
//! timestamps it is given (never a clock), and owns no heap memory: the types
//! are `Copy`, which the compiler enforces, so a value of one cannot hold a
//! `Vec` or a `Box`, and updates and queries never allocate.

mod bars;
mod ewma;

pub use bars::{Bar, RollingBars, WINDOW_SECS};
pub use ewma::{Ewma, EwmaVar};
