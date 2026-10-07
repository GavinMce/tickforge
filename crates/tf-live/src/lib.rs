//! The Databento live gateway as a data source (E06-S02).
//!
//! A small synchronous client, so the engine side keeps no async runtime (ADR 0049): [`protocol`] is the
//! gateway's text protocol, [`session`] logs in and starts a stream, [`feed`] is the one thread that
//! decodes it into the ingest queue, and [`provider::LiveProvider`] puts that behind
//! [`tf_provider::Provider`].
//!
//! What this has and has not been tried against: the greeting and challenge of the real gateway were
//! read (`lsg_version=0.9.4`, `cram=...`); a login, subscription and stream need a plan with live
//! access, so those are tested against a fake gateway written from the official client's behaviour.

pub mod feed;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod sha256;
pub mod testing;

#[cfg(test)]
mod tests;

pub use feed::{FeedShared, LiveFeed, RawSink, Returned, SharedSink, State};
pub use protocol::{ApiKey, LiveError, Sub, Symbols};
pub use provider::{LiveProvider, MAX_SESSIONS};
pub use session::{Config, Login, login};
