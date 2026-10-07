use std::ffi::c_char;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{
    Cmbp1Msg, ConsolidatedBidAskPair, FlagSet, MappingInterval, MetadataBuilder, OhlcvMsg,
    RecordHeader, SType, SymbolMapping, UNDEF_PRICE, rtype,
};

use crate::screen::{BarRow, Limits, SpreadRow, bars, select, spreads};

const D: i64 = 1_000_000_000;
static N: AtomicU32 = AtomicU32::new(0);

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tf-screen-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn metadata(symbols: &[(&str, u32)]) -> dbn::Metadata {
    let day = time::Date::from_calendar_date(2026, time::Month::May, 4).unwrap();
    MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
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
                        start_date: day,
                        end_date: day.next_day().unwrap(),
                        symbol: id.to_string(),
                    }],
                })
                .collect(),
        )
        .build()
}

/// `(raw id, open, high, low, close, volume)` per bar.
type Bar = (u32, i64, i64, i64, i64, u64);

fn bar_file(dir: &Path, name: &str, symbols: &[(&str, u32)], recs: &[Bar]) -> PathBuf {
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &metadata(symbols)).unwrap();
        for (i, (id, open, high, low, close, volume)) in recs.iter().enumerate() {
            e.encode_record(&OhlcvMsg {
                hd: RecordHeader::new::<OhlcvMsg>(rtype::OHLCV_1M, 81, *id, 60 * i as u64),
                open: *open,
                high: *high,
                low: *low,
                close: *close,
                volume: *volume,
            })
            .unwrap();
        }
    }
    let path = dir.join(name);
    fs::write(&path, zstd::encode_all(&bytes[..], 3).unwrap()).unwrap();
    path
}

/// `(raw id, bid, ask)` per quote, one a second.
fn quote_file(
    dir: &Path,
    name: &str,
    symbols: &[(&str, u32)],
    quotes: &[(u32, i64, i64)],
) -> PathBuf {
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &metadata(symbols)).unwrap();
        for (i, (id, bid, ask)) in quotes.iter().enumerate() {
            let ts = 1_000_000_000 * (i as u64 + 1);
            e.encode_record(&Cmbp1Msg {
                hd: RecordHeader::new::<Cmbp1Msg>(rtype::CMBP_1, 88, *id, ts),
                price: 0,
                size: 0,
                action: b'A' as c_char,
                side: b'N' as c_char,
                flags: FlagSet::empty(),
                _reserved1: [0],
                ts_recv: ts,
                ts_in_delta: 0,
                _reserved2: [0; 4],
                levels: [ConsolidatedBidAskPair {
                    bid_px: *bid,
                    ask_px: *ask,
                    bid_sz: 100,
                    ask_sz: 100,
                    bid_pb: 81,
                    _reserved1: [0; 2],
                    ask_pb: 82,
                    _reserved2: [0; 2],
                }],
            })
            .unwrap();
        }
    }
    let path = dir.join(name);
    fs::write(&path, zstd::encode_all(&bytes[..], 3).unwrap()).unwrap();
    path
}

#[test]
fn bars_become_one_row_per_symbol_across_days_by_the_files_own_names() {
    let dir = tmp();
    // The same name carries different raw ids on the two days, and one id on the second day has no name.
    let day1 = bar_file(
        &dir,
        "a.dbn.zst",
        &[("AAA", 10), ("BBB", 20)],
        &[
            (10, 10 * D, 11 * D, 9 * D, 10 * D + D / 2, 1_000),
            (20, 50 * D, 50 * D, 50 * D, 50 * D, 300),
            (10, 10 * D + D / 2, 12 * D, 10 * D, 11 * D, 2_000),
            // Nothing traded in the bar, and a bar with no price: neither is counted.
            (10, 11 * D, 11 * D, 11 * D, 11 * D, 0),
            (10, UNDEF_PRICE, UNDEF_PRICE, UNDEF_PRICE, UNDEF_PRICE, 5),
        ],
    );
    let day2 = bar_file(
        &dir,
        "b.dbn.zst",
        &[("AAA", 77)],
        &[
            (77, 11 * D, 13 * D, 8 * D, 12 * D, 500),
            (99, D, D, D, D, 1),
        ],
    );
    let rows = bars(&[day1, day2]).unwrap();
    assert_eq!(
        rows,
        [
            BarRow {
                symbol: "AAA".into(),
                bars: 3,
                first_open: 10 * D,
                last_close: 12 * D,
                low: 8 * D,
                high: 13 * D,
                shares: 3_500,
                // 1,000 x 10.50 + 2,000 x 11 + 500 x 12.
                dollars: (10_500 + 22_000 + 6_000) * D as u128,
            },
            BarRow {
                symbol: "BBB".into(),
                bars: 1,
                first_open: 50 * D,
                last_close: 50 * D,
                low: 50 * D,
                high: 50 * D,
                shares: 300,
                dollars: 15_000 * D as u128,
            },
        ]
    );
}

