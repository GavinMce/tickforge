//! Where a ledger's records are kept.
//!
//! A [`LedgerStore`] holds numbered records, one text line each, in order with no gaps. Two
//! implementations live here: [`FileStore`] (a checksummed log file, synced on every append) and
//! [`MemStore`] (for tests and simulation). Any other store, such as the Postgres one planned
//! next, proves itself by passing [`conformance`].
//!
//! The rules every store follows:
//! - Records are numbered from 1 and each append must be exactly one more than the last.
//! - An append is durable when it returns.
//! - After a crash the last record may be damaged (a torn write). `load` removes it and says so.
//!   Damage anywhere else is an error and the store refuses to open: it never skips a record.
//! - Only one writer at a time.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    /// A record before the last one is damaged, or records are out of order.
    Corrupt {
        line: u64,
        why: String,
    },
    /// An append that is not the next number.
    OutOfOrder {
        expected: u64,
        got: u64,
    },
    /// Another writer holds the ledger.
    LockHeld(String),
    /// `append` before `load`.
    NotLoaded,
    /// `append` to a store opened to be read.
    ReadOnly,
    BadPayload(&'static str),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(m) => write!(f, "ledger storage: {m}"),
            StoreError::Corrupt { line, why } => {
                write!(
                    f,
                    "ledger record {line} is damaged and it is not the last one: {why}"
                )
            }
            StoreError::OutOfOrder { expected, got } => {
                write!(
                    f,
                    "ledger append out of order: expected record {expected}, got {got}"
                )
            }
            StoreError::LockHeld(p) => write!(
                f,
                "the ledger is held by another writer ({p}); if that writer is gone, remove the lock file"
            ),
            StoreError::NotLoaded => f.write_str("the ledger was appended to before it was loaded"),
            StoreError::ReadOnly => {
                f.write_str("the ledger was opened to be read and cannot be written")
            }
            StoreError::BadPayload(m) => write!(f, "bad ledger record: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// What `load` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// Records 1..=n, in order.
    pub records: Vec<String>,
    /// If a damaged last record was removed, what it was.
    pub repaired: Option<String>,
}

pub trait LedgerStore {
    /// Every record, repairing a torn tail. Must be called before appending.
    fn load(&mut self) -> Result<Loaded, StoreError>;
    /// Append record number `seq`, which must be one more than the last. Durable on return.
    fn append(&mut self, seq: u64, payload: &str) -> Result<(), StoreError>;
}

fn check_payload(p: &str) -> Result<(), StoreError> {
    if p.is_empty() {
        return Err(StoreError::BadPayload("empty"));
    }
    if p.len() > 4096 {
        return Err(StoreError::BadPayload("longer than 4096 bytes"));
    }
    if p.contains(['\n', '\r']) {
        return Err(StoreError::BadPayload("contains a line break"));
    }
    Ok(())
}

/// FNV-1a, 64 bit: detects a torn or damaged line. Not a defence against tampering.
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn frame(seq: u64, payload: &str) -> String {
    format!(
        "{seq} {:016x} {payload}\n",
        fnv(format!("{seq} {payload}").as_bytes())
    )
}

/// The payload of a framed line if the line is record `seq` and its checksum holds.
fn unframe(line: &str, seq: u64) -> Result<String, String> {
    let mut p = line.splitn(3, ' ');
    let (Some(n), Some(sum), Some(payload)) = (p.next(), p.next(), p.next()) else {
        return Err("not `number checksum record`".to_owned());
    };
    if n.parse::<u64>() != Ok(seq) {
        return Err(format!("expected record {seq}, found `{n}`"));
    }
    if format!("{:016x}", fnv(format!("{n} {payload}").as_bytes())) != sum {
        return Err("checksum does not match".to_owned());
    }
    Ok(payload.to_owned())
}

/// Records in memory. Nothing survives the value; use it for tests and simulation.
#[derive(Debug, Default)]
pub struct MemStore {
    records: Vec<String>,
    loaded: bool,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::default()
    }

    /// A store holding `records`, as after a restart over the same data.
    pub fn from_records(records: Vec<String>) -> MemStore {
        MemStore {
            records,
            loaded: false,
        }
    }

    pub fn records(&self) -> &[String] {
        &self.records
    }
}

impl LedgerStore for MemStore {
    fn load(&mut self) -> Result<Loaded, StoreError> {
        self.loaded = true;
        Ok(Loaded {
            records: self.records.clone(),
            repaired: None,
        })
    }

