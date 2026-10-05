//! The persisted form: a line-based text file that diffs and reviews well.
//!
//! ```text
//! tfsm 1
//! security <id> <listed> <delisted|->
//! symbol   <id> <SYMBOL> <from> <to|->
//! native   <provider> <key> <id> <from> <to|->
//! ```
//!
//! Dates are `YYYYMMDD` and spans are inclusive; `-` is open-ended. `#` starts a
//! comment. Security ids must be `0, 1, 2, ...` in order: the file is
//! append-only and an id is its position, so removing or reordering a security
//! is a parse error rather than a silent renumbering.

use std::fmt::Write as _;
use std::path::Path;

use tf_core::ProviderId;

use crate::{Builder, Date, Error, SecurityMaster};

const PROVIDERS: [ProviderId; 3] = [
    ProviderId::Synthetic,
    ProviderId::Databento,
    ProviderId::Alpaca,
];

fn open(d: Option<Date>) -> String {
    d.map_or_else(|| "-".to_owned(), |d| d.to_string())
}

impl SecurityMaster {
    /// The canonical text form: securities in id order, their symbols by start
    /// date, then native mappings by (provider, key, start). Parsing it and
    /// writing it again gives the same text.
    pub fn to_text(&self) -> String {
        let mut out = String::from("tfsm 1\n");
        for (id, sec) in self.securities.iter().enumerate() {
            let _ = writeln!(out, "security {id} {} {}", sec.listed, open(sec.delisted));
            let mut symbols: Vec<_> = sec.symbols.iter().collect();
            symbols.sort_by_key(|s| s.span.from);
            for s in symbols {
                let _ = writeln!(
                    out,
                    "symbol {id} {} {} {}",
                    s.symbol,
                    s.span.from,
                    open(s.span.to)
                );
            }
        }
        let mut natives: Vec<_> = self.natives.iter().collect();
        natives.sort_by_key(|n| (n.provider.as_u8(), n.key, n.span.from));
        for n in natives {
            let _ = writeln!(
                out,
                "native {} {} {} {} {}",
                n.provider.as_str(),
                n.key,
                n.id,
                n.span.from,
                open(n.span.to)
            );
        }
        out
    }

    pub fn parse(text: &str) -> Result<SecurityMaster, Error> {
        let mut b = Builder::new();
        let mut seen_header = false;
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let content = raw.split('#').next().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let f: Vec<&str> = content.split_whitespace().collect();
            let err = |msg: &str| Error::Parse {
                line,
                msg: msg.to_owned(),
            };
            let at = |e: Error| match e {
                e @ Error::Parse { .. } => e,
                e => Error::Parse {
                    line,
                    msg: e.to_string(),
                },
            };
            if !seen_header {
                if f != ["tfsm", "1"] {
                    return Err(err("expected the header `tfsm 1`"));
                }
                seen_header = true;
                continue;
            }
            match f.as_slice() {
                ["security", id, listed, delisted] => {
                    let id = num(id, line, "security id")?;
                    let want = b.add_security(date(listed, line)?);
                    if id != want {
                        return Err(err(&format!(
                            "security id {id} out of order, expected {want}"
                        )));
                    }
                    if let Some(d) = open_date(delisted, line)? {
                        b.delist(id, d).map_err(at)?;
                    }
                }
                ["symbol", id, symbol, from, to] => {
                    let (id, from, to) = (
                        num(id, line, "security id")?,
                        date(from, line)?,
                        open_date(to, line)?,
                    );
                    b.add_symbol(id, symbol, from, to).map_err(at)?;
                }
                ["native", provider, key, id, from, to] => {
                    let provider = PROVIDERS
                        .into_iter()
                        .find(|p| p.as_str() == *provider)
                        .ok_or_else(|| err("unknown provider"))?;
                    let key = num(key, line, "native key")?;
                    let (id, from, to) = (
                        num(id, line, "security id")?,
                        date(from, line)?,
                        open_date(to, line)?,
                    );
                    b.map_native(provider, key, id, from, to).map_err(at)?;
                }
                _ => return Err(err("unrecognised or malformed line")),
            }
        }
        if !seen_header {
            return Err(Error::Parse {
                line: 0,
                msg: "empty file: expected the header `tfsm 1`".to_owned(),
            });
        }
        b.build()
    }

    /// Read and parse the file at `path`. Call once at startup.
    pub fn load(path: &Path) -> Result<SecurityMaster, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        SecurityMaster::parse(&text)
    }
}

fn num(s: &str, line: usize, what: &str) -> Result<u32, Error> {
    s.parse().map_err(|_| Error::Parse {
        line,
        msg: format!("bad {what} {s:?}"),
    })
}

fn date(s: &str, line: usize) -> Result<Date, Error> {
    Date::new(num(s, line, "date")?).map_err(|e| Error::Parse {
        line,
        msg: e.to_string(),
    })
}

fn open_date(s: &str, line: usize) -> Result<Option<Date>, Error> {
    if s == "-" {
        Ok(None)
    } else {
        date(s, line).map(Some)
    }
}
