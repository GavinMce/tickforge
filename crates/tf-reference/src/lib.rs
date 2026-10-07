//! Reference snapshots for the universe filter (E18-S02).
//!
//! Daily bars and a symbology file (both fetched by `scripts/fetch_reference.sh`; nothing here
//! touches the network) become one row per symbol: last close, average dollar and share volume, and
//! average true range. Alpaca's asset list adds tradable, shortable, easy-to-borrow and exchange; an
//! ETF list the person keeps adds `etf`. What no source has (float, short interest) is left out of
//! the snapshot, so a universe that uses it refuses to run (ADR 0041).

mod assets;
mod bars;
mod build;
mod history;

#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod tests;

pub use assets::{ASSET_COLUMNS, Asset, merge_assets, merge_etf_list, parse_assets};
pub use bars::{Bar, ReadError, Symbology, date_days, date_text, read_bars};
pub use build::{BAR_COLUMNS, BuildError, Params, Report, build};
pub use history::{
    ATR_PERIOD, DEFAULT_WICK_CLIP_PERMILLE, EMA_PERIOD, HISTORY_COLUMNS, HistoryError,
    HistoryParams, HistoryReport, HistoryRow, MinuteHistory, merge_history, read_minute_bars,
};
