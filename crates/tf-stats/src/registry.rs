//! The trial registry: every variant ever run, with the fingerprint of its rule and the date it was first run.
//!
//! Its size is what the deflated Sharpe ratio is corrected for, so a variant left out of it would make every other
//! result look better than it is. A variant that is not in it cannot be reported ([`crate::report`]). A fingerprint is
//! entered once and keeps its first date and name however often it is run again.
//!
//! The file is text, one line a variant in the order they were first run, and ends with a checksum of what precedes it so
//! that a hand edit or a cut-short write is noticed:
//!
//! ```text
//! trial registry v1
//! trial<TAB><fingerprint, 16 hex digits><TAB>YYYY-MM-DD<TAB><name>
//! end <checksum, 16 hex digits>
//! ```

use std::fs;
use std::path::Path;

use crate::StatsError;

const HEADER: &str = "trial registry v1";

/// One variant that has been run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trial {
    pub fingerprint: u64,
    pub name: String,
    /// `YYYY-MM-DD`: when it was first run.
    pub first_run: String,
}

/// Whether a variant was new to the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registered {
    New,
    /// Already in it, with its first date and name unchanged.
    Known,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    trials: Vec<Trial>,
}

/// `YYYY-MM-DD` with a month 01 to 12 and a day 01 to 31: a shape that sorts as a date does.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && matches!(s[5..7].parse::<u8>(), Ok(1..=12))
        && matches!(s[8..10].parse::<u8>(), Ok(1..=31))
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    /// How many variants have been run: the number of trials the deflated Sharpe ratio allows for.
    pub fn len(&self) -> usize {
        self.trials.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trials.is_empty()
    }

    pub fn trials(&self) -> &[Trial] {
        &self.trials
    }

    pub fn get(&self, fingerprint: u64) -> Option<&Trial> {
        self.trials.iter().find(|t| t.fingerprint == fingerprint)
    }

    pub fn contains(&self, fingerprint: u64) -> bool {
        self.get(fingerprint).is_some()
    }

    /// Enter a variant that is being run on `date`. A fingerprint already in the registry is left as it is.
    pub fn register(
        &mut self,
        fingerprint: u64,
        name: &str,
        date: &str,
    ) -> Result<Registered, StatsError> {
        if name.is_empty() || name.contains(['\t', '\n', '\r']) {
            return Err(StatsError::Parse(format!(
                "a variant's name cannot be empty or hold a tab or a line break: `{name}`"
            )));
        }
        if !is_date(date) {
            return Err(StatsError::Parse(format!(
                "`{date}` is not a date (YYYY-MM-DD)"
            )));
        }
        if self.contains(fingerprint) {
            return Ok(Registered::Known);
        }
        self.trials.push(Trial {
            fingerprint,
            name: name.to_owned(),
            first_run: date.to_owned(),
        });
        Ok(Registered::New)
    }

    /// The text of the registry.
    pub fn render(&self) -> String {
        let mut body = format!("{HEADER}\n");
        for t in &self.trials {
            body.push_str(&format!(
                "trial\t{:016x}\t{}\t{}\n",
                t.fingerprint, t.first_run, t.name
            ));
        }
        format!("{body}end {:016x}\n", fnv(body.as_bytes()))
    }

    pub fn parse(text: &str) -> Result<Registry, StatsError> {
        let bad = |m: String| StatsError::Parse(format!("trial registry: {m}"));
        let end = text
            .rfind("end ")
            .filter(|&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
            .ok_or_else(|| bad("cut short: no `end`".into()))?;
        let (body, tail) = text.split_at(end);
        if tail.matches('\n').count() != 1 || !tail.ends_with('\n') {
            return Err(bad("text after `end`".into()));
        }
        let want = u64::from_str_radix(tail[4..tail.len() - 1].trim(), 16)
            .map_err(|_| bad("the checksum is not hexadecimal".into()))?;
        if fnv(body.as_bytes()) != want {
            return Err(bad("the checksum does not match: edited or damaged".into()));
        }
        let mut lines = body.lines();
        if lines.next() != Some(HEADER) {
            return Err(bad(format!("not `{HEADER}`")));
        }
        let mut reg = Registry::new();
        for line in lines {
            let w: Vec<&str> = line.split('\t').collect();
            let ["trial", fp, date, name] = w.as_slice() else {
                return Err(bad(format!("a line this version does not know: `{line}`")));
            };
            let fp = u64::from_str_radix(fp, 16)
                .map_err(|_| bad(format!("`{fp}` is not a fingerprint")))?;
            if reg.register(fp, name, date)? == Registered::Known {
                return Err(bad(format!("{fp:016x} is entered twice")));
            }
        }
        Ok(reg)
    }

    /// The registry in `path`; an empty one if there is no file there yet.
    pub fn load(path: &Path) -> Result<Registry, StatsError> {
        match fs::read_to_string(path) {
            Ok(text) => Registry::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::new()),
            Err(e) => Err(StatsError::Io(format!("{}: {e}", path.display()))),
        }
    }

    /// Write the registry to `path` whole: to a `.part` file first and then renamed, so a stop leaves the old file or the
    /// new, never half.
    pub fn save(&self, path: &Path) -> Result<(), StatsError> {
        let io = |e: std::io::Error| StatsError::Io(format!("{}: {e}", path.display()));
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        fs::write(&part, self.render()).map_err(io)?;
        fs::rename(&part, path).map_err(io)
    }
}
