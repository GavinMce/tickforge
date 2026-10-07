//! The research history store (E19-S07).
//!
//! The only place an edge can be seen is twelve months of history, so history is kept as the provider delivered it:
//! one zstd-compressed DBN file per day and schema, `DIR/<dataset>/<schema>/<YYYY-MM-DD>.dbn.zst`, and one manifest,
//! `DIR/store.tfhist`, that says what each file must be. The network part is a script
//! (`scripts/pull_history.sh`: it asks the metadata service for the cost first and refuses above a cap); everything
//! here reads files.
//!
//! - **The manifest** is text. A line per day: dataset, schema, symbols (`ALL_SYMBOLS` or a comma list), date, bytes,
//!   SHA-256, cost in millionths of a dollar (`-` if unknown), records, and how many symbols the file maps. Lines
//!   `note KEY TEXT` say what the store is not: the borrow flags are not point in time, and whether names that left the
//!   market are present (measured from the files' own symbol mappings, never assumed).
//! - **`index`** (re)builds the lines for a dataset and schema from the files on disk, streaming (a day can be gigabytes).
//! - **`verify`** reads the manifest and every file again and names each one that is missing, short or longer than
//!   listed, altered (the checksum differs) or not listed.
//! - **`replay`** gives the days in date order as a [`CaptureReplay`], the capture's own replay: the same decoder, ids
//!   numbered in the order first seen and carried from day to day, so the host's replay path
//!   (`tf_host::replay_files`) runs on a store as on a capture. It refuses a missing or short file.
//!
//! Nothing here reads a clock or the network.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use dbn::decode::{DbnDecoder, DbnMetadata, DecodeRecordRef};
use tf_capture::CaptureReplay;
use tf_manifest::{Digest, Sha256};

/// The manifest's name, in the store's directory.
pub const MANIFEST: &str = "store.tfhist";
const HEADER: &str = "history store v1";
const EXT: &str = ".dbn.zst";

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Io(String),
    /// The manifest is not one: its line and what is wrong.
    Manifest {
        line: usize,
        why: String,
    },
    Dbn(String),
    /// Nothing to replay, or a file that must not be replayed.
    Unusable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Manifest { line, why } => write!(f, "{MANIFEST} line {line}: {why}"),
            Error::Dbn(e) => write!(f, "{e}"),
            Error::Unusable(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Io(format!("{}: {e}", path.display()))
}

/// One stored day of one schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Day {
    pub dataset: String,
    pub schema: String,
    /// `ALL_SYMBOLS`, or the symbols asked for, comma separated.
    pub symbols: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    pub bytes: u64,
    /// 64 lowercase hex characters.
    pub sha256: String,
    /// What the pull cost, in millionths of a dollar.
    pub cost_micros: Option<u64>,
    pub records: u64,
    /// Symbols the file maps (the instruments of that day, delisted names included).
    pub symbol_count: u64,
}

impl Day {
    /// Where the file is, under the store's directory.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.dataset)
            .join(&self.schema)
            .join(format!("{}{EXT}", self.date))
    }
}

/// The manifest: what is kept, and what it is not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
    /// `(key, text)`, in the order written.
    pub notes: Vec<(String, String)>,
    /// Sorted by dataset, schema, date.
    pub days: Vec<Day>,
}

fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
        && (1..=12).contains(&s[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&s[8..10].parse::<u32>().unwrap_or(0))
}

fn token(s: &str) -> bool {
    !s.is_empty() && !s.contains(char::is_whitespace)
}

impl Store {
    pub fn read(dir: &Path) -> Result<Store, Error> {
        let path = dir.join(MANIFEST);
        let text = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        Store::parse(&text)
    }

    pub fn write(&self, dir: &Path) -> Result<(), Error> {
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        let path = dir.join(MANIFEST);
        fs::write(&path, self.render()).map_err(|e| io(&path, e))
    }