    fn append(&mut self, seq: u64, payload: &str) -> Result<(), StoreError> {
        if !self.loaded {
            return Err(StoreError::NotLoaded);
        }
        check_payload(payload)?;
        let expected = self.records.len() as u64 + 1;
        if seq != expected {
            return Err(StoreError::OutOfOrder { expected, got: seq });
        }
        self.records.push(payload.to_owned());
        Ok(())
    }
}

/// A log file `ledger.log` in a directory, each line `number checksum record`, synced on every
/// append, with a lock file `ledger.lock` held while the store exists.
#[derive(Debug)]
pub struct FileStore {
    log: PathBuf,
    lock: PathBuf,
    file: Option<File>,
    last: Option<u64>,
}

fn io(path: &Path, e: std::io::Error) -> StoreError {
    StoreError::Io(format!("{}: {e}", path.display()))
}

fn read_log(log: &Path) -> Result<Vec<u8>, StoreError> {
    let mut bytes = Vec::new();
    match File::open(log) {
        Ok(mut f) => {
            f.read_to_end(&mut bytes).map_err(|e| io(log, e))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(log, e)),
    }
    Ok(bytes)
}

/// The good records, how many bytes they take, and what was wrong with the tail if anything.
/// Damage anywhere but the very end is an error.
fn parse_log(bytes: &[u8]) -> Result<(Vec<String>, usize, Option<String>), StoreError> {
    // Lines that end in a newline are complete; what follows the last newline is a torn tail.
    let complete_to = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut lines: Vec<&[u8]> = bytes[..complete_to].split(|b| *b == b'\n').collect();
    lines.pop(); // what follows the final newline: nothing
    let mut records = Vec::new();
    let mut good_to = 0usize;
    let mut repaired = None;
    for (k, line) in lines.iter().enumerate() {
        let seq = k as u64 + 1;
        let parsed = std::str::from_utf8(line)
            .map_err(|_| "not text".to_owned())
            .and_then(|l| unframe(l, seq));
        match parsed {
            Ok(p) => {
                records.push(p);
                good_to += line.len() + 1;
            }
            Err(why) if k + 1 == lines.len() && complete_to == bytes.len() => {
                // The last record, finished writing but damaged: a torn write.
                repaired = Some(format!("record {seq}: {why}"));
            }
            Err(why) => return Err(StoreError::Corrupt { line: seq, why }),
        }
    }
    if complete_to < bytes.len() {
        repaired = Some(format!(
            "{} byte(s) after record {}",
            bytes.len() - complete_to,
            records.len()
        ));
    }
    Ok((records, good_to, repaired))
}

/// A ledger opened to be read while an engine may be writing it: no lock is taken, nothing is
/// repaired or written, and a last record that is still being written is left out (and reported as
/// `repaired`, though nothing was changed). Appending is refused.
pub struct ReadOnlyStore {
    log: PathBuf,
}

impl ReadOnlyStore {
    pub fn open(dir: impl AsRef<Path>) -> ReadOnlyStore {
        ReadOnlyStore {
            log: dir.as_ref().join("ledger.log"),
        }
    }
}

impl LedgerStore for ReadOnlyStore {
    fn load(&mut self) -> Result<Loaded, StoreError> {
        let (records, _, repaired) = parse_log(&read_log(&self.log)?)?;
        Ok(Loaded { records, repaired })
    }

    fn append(&mut self, _seq: u64, _payload: &str) -> Result<(), StoreError> {
        Err(StoreError::ReadOnly)
    }
}

impl FileStore {
    /// Open (creating if needed) the ledger in `dir`, taking its lock.
    pub fn open(dir: impl AsRef<Path>) -> Result<FileStore, StoreError> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        let lock = dir.join("ledger.lock");
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(mut f) => {
                let _ = writeln!(f, "pid {}", std::process::id());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StoreError::LockHeld(lock.display().to_string()));
            }
            Err(e) => return Err(io(&lock, e)),
        }
        Ok(FileStore {
            log: dir.join("ledger.log"),
            lock,
            file: None,
            last: None,
        })
    }

    pub fn log_path(&self) -> &Path {
        &self.log
    }
}

impl Drop for FileStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock);
    }
}

