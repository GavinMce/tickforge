//! Throughput and tail-latency benchmarks for the run loop.
//!
//! [`run_all`] measures four scenarios (the loop with an empty sink, with Tier 0
//! as the sink, replaying a tape into Tier 0, and the synthetic generator into
//! Tier 0) and returns one [`Row`] each. Rows are JSON lines tagged with the
//! commit, so a file of them is a history; [`compare`] and [`markdown`] show
//! what changed against a baseline.
//!
//! This crate reads the wall clock, by design: it measures real time. Nothing in
//! the engine does.

mod hist;
mod report;
mod scenarios;

pub use hist::Histogram;
pub use report::{
    Delta, P99_FLAG_PERMILLE, Row, THROUGHPUT_FLAG_PERMILLE, compare, from_jsonl, markdown,
    to_jsonl,
};
pub use scenarios::{Env, Workload, run_all};
