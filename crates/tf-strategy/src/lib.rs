//! The strategy framework.
//!
//! Strategies are deterministic state machines: events and timers in, intents
//! out. They never talk to a broker; a risk gateway owns all order state.
//!
//! - [`intent`]: what a strategy asks for, and the rules a request must satisfy.
//! - [`lifecycle`]: the gateway's decisions and the state of an order.

pub mod intent;
pub mod lifecycle;

pub use intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
pub use lifecycle::{
    Decision, LifecycleError, Order, OrderId, OrderState, OrderUpdate, RejectReason,
};

#[cfg(test)]
mod tests;
