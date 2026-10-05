//! The strategy framework.
//!
//! Strategies are deterministic state machines: events and timers in, intents
//! out. They never talk to a broker; a risk gateway owns all order state.
//!
//! - [`intent`]: what a strategy asks for, and the rules a request must satisfy.
//! - [`lifecycle`]: the gateway's decisions and the state of an order.
//! - [`strategy`]: the [`Strategy`] trait, its [`Ctx`] and the [`Host`] that drives it.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

pub mod intent;
pub mod lifecycle;
pub mod strategy;

pub use intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
pub use lifecycle::{
    Decision, LifecycleError, Order, OrderId, OrderState, OrderUpdate, RejectReason,
};
pub use strategy::{Ctx, Host, MAX_TIMER_FIRES_PER_STEP, Request, Strategy, TimerId};

#[cfg(test)]
mod strategy_tests;
#[cfg(test)]
mod tests;
