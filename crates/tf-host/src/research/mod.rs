//! Research runs (E19-S13): strategy definitions over stored days, round trips out. See [`run`] for the run, [`trips`] for
//! what a record is and [`cost`] for the costs.

pub(crate) mod cost;
pub mod null;
mod run;
pub mod stats;
mod trips;

pub use cost::{CostError, CostModel};
pub use run::{
    CONFIG_FILE, DayFile, DayInput, DayOutcome, DaySource, ResearchError, Results, RunReport,
    Setup, read_config, run, run_day,
};
pub use trips::{Assembler, COLUMNS, OPEN_AT_END, Trip, Who};

#[cfg(test)]
mod tests;
