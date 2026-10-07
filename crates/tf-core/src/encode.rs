//! Stable binary encoding of [`Event`].
//!
//! Explicit little-endian, field by field: never a memcpy of the in-memory
//! struct (padding bytes are uninitialised and layout is not a contract).
//! This is the seed of the raw tape format and the input to golden hashes.
//!
//! Layout of one event: `tag:u8` then header (`ts_event:u64 ts_recv:u64 seq:u64
//! instrument:u32 provider:u8`) then the per-kind body.
//!
//! # Schema versions
//!
//! A stream or tape is a run of events back to back, optionally preceded by an
//! 8-byte header: `"TFEV"`, `version:u16`, `reserved:u16` (zero).
//!
//! - **v1**: bare events, no header; tags 1-3 (trade, quote, status).
//! - **v2**: adds tags 4-6 (correction, cancel-error, news) and the header. The
//!   layout of every v1 event is unchanged, so v1 data decodes as it is.
//! - **v3**: adds tag 7 (parameter change). Every earlier layout is unchanged, so
//!   v1 and v2 data decode as they are; a stream older than v3 containing tag 7 is
//!   corrupt.
//! - **v4**: adds tag 8 (tier change: a symbol promoted to or demoted from Tier 1).
//!   Nothing earlier changes; a stream older than v4 containing tag 8 is corrupt.
//!
//! Event tags stay below `b'T'`, the first byte of the header, so a headerless
//! v1 stream is never mistaken for one with a header. [`Decoder`] does the
//! dispatch; a v1 stream that contains a v2-only tag is corrupt.

use std::fmt;

use crate::event::{
    CancelError, CancelErrorKind, Correction, Event, Header, News, ParamChange, ParamScope, Quote,
    Status, StatusKind, TierAction, TierChange, Trade, TradeFlags,
};
use crate::ids::ProviderId;
use crate::px::Px;

const TAG_TRADE: u8 = 1;
const TAG_QUOTE: u8 = 2;
const TAG_STATUS: u8 = 3;
const TAG_CORRECTION: u8 = 4;
const TAG_CANCEL_ERROR: u8 = 5;
const TAG_NEWS: u8 = 6;
const TAG_PARAM_CHANGE: u8 = 7;
const TAG_TIER_CHANGE: u8 = 8;

/// The schema version this build writes.
pub const SCHEMA_VERSION: u16 = 5;
/// The first four bytes of a stream that has a header.
pub const STREAM_MAGIC: [u8; 4] = *b"TFEV";
pub const STREAM_HEADER_LEN: usize = 8;

