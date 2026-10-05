//! Canonical data model shared by every tickforge component.
//!
//! Rules this crate enforces for the rest of the system:
//! - prices are fixed-point integers, never floats;
//! - events are small, `Copy`, and carry no heap data;
//! - nothing here reads the wall clock except [`clock::SystemClock`], which
//!   exists only so production code can sit behind the [`clock::Clock`] trait.

pub mod clock;
pub mod encode;
pub mod event;
pub mod hash;
pub mod ids;
pub mod px;

pub use clock::{Clock, SimClock, SystemClock};
pub use encode::{DecodeError, Decoder, SCHEMA_VERSION};
pub use event::{
    CancelError, CancelErrorKind, Correction, Event, EventKind, Header, News, ParamChange,
    ParamScope, Quote, Status, StatusKind, TierAction, TierChange, Trade, TradeFlags,
};
pub use hash::Fnv1a64;
pub use ids::{InstrumentId, Nanos, ProviderId, SymbolTable};
pub use px::{PX_SCALE, Px};

pub const NANOS_PER_SEC: Nanos = 1_000_000_000;
