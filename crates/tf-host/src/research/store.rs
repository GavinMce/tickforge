//! The days of a history store as the days of a research run (E19-S31).
//!
//! A [`StoreSource`] is a [`DaySource`] over one dataset and schema of a [`tf_history`] store, between two dates, with the reference
//! snapshot of each day read from a directory of `<date>.snapshot` files (what the previous session's data gave, see
//! `tf research snapshots`). Nothing here reads the market's data: it names the files and checks them.
//!
//! - **A day's data is identified by its file's checksum and its snapshot,** so a day made from another file, another snapshot or
//!   another subset of symbols is made again.
//! - **A snapshot that is not from before the day is refused:** one built from the day's own bars would let a strategy know the
//!   close in the morning.
//! - **A subset of symbols** restricts the snapshot's rows, so only those names are in any universe; the stored file is read
//!   whole (the host drops what no strategy watches).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tf_universe::Snapshot;

use super::run::{DayInput, DaySource};
use crate::def::fnv;

pub struct StoreSource {
    dir: PathBuf,
    dataset: String,
    schema: String,
    days: Vec<tf_history::Day>,
    snapshots: PathBuf,
    symbols: Option<BTreeSet<String>>,
}

fn snapshot_path(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("{date}.snapshot"))
}

impl StoreSource {
    /// The days of `dataset` and `schema` in `store` from `from` to `to` (inclusive, either may be left open). Every day must have
    /// its snapshot in `snapshots`; the days that do not are named, so a run does not stop at the fortieth.
    pub fn open(
        store: &Path,
        dataset: &str,
        schema: &str,
        from: Option<&str>,
        to: Option<&str>,
        snapshots: &Path,
        symbols: Option<Vec<String>>,
    ) -> Result<StoreSource, String> {
        let manifest = tf_history::Store::read(store).map_err(|e| e.to_string())?;
        let days: Vec<tf_history::Day> = manifest
            .of(dataset, schema)
            .filter(|d| from.is_none_or(|f| d.date.as_str() >= f))
            .filter(|d| to.is_none_or(|t| d.date.as_str() <= t))
            .cloned()
            .collect();
        if days.is_empty() {
            return Err(format!(
                "no days of {dataset} {schema} in that range of the store"
            ));
        }
        let missing: Vec<&str> = days
            .iter()
            .filter(|d| !snapshot_path(snapshots, &d.date).is_file())
            .map(|d| d.date.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "no reference snapshot for {} of {} days (first: {}) in {}: `tf research snapshots` makes them",
                missing.len(),
                days.len(),
                missing
                    .iter()
                    .take(5)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" "),
                snapshots.display()
            ));
        }
        Ok(StoreSource {
            dir: store.to_owned(),
            dataset: dataset.to_owned(),
            schema: schema.to_owned(),
            days,
            snapshots: snapshots.to_owned(),
            symbols: symbols.map(|v| v.into_iter().collect()),
        })
    }

    fn snapshot_text(&self, date: &str) -> Result<String, String> {
        let path = snapshot_path(&self.snapshots, date);
        fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn day(&self, date: &str) -> Result<&tf_history::Day, String> {
        self.days
            .iter()
            .find(|d| d.date == date)
            .ok_or_else(|| format!("{date} is not a stored day in this range"))
    }
}

impl DaySource for StoreSource {
    fn dates(&self) -> Vec<String> {
        self.days.iter().map(|d| d.date.clone()).collect()
    }

    fn data_id(&self, date: &str) -> Result<String, String> {
        let day = self.day(date)?;
        let subset = self.symbols.as_ref().map_or(String::new(), |s| {
            s.iter().cloned().collect::<Vec<_>>().join(",")
        });
        let text = self.snapshot_text(date)?;
        Ok(format!(
            "{}+{:016x}",
            day.sha256,
            fnv(&[text.as_bytes(), b"|", subset.as_bytes()])
        ))
    }

    fn load(&mut self, date: &str) -> Result<DayInput, String> {
        self.day(date)?;
        let files = tf_history::files(
            &self.dir,
            &self.dataset,
            &self.schema,
            Some(date),
            Some(date),
        )
        .map_err(|e| e.to_string())?;
        let mut snapshot =
            Snapshot::parse(&self.snapshot_text(date)?).map_err(|e| e.to_string())?;
        if snapshot.as_of.as_str() >= date {
            return Err(format!(
                "the snapshot for {date} is as of {}: it must be from before the day, or the strategies would know the day",
                snapshot.as_of
            ));
        }
        if let Some(keep) = &self.symbols {
            snapshot.rows.retain(|r| keep.contains(&r.symbol));
            if snapshot.rows.is_empty() {
                return Err(format!(
                    "none of the symbols asked for is in the snapshot for {date}"
                ));
            }
        }
        Ok(DayInput { files, snapshot })
    }
}
