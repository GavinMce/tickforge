use std::ffi::c_char;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{
    FlagSet, MappingInterval, MetadataBuilder, RecordHeader, SType, Schema, SymbolMapping,
    TradeMsg, rtype,
};
use tf_core::Event;
use tf_manifest::{Digest, sha256};
use tf_provider::{Poll, Provider};

use super::*;

static N: AtomicU32 = AtomicU32::new(0);

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tf-history-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn day_of(date: &str) -> time::Date {
    let p: Vec<i32> = date.split('-').map(|x| x.parse().unwrap()).collect();
    time::Date::from_calendar_date(p[0], time::Month::try_from(p[1] as u8).unwrap(), p[2] as u8)
        .unwrap()
}

/// `(symbol, raw instrument id)` and the prices of a trade of each, one trade per entry of `trades`:
/// `(raw id, ts_recv, price)`.
fn write_day(
    dir: &Path,
    dataset: &str,
    schema: &str,
    date: &str,
    symbols: &[(&str, u32)],
    trades: &[(u32, u64, i64)],
) {
    let md = MetadataBuilder::new()
        .dataset(dataset.to_owned())
        .schema(Some(Schema::Trades))
        .start(0)
        .stype_in(Some(SType::RawSymbol))
        .stype_out(SType::InstrumentId)
        .symbols(symbols.iter().map(|s| s.0.to_owned()).collect())
        .mappings(
            symbols
                .iter()
                .map(|(s, id)| SymbolMapping {
                    raw_symbol: (*s).to_owned(),
                    intervals: vec![MappingInterval {
                        start_date: day_of(date),
                        end_date: day_of(date).next_day().unwrap(),
                        symbol: id.to_string(),
                    }],
                })
                .collect(),
        )
        .build();
    let mut bytes = Vec::new();
    {
        let mut enc = DbnEncoder::new(&mut bytes, &md).unwrap();
        for (i, (id, ts, px)) in trades.iter().enumerate() {
            let t = TradeMsg {
                hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, 81, *id, *ts),
                price: *px,
                size: 10,
                action: b'T' as c_char,
                side: b'N' as c_char,
                flags: FlagSet::empty(),
                depth: 0,
                ts_recv: *ts,
                ts_in_delta: 0,
                sequence: i as u32,
            };
            enc.encode_record(&t).unwrap();
        }
    }
    let z = zstd::encode_all(&bytes[..], 3).unwrap();
    let d = dir.join(dataset).join(schema);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(format!("{date}{EXT}")), z).unwrap();
}

const D: i64 = 1_000_000_000;

/// Three days: AAA and BBB throughout, CCC only on the first, DDD and EEE from the third.
fn three_days(dir: &Path) {
    write_day(
        dir,
        "XNAS.BASIC",
        "trades",
        "2026-09-30",
        &[("AAA", 100), ("BBB", 200), ("CCC", 300)],
        &[(100, 10, 10 * D), (200, 20, 20 * D), (300, 30, 30 * D)],
    );
    write_day(
        dir,
        "XNAS.BASIC",
        "trades",
        "2026-10-01",
        &[("AAA", 100), ("BBB", 200)],
        &[(200, 110, 21 * D), (100, 120, 11 * D)],
    );
    write_day(
        dir,
        "XNAS.BASIC",
        "trades",
        "2026-10-02",
        &[("AAA", 100), ("BBB", 200), ("DDD", 400), ("EEE", 500)],
        &[(100, 210, 12 * D), (400, 220, 40 * D), (500, 230, 50 * D)],
    );
}

#[test]
fn costs_in_dollars_become_millionths_rounded_up() {
    for (text, want) in [
        ("0.190103441477", Some(190_104)),
        ("1", Some(1_000_000)),
        ("0.5", Some(500_000)),
        ("12.000000", Some(12_000_000)),
        ("0.0000001", Some(1)),
        ("0.00000010", Some(1)),
        ("0.00000000", Some(0)),
        ("0.000000999", Some(1)),
        ("0.000000", Some(0)),
        (" 3.25\n", Some(3_250_000)),
        ("0.0000000000", Some(0)),
        ("", None),
        (".", None),
        ("abc", None),
        ("1.2x", None),
        ("-1", None),
    ] {
        assert_eq!(micros_of(text), want, "{text:?}");
    }
}

