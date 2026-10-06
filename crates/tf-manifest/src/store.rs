use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use crate::{Error, Manifest, RunResult};

/// What [`DirStore::put`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    Written,
    /// An identical result was already stored; nothing changed.
    Deduped,
}

/// Where [`DirStore::get_or_run`] got its result.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Cached,
    Fresh,
}

/// Results as files, one per manifest: `<root>/<first two hex>/<hash>.tfrs`.
/// A file is written whole and renamed into place, so a reader never sees half
/// of one, and two writers of the same (deterministic) result cannot clash.
#[derive(Clone, Debug)]
pub struct DirStore {
    root: PathBuf,
}

fn io(path: &std::path::Path, e: std::io::Error) -> Error {
    Error::Io(format!("{}: {e}", path.display()))
}

impl DirStore {
    pub fn new(root: impl Into<PathBuf>) -> DirStore {
        DirStore { root: root.into() }
    }

    pub fn path_for(&self, m: &Manifest) -> PathBuf {
        let h = m.hash().hex();
        self.root.join(&h[..2]).join(format!("{h}.tfrs"))
    }

    /// The stored result for `m`, if any. A file under `m`'s hash that belongs to
    /// a different manifest is an error, never returned as a hit.
    pub fn get(&self, m: &Manifest) -> Result<Option<RunResult>, Error> {
        let path = self.path_for(m);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&path, e)),
        };
        let stored = RunResult::parse(&text)?;
        if stored.manifest() != m {
            return Err(Error::Collision(m.hash()));
        }
        Ok(Some(stored))
    }

    /// Store a result. If one is already stored for the manifest, it must be
    /// identical (then nothing happens); a different one means the run is not
    /// deterministic, which is reported rather than overwritten.
    pub fn put(&self, r: &RunResult) -> Result<Put, Error> {
        if let Some(old) = self.get(r.manifest())? {
            if &old == r {
                return Ok(Put::Deduped);
            }
            return Err(Error::Mismatch {
                hash: r.key(),
                what: difference(&old, r),
            });
        }
        let path = self.path_for(r.manifest());
        let dir = path.parent().expect("a stored path has a parent");
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, r.to_text()).map_err(|e| io(&tmp, e))?;
        fs::rename(&tmp, &path).map_err(|e| io(&path, e))?;
        Ok(Put::Written)
    }

    /// Every result in the store, in no particular order, and how many files could not be read as
    /// one (a damaged or foreign file is counted and skipped, never fatal). A store that does not
    /// exist is an error.
    pub fn list(&self) -> Result<(Vec<RunResult>, usize), Error> {
        let mut runs = Vec::new();
        let mut skipped = 0;
        for sub in fs::read_dir(&self.root)
            .map_err(|e| io(&self.root, e))?
            .flatten()
        {
            if !sub.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(sub.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().is_none_or(|x| x != "tfrs") {
                    continue;
                }
                match fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| RunResult::parse(&t).ok())
                {
                    Some(r) => runs.push(r),
                    None => skipped += 1,
                }
            }
        }
        Ok((runs, skipped))
    }

    /// The stored result for `m`, or run `f` to produce one, store it and return
    /// it. `f` is not called when there is a stored result.
    pub fn get_or_run<F>(&self, m: &Manifest, f: F) -> Result<(RunResult, Source), Error>
    where
        F: FnOnce() -> Result<RunResult, String>,
    {
        if let Some(r) = self.get(m)? {
            return Ok((r, Source::Cached));
        }
        let r = f().map_err(Error::Failed)?;
        if r.manifest() != m {
            return Err(Error::Failed(
                "the run returned a result for a different manifest".into(),
            ));
        }
        self.put(&r)?;
        Ok((r, Source::Fresh))
    }
}

fn difference(old: &RunResult, new: &RunResult) -> String {
    if old.events != new.events {
        return format!("events {} vs {}", old.events, new.events);
    }
    if old.event_hash != new.event_hash {
        return format!(
            "event hash {:016x} vs {:016x}",
            old.event_hash, new.event_hash
        );
    }
    for (k, v) in old.metrics() {
        if new.metric(k) != Some(*v) {
            return format!("metric {k}: {v} vs {:?}", new.metric(k));
        }
    }
    "metrics differ".to_owned()
}
