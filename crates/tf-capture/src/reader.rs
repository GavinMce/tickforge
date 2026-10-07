//! Reading a capture: listing, checking, and replay through the [`Provider`] trait.

use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use dbn::Record;
use dbn::decode::{DbnDecoder, DecodeRecordRef};
use tf_core::{Event, Nanos, ProviderId};
use tf_databento::{Decoder, InstrumentMap, Item};
use tf_provider::{Capabilities, Poll, Provider, ProviderError, Subscription, WireFormat};
use tf_tape::TapeWriter;

use crate::{Entry, Error, FINAL_EXT, Fnv, PART_EXT, files_with, read_manifest};

/// The segments the manifest lists, in order.
pub fn list(dir: &Path) -> Result<Vec<Entry>, Error> {
    read_manifest(dir)
}

/// Count and checksum a finished segment by reading it.
pub(crate) fn scan(dir: &Path, name: &str, late: bool) -> Result<Entry, Error> {
    let path = dir.join(name);
    let mut sum = Fnv(Fnv::START);
    let mut bytes = 0u64;
    let mut f = File::open(&path)?;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sum.update(&buf[..n]);
        bytes += n as u64;
    }
    f.rewind()?;
    let mut dec = DbnDecoder::with_zstd(f)?;
    let (mut records, mut first, mut last) = (0u64, u64::MAX, 0u64);
    while let Some(rec) = dec.decode_record_ref()? {
        let recv = rec.raw_index_ts();
        records += 1;
        first = first.min(recv);
        last = last.max(recv);
    }
    if records == 0 {
        first = 0;
    }
    Ok(Entry {
        file: name.to_owned(),
        records,
        first_recv: first,
        last_recv: last,
        bytes,
        fnv: sum.0,
        recovered: false,
        late,
    })
}

/// What checking a capture found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub segments: usize,
    pub records: u64,
    pub bytes: u64,
    /// Everything wrong, one line each. Empty means the capture is what the manifest says.
    pub problems: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Check every segment against the manifest: present, the size and checksum recorded, the record
/// count and receive times recorded, and nothing finished or unfinished that the manifest does not
/// account for.
pub fn verify(dir: &Path) -> Result<Report, Error> {
    let listed = read_manifest(dir)?;
    let mut r = Report::default();
    for e in &listed {
        let path = dir.join(&e.file);
        if !path.exists() {
            r.problems.push(format!("{}: listed but missing", e.file));
            continue;
        }
        match scan(dir, &e.file, e.late) {
            Ok(got) => {
                if got.bytes != e.bytes {
                    r.problems.push(format!(
                        "{}: {} bytes, the manifest says {}",
                        e.file, got.bytes, e.bytes
                    ));
                }
                if got.fnv != e.fnv {
                    r.problems.push(format!(
                        "{}: checksum {:016x}, the manifest says {:016x}",
                        e.file, got.fnv, e.fnv
                    ));
                }
                if got.records != e.records {
                    r.problems.push(format!(
                        "{}: {} records, the manifest says {}",
                        e.file, got.records, e.records
                    ));
                }
                if (got.first_recv, got.last_recv) != (e.first_recv, e.last_recv) {
                    r.problems.push(format!(
                        "{}: receive times {}..{}, the manifest says {}..{}",
                        e.file, got.first_recv, got.last_recv, e.first_recv, e.last_recv
                    ));
                }
                r.records += got.records;
                r.bytes += got.bytes;
            }
            Err(err) => r
                .problems
                .push(format!("{}: cannot be read: {err}", e.file)),
        }
        r.segments += 1;
    }
    for name in files_with(dir, FINAL_EXT)? {
        if !listed.iter().any(|e| e.file == name) {
            r.problems
                .push(format!("{name}: finished but not listed in the manifest"));
        }
    }
    for name in files_with(dir, PART_EXT)? {
        r.problems.push(format!(
            "{name}: unfinished (a writer died or is still running)"
        ));
    }
    Ok(r)
}

/// A capture replayed as a [`Provider`]: every segment in order, mapped to events by
/// `tf-databento` with instrument ids that carry from one segment to the next. It delivers as fast
/// as it is polled; to replay at the recorded pace, turn it into a tape ([`to_tape`]) and replay that.
pub struct CaptureReplay {
    caps: Capabilities,
    files: Vec<PathBuf>,
    next_file: usize,
    current: Option<Decoder<'static>>,
    ids: InstrumentMap,
    sub: Subscription,
    keep_zero_size: bool,
    pub notices: u64,
    pub skipped: u64,
    failure: Option<String>,
}