#[test]
fn the_manifest_round_trips_and_a_bad_one_names_its_line() {
    let d = tmp();
    three_days(&d);
    let (store, _) = index(&d, "XNAS.BASIC", "trades", "ALL_SYMBOLS").unwrap();
    let text = store.render();
    assert!(text.starts_with("history store v1\n"));
    assert_eq!(Store::parse(&text).unwrap(), store);
    assert_eq!(Store::read(&d).unwrap(), store);
    let lines: Vec<&str> = text.lines().collect();
    let day = lines
        .iter()
        .find(|l| l.starts_with("day "))
        .unwrap()
        .to_string();
    for (bad, want) in [
        (
            text.replacen("history store v1", "history store v2", 1),
            "first line",
        ),
        (
            text.replacen(&day, &day.replace("2026-09-30", "2026-9-30"), 1),
            "YYYY-MM-DD",
        ),
        (text.replacen(&day, &format!("{day} extra"), 1), "10 fields"),
        (
            text.replacen(&day, &day.replacen(" ALL_SYMBOLS", "", 1), 1),
            "8 fields",
        ),
        (
            text.replacen(&day, "nonsense line", 1),
            "expected a `note` or a `day`",
        ),
        (format!("{text}note onlykey\n"), "note KEY TEXT"),
    ] {
        let e = Store::parse(&bad).unwrap_err();
        assert!(e.to_string().contains(want), "{want}: {e}");
    }
    // A checksum that is not hex, a cost that is not a number, and days out of order or repeated.
    let sha = store.days[0].sha256.clone();
    assert!(
        Store::parse(&text.replacen(&sha, &sha.to_uppercase(), 1))
            .unwrap_err()
            .to_string()
            .contains("64 lowercase")
    );
    let costed = day.replacen(" - ", " x ", 1);
    assert!(
        Store::parse(&text.replacen(&day, &costed, 1))
            .unwrap_err()
            .to_string()
            .contains("cost")
    );
    let last = lines
        .iter()
        .rev()
        .find(|l| l.starts_with("day "))
        .unwrap()
        .to_string();
    let mut twice = text.clone();
    twice.push_str(&format!("{last}\n"));
    assert!(
        Store::parse(&twice)
            .unwrap_err()
            .to_string()
            .contains("in order and listed once")
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn indexing_reads_the_files_and_says_what_the_store_is_not() {
    let d = tmp();
    three_days(&d);
    // A cost beside the second day only.
    fs::write(
        d.join("XNAS.BASIC/trades/2026-10-01.cost"),
        "0.190103441477\n",
    )
    .unwrap();
    let (store, rep) = index(&d, "XNAS.BASIC", "trades", "AAA,BBB,CCC,DDD").unwrap();
    assert_eq!((rep.days, rep.records, rep.empty.len()), (3, 8, 0));
    assert_eq!(store.days.len(), 3);
    let a = &store.days[0];
    let bytes = fs::read(a.path(&d)).unwrap();
    assert_eq!(a.bytes, bytes.len() as u64, "the size on disk");
    assert_eq!(
        a.sha256,
        Digest(sha256(&bytes)).hex(),
        "checked against the one-shot hash of the whole file"
    );
    assert_eq!(
        (a.records, a.symbol_count, a.symbols.as_str()),
        (3, 3, "AAA,BBB,CCC,DDD")
    );
    assert_eq!(store.days[1].symbol_count, 2);
    assert_eq!(store.days[2].symbol_count, 4);
    assert_eq!(
        store.days.iter().map(|d| d.cost_micros).collect::<Vec<_>>(),
        [None, Some(190_104), None]
    );
    assert_eq!(store.cost(), (190_104, 2));
    // What it is not: the borrow flags, and (measured, from the files' own mappings) whether names that left are
    // present: CCC is on the first day and not the last.
    let note = |k: &str| {
        store
            .notes
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert!(note("borrow_flags").contains("not point in time"));
    let s = note("survivorship:XNAS.BASIC/trades");
    assert!(
        s.contains(
            "3 symbols on 2026-09-30, 4 on 2026-10-02; 1 on the first day are not on the last"
        ) && s.contains("are present"),
        "{s}"
    );
    // The description prints the symbol count per day, the cost and the notes.
    let text = describe(&store);
    assert!(
        text.contains("XNAS.BASIC trades: 3 days, 2026-09-30 to 2026-10-02, 8 records"),
        "{text}"
    );
    assert!(
        text.contains("2026-10-01") && text.contains("$0.190104"),
        "{text}"
    );
    assert!(
        text.contains("pulled for $0.190104 (2 days with no cost recorded)"),
        "{text}"
    );
    assert!(text.contains("note borrow_flags:"), "{text}");
    // Another schema is added without touching the first; indexing again changes nothing.
    write_day(
        &d,
        "XNAS.BASIC",
        "tcbbo",
        "2026-10-02",
        &[("AAA", 100)],
        &[(100, 5, D)],
    );
    let (both, _) = index(&d, "XNAS.BASIC", "tcbbo", "AAA").unwrap();
    assert_eq!(both.kinds().len(), 2);
    assert_eq!(both.of("XNAS.BASIC", "trades").count(), 3);
    let note = both
        .notes
        .iter()
        .find(|(k, _)| k == "survivorship:XNAS.BASIC/tcbbo")
        .unwrap();
    assert!(note.1.contains("only one day"), "{note:?}");
    let (again, _) = index(&d, "XNAS.BASIC", "trades", "AAA,BBB,CCC,DDD").unwrap();
    assert_eq!(again.days, both.days);
    assert_eq!(
        again
            .notes
            .iter()
            .filter(|(k, _)| k == "borrow_flags")
            .count(),
        1
    );
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_day_with_no_records_is_reported_and_a_store_with_none_is_not_one() {
    let d = tmp();
    write_day(
        &d,
        "XNAS.BASIC",
        "trades",
        "2026-09-30",
        &[("AAA", 100)],
        &[(100, 10, D)],
    );
    write_day(&d, "XNAS.BASIC", "trades", "2026-10-01", &[], &[]);
    let (_, rep) = index(&d, "XNAS.BASIC", "trades", "ALL_SYMBOLS").unwrap();
    assert_eq!(rep.empty, ["2026-10-01"]);
    assert!(Store::read(&tmp()).is_err(), "no manifest");
    assert!(index(&d, "bad dataset", "trades", "ALL").is_err());
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn verify_names_every_file_that_is_missing_short_longer_altered_or_unlisted() {
    let d = tmp();
    three_days(&d);
    index(&d, "XNAS.BASIC", "trades", "ALL_SYMBOLS").unwrap();
    let r = verify(&d).unwrap();
    assert!(r.is_clean(), "{:?}", r.problems);
    assert_eq!(r.days, 3);
    let size: u64 = Store::read(&d).unwrap().days.iter().map(|d| d.bytes).sum();
    assert_eq!(r.bytes, size);

    let f = |date: &str| d.join(format!("XNAS.BASIC/trades/{date}{EXT}"));
    // Short: cut a few bytes off the end.
    let a = fs::read(f("2026-09-30")).unwrap();
    fs::write(f("2026-09-30"), &a[..a.len() - 5]).unwrap();
    // Altered: one byte changed, the size the same.
    let mut b = fs::read(f("2026-10-01")).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0x01;
    fs::write(f("2026-10-01"), &b).unwrap();
    // Missing.
    fs::remove_file(f("2026-10-02")).unwrap();
    // Not listed.
    write_day(
        &d,
        "XNAS.BASIC",
        "trades",
        "2026-10-05",
        &[("AAA", 100)],
        &[(100, 1, D)],
    );
    let r = verify(&d).unwrap();
    let text = r.problems.join("\n");
    assert!(
        text.contains("2026-09-30.dbn.zst: short: ") && text.contains("the manifest says"),
        "{text}"
    );
    assert!(
        text.contains("2026-10-01.dbn.zst: altered: checksum"),
        "{text}"
    );
    assert!(
        text.contains("2026-10-02.dbn.zst: listed but missing"),
        "{text}"
    );
    assert!(
        text.contains("2026-10-05.dbn.zst: present but not listed"),
        "{text}"
    );
    assert_eq!(r.problems.len(), 4, "{text}");
    // Longer than listed.
    let mut c = fs::read(f("2026-09-30")).unwrap();
    c.extend_from_slice(&a[a.len() - 5..]);
    c.push(0);
    fs::write(f("2026-09-30"), &c).unwrap();
    assert!(
        verify(&d)
            .unwrap()
            .problems
            .join("\n")
            .contains("longer than listed")
    );
    let _ = fs::remove_dir_all(&d);
}

fn all(mut p: impl Provider) -> Vec<Event> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match p.poll(&mut buf, 100) {
            Poll::Events(_) => out.extend_from_slice(&buf),
            _ => break,
        }
    }
    out
}

fn trades(events: &[Event]) -> Vec<(u32, u64, i64)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Trade(t) => Some((t.hdr.instrument, t.hdr.ts_recv, t.px.raw())),
            _ => None,
        })
        .collect()
}