impl LedgerStore for FileStore {
    fn load(&mut self) -> Result<Loaded, StoreError> {
        let bytes = read_log(&self.log)?;
        let (records, good_to, repaired) = parse_log(&bytes)?;
        if good_to < bytes.len() {
            let f = OpenOptions::new()
                .write(true)
                .open(&self.log)
                .map_err(|e| io(&self.log, e))?;
            f.set_len(good_to as u64).map_err(|e| io(&self.log, e))?;
            f.sync_all().map_err(|e| io(&self.log, e))?;
        }
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log)
                .map_err(|e| io(&self.log, e))?,
        );
        self.last = Some(records.len() as u64);
        Ok(Loaded { records, repaired })
    }

    fn append(&mut self, seq: u64, payload: &str) -> Result<(), StoreError> {
        let (Some(file), Some(last)) = (self.file.as_mut(), self.last) else {
            return Err(StoreError::NotLoaded);
        };
        check_payload(payload)?;
        if seq != last + 1 {
            return Err(StoreError::OutOfOrder {
                expected: last + 1,
                got: seq,
            });
        }
        let line = frame(seq, payload);
        file.write_all(line.as_bytes())
            .map_err(|e| io(&self.log, e))?;
        file.sync_data().map_err(|e| io(&self.log, e))?;
        self.last = Some(seq);
        Ok(())
    }
}

/// What the conformance suite needs from a store's test setup.
pub trait Harness {
    type Store: LedgerStore;
    /// A store over empty storage.
    fn fresh(&mut self) -> Self::Store;
    /// A store over the same storage as the last one, after the old one is gone (a restart).
    fn reopen(&mut self) -> Self::Store;
    /// Damage the end of the stored data as a crash mid-write would, if this store can be damaged
    /// that way; return whether it was. The last record is then expected to be dropped on load.
    fn tear_tail(&mut self) -> bool;
}

/// The behaviour every store must have. Panics (as a test) on a violation.
pub fn conformance<H: Harness>(h: &mut H) {
    // An empty store loads empty, and appends must be numbered.
    let mut s = h.fresh();
    assert_eq!(
        s.load().unwrap(),
        Loaded {
            records: vec![],
            repaired: None
        }
    );
    assert_eq!(
        s.append(2, "x"),
        Err(StoreError::OutOfOrder {
            expected: 1,
            got: 2
        })
    );
    assert_eq!(
        s.append(0, "x"),
        Err(StoreError::OutOfOrder {
            expected: 1,
            got: 0
        })
    );
    s.append(1, "first record").unwrap();
    s.append(2, "second record with  odd   spacing").unwrap();
    assert_eq!(
        s.append(2, "again"),
        Err(StoreError::OutOfOrder {
            expected: 3,
            got: 2
        }),
        "no repeats"
    );
    assert_eq!(
        s.append(4, "gap"),
        Err(StoreError::OutOfOrder {
            expected: 3,
            got: 4
        }),
        "no gaps"
    );
    // Payloads that would break the format are refused, and refusing changes nothing.
    for bad in ["", "two\nlines", "carriage\rreturn"] {
        assert!(
            matches!(s.append(3, bad), Err(StoreError::BadPayload(_))),
            "{bad:?}"
        );
    }
    assert!(matches!(
        s.append(3, &"x".repeat(5000)),
        Err(StoreError::BadPayload(_))
    ));
    s.append(3, "third").unwrap();
    drop(s);
    // After a restart everything is there, in order, byte for byte, and numbering carries on.
    let mut s = h.reopen();
    let l = s.load().unwrap();
    assert_eq!(
        l.records,
        ["first record", "second record with  odd   spacing", "third"]
    );
    assert_eq!(l.repaired, None);
    s.append(4, "fourth").unwrap();
    drop(s);
    let mut s = h.reopen();
    assert_eq!(s.load().unwrap().records.len(), 4);
    drop(s);
    // A store that cannot be appended to before it is loaded says so.
    let mut s = h.reopen();
    assert_eq!(s.append(5, "early"), Err(StoreError::NotLoaded));
    drop(s);
    // A torn last record is dropped and reported, and the earlier ones are untouched.
    if h.tear_tail() {
        let mut s = h.reopen();
        let l = s.load().unwrap();
        assert_eq!(
            l.records,
            ["first record", "second record with  odd   spacing", "third"]
        );
        assert!(l.repaired.is_some(), "the repair is reported");
        s.append(4, "fourth, again").unwrap();
        drop(s);
        let mut s = h.reopen();
        let l = s.load().unwrap();
        assert_eq!(l.records.len(), 4);
        assert_eq!(l.records[3], "fourth, again");
        assert_eq!(l.repaired, None, "repaired once, clean after");
    }
}
