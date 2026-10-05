use std::collections::BTreeMap;
use std::fmt::Write as _;

use tf_core::Nanos;

use crate::sha256::sha256;
use crate::{Digest, Error};

/// Version of the manifest layout and its canonical encoding. Changing either
/// changes every manifest hash, so it is a deliberate, versioned act.
pub const SCHEMA: u16 = 1;

/// Version of the result file layout.
pub(crate) const SCHEMA_RESULT: u16 = 1;

/// The data a run read: a named source and the `ts_recv` range of it, inclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRange {
    /// What the data is, for example `tape:<id>` or `synth:universe`.
    pub source: String,
    pub from: Nanos,
    pub to: Nanos,
}

/// A complete, reproducible description of one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    git_sha: String,
    kind: String,
    seed: u64,
    data: DataRange,
    // Sorted maps, so the canonical encoding never depends on insertion order.
    config: BTreeMap<String, String>,
    params: BTreeMap<String, String>,
}

fn token(s: &str, what: &str) -> Result<(), Error> {
    let ok = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/+".contains(&b));
    ok.then_some(()).ok_or_else(|| {
        Error::Invalid(format!(
            "{what} {s:?} must be non-empty letters, digits or . _ - : / +"
        ))
    })
}

fn text(s: &str, what: &str) -> Result<(), Error> {
    let ok = !s.is_empty() && !s.contains(['\n', '\r']) && s.trim() == s;
    ok.then_some(()).ok_or_else(|| {
        Error::Invalid(format!(
            "{what} {s:?} must be non-empty, one line, with no surrounding whitespace"
        ))
    })
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_pairs(out: &mut Vec<u8>, m: &BTreeMap<String, String>) {
    out.extend_from_slice(&(m.len() as u32).to_le_bytes());
    for (k, v) in m {
        put_str(out, k);
        put_str(out, v);
    }
}

impl Manifest {
    pub fn new(git_sha: &str, kind: &str, seed: u64, data: DataRange) -> Result<Manifest, Error> {
        token(git_sha, "git sha")?;
        token(kind, "kind")?;
        token(&data.source, "data source")?;
        if data.from > data.to {
            return Err(Error::Invalid(format!(
                "data range {}..{} ends before it starts",
                data.from, data.to
            )));
        }
        Ok(Manifest {
            git_sha: git_sha.to_owned(),
            kind: kind.to_owned(),
            seed,
            data,
            config: BTreeMap::new(),
            params: BTreeMap::new(),
        })
    }

    fn add(map: &mut BTreeMap<String, String>, what: &str, k: &str, v: &str) -> Result<(), Error> {
        token(k, &format!("{what} key"))?;
        text(v, &format!("{what} value"))?;
        match map.insert(k.to_owned(), v.to_owned()) {
            None => Ok(()),
            Some(_) => Err(Error::Invalid(format!("{what} key {k:?} given twice"))),
        }
    }

    /// Run configuration: how the run is set up (batch size, universe, ...).
    pub fn with_config(mut self, k: &str, v: &str) -> Result<Manifest, Error> {
        Manifest::add(&mut self.config, "config", k, v)?;
        Ok(self)
    }

    /// A strategy parameter. These also feed [`Manifest::params_hash`].
    pub fn with_param(mut self, k: &str, v: &str) -> Result<Manifest, Error> {
        Manifest::add(&mut self.params, "param", k, v)?;
        Ok(self)
    }

    pub fn git_sha(&self) -> &str {
        &self.git_sha
    }
    pub fn kind(&self) -> &str {
        &self.kind
    }
    pub fn seed(&self) -> u64 {
        self.seed
    }
    pub fn data(&self) -> &DataRange {
        &self.data
    }
    pub fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }
    pub fn params(&self) -> &BTreeMap<String, String> {
        &self.params
    }

    /// The bytes that are hashed: fixed field order, little-endian integers,
    /// length-prefixed strings, maps in key order. Independent of platform and
    /// of the order fields were added in.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = b"TFMF".to_vec();
        out.extend_from_slice(&SCHEMA.to_le_bytes());
        put_str(&mut out, &self.git_sha);
        put_str(&mut out, &self.kind);
        out.extend_from_slice(&self.seed.to_le_bytes());
        put_str(&mut out, &self.data.source);
        out.extend_from_slice(&self.data.from.to_le_bytes());
        out.extend_from_slice(&self.data.to.to_le_bytes());
        put_pairs(&mut out, &self.config);
        put_pairs(&mut out, &self.params);
        out
    }

    /// The key a run's results are stored under.
    pub fn hash(&self) -> Digest {
        Digest(sha256(&self.canonical_bytes()))
    }

    /// Hash of the parameters alone, to group runs that used the same ones.
    pub fn params_hash(&self) -> Digest {
        let mut out = b"TFPR".to_vec();
        put_pairs(&mut out, &self.params);
        Digest(sha256(&out))
    }

    pub fn to_text(&self) -> String {
        let mut s = format!("tfmf {SCHEMA}\n");
        let _ = writeln!(s, "git_sha {}", self.git_sha);
        let _ = writeln!(s, "kind {}", self.kind);
        let _ = writeln!(s, "seed {}", self.seed);
        let _ = writeln!(s, "data_source {}", self.data.source);
        let _ = writeln!(s, "data_from {}", self.data.from);
        let _ = writeln!(s, "data_to {}", self.data.to);
        let _ = writeln!(s, "params_hash {}", self.params_hash());
        for (k, v) in &self.config {
            let _ = writeln!(s, "config {k} {v}");
        }
        for (k, v) in &self.params {
            let _ = writeln!(s, "param {k} {v}");
        }
        s
    }

    /// Parse the text form. Strict: unknown lines, repeated fields and a
    /// `params_hash` that does not match the parameters are errors.
    pub fn parse(text: &str) -> Result<Manifest, Error> {
        Manifest::parse_at(text, 0)
    }

    /// As [`Manifest::parse`], with line numbers offset by `base` (for text embedded in a larger file).
    pub(crate) fn parse_at(text: &str, base: usize) -> Result<Manifest, Error> {
        let mut singles: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
        let (mut config, mut params) = (BTreeMap::new(), BTreeMap::new());
        let mut header = false;
        for (i, raw) in text.lines().enumerate() {
            let line = base + i + 1;
            let perr = |msg: String| Error::Parse { line, msg };
            if raw.trim().is_empty() {
                continue;
            }
            if !header {
                if raw != format!("tfmf {SCHEMA}") {
                    return Err(perr(format!("expected the header `tfmf {SCHEMA}`")));
                }
                header = true;
                continue;
            }
            let (word, rest) = raw
                .split_once(' ')
                .ok_or_else(|| perr("malformed line".into()))?;
            match word {
                "config" | "param" => {
                    let (k, v) = rest
                        .split_once(' ')
                        .ok_or_else(|| perr(format!("{word} needs a key and a value")))?;
                    let target = if word == "config" {
                        &mut config
                    } else {
                        &mut params
                    };
                    Manifest::add(target, word, k, v).map_err(|e| perr(e.to_string()))?;
                }
                "git_sha" | "kind" | "seed" | "data_source" | "data_from" | "data_to"
                | "params_hash" => {
                    if singles.insert(word, (line, rest)).is_some() {
                        return Err(perr(format!("{word} given twice")));
                    }
                }
                other => return Err(perr(format!("unknown field {other:?}"))),
            }
        }
        if !header {
            return Err(Error::Parse {
                line: base,
                msg: "empty: expected the header `tfmf 1`".into(),
            });
        }
        let get = |k: &str| -> Result<(usize, &str), Error> {
            singles.get(k).copied().ok_or_else(|| Error::Parse {
                line: base,
                msg: format!("missing {k}"),
            })
        };
        let num = |k: &str| -> Result<u64, Error> {
            let (line, v) = get(k)?;
            v.parse().map_err(|_| Error::Parse {
                line,
                msg: format!("{k} is not a number"),
            })
        };
        let at = |k: &str, e: Error| Error::Parse {
            line: singles.get(k).map_or(base, |s| s.0),
            msg: e.to_string(),
        };
        let data = DataRange {
            source: get("data_source")?.1.to_owned(),
            from: num("data_from")?,
            to: num("data_to")?,
        };
        let mut m = Manifest::new(get("git_sha")?.1, get("kind")?.1, num("seed")?, data)
            .map_err(|e| at("git_sha", e))?;
        m.config = config;
        m.params = params;
        let (line, stated) = get("params_hash")?;
        if Digest::from_hex(stated) != Some(m.params_hash()) {
            return Err(Error::Parse {
                line,
                msg: "params_hash does not match the params".into(),
            });
        }
        Ok(m)
    }
}
