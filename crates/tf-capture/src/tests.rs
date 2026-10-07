use std::ffi::c_char;
use std::io::Cursor;
use std::path::PathBuf;

use dbn::decode::{DbnDecoder, DecodeRecordRef};
use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{
    Cmbp1Msg, ConsolidatedBidAskPair, FlagSet, MetadataBuilder, RecordHeader, SType, StatusAction,
    StatusMsg, SystemMsg, TradeMsg, rtype,
};
use tf_core::{Event, ProviderId};
use tf_databento::{Decoder, Item};
use tf_provider::{Channels, Poll, Provider, Subscription};
use tf_tape::TapeReader;

use super::*;

const D: i64 = 1_000_000_000;
const SEC: u64 = 1_000_000_000;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-capture-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn cfg(dir: &Path, secs: u64) -> Config {
    Config {
        segment_secs: secs,
        ..Config::new(dir, "XNAS.BASIC")
    }
}

fn trade(
    publisher: u16,
    instrument: u32,
    ts_recv: u64,
    price: i64,
    size: u32,
    sequence: u32,
) -> TradeMsg {
    TradeMsg {
        hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, publisher, instrument, ts_recv - 1_000),
        price,
        size,
        action: b'T' as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        depth: 0,
        ts_recv,
        ts_in_delta: 0,
        sequence,
    }
}

fn quote(instrument: u32, ts_recv: u64, bid: i64, ask: i64) -> Cmbp1Msg {
    Cmbp1Msg {
        hd: RecordHeader::new::<Cmbp1Msg>(rtype::CMBP_1, 88, instrument, ts_recv - 500),
        price: 0,
        size: 0,
        action: b'A' as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        _reserved1: [0],
        ts_recv,
        ts_in_delta: 0,
        _reserved2: [0; 4],
        levels: [ConsolidatedBidAskPair {
            bid_px: bid,
            ask_px: ask,
            bid_sz: 1,
            ask_sz: 1,
            bid_pb: 81,
            _reserved1: [0; 2],
            ask_pb: 82,
            _reserved2: [0; 2],
        }],
    }
}

fn halt(instrument: u32, ts_recv: u64) -> StatusMsg {
    let mut s = StatusMsg {
        action: StatusAction::Halt as u16,
        ..StatusMsg::default()
    };
    s.hd = RecordHeader::new::<StatusMsg>(rtype::STATUS, 81, instrument, ts_recv - 10);
    s.ts_recv = ts_recv;
    s
}

/// A DBN stream of a mixed session: `n` rounds, one every `step` ns from `start`.
fn session(n: u64, start: u64, step: u64) -> Vec<u8> {
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(start)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        for i in 0..n {
            let t = start + i * step;
            e.encode_record(&trade(
                81 + (i % 2) as u16,
                20_000 + (i % 7) as u32,
                t,
                (100 + i as i64) * D / 10,
                1 + (i % 9) as u32,
                i as u32,
            ))
            .unwrap();
            e.encode_record(&quote(20_000 + (i % 7) as u32, t + 1, 99 * D, 101 * D))
                .unwrap();
            if i % 11 == 0 {
                e.encode_record(&halt(20_000 + (i % 7) as u32, t + 2))
                    .unwrap();
            }
            if i % 13 == 0 {
                e.encode_record(&SystemMsg::heartbeat(t + 3)).unwrap();
            }
        }
    }
    bytes
}

fn feed(w: &mut RawWriter, bytes: &[u8]) -> u64 {
    let mut dec = DbnDecoder::new(bytes).unwrap();
    let mut n = 0;
    while let Some(rec) = dec.decode_record_ref().unwrap() {
        w.write(&rec).unwrap();
        n += 1;
    }
    n
}

fn direct_events(bytes: &[u8]) -> Vec<Event> {
    Decoder::new(bytes)
        .unwrap()
        .filter_map(|i| {
            if let Item::Event(e) = i.unwrap() {
                Some(e)
            } else {
                None
            }
        })
        .collect()
}

fn replayed(dir: &Path) -> Vec<Event> {
    let mut r = CaptureReplay::open(dir).unwrap();
    let mut all = Vec::new();
    loop {
        let mut out = Vec::new();
        match r.poll(&mut out, 100) {
            Poll::Events(_) => all.extend(out),
            Poll::End => return all,
            other => panic!("{other:?}"),
        }
    }
}

fn names(dir: &Path, ext: &str) -> Vec<String> {
    files_with(dir, ext).unwrap()
}