    pub fn render(&self) -> String {
        let mut s = format!("{HEADER}\n");
        for (k, v) in &self.notes {
            let _ = writeln!(s, "note {k} {v}");
        }
        for d in &self.days {
            let cost = d.cost_micros.map_or("-".to_owned(), |c| c.to_string());
            let _ = writeln!(
                s,
                "day {} {} {} {} {} {} {cost} {} {}",
                d.dataset,
                d.schema,
                d.symbols,
                d.date,
                d.bytes,
                d.sha256,
                d.records,
                d.symbol_count
            );
        }
        s
    }

    pub fn parse(text: &str) -> Result<Store, Error> {
        let err = |line: usize, why: &str| Error::Manifest {
            line,
            why: why.to_owned(),
        };
        let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l));
        match lines.next() {
            Some((_, HEADER)) => {}
            _ => return Err(err(1, &format!("the first line must be `{HEADER}`"))),
        }
        let mut store = Store::default();
        for (n, line) in lines {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("note ") {
                let (k, v) = rest
                    .split_once(' ')
                    .filter(|(k, v)| token(k) && !v.trim().is_empty())
                    .ok_or_else(|| err(n, "expected `note KEY TEXT`"))?;
                store.notes.push((k.to_owned(), v.to_owned()));
            } else if let Some(rest) = line.strip_prefix("day ") {
                let f: Vec<&str> = rest.split(' ').collect();
                if f.len() != 9 {
                    return Err(err(n, &format!("{} fields, a day has 9", f.len())));
                }
                if !f[..4].iter().all(|x| token(x)) || !valid_date(f[3]) {
                    return Err(err(
                        n,
                        "a day needs a dataset, schema, symbols and a YYYY-MM-DD date",
                    ));
                }
                let num = |s: &str, what: &str| {
                    s.parse::<u64>()
                        .map_err(|_| err(n, &format!("{what} `{s}` is not a number")))
                };
                if Digest::from_hex(f[5]).is_none() {
                    return Err(err(n, "the checksum must be 64 lowercase hex characters"));
                }
                store.days.push(Day {
                    dataset: f[0].to_owned(),
                    schema: f[1].to_owned(),
                    symbols: f[2].to_owned(),
                    date: f[3].to_owned(),
                    bytes: num(f[4], "bytes")?,
                    sha256: f[5].to_owned(),
                    cost_micros: match f[6] {
                        "-" => None,
                        c => Some(num(c, "cost")?),
                    },
                    records: num(f[7], "records")?,
                    symbol_count: num(f[8], "symbols")?,
                });
            } else {
                return Err(err(n, "expected a `note` or a `day`"));
            }
        }
        let key = |d: &Day| (d.dataset.clone(), d.schema.clone(), d.date.clone());
        if let Some(w) = store.days.windows(2).find(|w| key(&w[0]) >= key(&w[1])) {
            return Err(err(
                0,
                &format!(
                    "days must be in order and listed once: {} {} {}",
                    w[1].dataset, w[1].schema, w[1].date
                ),
            ));
        }
        Ok(store)
    }

    /// The days of a dataset and schema, in date order.
    pub fn of<'a>(
        &'a self,
        dataset: &'a str,
        schema: &'a str,
    ) -> impl Iterator<Item = &'a Day> + 'a {
        self.days
            .iter()
            .filter(move |d| d.dataset == dataset && d.schema == schema)
    }

    /// The datasets and schemas held.
    pub fn kinds(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .days
            .iter()
            .map(|d| (d.dataset.clone(), d.schema.clone()))
            .collect();
        v.dedup();
        v
    }

    /// What the pulls cost, in millionths of a dollar, and how many days have no cost recorded.
    pub fn cost(&self) -> (u64, usize) {
        (
            self.days.iter().filter_map(|d| d.cost_micros).sum(),
            self.days.iter().filter(|d| d.cost_micros.is_none()).count(),
        )
    }
}

