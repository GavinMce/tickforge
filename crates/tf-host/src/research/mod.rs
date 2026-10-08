//! Research runs (E19-S13): strategy definitions over stored days, round trips out. See [`run`] for the run, [`trips`] for
//! what a record is and [`cost`] for the costs.

pub(crate) mod cost;
mod keep;
pub mod null;
mod run;
pub mod stats;
mod trips;

pub use cost::{CostError, CostModel};
pub use keep::{EvEvent, Evidence, EvidenceWindow, gather_evidence, gather_evidence_with};
pub use run::{
    CONFIG_FILE, DayFile, DayInput, DayOutcome, DaySource, ResearchError, Results, RunOptions,
    RunReport, Setup, read_config, run, run_day, run_with,
};
pub use trips::{Assembler, COLUMNS, OPEN_AT_END, Trip, Who};

#[cfg(test)]
mod tests;
