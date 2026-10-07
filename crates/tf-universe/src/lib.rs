//! Which symbols a strategy watches, as data (E18-S01).
//!
//! A [`Spec`] is a short text, with a canonical form and a fingerprint, in two layers. The *static*
//! layer is judged once, before the session, from a reference [`Snapshot`] of prior-day data; the
//! result is a [`Selection`] that is stored with the run. The *dynamic* layer ([`Selector`]) keeps the
//! top few of those by a live measurement, with hysteresis. See ADR 0041.

mod dynamic;
mod feature;
mod reference;
mod select;
mod spec;

#[cfg(test)]
mod tests;

pub use dynamic::{Change, HistInfo, LiveView, RefInfo, Selector, Tier0View};
pub use feature::{
    CUMVOL_CHECKPOINTS, HISTORY_FEATURES, Kind, LIVE_FEATURES, LiveFeature, STATIC_FEATURES,
    StaticFeature,
};
pub use reference::{RefRow, Snapshot, SnapshotError, valid_name};
pub use select::{Diff, SelectError, Selection, diff, passes, select};
pub use spec::{Cmp, Dynamic, LiveCond, Operand, Param, Spec, SpecError, StaticCond, Test};
