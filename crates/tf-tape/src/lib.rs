//! Raw tape: compressed, indexed, seekable storage of canonical events in
//! arrival order, so any session can be replayed bit-exactly.
//!
//! ```text
//! header  16 B  "TFTP" format:u16 schema:u16 block_events:u32 reserved:u32
//! block*        comp_len:u32 raw_len:u32 n_events:u32, then a zstd frame (with
//!               checksum) of `n_events` x [len:u16 | encoded event]
//! index         per block, 32 B: first_ts_recv:u64 last_ts_recv:u64 offset:u64
//!               n_events:u32 comp_len:u32
//! trailer 28 B  index_offset:u64 n_blocks:u64 n_events:u64 "TFTE"
//! ```
//!
//! All integers are little-endian. Each event uses the normal event encoding
//! (see `tf_core::encode`) at the header's `schema` version, so a tape written
//! under an older schema still decodes. Events must arrive in non-decreasing
//! `ts_recv` order, which is what makes [`TapeReader::scan`] a binary search over
//! the index plus a scan of one bounded block.
//!
//! A tape is only readable once [`TapeWriter::finish`] has written the index and
//! trailer; a tape whose writer died is reported as missing its footer, not
//! guessed at.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

use tf_core::{DecodeError, Event, SCHEMA_VERSION};

#[cfg(test)]
mod tests;

/// Version of the tape layout itself (not of the event schema inside it).
pub const FORMAT_VERSION: u16 = 1;
/// Events per block unless the writer is told otherwise.
pub const DEFAULT_BLOCK_EVENTS: u32 = 65_536;

