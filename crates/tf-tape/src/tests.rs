use std::fs::File;
use std::io::{BufWriter, Cursor};
use std::path::PathBuf;

use tf_core::{
    CancelError, CancelErrorKind, Correction, Event, Header, NANOS_PER_SEC, News, ProviderId, Px,
    Quote, Status, StatusKind, Trade, TradeFlags,
};
use tf_synth::{SynthConfig, SynthStream};

use super::*;

fn hdr(ts_recv: u64, seq: u64) -> Header {
    Header {
        ts_event: ts_recv - 5,
        ts_recv,
        seq,
        instrument: 7,
        provider: ProviderId::Alpaca,
    }
}

/// One of each kind, in arrival order (two share a `ts_recv`).
fn mixed() -> Vec<Event> {
    vec![
        Event::Trade(Trade {
            hdr: hdr(100, 0),
            px: Px::from_cents(1234),
            size: 300,
            flags: TradeFlags::ODD_LOT,
        }),
        Event::Quote(Quote {
            hdr: hdr(110, 1),
            bid_px: Px::from_cents(1233),
            ask_px: Px::from_cents(1235),
            bid_sz: 100,
            ask_sz: 200,
        }),
        Event::Status(Status {
            hdr: hdr(110, 2),
            kind: StatusKind::LuldBand,
            lo: Px::from_cents(900),
            hi: Px::from_cents(1500),
        }),
        Event::Correction(Correction {
            hdr: hdr(120, 3),
            orig_px: Px::from_cents(1234),
            orig_size: 300,
            px: Px::from_cents(1230),
            size: 200,
        }),
        Event::CancelError(CancelError {
            hdr: hdr(130, 4),
            kind: CancelErrorKind::Error,
            px: Px::from_cents(1234),
            size: 300,
        }),
        Event::News(News {
            hdr: hdr(140, 5),
            article_id: u64::MAX,
        }),
    ]
}

fn synth(symbols: usize, secs: u64) -> Vec<Event> {
    SynthStream::new(&SynthConfig::universe(
        3,
        symbols,
        secs * NANOS_PER_SEC,
        400,
    ))
    .collect()
}

fn write_tape(events: &[Event], block_events: u32) -> Vec<u8> {
    let mut w = TapeWriter::with_block_events(Vec::new(), block_events).unwrap();
    for e in events {
        w.write(e).unwrap();
    }
    w.finish().unwrap()
}

fn read_all(bytes: &[u8]) -> Result<Vec<Event>, Error> {
    let mut r = TapeReader::open(Cursor::new(bytes))?;
    r.scan(0).collect()
}

#[test]
fn every_kind_round_trips_across_block_boundaries() {
    let evs = mixed();
    let bytes = write_tape(&evs, 2);
    let mut r = TapeReader::open(Cursor::new(&bytes)).unwrap();
    assert_eq!(
        (r.events(), r.blocks(), r.schema_version()),
        (6, 3, SCHEMA_VERSION)
    );
    assert_eq!(r.time_range(), Some((100, 140)));
    assert_eq!(r.scan(0).collect::<Result<Vec<_>, _>>().unwrap(), evs);
}

#[test]
fn an_empty_tape_is_valid() {
    let bytes = write_tape(&[], 10);
    let mut r = TapeReader::open(Cursor::new(&bytes)).unwrap();
    assert_eq!((r.events(), r.blocks(), r.time_range()), (0, 0, None));
    assert!(r.scan(0).next().is_none());
}

#[test]
fn a_synthetic_session_round_trips_and_compresses() {
    let evs = synth(50, 120);
    assert!(evs.len() > 2_000);
    let bytes = write_tape(&evs, 100);
    let raw: usize = evs
        .iter()
        .map(|e| {
            let mut b = Vec::new();
            e.encode(&mut b);
            b.len() + 2
        })
        .sum();
    assert!(
        bytes.len() * 2 < raw,
        "tape {} B vs {} B of raw events",
        bytes.len(),
        raw
    );
    let r = TapeReader::open(Cursor::new(&bytes)).unwrap();
    assert_eq!(r.blocks(), evs.len().div_ceil(100));
    assert_eq!(read_all(&bytes).unwrap(), evs);
}

#[test]
fn scan_from_any_time_equals_filtering_the_whole_stream() {
    let evs = synth(30, 120);
    let bytes = write_tape(&evs, 64);
    let mut r = TapeReader::open(Cursor::new(&bytes)).unwrap();
    let (first, last) = r.time_range().unwrap();
    let mut targets = vec![
        0,
        first - 1,
        first,
        first + 1,
        last - 1,
        last,
        last + 1,
        u64::MAX,
    ];
    targets.extend(
        evs.iter()
            .step_by(97)
            .flat_map(|e| [e.ts_recv(), e.ts_recv() + 1]),
    );
    for t in targets {
        let want: Vec<Event> = evs.iter().filter(|e| e.ts_recv() >= t).copied().collect();
        let got: Vec<Event> = r.scan(t).collect::<Result<_, _>>().unwrap();
        assert_eq!(got, want, "scan from {t}");
    }
}

#[test]
fn a_seek_reads_one_block_not_the_tape() {
    let evs = synth(50, 120);
    let bytes = write_tape(&evs, 50);
    let mut r = TapeReader::open(Cursor::new(&bytes)).unwrap();
    assert!(r.blocks() > 40);
    assert_eq!(
        r.blocks_loaded(),
        0,
        "opening reads only header, trailer and index"
    );

    for i in [0, evs.len() / 3, evs.len() / 2, evs.len() - 1] {
        let ts = evs[i].ts_recv();
        let before = r.blocks_loaded();
        let got = r.scan(ts).next().unwrap().unwrap();
        assert!(got.ts_recv() >= ts);
        assert_eq!(r.blocks_loaded() - before, 1, "seek to event {i}");
    }
    let before = r.blocks_loaded();
    assert_eq!(r.scan(0).count(), evs.len());
    assert_eq!(r.blocks_loaded() - before, r.blocks() as u64);
}

