//! Raw capture: the provider's own records, kept exactly as they arrived.
//!
//! A day of live data is only worth having if it can be replayed bit for bit, and the normalized
//! event is a lossy view of what the vendor sent. So the records a feed delivers (Databento's DBN
//! records, not our events) are written to rolling files as they arrive, and any session can then
//! be read back through the same mapping (`tf-databento`) or turned into a normalized tape.
//!
//! The rules the writer keeps, because a process can die on a trading day:
//! - **A file is only ever complete or recoverable.** Records go into `NAME.part`, flushed to a
//!   decodable point whenever [`RawWriter::sync`] is called (the feed loop does it about once a
//!   second) and fsynced. A segment is closed by finishing the compressed stream, fsyncing, and
//!   renaming to `NAME.dbn.zst`; readers never see a `.part`.
//! - **Start-up repairs.** A `.part` left by a dead process is read as far as it decodes and
//!   rewritten as a clean segment marked `recovered`; a finished segment the manifest does not list
//!   (the process died between the rename and the manifest line) is listed.
//! - **The manifest** (`manifest.tfcap`, append-only text) lists each segment with its record count,
//!   first and last receive time, size and checksum, so a capture can be checked later without
//!   trusting the file names.
//! - **Segments roll on the receive-time clock**, not the wall clock: every `segment_secs` of
//!   `ts_recv`, so the same input always makes the same files.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dbn::decode::{DbnDecoder, DecodeRecordRef};
use dbn::encode::EncodeRecordRef;
use dbn::encode::dbn::{MetadataEncoder, RecordEncoder};
use dbn::{MetadataBuilder, Record, RecordRef, SType};

mod reader;

pub use reader::{CaptureReplay, Report, list, to_tape, verify};

#[cfg(test)]
mod tests;

