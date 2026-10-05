//! The strategy framework.
//!
//! Strategies are deterministic state machines: events and timers in, intents
//! out. They never talk to a broker; a risk gateway owns all order state.
//!
//! - [`intent`]: what a strategy asks for, and the rules a request must satisfy.
//! - [`lifecycle`]: the gateway's decisions and the state of an order.
//! - [`momentum`]: Strategy 1, long side.
//! - [`report`]: what a backtest earned, risked and paid, overall and per scenario.
//! - [`sim`]: a simulated broker (fills against recorded quotes, latency, slippage, borrow cost).
//! - [`strategy`]: the [`Strategy`] trait, its [`Ctx`] and the [`Host`] that drives it.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

pub mod intent;
pub mod lifecycle;
pub mod momentum;
pub mod report;
pub mod sim;
pub mod strategy;

pub use intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
pub use lifecycle::{
    Decision, LifecycleError, Order, OrderId, OrderState, OrderUpdate, RejectReason,
};
pub use momentum::{MomentumLong, MomentumParams, MomentumStats};
pub use report::{Report, ReportBuilder, Stats};
pub use sim::{Fill, SimBroker, SimConfig, run_backtest, run_backtest_observed};
pub use strategy::{Ctx, Host, MAX_TIMER_FIRES_PER_STEP, Request, Strategy, TimerId};

#[cfg(test)]
mod momentum_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod sim_tests;
#[cfg(test)]
mod strategy_tests;
#[cfg(test)]
mod tests;