// A headerless v1 stream opens with an event tag; no tag may look like the magic.
const _: () = assert!(TAG_TIER_CHANGE < STREAM_MAGIC[0]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadTag(u8),
    BadProvider(u8),
    BadStatusKind(u8),
    BadCancelKind(u8),
    BadParamScope(u8),
    BadTierAction(u8),
    /// Starts like a stream header but is not one.
    BadMagic,
    /// A header with a version this build cannot read (zero, or newer).
    UnsupportedVersion(u16),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "truncated event"),
            DecodeError::BadTag(t) => write!(f, "unknown event tag {t}"),
            DecodeError::BadProvider(p) => write!(f, "unknown provider id {p}"),
            DecodeError::BadStatusKind(k) => write!(f, "unknown status kind {k}"),
            DecodeError::BadCancelKind(k) => write!(f, "unknown cancel/error kind {k}"),
            DecodeError::BadParamScope(k) => write!(f, "unknown parameter scope {k}"),
            DecodeError::BadTierAction(k) => write!(f, "unknown tier action {k}"),
            DecodeError::BadMagic => write!(f, "bad stream header magic"),
            DecodeError::UnsupportedVersion(v) => write!(f, "unsupported schema version {v}"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn put_hdr(out: &mut Vec<u8>, h: &Header) {
    out.extend_from_slice(&h.ts_event.to_le_bytes());
    out.extend_from_slice(&h.ts_recv.to_le_bytes());
    out.extend_from_slice(&h.seq.to_le_bytes());
    out.extend_from_slice(&h.instrument.to_le_bytes());
    out.push(h.provider.as_u8());
}

/// Append the stream header for [`SCHEMA_VERSION`] to `out`.
pub fn write_stream_header(out: &mut Vec<u8>) {
    out.extend_from_slice(&STREAM_MAGIC);
    out.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
}

/// The schema version of a stream and the offset of its first event.
///
/// A buffer that does not open with the header magic is a v1 stream: v1 had no
/// header, and its first byte is an event tag, which can never be the magic's.
pub fn read_stream_header(buf: &[u8]) -> Result<(u16, usize), DecodeError> {
    if buf.first() != Some(&STREAM_MAGIC[0]) {
        return Ok((1, 0));
    }
    if buf.len() < STREAM_HEADER_LEN {
        return Err(DecodeError::Truncated);
    }
    if buf[..4] != STREAM_MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = u16::from_le_bytes([buf[4], buf[5]]);
    if version == 0 || version > SCHEMA_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    Ok((version, STREAM_HEADER_LEN))
}

impl Event {
    /// Append the encoding of `self` to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Event::Trade(t) => {
                out.push(TAG_TRADE);
                put_hdr(out, &t.hdr);
                out.extend_from_slice(&t.px.raw().to_le_bytes());
                out.extend_from_slice(&t.size.to_le_bytes());
                out.extend_from_slice(&t.flags.0.to_le_bytes());
            }
            Event::Quote(q) => {
                out.push(TAG_QUOTE);
                put_hdr(out, &q.hdr);
                out.extend_from_slice(&q.bid_px.raw().to_le_bytes());
                out.extend_from_slice(&q.ask_px.raw().to_le_bytes());
                out.extend_from_slice(&q.bid_sz.to_le_bytes());
                out.extend_from_slice(&q.ask_sz.to_le_bytes());
            }
            Event::Status(s) => {
                out.push(TAG_STATUS);
                put_hdr(out, &s.hdr);
                out.push(s.kind as u8);
                out.extend_from_slice(&s.lo.raw().to_le_bytes());
                out.extend_from_slice(&s.hi.raw().to_le_bytes());
            }
            Event::Correction(c) => {
                out.push(TAG_CORRECTION);
                put_hdr(out, &c.hdr);
                out.extend_from_slice(&c.orig_px.raw().to_le_bytes());
                out.extend_from_slice(&c.orig_size.to_le_bytes());
                out.extend_from_slice(&c.px.raw().to_le_bytes());
                out.extend_from_slice(&c.size.to_le_bytes());
            }
            Event::CancelError(c) => {
                out.push(TAG_CANCEL_ERROR);
                put_hdr(out, &c.hdr);
                out.push(c.kind as u8);
                out.extend_from_slice(&c.px.raw().to_le_bytes());
                out.extend_from_slice(&c.size.to_le_bytes());
            }
            Event::News(n) => {
                out.push(TAG_NEWS);
                put_hdr(out, &n.hdr);
                out.extend_from_slice(&n.article_id.to_le_bytes());
            }
            Event::ParamChange(p) => {
                out.push(TAG_PARAM_CHANGE);
                put_hdr(out, &p.hdr);
                out.extend_from_slice(&p.param.to_le_bytes());
                out.push(p.scope as u8);
                out.extend_from_slice(&p.proposer.to_le_bytes());
                out.extend_from_slice(&p.reason.to_le_bytes());
                out.extend_from_slice(&p.new_value.to_le_bytes());
                out.extend_from_slice(&p.evidence.to_le_bytes());
            }
            Event::TierChange(t) => {
                out.push(TAG_TIER_CHANGE);
                put_hdr(out, &t.hdr);
                out.push(t.action as u8);
                out.push(t.reason);
                out.extend_from_slice(&t.score.to_le_bytes());
            }
        }
    }

    /// Decode one event of the current schema from the front of `buf`; returns
    /// it and the bytes consumed. Use [`Decoder`] for whole streams.
    pub fn decode(buf: &[u8]) -> Result<(Event, usize), DecodeError> {
        Event::decode_versioned(SCHEMA_VERSION, buf)
    }

    /// Like [`Event::decode`] for an event of schema `version`: a v1 stream
    /// cannot contain the kinds that v2 introduced.
    pub fn decode_versioned(version: u16, buf: &[u8]) -> Result<(Event, usize), DecodeError> {
        if version == 0 || version > SCHEMA_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let mut r = Reader { buf, pos: 0 };
        let tag = r.u8()?;
        if (version < 2 && tag >= TAG_CORRECTION)
            || (version < 3 && tag >= TAG_PARAM_CHANGE)
            || (version < 4 && tag >= TAG_TIER_CHANGE)
        {
            return Err(DecodeError::BadTag(tag));
        }
        let hdr = Header {
            ts_event: r.u64()?,
            ts_recv: r.u64()?,
            seq: r.u64()?,
            instrument: r.u32()?,
            provider: {
                let p = r.u8()?;
                ProviderId::from_u8(p).ok_or(DecodeError::BadProvider(p))?
            },
        };
        let ev = match tag {
            TAG_TRADE => Event::Trade(Trade {
                hdr,
                px: Px::from_raw(r.i64()?),
                size: r.u32()?,
                flags: TradeFlags(r.u16()?),
            }),
            TAG_QUOTE => Event::Quote(Quote {
                hdr,
                bid_px: Px::from_raw(r.i64()?),
                ask_px: Px::from_raw(r.i64()?),
                bid_sz: r.u32()?,
                ask_sz: r.u32()?,
            }),
            TAG_STATUS => {
                let k = r.u8()?;
                Event::Status(Status {
                    hdr,
                    kind: {
                        let kind = StatusKind::from_u8(k).ok_or(DecodeError::BadStatusKind(k))?;
                        // Schema v5 added the end of a short-sale restriction: an older stream cannot say it.
                        if version < 5 && kind == StatusKind::ShortSaleRestrictionLifted {
                            return Err(DecodeError::BadStatusKind(k));
                        }
                        kind
                    },
                    lo: Px::from_raw(r.i64()?),
                    hi: Px::from_raw(r.i64()?),
                })
            }
            TAG_CORRECTION => Event::Correction(Correction {
                hdr,
                orig_px: Px::from_raw(r.i64()?),
                orig_size: r.u32()?,
                px: Px::from_raw(r.i64()?),
                size: r.u32()?,
            }),
            TAG_CANCEL_ERROR => {
                let k = r.u8()?;
                Event::CancelError(CancelError {
                    hdr,
                    kind: CancelErrorKind::from_u8(k).ok_or(DecodeError::BadCancelKind(k))?,
                    px: Px::from_raw(r.i64()?),
                    size: r.u32()?,
                })
            }
            TAG_NEWS => Event::News(News {
                hdr,
                article_id: r.u64()?,
            }),
            TAG_TIER_CHANGE => {
                let a = r.u8()?;
                Event::TierChange(TierChange {
                    hdr,
                    action: TierAction::from_u8(a).ok_or(DecodeError::BadTierAction(a))?,
                    reason: r.u8()?,
                    score: r.i64()?,
                })
            }
            TAG_PARAM_CHANGE => {
                let param = r.u16()?;
                let sc = r.u8()?;
                Event::ParamChange(ParamChange {
                    hdr,
                    param,
                    scope: ParamScope::from_u8(sc).ok_or(DecodeError::BadParamScope(sc))?,
                    proposer: r.u16()?,
                    reason: r.u16()?,
                    new_value: r.i64()?,
                    evidence: r.u64()?,
                })
            }
            other => return Err(DecodeError::BadTag(other)),
        };
        Ok((ev, r.pos))
    }
}