/// The size and SHA-256 of a file, read in pieces.
fn hash(path: &Path) -> Result<(u64, String), Error> {
    let mut f = File::open(path).map_err(|e| io(path, e))?;
    let mut sha = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| io(path, e))?;
        if n == 0 {
            break;
        }
        sha.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((bytes, Digest(sha.finish()).hex()))
}

/// The number of records and the symbols a file maps, found by decoding it.
fn decode(path: &Path) -> Result<(u64, BTreeSet<String>), Error> {
    let dbn = |e: dbn::Error| Error::Dbn(format!("{}: {e}", path.display()));
    let mut dec = DbnDecoder::with_zstd(File::open(path).map_err(|e| io(path, e))?).map_err(dbn)?;
    let symbols = dec
        .metadata()
        .mappings
        .iter()
        .map(|m| m.raw_symbol.clone())
        .collect();
    let mut records = 0u64;
    while dec.decode_record_ref().map_err(dbn)?.is_some() {
        records += 1;
    }
    Ok((records, symbols))
}

/// What one file is, found by reading it.
struct Scan {
    bytes: u64,
    sha256: String,
    records: u64,
    symbols: BTreeSet<String>,
}

fn scan(path: &Path) -> Result<Scan, Error> {
    let (bytes, sha256) = hash(path)?;
    let (records, symbols) = decode(path)?;
    Ok(Scan {
        bytes,
        sha256,
        records,
        symbols,
    })
}

/// The dates of the files under `dir/dataset/schema`, ascending.
fn files_of(dir: &Path, dataset: &str, schema: &str) -> Result<Vec<String>, Error> {
    let d = dir.join(dataset).join(schema);
    let mut dates = Vec::new();
    for e in fs::read_dir(&d).map_err(|e| io(&d, e))? {
        let name = e
            .map_err(|e| io(&d, e))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if let Some(date) = name.strip_suffix(EXT) {
            if valid_date(date) {
                dates.push(date.to_owned());
            }
        }
    }
    dates.sort();
    Ok(dates)
}

/// A cost in dollars as text (`0.190103441477`), to millionths of a dollar, rounded up so it is never understated.
pub fn micros_of(dollars: &str) -> Option<u64> {
    let t = dollars.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut micros = whole.parse::<u64>().unwrap_or(0).checked_mul(1_000_000)?;
    let mut f = frac.to_owned();
    f.truncate(6);
    while f.len() < 6 {
        f.push('0');
    }
    micros = micros.checked_add(f.parse::<u64>().ok()?)?;
    // Anything past the sixth place rounds the cost up.
    if frac.len() > 6 && frac[6..].bytes().any(|b| b != b'0') {
        micros += 1;
    }
    Some(micros)
}

/// What indexing found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexReport {
    pub days: usize,
    pub bytes: u64,
    pub records: u64,
    /// Days with no records (a holiday the pull asked for, or a failed pull).
    pub empty: Vec<String>,
}

