//! Building blocks for the hot engine.
//!
//! Everything here is integer-only (no floats, no `rand`), takes time from the
//! timestamps it is given (never a clock), and owns no heap memory: the types
//! are `Copy`, which the compiler enforces, so a value of one cannot hold a
//! `Vec` or a `Box`, and updates and queries never allocate. The exceptions are
//! [`Tier0`], which allocates its arrays once at construction, and [`Tier1`] and
//! [`MtfBars`], which allocate one box when a symbol is promoted or tracked; neither allocates per event.

mod bars;
mod claims;
mod ewma;
mod indicators;
mod mtf;
mod promoter;
mod scanner;
mod session;
mod tier0;
mod tier1;

pub use bars::{Bar, RollingBars, WINDOW_SECS};
pub use claims::{DEFAULT_PRIORITY, Denied, Grant, Owner, OwnerStats};
pub use ewma::{Ewma, EwmaVar};
pub use indicators::{
    Atr, Ema, Extremes, OpeningRange, RateOfChange, RollingVwap, Rsi, Seed, Sma, Vwap,
};
pub use mtf::{BAR_DEPTH, BarClose, MtfBars, MtfConfig, SymbolBars, TfBar, Timeframe, TrackError};
pub use promoter::{Promoter, PromoterConfig, PromoterError, TierError, reason as tier_reason};
pub use scanner::{BASE_SECS, Hit, Scanner, ScannerConfig, ScannerError};
pub use session::{SessionState, Sessions};
pub use tier0::{Level, SymbolState, Tier0};
pub use tier1::{
    BAR_SECS, PromoteError, PullbackFeatures, QUOTE_RING, Quote1, TICK_RING, Tick, Tier1,
    Tier1Symbol,
};
