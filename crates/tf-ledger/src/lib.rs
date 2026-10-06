//! The order ledger.
//!
//! Every input to the risk gateway and the order state machine, with the gateway's answer to
//! each decision, is written to an append-only log before it is relied on. After a restart the
//! log is replayed into a fresh gateway and order book, checking each recorded decision against
//! what the replay decides, so the recovered state is the state that was lost, or the restart
//! fails loudly.
//!
//! - [`codec`]: the text form of a record.
//! - [`store`]: where records are kept (`LedgerStore`: a file with a lock, memory, and the
//!   conformance suite any other store must pass).
//! - [`journal`]: the gateway and order book that write the ledger and recover from it.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

pub mod codec;
pub mod journal;
pub mod store;

pub use codec::{CodecError, Input, Record};
pub use journal::{Journal, JournalError, Recovery};
pub use store::{
    FileStore, Harness, LedgerStore, Loaded, MemStore, ReadOnlyStore, StoreError, conformance,
};

#[cfg(test)]
mod tests;