#[test]
fn a_file_that_is_not_there_or_not_dbn_is_an_error() {
    let dir = tmp();
    assert!(bars(&[dir.join("nothing.dbn.zst")]).is_err());
    let junk = dir.join("junk.dbn.zst");
    fs::write(&junk, b"not zstd").unwrap();
    assert!(bars(std::slice::from_ref(&junk)).is_err());
    assert!(spreads(&[junk]).is_err());
}

#[test]
fn spreads_are_the_quoted_spread_over_the_mid_at_each_two_sided_quote() {
    let dir = tmp();
    let f = quote_file(
        &dir,
        "q.dbn.zst",
        &[("AAA", 10), ("BBB", 20), ("CCC", 30)],
        &[
            // 0.02 over 100.00: 2 bp, which is 200 hundredths.
            (10, 99_990_000_000, 100_010_000_000),
            // 0.04 over 100.00: 4 bp.
            (10, 99_980_000_000, 100_020_000_000),
            // Locked, crossed, one-sided and empty quotes are not counted.
            (10, 100 * D, 100 * D),
            (10, 101 * D, 100 * D),
            (10, 0, 100 * D),
            (10, 100 * D, 0),
            // 0.01 over 10.00 (mid 9.995..): about 10 bp.
            (20, 9_995_000_000, 10_005_000_000),
            // Only a crossed quote: no row.
            (30, 5 * D, 4 * D),
        ],
    );
    let rows = spreads(&[f]).unwrap();
    assert_eq!(
        rows,
        [
            SpreadRow {
                symbol: "AAA".into(),
                quotes: 2,
                sum_bp_x100: 200 + 400
            },
            SpreadRow {
                symbol: "BBB".into(),
                quotes: 1,
                sum_bp_x100: 1_000
            },
        ]
    );
    assert_eq!(rows[0].mean_bp_x100(), Some(300));
    assert_eq!(SpreadRow::default().mean_bp_x100(), None);
}

fn row(symbol: &str, bars: u64, close: i64, dollars: u128) -> BarRow {
    BarRow {
        symbol: symbol.into(),
        bars,
        last_close: close * D,
        dollars: dollars * D as u128,
        ..BarRow::default()
    }
}

#[test]
fn a_screen_keeps_the_symbols_that_meet_every_limit_that_is_set() {
    let b = [
        row("AAA", 390, 10, 5_000_000),
        row("BBB", 390, 200, 50_000_000),
        row("CCC", 20, 12, 90_000),
        row("DDD", 390, 3, 1_000_000),
    ];
    let s = [
        SpreadRow {
            symbol: "AAA".into(),
            quotes: 4,
            sum_bp_x100: 4 * 300,
        },
        SpreadRow {
            symbol: "BBB".into(),
            quotes: 1,
            sum_bp_x100: 50,
        },
        SpreadRow {
            symbol: "CCC".into(),
            quotes: 2,
            sum_bp_x100: 2 * 2_000,
        },
    ];
    let pick = |l: Limits| select(&b, &s, &l);
    assert_eq!(pick(Limits::default()), ["AAA", "BBB", "CCC", "DDD"]);
    assert_eq!(
        pick(Limits {
            min_price: Some(10 * D),
            ..Limits::default()
        }),
        ["AAA", "BBB", "CCC"]
    );
    // At the limit passes.
    assert_eq!(
        pick(Limits {
            max_price: Some(12 * D),
            ..Limits::default()
        }),
        ["AAA", "CCC", "DDD"]
    );
    assert_eq!(
        pick(Limits {
            min_dollars: Some(1_000_000 * D as u128),
            ..Limits::default()
        }),
        ["AAA", "BBB", "DDD"]
    );
    assert_eq!(
        pick(Limits {
            min_bars: Some(390),
            ..Limits::default()
        }),
        ["AAA", "BBB", "DDD"]
    );
    // A spread limit: DDD has no quotes and does not pass; AAA's 3 bp is within 3 bp.
    assert_eq!(
        pick(Limits {
            max_spread_bp_x100: Some(300),
            ..Limits::default()
        }),
        ["AAA", "BBB"]
    );
    assert_eq!(
        pick(Limits {
            min_price: Some(5 * D),
            max_price: Some(100 * D),
            min_dollars: Some(100_000 * D as u128),
            min_bars: Some(100),
            max_spread_bp_x100: Some(10_000),
        }),
        ["AAA"]
    );
}