/// (Re)build the manifest lines for `dataset` and `schema` from the files on disk, keeping the lines of other
/// datasets and schemas, and say in the notes what the store is not. A day's cost is read from `<date>.cost` beside
/// the file (the pull script writes it); `symbols` is what was asked for.
pub fn index(
    dir: &Path,
    dataset: &str,
    schema: &str,
    symbols: &str,
) -> Result<(Store, IndexReport), Error> {
    if !token(dataset) || !token(schema) || !token(symbols) {
        return Err(Error::Unusable(
            "dataset, schema and symbols must be single words (symbols comma separated)".to_owned(),
        ));
    }
    let mut store = Store::read(dir).unwrap_or_default();
    store
        .days
        .retain(|d| !(d.dataset == dataset && d.schema == schema));
    let mut report = IndexReport::default();
    let mut first_last: Vec<(String, BTreeSet<String>)> = Vec::new();
    let dates = files_of(dir, dataset, schema)?;
    for (i, date) in dates.iter().enumerate() {
        let day = Day {
            dataset: dataset.to_owned(),
            schema: schema.to_owned(),
            symbols: symbols.to_owned(),
            date: date.clone(),
            bytes: 0,
            sha256: String::new(),
            cost_micros: None,
            records: 0,
            symbol_count: 0,
        };
        let path = day.path(dir);
        let s = scan(&path)?;
        let cost = fs::read_to_string(path.with_file_name(format!("{date}.cost")))
            .ok()
            .and_then(|t| micros_of(&t));
        if s.records == 0 {
            report.empty.push(date.clone());
        }
        report.days += 1;
        report.bytes += s.bytes;
        report.records += s.records;
        if i == 0 || i + 1 == dates.len() {
            first_last.push((date.clone(), s.symbols.clone()));
        }
        store.days.push(Day {
            bytes: s.bytes,
            sha256: s.sha256,
            cost_micros: cost,
            records: s.records,
            symbol_count: s.symbols.len() as u64,
            ..day
        });
    }
    store
        .days
        .sort_by(|a, b| (&a.dataset, &a.schema, &a.date).cmp(&(&b.dataset, &b.schema, &b.date)));
    // What the store is not.
    store
        .notes
        .retain(|(k, _)| k != "borrow_flags" && k != &format!("survivorship:{dataset}/{schema}"));
    store.notes.push((
        "borrow_flags".to_owned(),
        "not point in time: the easy-to-borrow and shortable flags are today's list applied to every day, so historical \
         shorts look easier than they were"
            .to_owned(),
    ));
    let survivorship = match first_last.as_slice() {
        [(d0, a), (d1, b)] if d0 != d1 => {
            let gone: Vec<&String> = a.difference(b).collect();
            format!(
                "{} symbols on {d0}, {} on {d1}; {} on the first day are not on the last (delisted, renamed or merged), \
                 so names that later left the market are {}",
                a.len(),
                b.len(),
                gone.len(),
                if gone.is_empty() {
                    "NOT shown to be present"
                } else {
                    "present"
                }
            )
        }
        [(d, a)] => format!(
            "only one day ({d}, {} symbols): whether names that later left are present is not known",
            a.len()
        ),
        _ => "no days: not known".to_owned(),
    };
    store
        .notes
        .push((format!("survivorship:{dataset}/{schema}"), survivorship));
    store.write(dir)?;
    Ok((store, report))
}