impl CaptureReplay {
    pub fn open(dir: &Path) -> Result<CaptureReplay, Error> {
        if let Some(p) = files_with(dir, PART_EXT)?.first() {
            return Err(Error::Damaged(format!(
                "{p} is unfinished: open the capture with a RawWriter to repair it first"
            )));
        }
        let listed = read_manifest(dir)?;
        for e in &listed {
            if !dir.join(&e.file).exists() {
                return Err(Error::Damaged(format!("{} is listed but missing", e.file)));
            }
        }
        Ok(CaptureReplay::from_files(
            listed.iter().map(|e| dir.join(&e.file)).collect(),
        ))
    }

    /// Replay zstd-compressed DBN files, in the order given, as one stream: ids are numbered in the order first seen
    /// and carry from file to file, exactly as for a capture. A capture's segments are such files; so is each day of
    /// a history store (E19-S07). The files are not checked here (see `verify` for a capture).
    pub fn from_files(files: Vec<PathBuf>) -> CaptureReplay {
        CaptureReplay {
            caps: Capabilities {
                provider: ProviderId::Databento,
                max_connections: 1,
                max_symbols_per_session: None,
                wildcard: true,
                replay_window_secs: None,
                wire: WireFormat::Binary,
            },
            files,
            next_file: 0,
            current: None,
            ids: InstrumentMap::default(),
            sub: Subscription::all(tf_provider::Channels::ALL),
            keep_zero_size: false,
            notices: 0,
            skipped: 0,
            failure: None,
        }
    }

    /// Why the replay ended early, if it did: a file that could not be opened or a record that could not be decoded.
    /// Without it the stream just ends, as it does at the end of the last file.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    pub fn keep_zero_size(mut self, keep: bool) -> Self {
        self.keep_zero_size = keep;
        self
    }

    /// The instrument ids assigned so far.
    pub fn instruments(&self) -> &InstrumentMap {
        match &self.current {
            Some(d) => d.instruments(),
            None => &self.ids,
        }
    }

    fn advance(&mut self) -> Result<bool, ProviderError> {
        if let Some(d) = self.current.take() {
            self.ids = d.into_instruments();
        }
        let Some(path) = self.files.get(self.next_file) else {
            return Ok(false);
        };
        let d = Decoder::from_zstd_file(path)
            .map_err(|e| ProviderError::Source(format!("{}: {e}", path.display())))?
            .with_instruments(std::mem::take(&mut self.ids))
            .keep_zero_size(self.keep_zero_size);
        self.current = Some(d);
        self.next_file += 1;
        Ok(true)
    }
}

impl Provider for CaptureReplay {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn subscribe(&mut self, sub: &Subscription) -> Result<(), ProviderError> {
        self.sub = sub.clone();
        Ok(())
    }

    fn reconnect(&mut self, _resume_from: Option<Nanos>) -> Result<(), ProviderError> {
        Err(ProviderError::Unsupported(
            "a capture replay is a file and does not reconnect",
        ))
    }

    fn disconnect(&mut self) {}

    fn poll(&mut self, out: &mut Vec<Event>, max: usize) -> Poll {
        let before = out.len();
        while out.len() - before < max {
            if self.current.is_none() {
                match self.advance() {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        self.failure = Some(e.to_string());
                        return Poll::End;
                    }
                }
            }
            let Some(d) = self.current.as_mut() else {
                break;
            };
            match d.next_item() {
                Ok(Some(Item::Event(e))) => {
                    if self.sub.matches(&e) {
                        out.push(e);
                    } else {
                        self.skipped += 1;
                    }
                }
                Ok(Some(Item::Notice(_))) => self.notices += 1,
                Ok(Some(_)) => {}
                Ok(None) => match self.advance() {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(e) => {
                        self.failure = Some(e.to_string());
                        break;
                    }
                },
                Err(e) => {
                    self.failure = Some(e.to_string());
                    return Poll::End;
                }
            }
        }
        match out.len() - before {
            0 => Poll::End,
            n => Poll::Events(n),
        }
    }
}

/// Write a capture's events to a normalized tape, in arrival order. Returns how many.
pub fn to_tape<W: Write>(dir: &Path, w: W) -> Result<(u64, W), Error> {
    let mut replay = CaptureReplay::open(dir)?;
    let mut tape = TapeWriter::new(w).map_err(|e| Error::Damaged(format!("tape: {e}")))?;
    let mut out = Vec::with_capacity(4096);
    let mut n = 0u64;
    loop {
        out.clear();
        match replay.poll(&mut out, 4096) {
            Poll::Events(_) => {
                for e in &out {
                    tape.write(e)
                        .map_err(|e| Error::Damaged(format!("tape: {e}")))?;
                    n += 1;
                }
            }
            Poll::End => break,
            Poll::Idle | Poll::Disconnected => {}
        }
    }
    let w = tape
        .finish()
        .map_err(|e| Error::Damaged(format!("tape: {e}")))?;
    let _ = fs::metadata(dir);
    Ok((n, w))
}