pub const MANIFEST: &str = "manifest.tfcap";
const MANIFEST_HEAD: &str = "tfcap 1";
const FINAL_EXT: &str = "dbn.zst";
const PART_EXT: &str = "part";

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Dbn(dbn::Error),
    /// The manifest, or a line of it, is not what this version writes.
    Manifest(String),
    /// A capture that does not hold together; says what.
    Damaged(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "capture i/o: {e}"),
            Error::Dbn(e) => write!(f, "capture DBN: {e}"),
            Error::Manifest(m) => write!(f, "capture manifest: {m}"),
            Error::Damaged(m) => write!(f, "capture damaged: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<dbn::Error> for Error {
    fn from(e: dbn::Error) -> Error {
        Error::Dbn(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub dir: PathBuf,
    /// The Databento dataset (recorded in each file's metadata).
    pub dataset: String,
    /// Length of a segment, in seconds of receive time.
    pub segment_secs: u64,
    /// zstd level (3 is a good trade of speed for size; the feed is bursty).
    pub level: i32,
}

impl Config {
    pub fn new(dir: impl Into<PathBuf>, dataset: &str) -> Config {
        Config {
            dir: dir.into(),
            dataset: dataset.to_owned(),
            segment_secs: 900,
            level: 3,
        }
    }
}

/// FNV-1a over the bytes written: catches a damaged or truncated file, not tampering.
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    const START: u64 = 0xcbf2_9ce4_8422_2325;

    fn update(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

/// A file that counts and checksums what goes into it.
struct Sink {
    file: File,
    bytes: u64,
    sum: Fnv,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.bytes += n as u64;
        self.sum.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// A segment being written.
struct Open {
    name: String,
    part: PathBuf,
    enc: zstd::Encoder<'static, Sink>,
    sync: File,
    records: u64,
    first_recv: u64,
    last_recv: u64,
    slot: u64,
    recovered: bool,
}

/// What one finished segment holds, as the manifest lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub file: String,
    pub records: u64,
    pub first_recv: u64,
    pub last_recv: u64,
    pub bytes: u64,
    pub fnv: u64,
    pub recovered: bool,
    /// Found finished but not listed (the manifest line was lost) and listed on start-up.
    pub late: bool,
}

impl Entry {
    fn line(&self) -> String {
        format!(
            "segment {} records={} first_recv={} last_recv={} bytes={} fnv={:016x} recovered={} late={}\n",
            self.file,
            self.records,
            self.first_recv,
            self.last_recv,
            self.bytes,
            self.fnv,
            u8::from(self.recovered),
            u8::from(self.late)
        )
    }

    fn parse(line: &str) -> Result<Entry, Error> {
        let bad = || Error::Manifest(format!("not a segment line: {line}"));
        let mut it = line.split(' ');
        if it.next() != Some("segment") {
            return Err(bad());
        }
        let file = it.next().ok_or_else(bad)?.to_owned();
        let mut get = |key: &str, radix: u32| -> Result<u64, Error> {
            let part = it.next().ok_or_else(bad)?;
            let v = part
                .strip_prefix(key)
                .and_then(|v| v.strip_prefix('='))
                .ok_or_else(bad)?;
            u64::from_str_radix(v, radix).map_err(|_| bad())
        };
        Ok(Entry {
            file,
            records: get("records", 10)?,
            first_recv: get("first_recv", 10)?,
            last_recv: get("last_recv", 10)?,
            bytes: get("bytes", 10)?,
            fnv: get("fnv", 16)?,
            recovered: get("recovered", 10)? == 1,
            late: get("late", 10)? == 1,
        })
    }
}

/// What opening a capture directory found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Unfinished files rewritten as clean segments: (file, records kept).
    pub recovered: Vec<(String, u64)>,
    /// Finished segments the manifest did not list, now listed.
    pub listed: Vec<String>,
}

pub struct RawWriter {
    cfg: Config,
    manifest: File,
    seg: Option<Open>,
    next: u64,
    records: u64,
    segments: u64,
}

pub fn read_manifest(dir: &Path) -> Result<Vec<Entry>, Error> {
    let path = dir.join(MANIFEST);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut lines = text.lines();
    match lines.next() {
        Some(MANIFEST_HEAD) | None => {}
        Some(other) => {
            return Err(Error::Manifest(format!(
                "first line is `{other}`, expected `{MANIFEST_HEAD}`"
            )));
        }
    }
    lines.filter(|l| !l.is_empty()).map(Entry::parse).collect()
}

fn segment_name(first_recv: u64, n: u64) -> String {
    format!("raw-{first_recv:020}-{n:06}")
}

/// Segment files in `dir` with the given extension, by name.
fn files_with(dir: &Path, ext: &str) -> Result<Vec<String>, Error> {
    let mut v: Vec<String> = fs::read_dir(dir)?
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("raw-") && n.ends_with(&format!(".{ext}")))
        .collect();
    v.sort();
    Ok(v)
}

fn start_segment(
    cfg: &Config,
    name: &str,
    first_recv: u64,
    recovered: bool,
) -> Result<Open, Error> {
    let part = cfg.dir.join(format!("{name}.{PART_EXT}"));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)?;
    let sync = file.try_clone()?;
    let sink = Sink {
        file,
        bytes: 0,
        sum: Fnv(Fnv::START),
    };
    let mut enc = zstd::Encoder::new(sink, cfg.level)?;
    enc.include_checksum(true)?;
    let metadata = MetadataBuilder::new()
        .dataset(cfg.dataset.clone())
        .schema(None)
        .start(first_recv)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    MetadataEncoder::new(&mut enc).encode(&metadata)?;
    Ok(Open {
        name: name.to_owned(),
        part,
        enc,
        sync,
        records: 0,
        first_recv,
        last_recv: first_recv,
        slot: first_recv / (cfg.segment_secs.max(1) * 1_000_000_000),
        recovered,
    })
}

impl Open {
    fn put(&mut self, rec: &RecordRef<'_>, recv: u64) -> Result<(), Error> {
        RecordEncoder::new(&mut self.enc).encode_record_ref(*rec)?;
        self.records += 1;
        self.last_recv = self.last_recv.max(recv);
        Ok(())
    }

    /// Make everything written so far decodable and durable.
    fn sync(&mut self) -> Result<(), Error> {
        self.enc.flush()?;
        self.sync.sync_data()?;
        Ok(())
    }

    /// Finish the compressed stream, make it durable and give it its final name.
    fn close(self, dir: &Path) -> Result<Entry, Error> {
        let sink = self.enc.finish()?;
        sink.file.sync_all()?;
        let file = format!("{}.{FINAL_EXT}", self.name);
        fs::rename(&self.part, dir.join(&file))?;
        File::open(dir)?.sync_all()?;
        Ok(Entry {
            file,
            records: self.records,
            first_recv: self.first_recv,
            last_recv: self.last_recv,
            bytes: sink.bytes,
            fnv: sink.sum.0,
            recovered: self.recovered,
            late: false,
        })
    }
}

