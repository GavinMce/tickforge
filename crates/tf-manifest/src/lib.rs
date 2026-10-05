//! Run manifests and results.
//!
//! A run is a pure function of its inputs, so it can be described completely and
//! its results cached by that description. A [`Manifest`] records what was run:
//! git sha, kind of run, seed, config, strategy parameters (and their hash) and
//! the range of data. Its SHA-256 over a canonical byte encoding is the key under
//! which a [`RunResult`] is stored in a [`DirStore`], so a rerun of the same
//! manifest is found and skipped.
//!
//! Both have a line-based text form that diffs and reviews well. The canonical
//! encoding is what is hashed; the text is what is stored.
//!
//! Determinism is checked, not assumed: if a rerun of a manifest produces
//! results that differ from the stored ones, [`DirStore::put`] reports
//! [`Error::Mismatch`] instead of overwriting or ignoring it.

use std::fmt;

mod manifest;
mod result;
mod sha256;
mod store;

pub use manifest::{DataRange, Manifest, SCHEMA};
pub use result::RunResult;
pub use store::{DirStore, Put, Source};

#[cfg(test)]
mod tests;

/// A SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// 64 lowercase hex characters.
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Digest> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Digest(out))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", &self.hex()[..12])
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// A field that cannot be written to a manifest or result.
    Invalid(String),
    Parse {
        line: usize,
        msg: String,
    },
    Io(String),
    /// The file stored under a manifest's hash belongs to a different manifest.
    Collision(Digest),
    /// A rerun of a manifest produced different results from the stored ones.
    Mismatch {
        hash: Digest,
        what: String,
    },
    /// The closure that was to produce a result failed.
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "invalid: {m}"),
            Error::Parse { line, msg } => write!(f, "line {line}: {msg}"),
            Error::Io(m) => write!(f, "{m}"),
            Error::Collision(h) => write!(f, "hash {h} is stored for a different manifest"),
            Error::Mismatch { hash, what } => write!(
                f,
                "rerun of {hash} gave different results ({what}): the run is not deterministic"
            ),
            Error::Failed(m) => write!(f, "run failed: {m}"),
        }
    }
}

impl std::error::Error for Error {}
