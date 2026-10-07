//! The multi-strategy live host (E18-S05).
//!
//! One engine step per market event serves up to twenty strategies at once:
//!
//! 1. the brokers see the event; what they did (acknowledged, filled, closed) is recorded in the ledger
//!    and told to the strategy that owns the order;
//! 2. Tier 0 and the shared promoter take the event;
//! 3. every running strategy takes it (a periodic review and, if it asks, events for its members), and
//!    each strategy's dynamic universe is re-ranked on schedule;
//! 4. what the strategies asked for goes through the risk gateway (kill switch, loss limits, per
//!    strategy budgets: its **sub-account**), is written to the ledger, and goes to the strategy's
//!    broker: the simulated one, or a paper broker;
//! 5. once a second of event time the loss limits are looked at and strategies being flattened are
//!    pushed on.
//!
//! **One strategy failing does not stop the others.** A strategy that panics, is killed, or crosses a
//! loss limit is stopped alone: its working opens are cancelled and what it holds is closed by the
//! host through the same gateway. The others keep running.
//!
//! **Admission.** A strategy is added only with a [`Certificate`] from [`certify`]: proof that this
//! strategy, as configured, was replayed on a tape through this very machinery without a panic or a
//! ledger refusal. The host refuses one that has not been.
//!
//! **Tier 1 holds** are derived from the ledger: a strategy holds a symbol while it has a position or an
//! order working in it, including an order whose placement got no answer.

mod certify;
mod daily;
mod def;
mod equiv;
mod host;
pub mod library;
mod replay;
pub mod research;
mod runner;

#[cfg(test)]
mod bars_tests;
#[cfg(test)]
mod daily_tests;
#[cfg(test)]
mod exit_tests;
#[cfg(test)]
mod library_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod short_tests;
#[cfg(test)]
mod tests;

pub use certify::{CertifyError, certify};
pub use daily::{CaptureFacts, DailyReport, StrategySection, SystemInputs, SystemSection, money};
pub use def::{Build, Certificate, Route, StrategyDef};
pub use equiv::{Answer, Difference, Log, Rec, Verdict, compare, symbols_fingerprint};
pub use host::{
    AdmitError, BarsConfig, FillNote, GapNote, Host, HostConfig, HostError, REASON_FLATTEN,
    Reference, SlotState, StopReason, StrategyStats,
};
pub use replay::{
    ReplayError, Replayed, Report, replay, replay_capture, replay_events, replay_files, report,
    symbol_table,
};
pub use runner::{DynRunner, runner};