const HEADER_MAGIC: [u8; 4] = *b"TFTP";
const TRAILER_MAGIC: [u8; 4] = *b"TFTE";
const HEADER_LEN: u64 = 16;
const TRAILER_LEN: u64 = 28;
const BLOCK_HEADER_LEN: u64 = 12;
const INDEX_ENTRY_LEN: u64 = 32;
/// Events are at most 64 bytes in memory; their encoding is smaller. Used only to
/// bound allocations when reading a damaged file.
const MAX_ENCODED_EVENT: u64 = 64;
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Not a tape: the first bytes are not the header magic.
    BadMagic,
    UnsupportedFormat(u16),
    /// The tape's events are of a schema this build does not know.
    UnsupportedSchema(u16),
    Corrupt(&'static str),
    Decode(DecodeError),
    /// An event arrived with an earlier `ts_recv` than the one before it.
    OutOfOrder {
        prev: u64,
        got: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::BadMagic => write!(f, "not a tape (bad magic)"),
            Error::UnsupportedFormat(v) => write!(f, "unsupported tape format version {v}"),
            Error::UnsupportedSchema(v) => write!(f, "unsupported event schema version {v}"),
            Error::Corrupt(m) => write!(f, "corrupt tape: {m}"),
            Error::Decode(e) => write!(f, "bad event on tape: {e}"),
            Error::OutOfOrder { prev, got } => {
                write!(f, "ts_recv went backwards: {got} after {prev}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Self {
        Error::Decode(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockEntry {
    first_recv: u64,
    last_recv: u64,
    offset: u64,
    n_events: u32,
    comp_len: u32,
}

/// Writes a tape to any [`Write`]. Call [`TapeWriter::finish`] to complete it.
pub struct TapeWriter<W: Write> {
    w: W,
    block_events: u32,
    compressor: zstd::bulk::Compressor<'static>,
    raw: Vec<u8>,
    scratch: Vec<u8>,
    in_block: u32,
    block_first_recv: u64,
    last_recv: u64,
    total: u64,
    pos: u64,
    index: Vec<BlockEntry>,
}

impl<W: Write> TapeWriter<W> {
    pub fn new(w: W) -> Result<Self, Error> {
        Self::with_block_events(w, DEFAULT_BLOCK_EVENTS)
    }

    /// `block_events` bounds how many events a seek may have to scan past.
    pub fn with_block_events(mut w: W, block_events: u32) -> Result<Self, Error> {
        let block_events = block_events.max(1);
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(&HEADER_MAGIC);
        header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        header.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        header.extend_from_slice(&block_events.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        w.write_all(&header)?;

        let mut compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL)?;
        // The frame checksum is how a flipped bit is caught on read.
        compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
        Ok(TapeWriter {
            w,
            block_events,
            compressor,
            raw: Vec::new(),
            scratch: Vec::with_capacity(64),
            in_block: 0,
            block_first_recv: 0,
            last_recv: 0,
            total: 0,
            pos: HEADER_LEN,
            index: Vec::new(),
        })
    }

    pub fn write(&mut self, ev: &Event) -> Result<(), Error> {
        let ts = ev.ts_recv();
        if self.total > 0 && ts < self.last_recv {
            return Err(Error::OutOfOrder {
                prev: self.last_recv,
                got: ts,
            });
        }
        if self.in_block == 0 {
            self.block_first_recv = ts;
        }
        self.scratch.clear();
        ev.encode(&mut self.scratch);
        let len = u16::try_from(self.scratch.len()).expect("an event encodes to well under 64 KiB");
        self.raw.extend_from_slice(&len.to_le_bytes());
        self.raw.extend_from_slice(&self.scratch);
        self.in_block += 1;
        self.total += 1;
        self.last_recv = ts;
        if self.in_block >= self.block_events {
            self.flush_block()?;
        }
        Ok(())
    }

    /// Events written so far.
    pub fn events(&self) -> u64 {
        self.total
    }

    fn flush_block(&mut self) -> Result<(), Error> {
        if self.in_block == 0 {
            return Ok(());
        }
        let comp = self.compressor.compress(&self.raw)?;
        let comp_len = u32::try_from(comp.len()).map_err(|_| Error::Corrupt("block too large"))?;
        let raw_len =
            u32::try_from(self.raw.len()).map_err(|_| Error::Corrupt("block too large"))?;
        self.w.write_all(&comp_len.to_le_bytes())?;
        self.w.write_all(&raw_len.to_le_bytes())?;
        self.w.write_all(&self.in_block.to_le_bytes())?;
        self.w.write_all(&comp)?;
        self.index.push(BlockEntry {
            first_recv: self.block_first_recv,
            last_recv: self.last_recv,
            offset: self.pos,
            n_events: self.in_block,
            comp_len,
        });
        self.pos += BLOCK_HEADER_LEN + u64::from(comp_len);
        self.raw.clear();
        self.in_block = 0;
        Ok(())
    }

    /// Write the last block, the index and the trailer; returns the writer.
    pub fn finish(mut self) -> Result<W, Error> {
        self.flush_block()?;
        let index_offset = self.pos;
        for e in &self.index {
            self.w.write_all(&e.first_recv.to_le_bytes())?;
            self.w.write_all(&e.last_recv.to_le_bytes())?;
            self.w.write_all(&e.offset.to_le_bytes())?;
            self.w.write_all(&e.n_events.to_le_bytes())?;
            self.w.write_all(&e.comp_len.to_le_bytes())?;
        }
        self.w.write_all(&index_offset.to_le_bytes())?;
        self.w.write_all(&(self.index.len() as u64).to_le_bytes())?;
        self.w.write_all(&self.total.to_le_bytes())?;
        self.w.write_all(&TRAILER_MAGIC)?;
        self.w.flush()?;
        Ok(self.w)
    }
}

/// Reads a finished tape from any [`Read`] + [`Seek`]. Opening reads only the
/// header, trailer and index; blocks are read and decompressed on demand.
pub struct TapeReader<R> {
    r: R,
    schema: u16,
    index: Vec<BlockEntry>,
    total: u64,
    index_offset: u64,
    blocks_loaded: u64,
    decompressor: zstd::bulk::Decompressor<'static>,
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes(b.try_into().expect("2 bytes"))
}
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().expect("4 bytes"))
}
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b.try_into().expect("8 bytes"))
}

impl<R: Read + Seek> TapeReader<R> {
    pub fn open(mut r: R) -> Result<Self, Error> {
        let len = r.seek(SeekFrom::End(0))?;
        if len < HEADER_LEN {
            return Err(if len < 4 {
                Error::BadMagic
            } else {
                Error::Corrupt("shorter than the header")
            });
        }
        let mut header = [0u8; HEADER_LEN as usize];
        r.seek(SeekFrom::Start(0))?;
        r.read_exact(&mut header)?;
        if header[..4] != HEADER_MAGIC {
            return Err(Error::BadMagic);
        }
        let format = le_u16(&header[4..6]);
        if format != FORMAT_VERSION {
            return Err(Error::UnsupportedFormat(format));
        }
        let schema = le_u16(&header[6..8]);
        if schema == 0 || schema > SCHEMA_VERSION {
            return Err(Error::UnsupportedSchema(schema));
        }

        if len < HEADER_LEN + TRAILER_LEN {
            return Err(Error::Corrupt("missing footer: the tape was not finished"));
        }
        let mut trailer = [0u8; TRAILER_LEN as usize];
        r.seek(SeekFrom::Start(len - TRAILER_LEN))?;
        r.read_exact(&mut trailer)?;
        if trailer[24..] != TRAILER_MAGIC {
            return Err(Error::Corrupt("missing footer: the tape was not finished"));
        }
        let index_offset = le_u64(&trailer[0..8]);
        let n_blocks = le_u64(&trailer[8..16]);
        let total = le_u64(&trailer[16..24]);

        // Check the sizes against the file before allocating anything from them.
        let index_len = n_blocks
            .checked_mul(INDEX_ENTRY_LEN)
            .ok_or(Error::Corrupt("index size overflows"))?;
        if index_offset < HEADER_LEN
            || index_offset.checked_add(index_len) != Some(len - TRAILER_LEN)
        {
            return Err(Error::Corrupt("index does not fit the file"));
        }
        let mut raw = vec![0u8; index_len as usize];
        r.seek(SeekFrom::Start(index_offset))?;
        r.read_exact(&mut raw)?;

        let mut index = Vec::with_capacity(n_blocks as usize);
        let (mut events, mut prev_last, mut prev_end) = (0u64, 0u64, HEADER_LEN);
        for c in raw.chunks_exact(INDEX_ENTRY_LEN as usize) {
            let e = BlockEntry {
                first_recv: le_u64(&c[0..8]),
                last_recv: le_u64(&c[8..16]),
                offset: le_u64(&c[16..24]),
                n_events: le_u32(&c[24..28]),
                comp_len: le_u32(&c[28..32]),
            };
            let end = e
                .offset
                .checked_add(BLOCK_HEADER_LEN + u64::from(e.comp_len));
            let sane = e.n_events > 0
                && e.first_recv <= e.last_recv
                && e.first_recv >= prev_last
                && e.offset == prev_end
                && end.is_some_and(|end| end <= index_offset);
            if !sane {
                return Err(Error::Corrupt("index entries are inconsistent"));
            }
            (events, prev_last, prev_end) = (
                events + u64::from(e.n_events),
                e.last_recv,
                end.unwrap_or(0),
            );
            index.push(e);
        }
        if events != total || prev_end != index_offset {
            return Err(Error::Corrupt("index does not account for the whole file"));
        }

        Ok(TapeReader {
            r,
            schema,
            index,
            total,
            index_offset,
            blocks_loaded: 0,
            decompressor: zstd::bulk::Decompressor::new()?,
        })
    }

    /// Total events on the tape.
    pub fn events(&self) -> u64 {
        self.total
    }

    pub fn blocks(&self) -> usize {
        self.index.len()
    }

    /// The event schema version the tape was written under.
    pub fn schema_version(&self) -> u16 {
        self.schema
    }

    /// `ts_recv` of the first and last event, or `None` for an empty tape.
    pub fn time_range(&self) -> Option<(u64, u64)> {
        Some((self.index.first()?.first_recv, self.index.last()?.last_recv))
    }

    /// How many blocks have been read and decompressed so far.
    pub fn blocks_loaded(&self) -> u64 {
        self.blocks_loaded
    }

    /// A position at the first event with `ts_recv >= from`. Finding it is a
    /// binary search over the index; no block is read until an event is asked for.
    pub fn cursor(&self, from: u64) -> Cursor {
        Cursor {
            next_block: self.index.partition_point(|b| b.last_recv < from),
            raw: Vec::new(),
            pos: 0,
            left: 0,
            from,
        }
    }

    /// Iterate events with `ts_recv >= from`, in order. Only blocks that are
    /// reached are read.
    pub fn scan(&mut self, from: u64) -> Scan<'_, R> {
        let cursor = self.cursor(from);
        Scan {
            reader: self,
            cursor,
            done: false,
        }
    }

    /// The next event at the cursor, or `None` at the end of the tape. After an
    /// error the cursor is not usable.
    pub fn next_event(&mut self, cur: &mut Cursor) -> Result<Option<Event>, Error> {
        loop {
            if cur.left == 0 {
                if cur.next_block >= self.index.len() {
                    return Ok(None);
                }
                let (raw, n) = self.load_block(cur.next_block)?;
                cur.next_block += 1;
                (cur.raw, cur.pos, cur.left) = (raw, 0, n);
            }
            let rest = &cur.raw[cur.pos..];
            if rest.len() < 2 {
                return Err(Error::Corrupt("block ends inside a length prefix"));
            }
            let len = usize::from(le_u16(&rest[..2]));
            let body = rest
                .get(2..2 + len)
                .ok_or(Error::Corrupt("event runs past its block"))?;
            let (ev, used) = Event::decode_versioned(self.schema, body)?;
            if used != len {
                return Err(Error::Corrupt(
                    "event length prefix disagrees with its encoding",
                ));
            }
            cur.pos += 2 + len;
            cur.left -= 1;
            if cur.left == 0 && cur.pos != cur.raw.len() {
                return Err(Error::Corrupt("block has bytes after its last event"));
            }
            if ev.ts_recv() >= cur.from {
                return Ok(Some(ev));
            }
        }
    }

    fn load_block(&mut self, i: usize) -> Result<(Vec<u8>, u32), Error> {
        let e = self.index[i];
        let mut head = [0u8; BLOCK_HEADER_LEN as usize];
        self.r.seek(SeekFrom::Start(e.offset))?;
        self.r.read_exact(&mut head)?;
        let (comp_len, raw_len, n) = (
            le_u32(&head[0..4]),
            le_u32(&head[4..8]),
            le_u32(&head[8..12]),
        );
        if comp_len != e.comp_len || n != e.n_events {
            return Err(Error::Corrupt("block header disagrees with the index"));
        }
        // A block of n events cannot decompress to more than this.
        if u64::from(raw_len) > u64::from(n) * (2 + MAX_ENCODED_EVENT) {
            return Err(Error::Corrupt("block claims an impossible size"));
        }
        let mut comp = vec![0u8; comp_len as usize];
        self.r.read_exact(&mut comp)?;
        let raw = self
            .decompressor
            .decompress(&comp, raw_len as usize)
            .map_err(|_| Error::Corrupt("a compressed block is damaged"))?;
        if raw.len() != raw_len as usize {
            return Err(Error::Corrupt("block decompressed to the wrong size"));
        }
        self.blocks_loaded += 1;
        debug_assert!(e.offset + BLOCK_HEADER_LEN + u64::from(e.comp_len) <= self.index_offset);
        Ok((raw, n))
    }
}

/// A position in a tape, separate from the reader so it can be kept alongside
/// it (an iterator that borrows the reader cannot be). See [`TapeReader::cursor`].
#[derive(Debug)]
pub struct Cursor {
    next_block: usize,
    raw: Vec<u8>,
    pos: usize,
    left: u32,
    from: u64,
}

/// Iterator over a tape's events from a point in time; see [`TapeReader::scan`].
/// After an error it yields nothing more.
pub struct Scan<'a, R> {
    reader: &'a mut TapeReader<R>,
    cursor: Cursor,
    done: bool,
}

impl<R: Read + Seek> Iterator for Scan<'_, R> {
    type Item = Result<Event, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.reader.next_event(&mut self.cursor) {
            Ok(Some(ev)) => Some(Ok(ev)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
