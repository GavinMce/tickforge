//! The Alpaca trading adapter (paper).
//!
//! What this crate does, and does not:
//! - [`wire`]: turns an accepted intent into the body of `POST /v2/orders` (limit orders, marketable
//!   -limit collars, OTO and bracket orders), with prices rounded so they are never looser than the
//!   intent says.
//! - [`events`]: reads the `trade_updates` stream and the order objects of REST responses, and says
//!   what each means for the ledger (acknowledged, filled for so many shares at what price, ended).
//! - [`broker`]: the adapter as a `tf_strategy::broker::Broker`, so a host runs against it or the simulator alike.
//! - [`client`]: the conversation with Alpaca over a [`client::Transport`], including the
//!   outcome-unknown case (a request that timed out must be looked up by its client order id, never
//!   sent again blind).
//! - It does **not** open a network connection. The HTTPS and WebSocket transport needs a TLS
//!   dependency and is its own decision; everything here is exercised against fixtures written from
//!   Alpaca's documentation and against a scripted fake transport.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

pub mod broker;
pub mod client;
pub mod drive;
pub mod events;
pub mod json;
pub mod wire;

#[cfg(test)]
mod tests;