#[test]
fn stored_days_replay_in_date_order_with_ids_numbered_as_a_capture_numbers_them() {
    let d = tmp();
    three_days(&d);
    index(&d, "XNAS.BASIC", "trades", "ALL_SYMBOLS").unwrap();
    let mut rp = replay(&d, "XNAS.BASIC", "trades", None, None).unwrap();
    let mut buf = Vec::new();
    let mut events = Vec::new();
    while let Poll::Events(_) = rp.poll(&mut buf, 100) {
        events.append(&mut buf);
    }
    // The instruments are named from the days' own metadata, in the order they were first seen.
    let ids = rp.instruments();
    let names: Vec<Option<&str>> = (0..ids.len() as u32).map(|i| ids.symbol(i)).collect();
    assert_eq!(
        names,
        [
            Some("AAA"),
            Some("BBB"),
            Some("CCC"),
            Some("DDD"),
            Some("EEE")
        ]
    );
    let got = trades(&events);
    // Raw ids 100, 200, 300 are numbered 0, 1, 2 in the order first seen, and carry to the days after: raw 200 and
    // 100 on the second day are 1 and 0; raw 400, new on the third day, is 3.
    assert_eq!(
        got,
        [
            (0, 10, 10 * D),
            (1, 20, 20 * D),
            (2, 30, 30 * D),
            (1, 110, 21 * D),
            (0, 120, 11 * D),
            (0, 210, 12 * D),
            (3, 220, 40 * D),
            (4, 230, 50 * D),
        ]
    );
    assert!(got.windows(2).all(|w| w[0].1 <= w[1].1), "in time order");
    // A range of days: the ids start from the first day played.
    let mid = trades(&all(replay(
        &d,
        "XNAS.BASIC",
        "trades",
        Some("2026-10-01"),
        Some("2026-10-01"),
    )
    .unwrap()));
    assert_eq!(mid, [(0, 110, 21 * D), (1, 120, 11 * D)]);
    let late = trades(&all(replay(
        &d,
        "XNAS.BASIC",
        "trades",
        Some("2026-10-02"),
        None,
    )
    .unwrap()));
    assert_eq!(late.len(), 3);
    // The files are the ones a caller that takes a list (the host's replay_files) plays.
    let files = files(&d, "XNAS.BASIC", "trades", None, None).unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(trades(&all(CaptureReplay::from_files(files))), got);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_missing_or_short_file_or_an_empty_range_is_not_replayed() {
    let d = tmp();
    three_days(&d);
    index(&d, "XNAS.BASIC", "trades", "ALL_SYMBOLS").unwrap();
    let e = replay(&d, "XNAS.BASIC", "trades", Some("2027-01-01"), None)
        .err()
        .unwrap();
    assert!(
        e.to_string().contains("no days of XNAS.BASIC trades"),
        "{e}"
    );
    assert!(replay(&d, "XNAS.BASIC", "tcbbo", None, None).is_err());
    let f = d.join("XNAS.BASIC/trades/2026-10-01.dbn.zst");
    let b = fs::read(&f).unwrap();
    fs::write(&f, &b[..b.len() - 1]).unwrap();
    let e = replay(&d, "XNAS.BASIC", "trades", None, None)
        .err()
        .unwrap();
    assert!(
        e.to_string().contains("2026-10-01")
            && e.to_string().contains("not the file that was stored"),
        "{e}"
    );
    // A file longer than listed is not the stored one either.
    let mut longer = b.clone();
    longer.push(0);
    fs::write(&f, &longer).unwrap();
    assert!(replay(&d, "XNAS.BASIC", "trades", None, None).is_err());
    fs::write(&f, &b[..b.len() - 1]).unwrap();
    // The day before it can still be played alone.
    assert!(replay(&d, "XNAS.BASIC", "trades", None, Some("2026-09-30")).is_ok());
    fs::remove_file(&f).unwrap();
    let e = replay(&d, "XNAS.BASIC", "trades", None, None)
        .err()
        .unwrap();
    assert!(e.to_string().contains("listed but missing"), "{e}");
    let _ = fs::remove_dir_all(&d);
}