/// Decodes a whole stream or tape: reads the header if there is one (a stream
/// without one is v1), then yields events until the end. After an error it
/// yields nothing more, since there is no way to find the next event boundary.
#[derive(Debug)]
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    version: u16,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let (version, pos) = read_stream_header(buf)?;
        Ok(Decoder { buf, pos, version })
    }

    /// The schema version of the stream being read.
    pub fn version(&self) -> u16 {
        self.version
    }
}

impl Iterator for Decoder<'_> {
    type Item = Result<Event, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        match Event::decode_versioned(self.version, &self.buf[self.pos..]) {
            Ok((ev, n)) => {
                self.pos += n;
                Some(Ok(ev))
            }
            Err(e) => {
                self.pos = self.buf.len();
                Some(Err(e))
            }
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let end = self.pos + N;
        let slice = self.buf.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(slice.try_into().expect("slice length equals N"))
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.take()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(seq: u64) -> Header {
        Header {
            ts_event: 1_000 + seq,
            ts_recv: 2_000 + seq,
            seq,
            instrument: 42,
            provider: ProviderId::Alpaca,
        }
    }

    /// The first three are the events in `V1_TAPE_HEX`; keep them as they are.
    fn samples() -> Vec<Event> {
        vec![
            Event::Trade(Trade {
                hdr: hdr(1),
                px: Px::from_cents(1234),
                size: 300,
                flags: TradeFlags::ODD_LOT,
            }),
            Event::Quote(Quote {
                hdr: hdr(2),
                bid_px: Px::from_cents(1233),
                ask_px: Px::from_cents(1235),
                bid_sz: 100,
                ask_sz: 200,
            }),
            Event::Status(Status {
                hdr: hdr(3),
                kind: StatusKind::LuldBand,
                lo: Px::from_cents(900),
                hi: Px::from_cents(1500),
            }),
            Event::Correction(Correction {
                hdr: hdr(4),
                orig_px: Px::from_cents(1234),
                orig_size: 300,
                px: Px::from_cents(1230),
                size: 200,
            }),
            Event::CancelError(CancelError {
                hdr: hdr(5),
                kind: CancelErrorKind::Error,
                px: Px::from_cents(1234),
                size: 300,
            }),
            Event::News(News {
                hdr: hdr(6),
                article_id: 0x0123_4567_89ab_cdef,
            }),
            Event::ParamChange(ParamChange {
                hdr: hdr(7),
                param: 513,
                scope: ParamScope::Instrument,
                proposer: 9,
                reason: 77,
                new_value: -1_234_567_890_123,
                evidence: 0xfedc_ba98_7654_3210,
            }),
            Event::TierChange(TierChange {
                hdr: hdr(8),
                action: TierAction::Demote,
                reason: 3,
                score: -4_321,
            }),
        ]
    }

    fn encode_all(evs: &[Event]) -> Vec<u8> {
        let mut buf = Vec::new();
        for e in evs {
            e.encode(&mut buf);
        }
        buf
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// A tape written by the v1 encoder (commit 3a61d90, before schema
    /// versions existed): `samples()[..3]`, byte for byte, with no header.
    const V1_TAPE_HEX: &str = "01e903000000000000d10700000000000001000000000000002a00000002007585df020000002c010000020002ea03000000000000d20700000000000002000000000000002a0000000280deecde02000000800b1ee00200000064000000c800000003eb03000000000000d30700000000000003000000000000002a0000000202001a71180200000000d6117e03000000";

    #[test]
    fn roundtrip_back_to_back() {
        let evs = samples();
        let buf = encode_all(&evs);
        let mut pos = 0;
        let mut got = Vec::new();
        while pos < buf.len() {
            let (e, n) = Event::decode(&buf[pos..]).unwrap();
            got.push(e);
            pos += n;
        }
        assert_eq!(got, evs);
    }

    #[test]
    fn truncated_and_bad_input_are_errors() {
        let mut buf = Vec::new();
        samples()[0].encode(&mut buf);
        assert_eq!(
            Event::decode(&buf[..buf.len() - 1]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(Event::decode(&[]), Err(DecodeError::Truncated));
        let mut bad = buf.clone();
        bad[0] = 99;
        assert_eq!(Event::decode(&bad), Err(DecodeError::BadTag(99)));
    }

    #[test]
    fn every_prefix_of_every_kind_is_truncated_never_a_panic() {
        for ev in samples() {
            let mut buf = Vec::new();
            ev.encode(&mut buf);
            for n in 0..buf.len() {
                assert_eq!(
                    Event::decode(&buf[..n]),
                    Err(DecodeError::Truncated),
                    "{:?} cut to {n} bytes",
                    ev.kind()
                );
            }
            assert_eq!(Event::decode(&buf), Ok((ev, buf.len())));
        }
    }

    #[test]
    fn encoded_sizes_are_documented_sizes() {
        let sizes: Vec<usize> = samples()
            .iter()
            .map(|e| {
                let mut b = Vec::new();
                e.encode(&mut b);
                b.len()
            })
            .collect();
        // trade, quote, status, correction, cancel-error, news, param change, tier change
        assert_eq!(sizes, vec![44, 54, 47, 54, 43, 38, 53, 40]);
    }

    #[test]
    fn a_v1_tape_from_the_old_encoder_still_decodes() {
        let tape = unhex(V1_TAPE_HEX);
        let mut dec = Decoder::new(&tape).unwrap();
        assert_eq!(dec.version(), 1);
        let got: Vec<Event> = dec.by_ref().map(Result::unwrap).collect();
        assert_eq!(got, samples()[..3]);

        // The v1 layout is frozen: today's encoder writes the same bytes.
        assert_eq!(encode_all(&samples()[..3]), tape);
    }

    #[test]
    fn a_v1_stream_cannot_contain_v2_only_kinds() {
        for ev in &samples()[3..] {
            let mut buf = Vec::new();
            ev.encode(&mut buf);
            let mut dec = Decoder::new(&buf).unwrap();
            assert_eq!(dec.version(), 1, "no header, so v1");
            let tag = buf[0];
            assert_eq!(dec.next(), Some(Err(DecodeError::BadTag(tag))));
            assert_eq!(dec.next(), None, "nothing after an error");
        }
    }

    #[test]
    fn a_v2_stream_still_decodes_and_cannot_contain_the_v3_kind() {
        let v2_header = [b'T', b'F', b'E', b'V', 2, 0, 0, 0];
        let mut ok = v2_header.to_vec();
        ok.extend(encode_all(&samples()[..6]));
        let mut dec = Decoder::new(&ok).unwrap();
        assert_eq!(dec.version(), 2);
        let got: Vec<Event> = dec.by_ref().map(Result::unwrap).collect();
        assert_eq!(got, samples()[..6]);

        let mut bad = v2_header.to_vec();
        bad.extend(encode_all(&samples()[6..]));
        let mut dec = Decoder::new(&bad).unwrap();
        assert_eq!(dec.next(), Some(Err(DecodeError::BadTag(7))));
        assert_eq!(dec.next(), None);
    }

    #[test]
    fn a_v3_stream_cannot_contain_the_v4_kind_and_a_tier_change_round_trips() {
        let v3_header = [b'T', b'F', b'E', b'V', 3, 0, 0, 0];
        let mut ok = v3_header.to_vec();
        ok.extend(encode_all(&samples()[..7]));
        let mut dec = Decoder::new(&ok).unwrap();
        assert_eq!(dec.version(), 3);
        let got: Vec<Event> = dec.by_ref().map(Result::unwrap).collect();
        assert_eq!(got, samples()[..7]);

        let mut bad = v3_header.to_vec();
        bad.extend(encode_all(&samples()[7..]));
        let mut dec = Decoder::new(&bad).unwrap();
        assert_eq!(dec.next(), Some(Err(DecodeError::BadTag(8))));

        // An unknown action byte is an error, not a guess.
        let mut buf = Vec::new();
        samples()[7].encode(&mut buf);
        buf[1 + 29] = 7; // tag, header, then the action byte
        assert_eq!(Event::decode(&buf), Err(DecodeError::BadTierAction(7)));
    }

    #[test]
    fn a_param_change_with_an_unknown_scope_is_an_error() {
        let mut buf = Vec::new();
        samples()[6].encode(&mut buf);
        // tag, then the 29-byte header, then param (2 bytes), then the scope byte.
        buf[1 + 29 + 2] = 9;
        assert_eq!(Event::decode(&buf), Err(DecodeError::BadParamScope(9)));
    }

    #[test]
    fn the_end_of_a_short_sale_restriction_is_a_v5_status_kind() {
        let lifted = Event::Status(Status {
            hdr: Header {
                ts_event: 5,
                ts_recv: 6,
                seq: 7,
                instrument: 3,
                provider: ProviderId::Databento,
            },
            kind: StatusKind::ShortSaleRestrictionLifted,
            lo: Px::ZERO,
            hi: Px::ZERO,
        });
        let mut buf = Vec::new();
        lifted.encode(&mut buf);
        // The kind is the byte after the tag and the 29-byte header.
        assert_eq!(buf[1 + 29], 4);
        assert_eq!(Event::decode(&buf), Ok((lifted, buf.len())));
        // A v4 stream cannot say it; the kinds it could say are unchanged.
        assert_eq!(
            Event::decode_versioned(4, &buf),
            Err(DecodeError::BadStatusKind(4))
        );
        buf[1 + 29] = 3;
        assert!(Event::decode_versioned(4, &buf).is_ok());
        buf[1 + 29] = 5;
        assert_eq!(Event::decode(&buf), Err(DecodeError::BadStatusKind(5)));
        let mut stream = vec![b'T', b'F', b'E', b'V', 5, 0, 0, 0];
        lifted.encode(&mut stream);
        assert_eq!(Decoder::new(&stream).unwrap().next(), Some(Ok(lifted)));
        // The same bytes under a v4 header are corrupt.
        stream[4] = 4;
        assert_eq!(
            Decoder::new(&stream).unwrap().next(),
            Some(Err(DecodeError::BadStatusKind(4)))
        );
    }

    #[test]
    fn a_stream_with_a_header_carries_its_version() {
        let mut buf = Vec::new();
        write_stream_header(&mut buf);
        assert_eq!(buf, [b'T', b'F', b'E', b'V', 5, 0, 0, 0]);
        assert_eq!(buf.len(), STREAM_HEADER_LEN);
        buf.extend(encode_all(&samples()));

        let mut dec = Decoder::new(&buf).unwrap();
        assert_eq!(dec.version(), SCHEMA_VERSION);
        let got: Vec<Event> = dec.by_ref().map(Result::unwrap).collect();
        assert_eq!(got, samples());
    }

    #[test]
    fn bad_headers_are_errors_not_guesses() {
        let header = |v: u16| {
            let mut b = STREAM_MAGIC.to_vec();
            b.extend_from_slice(&v.to_le_bytes());
            b.extend_from_slice(&[0, 0]);
            b
        };
        assert_eq!(
            Decoder::new(&header(6)).unwrap_err(),
            DecodeError::UnsupportedVersion(6)
        );
        assert_eq!(
            Decoder::new(&header(0)).unwrap_err(),
            DecodeError::UnsupportedVersion(0)
        );
        assert_eq!(
            Decoder::new(&header(2)[..5]).unwrap_err(),
            DecodeError::Truncated
        );
        assert_eq!(Decoder::new(b"T").unwrap_err(), DecodeError::Truncated);
        assert_eq!(
            Decoder::new(b"TXEV\x02\x00\x00\x00").unwrap_err(),
            DecodeError::BadMagic
        );
        assert_eq!(
            Event::decode_versioned(6, &[]).unwrap_err(),
            DecodeError::UnsupportedVersion(6)
        );
    }

    #[test]
    fn an_empty_buffer_is_an_empty_v1_tape() {
        let mut dec = Decoder::new(&[]).unwrap();
        assert_eq!(dec.version(), 1);
        assert_eq!(dec.next(), None);
    }

    #[test]
    fn the_decoder_stops_at_the_first_error() {
        let mut buf = encode_all(&samples()[..1]);
        let second = encode_all(&samples()[1..2]);
        buf.extend_from_slice(&second[..second.len() - 1]);
        let got: Vec<_> = Decoder::new(&buf).unwrap().collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], Ok(samples()[0]));
        assert_eq!(got[1], Err(DecodeError::Truncated));
    }

    #[test]
    fn unknown_cancel_kind_is_an_error() {
        let mut buf = Vec::new();
        samples()[4].encode(&mut buf);
        let kind_at = 1 + 29; // tag + header
        buf[kind_at] = 7;
        assert_eq!(Event::decode(&buf), Err(DecodeError::BadCancelKind(7)));
    }
}
