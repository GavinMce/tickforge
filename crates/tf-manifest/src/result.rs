use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::manifest::SCHEMA_RESULT;
use crate::{Digest, Error, Manifest};

/// What a run produced, with the manifest that describes it.
///
/// Integers only (no floats), so a result is exactly reproducible and compares
/// with `==`. `event_hash` is the run's event-stream hash: two runs of one
/// manifest must agree on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    manifest: Manifest,
    pub events: u64,
    pub event_hash: u64,
    metrics: BTreeMap<String, i64>,
}

impl RunResult {
    pub fn new(manifest: Manifest, events: u64, event_hash: u64) -> RunResult {
        RunResult {
            manifest,
            events,
            event_hash,
            metrics: BTreeMap::new(),
        }
    }

    /// A named integer outcome (for example `trades` or `pnl_cents`).
    pub fn with_metric(mut self, name: &str, value: i64) -> Result<RunResult, Error> {
        let ok = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !ok {
            return Err(Error::Invalid(format!(
                "metric name {name:?} must be letters, digits or . _ -"
            )));
        }
        if self.metrics.insert(name.to_owned(), value).is_some() {
            return Err(Error::Invalid(format!("metric {name:?} given twice")));
        }
        Ok(self)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn metrics(&self) -> &BTreeMap<String, i64> {
        &self.metrics
    }

    pub fn metric(&self, name: &str) -> Option<i64> {
        self.metrics.get(name).copied()
    }

    /// The key this result is stored under.
    pub fn key(&self) -> Digest {
        self.manifest.hash()
    }

    /// The stored form: a header, the numbers, then the manifest it belongs to.
    pub fn to_text(&self) -> String {
        let mut s = format!("tfrs {SCHEMA_RESULT}\n");
        let _ = writeln!(s, "manifest_hash {}", self.key());
        let _ = writeln!(s, "events {}", self.events);
        let _ = writeln!(s, "event_hash {:016x}", self.event_hash);
        for (k, v) in &self.metrics {
            let _ = writeln!(s, "metric {k} {v}");
        }
        s.push_str("manifest\n");
        s.push_str(&self.manifest.to_text());
        s
    }

    /// Parse the stored form; the manifest hash it states must match the
    /// manifest it contains.
    pub fn parse(text: &str) -> Result<RunResult, Error> {
        let marker = text
            .lines()
            .position(|l| l == "manifest")
            .ok_or(Error::Parse {
                line: 0,
                msg: "missing the manifest section".into(),
            })?;
        let head: Vec<&str> = text.lines().take(marker).collect();
        let manifest_text: String = text
            .lines()
            .skip(marker + 1)
            .map(|l| format!("{l}\n"))
            .collect();
        let manifest = Manifest::parse_at(&manifest_text, marker + 1)?;

        let perr = |line: usize, msg: &str| Error::Parse {
            line: line + 1,
            msg: msg.to_owned(),
        };
        if head.first().copied() != Some(&format!("tfrs {SCHEMA_RESULT}")[..]) {
            return Err(perr(
                0,
                &format!("expected the header `tfrs {SCHEMA_RESULT}`"),
            ));
        }
        let (mut stated, mut events, mut event_hash) = (None, None, None);
        let mut metrics = BTreeMap::new();
        for (i, l) in head.iter().enumerate().skip(1) {
            let f: Vec<&str> = l.split(' ').collect();
            match f.as_slice() {
                ["manifest_hash", h] => stated = Digest::from_hex(h),
                ["events", n] => events = n.parse().ok(),
                ["event_hash", h] if h.len() == 16 => event_hash = u64::from_str_radix(h, 16).ok(),
                ["metric", k, v] => {
                    let v: i64 = v
                        .parse()
                        .map_err(|_| perr(i, "metric value is not an integer"))?;
                    if metrics.insert((*k).to_owned(), v).is_some() {
                        return Err(perr(i, "metric given twice"));
                    }
                }
                _ => return Err(perr(i, "unrecognised or malformed line")),
            }
        }
        if stated != Some(manifest.hash()) {
            return Err(perr(
                0,
                "manifest_hash is missing or does not match the manifest",
            ));
        }
        let mut r = RunResult::new(
            manifest,
            events.ok_or_else(|| perr(0, "missing events"))?,
            event_hash.ok_or_else(|| perr(0, "missing event_hash"))?,
        );
        for (k, v) in metrics {
            r = r.with_metric(&k, v).map_err(|e| perr(0, &e.to_string()))?;
        }
        Ok(r)
    }
}
