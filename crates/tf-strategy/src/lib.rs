//! The strategy framework.
//!
//! Strategies are deterministic state machines: events and timers in, intents
//! out. They never talk to a broker; a risk gateway owns all order state.
//!
//! - [`intent`]: what a strategy asks for, and the rules a request must satisfy.
//! - [`lifecycle`]: the gateway's decisions and the state of an order.
//! - [`trend`]: an example strategy on the indicator and bar APIs (EMA cross, VWAP reclaim).
//! - [`momentum`]: Strategy 1, long side.
//! - [`rules`]: entry conditions as data, with the per-decision evidence.
//! - [`report`]: what a backtest earned, risked and paid, overall and per scenario.
//! - [`session_rules`]: what the broker accepts in the extended hours.
//! - [`sim`]: a simulated broker (fills against recorded quotes, latency, slippage, borrow cost).
//! - [`broker`]: what a broker looks like to the system (placement outcomes, events), implemented by [`sim`] and the Alpaca adapter.
//! - [`exits`]: stops, targets and time exits a strategy holds itself, for the extended hours and for simulation.
//! - [`cross`]: strategies over many symbols (periodic review of a member view).
//! - [`strategy`]: the [`Strategy`] trait, its [`Ctx`] and the [`Host`] that drives it.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

pub mod broker;
pub mod cross;
pub mod exits;
pub mod intent;
pub mod lifecycle;
pub mod momentum;
pub mod report;
pub mod rule_diff;
pub mod rules;
pub mod session_rules;
pub mod sim;
pub mod strategy;
pub mod testing;
pub mod trend;

pub use cross::{CrossRunner, CrossStrategy, Market, MemberView, Members};
pub use exits::{ExitBook, ExitPlan, ExitReason, ExitStats, flat_by};
pub use intent::{
    Intent, IntentError, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
};
pub use lifecycle::{
    Decision, LifecycleError, Order, OrderId, OrderState, OrderUpdate, RejectReason,
};
pub use momentum::{
    Decline, DeclineReason, EntryTrace, MomentumLong, MomentumParams, MomentumStats, tunable_specs,
};
pub use report::{Report, ReportBuilder, Stats};
pub use rules::{Evaluation, RuleError, RuleSet};
pub use sim::{
    Borrow, FaultPlan, Fill, SHORT_REFUSED_CODE, SimBroker, SimConfig, run_backtest,
    run_backtest_observed,
};
pub use strategy::{BarsError, Ctx, Host, MAX_TIMER_FIRES_PER_STEP, Request, Strategy, TimerId};
pub use tf_engine::{MtfBars, MtfConfig, SymbolBars, TfBar, Timeframe};
pub use tf_params::{ParamStore, Proposal, Target};
pub use trend::{TrendLong, TrendParams, TrendStats};

#[cfg(test)]
mod broker_tests;
#[cfg(test)]
mod cross_tests;
#[cfg(test)]
mod exits_tests;
#[cfg(test)]
mod momentum_tests;
#[cfg(test)]
mod params_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod rule_diff_tests;
#[cfg(test)]
mod rules_tests;
#[cfg(test)]
mod sim_tests;
#[cfg(test)]
mod strategy_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod trend_tests;