#[test]
fn ts_recv_must_not_go_backwards() {
    let mut w = TapeWriter::new(Vec::new()).unwrap();
    let evs = mixed();
    w.write(&evs[1]).unwrap();
    w.write(&evs[2]).unwrap(); // same ts_recv is fine
    assert!(matches!(
        w.write(&evs[0]),
        Err(Error::OutOfOrder {
            prev: 110,
            got: 100
        })
    ));
    assert_eq!(w.events(), 2, "a rejected event is not counted");
}

fn patched(bytes: &[u8], at: usize, with: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    b[at..at + with.len()].copy_from_slice(with);
    b
}

#[test]
fn headers_and_versions_are_checked() {
    let bytes = write_tape(&mixed(), 4);
    let open = |b: &[u8]| TapeReader::open(Cursor::new(b.to_vec())).err();
    assert!(matches!(
        open(&patched(&bytes, 0, b"XXXX")),
        Some(Error::BadMagic)
    ));
    assert!(matches!(open(b""), Some(Error::BadMagic)));
    assert!(matches!(
        open(&patched(&bytes, 4, &9u16.to_le_bytes())),
        Some(Error::UnsupportedFormat(9))
    ));
    assert!(matches!(
        open(&patched(&bytes, 6, &0u16.to_le_bytes())),
        Some(Error::UnsupportedSchema(0))
    ));
    let newer = (SCHEMA_VERSION + 1).to_le_bytes();
    assert!(matches!(
        open(&patched(&bytes, 6, &newer)),
        Some(Error::UnsupportedSchema(_))
    ));
}

#[test]
fn an_unfinished_tape_says_so() {
    // A writer that died before finish(): header and blocks, no index or trailer.
    let mut w = TapeWriter::with_block_events(Vec::new(), 2).unwrap();
    for e in mixed() {
        w.write(&e).unwrap();
    }
    let TapeWriter { w: dead, .. } = w;
    assert!(dead.len() > 16);
    match TapeReader::open(Cursor::new(dead)) {
        Err(Error::Corrupt(m)) => assert!(m.contains("footer"), "{m}"),
        other => panic!("expected a missing-footer error, got {:?}", other.err()),
    }
}

#[test]
fn damage_is_an_error_or_harmless_never_a_panic_or_wrong_data() {
    let evs = mixed();
    let bytes = write_tape(&evs, 2);

    // Every truncation fails to open (the trailer is gone) or fails to read.
    for n in 0..bytes.len() {
        assert!(
            read_all(&bytes[..n]).is_err(),
            "truncated to {n} bytes was accepted"
        );
    }

    // Flip each byte, two ways: reading either fails or returns exactly the
    // original events (a flip in an ignored field). It never returns different data.
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0xFF] {
            let mut b = bytes.clone();
            b[i] ^= mask;
            if let Ok(got) = read_all(&b) {
                assert_eq!(got, evs, "byte {i} ^ {mask:#x} silently changed the events");
            }
        }
    }
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "10M events; runs in release builds, which CI does"
)]
fn ten_million_synthetic_events_round_trip_bit_exactly_and_seek() {
    let cfg = SynthConfig::universe(42, 5000, 2000 * NANOS_PER_SEC, 20);
    let tmp =
        TempFile(std::env::temp_dir().join(format!("tf-tape-{}-10m.tape", std::process::id())));

    let mut w = TapeWriter::new(BufWriter::new(File::create(&tmp.0).unwrap())).unwrap();
    let mut raw_bytes = 0u64;
    let mut scratch = Vec::new();
    for ev in SynthStream::new(&cfg) {
        scratch.clear();
        ev.encode(&mut scratch);
        raw_bytes += scratch.len() as u64 + 2;
        w.write(&ev).unwrap();
    }
    let n = w.events();
    w.finish().unwrap();
    assert!(n >= 10_000_000, "only {n} events");
    let size = std::fs::metadata(&tmp.0).unwrap().len();
    eprintln!(
        "{n} events, {raw_bytes} B raw -> {size} B tape ({:.1}%)",
        100.0 * size as f64 / raw_bytes as f64
    );

    assert!(
        size * 10 < raw_bytes * 6,
        "tape is {size} B for {raw_bytes} B of events"
    );

    // Read it back and compare with a regenerated stream, event for event.
    let mut r = TapeReader::open(File::open(&tmp.0).unwrap()).unwrap();
    assert_eq!(r.events(), n);
    let samples = [n / 100, n / 3, n / 2, n - 1_000, n - 1];
    let mut seeks: Vec<(u64, Event)> = Vec::new();
    {
        let mut scan = r.scan(0);
        let (mut cur_ts, mut cur_first) = (u64::MAX, None);
        for (i, want) in SynthStream::new(&cfg).enumerate() {
            let got = scan.next().expect("tape ended early").unwrap();
            assert_eq!(got, want, "event {i} differs");
            if want.ts_recv() != cur_ts {
                (cur_ts, cur_first) = (want.ts_recv(), Some(want));
            }
            if samples.contains(&(i as u64)) {
                seeks.push((cur_ts, cur_first.unwrap()));
            }
        }
        assert!(scan.next().is_none(), "tape has extra events");
    }

    // Seeking lands on the first event at that time, reading one block.
    for (ts, want) in seeks {
        let before = r.blocks_loaded();
        let got = r.scan(ts).next().unwrap().unwrap();
        assert_eq!(got, want, "seek to {ts}");
        assert_eq!(r.blocks_loaded() - before, 1);
    }
}