/// What verifying found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub days: usize,
    pub bytes: u64,
    /// One line each: the file and what is wrong. Empty means every file is what the manifest says.
    pub problems: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Read the manifest and every file again: a file that is missing, shorter or longer than listed, altered (its
/// checksum or record count differs), unreadable, or present and not listed is named.
pub fn verify(dir: &Path) -> Result<Report, Error> {
    let store = Store::read(dir)?;
    let mut r = Report::default();
    for d in &store.days {
        let name = format!("{}/{}/{}{EXT}", d.dataset, d.schema, d.date);
        let path = d.path(dir);
        r.days += 1;
        let Ok(meta) = fs::metadata(&path) else {
            r.problems.push(format!("{name}: listed but missing"));
            continue;
        };
        if meta.len() != d.bytes {
            let what = if meta.len() < d.bytes {
                "short"
            } else {
                "longer than listed"
            };
            r.problems.push(format!(
                "{name}: {what}: {} bytes, the manifest says {}",
                meta.len(),
                d.bytes
            ));
            continue;
        }
        // The checksum first: a file altered inside its compression may not decode at all, and is still just altered.
        let (bytes, sha) = match hash(&path) {
            Ok(h) => h,
            Err(e) => {
                r.problems.push(format!("{name}: cannot be read: {e}"));
                continue;
            }
        };
        r.bytes += bytes;
        if sha != d.sha256 {
            r.problems.push(format!(
                "{name}: altered: checksum {sha}, the manifest says {}",
                d.sha256
            ));
            continue;
        }
        match decode(&path) {
            Ok((records, _)) if records != d.records => r.problems.push(format!(
                "{name}: {records} records, the manifest says {}",
                d.records
            )),
            Ok(_) => {}
            Err(e) => r.problems.push(format!("{name}: cannot be read: {e}")),
        }
    }
    for (dataset, schema) in store.kinds() {
        let listed: BTreeSet<&str> = store
            .of(&dataset, &schema)
            .map(|d| d.date.as_str())
            .collect();
        for date in files_of(dir, &dataset, &schema).unwrap_or_default() {
            if !listed.contains(date.as_str()) {
                r.problems.push(format!(
                    "{dataset}/{schema}/{date}{EXT}: present but not listed"
                ));
            }
        }
    }
    Ok(r)
}

/// The days of `dataset` and `schema` from `from` to `to` inclusive (either may be left open), in date order, as one
/// replay. A file that is missing, or whose size is not the listed size, is refused: run [`verify`] to see why.
pub fn replay(
    dir: &Path,
    dataset: &str,
    schema: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<CaptureReplay, Error> {
    Ok(CaptureReplay::from_files(files(
        dir, dataset, schema, from, to,
    )?))
}

/// The files [`replay`] plays, for a caller (the host's `replay_files`) that takes a list.
pub fn files(
    dir: &Path,
    dataset: &str,
    schema: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<PathBuf>, Error> {
    let store = Store::read(dir)?;
    let mut out = Vec::new();
    for d in store.of(dataset, schema) {
        if from.is_some_and(|f| d.date.as_str() < f) || to.is_some_and(|t| d.date.as_str() > t) {
            continue;
        }
        let path = d.path(dir);
        match fs::metadata(&path) {
            Ok(m) if m.len() == d.bytes => out.push(path),
            Ok(m) => {
                return Err(Error::Unusable(format!(
                    "{}/{}/{}: {} bytes, the manifest says {}: it is not the file that was stored",
                    d.dataset,
                    d.schema,
                    d.date,
                    m.len(),
                    d.bytes
                )));
            }
            Err(_) => {
                return Err(Error::Unusable(format!(
                    "{}/{}/{}: listed but missing",
                    d.dataset, d.schema, d.date
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(Error::Unusable(format!(
            "no days of {dataset} {schema} in that range"
        )));
    }
    Ok(out)
}

/// A plain account of a store: what it holds, per day the symbols, the cost, and what it is not.
pub fn describe(store: &Store) -> String {
    let mut s = String::new();
    for (dataset, schema) in store.kinds() {
        let days: Vec<&Day> = store.of(&dataset, &schema).collect();
        let bytes: u64 = days.iter().map(|d| d.bytes).sum();
        let records: u64 = days.iter().map(|d| d.records).sum();
        let _ = writeln!(
            s,
            "{dataset} {schema}: {} days, {} to {}, {records} records, {} MB",
            days.len(),
            days[0].date,
            days[days.len() - 1].date,
            bytes / 1_000_000
        );
        for d in &days {
            let _ = writeln!(
                s,
                "  {}  {:>9} symbols  {:>12} records  {:>12} bytes{}",
                d.date,
                d.symbol_count,
                d.records,
                d.bytes,
                d.cost_micros.map_or(String::new(), |c| format!(
                    "  ${}.{:06}",
                    c / 1_000_000,
                    c % 1_000_000
                ))
            );
        }
    }
    let (micros, unknown) = store.cost();
    let _ = writeln!(
        s,
        "pulled for ${}.{:06}{}",
        micros / 1_000_000,
        micros % 1_000_000,
        if unknown > 0 {
            format!(" ({unknown} days with no cost recorded)")
        } else {
            String::new()
        }
    );
    for (k, v) in &store.notes {
        let _ = writeln!(s, "note {k}: {v}");
    }
    s
}

#[cfg(test)]
mod tests;
