//! Stable binary encoding of [`Event`].
//!
//! Explicit little-endian, field by field: never a memcpy of the in-memory
//! struct (padding bytes are uninitialised and layout is not a contract).
//! This is the seed of the raw tape format and the input to golden hashes.
//!
//! Layout: `tag:u8` then header (`ts_event:u64 ts_recv:u64 seq:u64
//! instrument:u32 provider:u8`) then the per-kind body.

use std::fmt;

use crate::event::{Event, Header, Quote, Status, StatusKind, Trade, TradeFlags};
use crate::ids::ProviderId;
use crate::px::Px;

const TAG_TRADE: u8 = 1;
const TAG_QUOTE: u8 = 2;
const TAG_STATUS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadTag(u8),
    BadProvider(u8),
    BadStatusKind(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "truncated event"),
            DecodeError::BadTag(t) => write!(f, "unknown event tag {t}"),
            DecodeError::BadProvider(p) => write!(f, "unknown provider id {p}"),
            DecodeError::BadStatusKind(k) => write!(f, "unknown status kind {k}"),
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
        }
    }

    /// Decode one event from the front of `buf`; returns it and the bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(Event, usize), DecodeError> {
        let mut r = Reader { buf, pos: 0 };
        let tag = r.u8()?;
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
                    kind: StatusKind::from_u8(k).ok_or(DecodeError::BadStatusKind(k))?,
                    lo: Px::from_raw(r.i64()?),
                    hi: Px::from_raw(r.i64()?),
                })
            }
            other => return Err(DecodeError::BadTag(other)),
        };
        Ok((ev, r.pos))
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
        ]
    }

    #[test]
    fn roundtrip_back_to_back() {
        let evs = samples();
        let mut buf = Vec::new();
        for e in &evs {
            e.encode(&mut buf);
        }
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
    fn encoded_sizes_are_documented_sizes() {
        let sizes: Vec<usize> = samples()
            .iter()
            .map(|e| {
                let mut b = Vec::new();
                e.encode(&mut b);
                b.len()
            })
            .collect();
        assert_eq!(sizes, vec![44, 54, 47]);
    }
}