impl RawWriter {
    /// Open a capture directory (creating it), repairing what a dead process left behind.
    pub fn open(cfg: Config) -> Result<(RawWriter, Recovery), Error> {
        fs::create_dir_all(&cfg.dir)?;
        let manifest_path = cfg.dir.join(MANIFEST);
        let fresh = !manifest_path.exists();
        let manifest = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&manifest_path)?;
        let mut w = RawWriter {
            cfg,
            manifest,
            seg: None,
            next: 1,
            records: 0,
            segments: 0,
        };
        if fresh {
            w.manifest
                .write_all(format!("{MANIFEST_HEAD}\n").as_bytes())?;
            w.manifest.sync_all()?;
        }
        let mut recovery = Recovery::default();
        let listed = read_manifest(&w.cfg.dir)?;
        // Finished segments the manifest does not know.
        for name in files_with(&w.cfg.dir, FINAL_EXT)? {
            if !listed.iter().any(|e| e.file == name) {
                let e = reader::scan(&w.cfg.dir, &name, true)?;
                w.append(&e)?;
                recovery.listed.push(name);
            }
        }
        // Unfinished segments: read as far as they decode, rewrite clean, then remove.
        for name in files_with(&w.cfg.dir, PART_EXT)? {
            let part = w.cfg.dir.join(&name);
            let stem = name.trim_end_matches(&format!(".{PART_EXT}")).to_owned();
            let kept = w.recover_part(&part, &stem)?;
            recovery.recovered.push((name, kept));
        }
        let all = read_manifest(&w.cfg.dir)?;
        w.next = all
            .iter()
            .filter_map(|e| {
                e.file
                    .rsplit('-')
                    .next()?
                    .split('.')
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .map_or(1, |n| n + 1);
        Ok((w, recovery))
    }

    fn append(&mut self, e: &Entry) -> Result<(), Error> {
        self.manifest.write_all(e.line().as_bytes())?;
        self.manifest.sync_all()?;
        self.segments += 1;
        Ok(())
    }

    fn recover_part(&mut self, part: &Path, stem: &str) -> Result<u64, Error> {
        let file = File::open(part)?;
        let zr = zstd::Decoder::new(file)?;
        let mut kept = 0u64;
        let mut seg: Option<Open> = None;
        if let Ok(mut dec) = DbnDecoder::new(zr) {
            while let Ok(Some(rec)) = dec.decode_record_ref() {
                let recv = rec.raw_index_ts();
                if seg.is_none() {
                    // The same name with a marker, so the recovered file sorts where the dead one did.
                    let name = format!("{stem}-r");
                    seg = Some(start_segment(&self.cfg, &name, recv, true)?);
                }
                if let Some(s) = &mut seg {
                    s.put(&rec, recv)?;
                    kept += 1;
                }
            }
        }
        if let Some(s) = seg {
            let e = s.close(&self.cfg.dir)?;
            self.append(&e)?;
        }
        fs::remove_file(part)?;
        File::open(&self.cfg.dir)?.sync_all()?;
        Ok(kept)
    }

    /// Append one record exactly as it arrived.
    pub fn write(&mut self, rec: &RecordRef<'_>) -> Result<(), Error> {
        let recv = rec.raw_index_ts();
        let slot = recv / (self.cfg.segment_secs.max(1) * 1_000_000_000);
        if self.seg.as_ref().is_some_and(|s| slot != s.slot) {
            self.close_segment()?;
        }
        if self.seg.is_none() {
            let name = segment_name(recv, self.next);
            self.next += 1;
            self.seg = Some(start_segment(&self.cfg, &name, recv, false)?);
        }
        if let Some(s) = &mut self.seg {
            s.put(rec, recv)?;
        }
        self.records += 1;
        Ok(())
    }

    /// Make what is written decodable and durable. Call about once a second and when the feed is idle.
    pub fn sync(&mut self) -> Result<(), Error> {
        match &mut self.seg {
            Some(s) => s.sync(),
            None => Ok(()),
        }
    }

    fn close_segment(&mut self) -> Result<(), Error> {
        if let Some(s) = self.seg.take() {
            let e = s.close(&self.cfg.dir)?;
            self.append(&e)?;
        }
        Ok(())
    }

    /// Records written and segments closed so far.
    pub fn counts(&self) -> (u64, u64) {
        (self.records, self.segments)
    }

    /// Close the open segment and finish.
    pub fn finish(mut self) -> Result<(u64, u64), Error> {
        self.close_segment()?;
        Ok((self.records, self.segments))
    }
}