#[test]
fn a_capture_rolls_on_receive_time_lists_every_segment_and_replays_exactly_what_was_written() {
    let dir = scratch("roll");
    let bytes = session(400, 1_000 * SEC, SEC / 100); // four seconds of records
    let (mut w, rec) = RawWriter::open(cfg(&dir, 1)).unwrap();
    assert_eq!(rec, Recovery::default());
    let n = feed(&mut w, &bytes);
    let (records, segments) = w.finish().unwrap();
    assert_eq!(records, n);
    assert!((4..=5).contains(&segments), "{segments}");
    assert!(names(&dir, PART_EXT).is_empty());
    let finals = names(&dir, FINAL_EXT);
    assert_eq!(finals.len() as u64, segments);
    assert!(
        finals.windows(2).all(|p| p[0] < p[1]),
        "names sort in time order: {finals:?}"
    );
    let listed = list(&dir).unwrap();
    assert_eq!(
        listed.iter().map(|e| e.file.clone()).collect::<Vec<_>>(),
        finals
    );
    assert_eq!(listed.iter().map(|e| e.records).sum::<u64>(), n);
    assert!(
        listed
            .iter()
            .all(|e| !e.recovered && !e.late && e.bytes > 0)
    );
    assert!(
        listed
            .windows(2)
            .all(|p| p[0].last_recv <= p[1].first_recv + SEC),
        "segments follow each other"
    );
    // Checked, and replayed: the same events, and the same ids for the same instruments across segments.
    let rep = verify(&dir).unwrap();
    assert!(rep.is_clean(), "{:?}", rep.problems);
    assert_eq!((rep.segments as u64, rep.records), (segments, n));
    assert_eq!(replayed(&dir), direct_events(&bytes));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn segment_numbers_go_on_after_a_reopen_and_an_empty_capture_has_nothing_to_replay() {
    let dir = scratch("reopen");
    let (w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    assert_eq!(w.finish().unwrap(), (0, 0));
    assert!(replayed(&dir).is_empty());
    let a = session(150, 2_000 * SEC, SEC / 100);
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    feed(&mut w, &a);
    w.finish().unwrap();
    let before = list(&dir).unwrap().len();
    let b = session(150, 2_010 * SEC, SEC / 100);
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    feed(&mut w, &b);
    w.finish().unwrap();
    let all = list(&dir).unwrap();
    assert!(all.len() > before);
    let mut files: Vec<_> = all.iter().map(|e| e.file.clone()).collect();
    let sorted = {
        let mut s = files.clone();
        s.sort();
        s
    };
    assert_eq!(files, sorted);
    files.dedup();
    assert_eq!(files.len(), all.len(), "no name was used twice");
    assert!(verify(&dir).unwrap().is_clean());
    assert_eq!(
        replayed(&dir).len(),
        direct_events(&a).len() + direct_events(&b).len()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn what_a_dead_writer_left_is_recovered_up_to_the_last_sync_and_nothing_after_is_invented() {
    let dir = scratch("crash");
    let bytes = session(300, 5_000 * SEC, SEC / 1_000); // all inside one segment
    let mut dec = DbnDecoder::new(&bytes[..]).unwrap();
    let (mut w, _) = RawWriter::open(cfg(&dir, 900)).unwrap();
    let mut written = 0u64;
    let mut synced = 0u64;
    while let Some(rec) = dec.decode_record_ref().unwrap() {
        w.write(&rec).unwrap();
        written += 1;
        if written == 400 {
            w.sync().unwrap();
            synced = written;
        }
    }
    assert!(written > synced && synced == 400);
    assert_eq!(names(&dir, PART_EXT).len(), 1);
    std::mem::forget(w); // the process dies: nothing is finished, nothing is flushed
    let (mut w2, rec) = RawWriter::open(cfg(&dir, 900)).unwrap();
    assert_eq!(rec.recovered.len(), 1);
    assert!(
        rec.recovered[0].1 >= synced && rec.recovered[0].1 <= written,
        "{rec:?}"
    );
    assert!(names(&dir, PART_EXT).is_empty());
    let listed = list(&dir).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].recovered);
    assert_eq!(listed[0].records, rec.recovered[0].1);
    // What came back is a prefix of what was written, record for record.
    let got = replayed(&dir);
    let want = direct_events(&bytes);
    assert!(!got.is_empty() && got.len() <= want.len());
    assert_eq!(got[..], want[..got.len()]);
    // The writer carries on, with a new segment, and the whole capture checks out.
    let more = session(50, 5_100 * SEC, SEC / 100);
    feed(&mut w2, &more);
    w2.finish().unwrap();
    assert!(verify(&dir).unwrap().is_clean());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_unfinished_file_cut_at_any_byte_recovers_a_clean_prefix() {
    let base = scratch("cuts");
    let src = base.join("src");
    let bytes = session(60, 7_000 * SEC, SEC / 1_000);
    let (mut w, _) = RawWriter::open(cfg(&src, 900)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    let seg = &list(&src).unwrap()[0];
    let whole = fs::read(src.join(&seg.file)).unwrap();
    let want = direct_events(&bytes);
    let mut last_kept = 0u64;
    let mut cuts: Vec<usize> = (0..whole.len()).step_by(37).collect();
    cuts.extend(whole.len().saturating_sub(300)..whole.len());
    cuts.sort_unstable();
    cuts.dedup();
    for cut in cuts {
        let dir = base.join(format!("cut{cut}"));
        fs::create_dir_all(&dir).unwrap();
        let stem = "raw-00000000000007000000000000-000001";
        fs::write(dir.join(format!("{stem}.part")), &whole[..cut]).unwrap();
        let (w, rec) = RawWriter::open(cfg(&dir, 900)).unwrap();
        drop(w);
        assert!(names(&dir, PART_EXT).is_empty(), "cut {cut}");
        let kept = rec.recovered.first().map_or(0, |r| r.1);
        assert!(
            kept >= last_kept,
            "cut {cut}: {kept} < {last_kept}: more bytes never recover fewer records"
        );
        last_kept = kept;
        if kept > 0 {
            let got = replayed(&dir);
            assert!(
                got.len() <= want.len(),
                "cut {cut}: kept {kept}, replayed {} but only {} were written",
                got.len(),
                want.len()
            );
            assert_eq!(got[..], want[..got.len()], "cut {cut}: a prefix");
            assert!(verify(&dir).unwrap().is_clean(), "cut {cut}");
        } else {
            assert!(list(&dir).unwrap().is_empty(), "cut {cut}: nothing to list");
        }
    }
    assert_eq!(
        last_kept, seg.records,
        "the whole file recovers every record"
    );
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn a_finished_segment_the_manifest_forgot_is_listed_when_the_capture_is_opened() {
    let dir = scratch("forgot");
    let bytes = session(300, 9_000 * SEC, SEC / 100);
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    let full = list(&dir).unwrap();
    // The process died after the rename and before the manifest line: the last line is gone.
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    let cut = text.trim_end().rsplit_once('\n').unwrap().0.to_owned() + "\n";
    fs::write(dir.join(MANIFEST), cut).unwrap();
    assert!(!verify(&dir).unwrap().is_clean());
    let (w, rec) = RawWriter::open(cfg(&dir, 1)).unwrap();
    drop(w);
    assert_eq!(rec.listed, [full.last().unwrap().file.clone()]);
    let now = list(&dir).unwrap();
    assert_eq!(now.len(), full.len());
    assert!(now.last().unwrap().late);
    assert_eq!(
        (now.last().unwrap().records, now.last().unwrap().fnv),
        (full.last().unwrap().records, full.last().unwrap().fnv)
    );
    assert!(verify(&dir).unwrap().is_clean());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn checking_a_capture_finds_every_kind_of_damage() {
    let dir = scratch("damage");
    let bytes = session(300, 11_000 * SEC, SEC / 100);
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    let files = names(&dir, FINAL_EXT);
    assert!(files.len() >= 3);
    let has = |r: &Report, needle: &str| r.problems.iter().any(|p| p.contains(needle));
    // A flipped byte.
    let p0 = dir.join(&files[0]);
    let mut b = fs::read(&p0).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0x55;
    fs::write(&p0, &b).unwrap();
    let r = verify(&dir).unwrap();
    assert!(
        has(&r, &format!("{}: checksum", files[0]))
            || has(&r, &format!("{}: cannot be read", files[0])),
        "{:?}",
        r.problems
    );
    b[mid] ^= 0x55;
    fs::write(&p0, &b).unwrap();
    assert!(verify(&dir).unwrap().is_clean());
    // A cut-short file.
    let p1 = dir.join(&files[1]);
    let b1 = fs::read(&p1).unwrap();
    fs::write(&p1, &b1[..b1.len() - 30]).unwrap();
    let r = verify(&dir).unwrap();
    assert!(has(&r, &format!("{}:", files[1])), "{:?}", r.problems);
    fs::write(&p1, &b1).unwrap();
    // A missing file, a stray finished file, a leftover unfinished one.
    let p2 = dir.join(&files[2]);
    let b2 = fs::read(&p2).unwrap();
    fs::remove_file(&p2).unwrap();
    assert!(has(&verify(&dir).unwrap(), "listed but missing"));
    assert!(matches!(CaptureReplay::open(&dir), Err(Error::Damaged(m)) if m.contains("missing")));
    fs::write(&p2, &b2).unwrap();
    fs::write(dir.join("raw-99999999999999999999-000099.dbn.zst"), &b2).unwrap();
    assert!(has(&verify(&dir).unwrap(), "not listed"));
    fs::remove_file(dir.join("raw-99999999999999999999-000099.dbn.zst")).unwrap();
    fs::write(dir.join("raw-00000000000000000001-000098.part"), b"half").unwrap();
    let r = verify(&dir).unwrap();
    assert!(has(&r, "unfinished"), "{:?}", r.problems);
    assert!(
        matches!(CaptureReplay::open(&dir), Err(Error::Damaged(m)) if m.contains("unfinished"))
    );
    fs::remove_file(dir.join("raw-00000000000000000001-000098.part")).unwrap();
    assert!(verify(&dir).unwrap().is_clean());
    // The manifest saying something else than the file.
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    let first = &list(&dir).unwrap()[0];
    fs::write(
        dir.join(MANIFEST),
        text.replacen(
            &format!("records={}", first.records),
            &format!("records={}", first.records + 1),
            1,
        ),
    )
    .unwrap();
    assert!(has(&verify(&dir).unwrap(), "the manifest says"));
    fs::write(dir.join(MANIFEST), &text).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_manifest_that_is_not_ours_is_refused_not_guessed_at() {
    let dir = scratch("manifest");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(MANIFEST), "tfcap 99\n").unwrap();
    assert!(matches!(list(&dir), Err(Error::Manifest(_))));
    assert!(RawWriter::open(cfg(&dir, 1)).is_err());
    fs::write(
        dir.join(MANIFEST),
        "tfcap 1\nsegment x records=notanumber\n",
    )
    .unwrap();
    assert!(matches!(list(&dir), Err(Error::Manifest(m)) if m.contains("segment")));
    fs::write(dir.join(MANIFEST), "tfcap 1\nsomething else\n").unwrap();
    assert!(list(&dir).is_err());
    fs::write(dir.join(MANIFEST), "tfcap 1\n\n").unwrap();
    assert!(list(&dir).unwrap().is_empty());
    for e in [Error::Manifest("x".into()), Error::Damaged("y".into())] {
        assert!(!e.to_string().is_empty());
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_replay_is_a_provider_that_filters_pages_and_ends() {
    let dir = scratch("provider");
    let bytes = session(200, 13_000 * SEC, SEC / 100);
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    let all = direct_events(&bytes);
    let mut r = CaptureReplay::open(&dir).unwrap();
    assert_eq!(r.capabilities().provider, ProviderId::Databento);
    r.connect().unwrap();
    // Pages never exceed what was asked for.
    let mut out = Vec::new();
    assert_eq!(r.poll(&mut out, 7), Poll::Events(7));
    assert_eq!(out.len(), 7);
    assert_eq!(out[..], all[..7]);
    // Only trades, from here on.
    r.subscribe(&Subscription::all(Channels::TRADES)).unwrap();
    let mut rest = Vec::new();
    while let Poll::Events(_) = r.poll(&mut rest, 50) {}
    assert!(rest.iter().all(|e| matches!(e, Event::Trade(_))));
    let want_trades = all[7..]
        .iter()
        .filter(|e| matches!(e, Event::Trade(_)))
        .count();
    assert_eq!(rest.len(), want_trades);
    assert!(
        r.skipped > 0 && r.notices > 0,
        "quotes were set aside and heartbeats counted: {} {}",
        r.skipped,
        r.notices
    );
    assert_eq!(r.poll(&mut Vec::new(), 10), Poll::End);
    assert!(r.reconnect(None).is_err());
    r.disconnect();
    assert!(r.instruments().len() > 1);
    // A list of instruments.
    let mut r = CaptureReplay::open(&dir).unwrap();
    r.subscribe(&Subscription::list(Channels::ALL, vec![0]))
        .unwrap();
    let mut only = Vec::new();
    while let Poll::Events(_) = r.poll(&mut only, 1_000) {}
    assert!(!only.is_empty() && only.iter().all(|e| e.instrument() == 0));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn zero_share_prints_are_dropped_in_a_replay_unless_asked_for() {
    let dir = scratch("zero");
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(1)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        e.encode_record(&trade(82, 5, 100_000, 5 * D, 0, 1))
            .unwrap();
        e.encode_record(&trade(82, 5, 200_000, 5 * D, 4, 2))
            .unwrap();
    }
    let (mut w, _) = RawWriter::open(cfg(&dir, 900)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    assert_eq!(
        replayed(&dir).len(),
        1,
        "the capture keeps both records, the replay maps one"
    );
    let mut r = CaptureReplay::open(&dir).unwrap().keep_zero_size(true);
    let mut out = Vec::new();
    while let Poll::Events(_) = r.poll(&mut out, 10) {}
    assert_eq!(out.len(), 2);
    assert_eq!(verify(&dir).unwrap().records, 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_capture_becomes_a_normalized_tape_with_the_same_events() {
    let dir = scratch("tape");
    let bytes = session(200, 15_000 * SEC, SEC / 100);
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    let (n, tape) = to_tape(&dir, Cursor::new(Vec::new())).unwrap();
    let want = direct_events(&bytes);
    assert_eq!(n as usize, want.len());
    let mut reader = TapeReader::open(Cursor::new(tape.into_inner())).unwrap();
    assert_eq!(reader.events() as usize, want.len());
    let got: Vec<Event> = reader.scan(0).map(|e| e.unwrap()).collect();
    assert_eq!(got, want);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn records_that_arrive_a_little_out_of_order_are_kept_and_the_clock_never_splits_one_apart() {
    let dir = scratch("order");
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(1)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        for (i, t) in [10 * SEC, 11 * SEC, 10 * SEC + 5, 12 * SEC, 11 * SEC + 7]
            .into_iter()
            .enumerate()
        {
            e.encode_record(&trade(81, 1, t, 5 * D, 1, i as u32))
                .unwrap();
        }
    }
    let (mut w, _) = RawWriter::open(cfg(&dir, 1)).unwrap();
    assert_eq!(feed(&mut w, &bytes), 5);
    let (records, _) = w.finish().unwrap();
    assert_eq!(records, 5);
    assert_eq!(verify(&dir).unwrap().records, 5);
    assert_eq!(
        replayed(&dir),
        direct_events(&bytes),
        "arrival order, whatever the receive times"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn counts_and_defaults() {
    let c = Config::new("x", "XNAS.BASIC");
    assert_eq!((c.segment_secs, c.level), (900, 3));
    let dir = scratch("counts");
    let (mut w, _) = RawWriter::open(cfg(&dir, 900)).unwrap();
    assert_eq!(w.counts(), (0, 0));
    w.sync().unwrap();
    feed(&mut w, &session(5, 20_000 * SEC, SEC / 100));
    assert_eq!(w.counts().0, 5 + 5 + 1 + 1);
    let (r, s) = w.finish().unwrap();
    assert_eq!((r, s), (12, 1));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_segments_last_receive_time_is_the_latest_one_seen_not_the_last_written() {
    let dir = scratch("lastrecv");
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(1)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        for (i, t) in [100_000u64, 900_000, 500_000].into_iter().enumerate() {
            e.encode_record(&trade(81, 1, t, 5 * D, 1, i as u32))
                .unwrap();
        }
    }
    let (mut w, _) = RawWriter::open(cfg(&dir, 900)).unwrap();
    feed(&mut w, &bytes);
    w.finish().unwrap();
    let e = &list(&dir).unwrap()[0];
    assert_eq!(
        (e.first_recv, e.last_recv, e.records),
        (100_000, 900_000, 3)
    );
    assert!(verify(&dir).unwrap().is_clean());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn each_thing_the_manifest_records_is_checked_on_its_own() {
    let dir = scratch("tamper");
    let (mut w, _) = RawWriter::open(cfg(&dir, 900)).unwrap();
    feed(&mut w, &session(40, 17_000 * SEC, SEC / 100));
    w.finish().unwrap();
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    let e = list(&dir).unwrap().remove(0);
    let cases = [
        (
            format!("bytes={}", e.bytes),
            format!("bytes={}", e.bytes + 1),
            "bytes, the manifest says",
        ),
        (
            format!("fnv={:016x}", e.fnv),
            format!("fnv={:016x}", e.fnv ^ 1),
            "checksum",
        ),
        (
            format!("first_recv={}", e.first_recv),
            format!("first_recv={}", e.first_recv + 1),
            "receive times",
        ),
        (
            format!("last_recv={}", e.last_recv),
            format!("last_recv={}", e.last_recv + 1),
            "receive times",
        ),
        (
            format!("records={}", e.records),
            format!("records={}", e.records + 1),
            "records, the manifest says",
        ),
    ];
    for (from, to, expect) in cases {
        fs::write(dir.join(MANIFEST), text.replacen(&from, &to, 1)).unwrap();
        let r = verify(&dir).unwrap();
        assert_eq!(r.problems.len(), 1, "{from}: {:?}", r.problems);
        assert!(r.problems[0].contains(expect), "{from}: {:?}", r.problems);
    }
    fs::write(dir.join(MANIFEST), &text).unwrap();
    assert!(verify(&dir).unwrap().is_clean());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_replay_that_ended_because_a_file_could_not_be_read_says_so() {
    let dir = scratch("failure");
    let (mut w, _) = RawWriter::open(cfg(&dir, 2)).unwrap();
    feed(&mut w, &session(8, 1_000_000_000, 500_000_000));
    w.finish().unwrap();
    let files: Vec<PathBuf> = list(&dir)
        .unwrap()
        .iter()
        .map(|e| dir.join(&e.file))
        .collect();
    assert!(files.len() >= 3);
    let drain = |files: Vec<PathBuf>| {
        let mut r = CaptureReplay::from_files(files);
        let mut n = 0;
        loop {
            let mut out = Vec::new();
            match r.poll(&mut out, 100) {
                Poll::Events(k) => n += k,
                _ => break,
            }
        }
        (n, r.failure().map(str::to_owned))
    };
    // Whole: it ends and there is nothing to say.
    let (all, why) = drain(files.clone());
    assert!(all > 10 && why.is_none());
    // A file missing after the first, and one that is not zstd, in the middle: the stream ends there and says why.
    let mut gone = files.clone();
    gone.insert(2, dir.join("missing.dbn.zst"));
    let (n, why) = drain(gone);
    assert!(n < all && why.unwrap().contains("missing.dbn.zst"));
    let junk = dir.join("junk.dbn.zst");
    fs::write(&junk, b"not zstd at all").unwrap();
    let mut bad = files.clone();
    bad.insert(1, junk);
    let (n, why) = drain(bad);
    assert!(n < all && why.unwrap().contains("junk.dbn.zst"));
    // A file cut short in its compressed stream.
    let cut = &files[1];
    let b = fs::read(cut).unwrap();
    fs::write(cut, &b[..b.len() / 2]).unwrap();
    let (n, why) = drain(files);
    assert!(n < all && why.is_some());
}

#[test]
fn a_first_file_that_is_missing_and_a_record_that_cannot_be_decoded_each_say_so() {
    let dir = scratch("failure-first");
    let (mut w, _) = RawWriter::open(cfg(&dir, 2)).unwrap();
    feed(&mut w, &session(8, 1_000_000_000, 500_000_000));
    w.finish().unwrap();
    let files: Vec<PathBuf> = list(&dir)
        .unwrap()
        .iter()
        .map(|e| dir.join(&e.file))
        .collect();
    let drain = |files: Vec<PathBuf>| {
        let mut r = CaptureReplay::from_files(files);
        let mut n = 0;
        loop {
            let mut out = Vec::new();
            match r.poll(&mut out, 100) {
                Poll::Events(k) => n += k,
                _ => break,
            }
        }
        (n, r.failure().map(str::to_owned))
    };
    // The very first file cannot be opened: nothing comes out, and the replay says which file it was.
    let (n, why) = drain(vec![dir.join("missing.dbn.zst")]);
    assert_eq!(n, 0);
    assert!(why.unwrap().contains("missing.dbn.zst"));
    // A file that is a whole zstd stream with a well-formed header and a first record whose length is zero.
    let mut raw = zstd::decode_all(fs::File::open(&files[0]).unwrap()).unwrap();
    let meta = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
    raw[8 + meta] = 0;
    let bad = dir.join("bad-record.dbn.zst");
    fs::write(&bad, zstd::encode_all(&raw[..], 0).unwrap()).unwrap();
    let (n, why) = drain(vec![bad.clone(), files[1].clone()]);
    assert_eq!(n, 0, "nothing after the record that cannot be read");
    assert!(why.is_some());
}
