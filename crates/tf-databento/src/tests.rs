use std::ffi::c_char;

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{
    BidAskPair, CbboMsg, Cmbp1Msg, ConsolidatedBidAskPair, ErrorCode, ErrorMsg, FlagSet, Mbp1Msg,
    MetadataBuilder, OhlcvMsg, RecordHeader, SType, Schema, StatusAction, StatusMsg,
    SymbolMappingMsg, SystemCode, SystemMsg, TradeMsg, UNDEF_PRICE, rtype,
};
use tf_core::{Event, ProviderId, Px, StatusKind};

use super::*;

const D: i64 = 1_000_000_000;

fn metadata(schema: Schema) -> dbn::Metadata {
    MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(Some(schema))
        .start(0)
        .stype_in(Some(SType::RawSymbol))
        .stype_out(SType::InstrumentId)
        .build()
}

/// A DBN stream holding whatever `write` puts in it.
fn dbn(schema: Schema, write: impl FnOnce(&mut DbnEncoder<&mut Vec<u8>>)) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut enc = DbnEncoder::new(&mut bytes, &metadata(schema)).unwrap();
        write(&mut enc);
    }
    bytes
}

fn trade(
    publisher: u16,
    instrument: u32,
    ts_event: u64,
    ts_recv: u64,
    price: i64,
    size: u32,
    sequence: u32,
) -> TradeMsg {
    TradeMsg {
        hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, publisher, instrument, ts_event),
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

fn items(bytes: &[u8]) -> Vec<Item> {
    Decoder::new(bytes).unwrap().map(|i| i.unwrap()).collect()
}

fn events(bytes: &[u8]) -> Vec<Event> {
    items(bytes)
        .into_iter()
        .filter_map(|i| match i {
            Item::Event(e) => Some(e),
            _ => None,
        })
        .collect()
}

#[test]
fn trades_become_trade_events_with_raw_prices_and_dense_ids() {
    let bytes = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(
            81,
            20433,
            1_000,
            1_200,
            1_718_930_000_000,
            1,
            24_710_164,
        ))
        .unwrap();
        e.encode_record(&trade(
            82,
            23037,
            2_000,
            2_200,
            10_870_000_000,
            75,
            20_920_448,
        ))
        .unwrap();
        e.encode_record(&trade(82, 20433, 3_000, 3_200, 1, 5, 7))
            .unwrap();
    });
    let ev = events(&bytes);
    assert_eq!(ev.len(), 3);
    let Event::Trade(a) = ev[0] else { panic!() };
    assert_eq!(
        (a.px, a.size, a.flags),
        (Px::from_raw(1_718_930_000_000), 1, TradeFlags::NONE)
    );
    assert_eq!(
        (a.hdr.ts_event, a.hdr.ts_recv, a.hdr.provider),
        (1_000, 1_200, ProviderId::Databento)
    );
    assert_eq!(
        a.hdr.seq,
        (81u64 << 32) | 24_710_164,
        "the publisher is in the sequence"
    );
    assert_eq!(a.hdr.instrument, 0);
    let Event::Trade(b) = ev[1] else { panic!() };
    assert_eq!(b.hdr.instrument, 1, "dense ids in order of first sight");
    assert_eq!(b.hdr.seq, (82u64 << 32) | 20_920_448);
    let Event::Trade(c) = ev[2] else { panic!() };
    assert_eq!(c.hdr.instrument, 0, "the same instrument keeps its id");
    assert_eq!(
        c.px.raw(),
        1,
        "a price of one billionth of a dollar survives: nothing is rounded"
    );
    assert_ne!(
        a.hdr.seq,
        (82u64 << 32) | 24_710_164,
        "two venues' sequences do not collide"
    );
    let mut d = Decoder::new(&bytes[..]).unwrap();
    while d.next_item().unwrap().is_some() {}
    let s = d.stats();
    assert_eq!(
        (s.records, s.trades, s.quotes, s.bad_trades, s.zero_size),
        (3, 3, 0, 0, 0)
    );
    assert_eq!(d.instruments().len(), 2);
    assert_eq!(
        (
            d.instruments().raw_of(0),
            d.instruments().raw_of(1),
            d.instruments().raw_of(2)
        ),
        (Some(20433), Some(23037), None)
    );
    assert_eq!(d.instruments().dense(23037), Some(1));
    assert_eq!(d.instruments().dense(5), None);
    assert!(!d.instruments().is_empty());
}

#[test]
fn a_trade_without_a_real_price_is_dropped_and_counted_as_bad() {
    let bytes = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(81, 1, 1, 1, UNDEF_PRICE, 10, 1))
            .unwrap();
        e.encode_record(&trade(81, 1, 2, 2, 0, 10, 2)).unwrap();
        e.encode_record(&trade(81, 1, 3, 3, -5 * D, 10, 3)).unwrap();
        e.encode_record(&trade(81, 1, 5, 5, 5 * D, 10, 5)).unwrap();
    });
    let mut d = Decoder::new(&bytes[..]).unwrap();
    let mut n = 0;
    while let Some(i) = d.next_item().unwrap() {
        assert!(matches!(i, Item::Event(Event::Trade(t)) if t.hdr.ts_event == 5));
        n += 1;
    }
    assert_eq!(n, 1);
    assert_eq!(
        (
            d.stats().bad_trades,
            d.stats().zero_size,
            d.stats().trades,
            d.stats().records
        ),
        (3, 0, 1, 4)
    );
}

#[test]
fn a_print_of_zero_shares_is_real_counted_apart_and_dropped_unless_kept() {
    let bytes = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(82, 1, 1, 1, 174_716_900_000, 0, 1))
            .unwrap();
        e.encode_record(&trade(82, 1, 2, 2, 5 * D, 7, 2)).unwrap();
        e.encode_record(&trade(82, 1, 3, 3, UNDEF_PRICE, 0, 3))
            .unwrap();
    });
    let mut d = Decoder::new(&bytes[..]).unwrap();
    let got: Vec<Event> = std::iter::from_fn(|| d.next_item().unwrap())
        .filter_map(|i| {
            if let Item::Event(e) = i {
                Some(e)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (d.stats().zero_size, d.stats().bad_trades, d.stats().trades),
        (1, 1, 1),
        "a zero-size print at no price is bad, not zero-size"
    );
    // Kept: the sub-penny price and the zero size arrive as they are.
    let mut d = Decoder::new(&bytes[..]).unwrap().keep_zero_size(true);
    let got: Vec<Event> = std::iter::from_fn(|| d.next_item().unwrap())
        .filter_map(|i| {
            if let Item::Event(e) = i {
                Some(e)
            } else {
                None
            }
        })
        .collect();
    let [Event::Trade(a), Event::Trade(b)] = got[..] else {
        panic!("{got:?}")
    };
    assert_eq!((a.px.raw(), a.size, b.size), (174_716_900_000, 0, 7));
    assert_eq!((d.stats().zero_size, d.stats().trades), (0, 2));
    // The same rule for the trade inside a quote record.
    let tb = dbn(Schema::Tbbo, |e| {
        e.encode_record(&mbp1(b'T', 150 * D, 0, pair(149 * D, 151 * D, 7, 9), 1))
            .unwrap();
    });
    let mut d = Decoder::new(&tb[..]).unwrap();
    let n = std::iter::from_fn(|| d.next_item().unwrap()).count();
    assert_eq!(
        (n, d.stats().zero_size, d.stats().quotes),
        (1, 1, 1),
        "the book still arrives"
    );
}

fn pair(bid: i64, ask: i64, bid_sz: u32, ask_sz: u32) -> BidAskPair {
    BidAskPair {
        bid_px: bid,
        ask_px: ask,
        bid_sz,
        ask_sz,
        bid_ct: 1,
        ask_ct: 1,
    }
}

fn mbp1(action: u8, price: i64, size: u32, levels: BidAskPair, sequence: u32) -> Mbp1Msg {
    Mbp1Msg {
        hd: RecordHeader::new::<Mbp1Msg>(rtype::MBP_1, 82, 20433, 5_000),
        price,
        size,
        action: action as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        depth: 0,
        ts_recv: 5_300,
        ts_in_delta: 0,
        sequence,
        levels: [levels],
    }
}

#[test]
fn a_trade_with_its_book_is_a_quote_then_the_trade_and_they_share_a_sequence() {
    let bytes = dbn(Schema::Tbbo, |e| {
        e.encode_record(&mbp1(b'T', 150 * D, 100, pair(149 * D, 151 * D, 7, 9), 42))
            .unwrap();
    });
    let ev = events(&bytes);
    assert_eq!(ev.len(), 2);
    let (Event::Quote(q), Event::Trade(t)) = (ev[0], ev[1]) else {
        panic!("{ev:?}")
    };
    assert_eq!(
        (q.bid_px.raw(), q.ask_px.raw(), q.bid_sz, q.ask_sz),
        (149 * D, 151 * D, 7, 9)
    );
    assert_eq!((t.px.raw(), t.size), (150 * D, 100));
    assert_eq!(q.hdr, t.hdr, "one record, one header");
    assert_eq!(q.hdr.seq, (82u64 << 32) | 42);
    assert_eq!((q.hdr.ts_event, q.hdr.ts_recv), (5_000, 5_300));
}

#[test]
fn a_book_update_that_is_not_a_trade_is_only_a_quote_and_an_empty_side_is_zero() {
    let bytes = dbn(Schema::Mbp1, |e| {
        e.encode_record(&mbp1(b'A', 150 * D, 100, pair(149 * D, 151 * D, 7, 9), 1))
            .unwrap();
        e.encode_record(&mbp1(
            b'C',
            UNDEF_PRICE,
            0,
            pair(149 * D, UNDEF_PRICE, 7, 99),
            2,
        ))
        .unwrap();
        e.encode_record(&mbp1(b'M', 0, 0, pair(UNDEF_PRICE, UNDEF_PRICE, 5, 5), 3))
            .unwrap();
    });
    let ev = events(&bytes);
    assert_eq!(ev.len(), 3, "no trades among them");
    let qs: Vec<_> = ev
        .iter()
        .map(|e| {
            if let Event::Quote(q) = e {
                *q
            } else {
                panic!()
            }
        })
        .collect();
    assert_eq!((qs[0].bid_px.raw(), qs[0].ask_px.raw()), (149 * D, 151 * D));
    assert_eq!(
        (qs[1].bid_px.raw(), qs[1].ask_px, qs[1].bid_sz, qs[1].ask_sz),
        (149 * D, Px::ZERO, 7, 0),
        "no ask: price 0 and size 0, whatever the record said"
    );
    assert_eq!(
        (qs[2].bid_px, qs[2].ask_px, qs[2].bid_sz, qs[2].ask_sz),
        (Px::ZERO, Px::ZERO, 0, 0)
    );
    // A trade action with no real price gives the quote and no trade.
    let bytes = dbn(Schema::Tbbo, |e| {
        e.encode_record(&mbp1(b'T', UNDEF_PRICE, 10, pair(D, 2 * D, 1, 1), 1))
            .unwrap();
    });
    let mut d = Decoder::new(&bytes[..]).unwrap();
    let mut kinds = vec![];
    while let Some(Item::Event(e)) = d.next_item().unwrap() {
        kinds.push(matches!(e, Event::Quote(_)));
    }
    assert_eq!(kinds, [true]);
    assert_eq!(d.stats().bad_trades, 1);
}

fn cpair(bid: i64, ask: i64, bid_sz: u32, ask_sz: u32) -> ConsolidatedBidAskPair {
    ConsolidatedBidAskPair {
        bid_px: bid,
        ask_px: ask,
        bid_sz,
        ask_sz,
        bid_pb: 81,
        _reserved1: [0; 2],
        ask_pb: 82,
        _reserved2: [0; 2],
    }
}

fn cmbp1(
    rtype_: u8,
    action: u8,
    price: i64,
    size: u32,
    ts_recv: u64,
    levels: ConsolidatedBidAskPair,
) -> Cmbp1Msg {
    Cmbp1Msg {
        hd: RecordHeader::new::<Cmbp1Msg>(rtype_, 88, 777, 9_000),
        price,
        size,
        action: action as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        _reserved1: [0],
        ts_recv,
        ts_in_delta: 0,
        _reserved2: [0; 4],
        levels: [levels],
    }
}

#[test]
fn consolidated_records_have_no_sequence_so_the_receive_time_stands_in() {
    let bytes = dbn(Schema::Cmbp1, |e| {
        e.encode_record(&cmbp1(
            rtype::CMBP_1,
            b'A',
            0,
            0,
            9_100,
            cpair(10 * D, 11 * D, 3, 4),
        ))
        .unwrap();
        e.encode_record(&cmbp1(
            rtype::TCBBO,
            b'T',
            10 * D,
            50,
            9_200,
            cpair(10 * D, 11 * D, 3, 4),
        ))
        .unwrap();
    });
    let ev = events(&bytes);
    assert_eq!(ev.len(), 3, "a quote, then a quote and a trade");
    let hdr = |e: &Event| *e.hdr();
    assert_eq!(hdr(&ev[0]).seq, 9_100);
    assert_eq!(hdr(&ev[1]).seq, 9_200);
    assert_eq!(hdr(&ev[2]).seq, 9_200);
    assert!(matches!(ev[1], Event::Quote(_)) && matches!(ev[2], Event::Trade(_)));
    // The same record read twice gives the same sequence: a replay can be told from a new record.
    assert_eq!(events(&bytes)[1], ev[1]);
}

#[test]
fn consolidated_bbo_snapshots_are_quotes() {
    let rec = CbboMsg {
        hd: RecordHeader::new::<CbboMsg>(rtype::CBBO_1S, 0, 5, 1_000_000_000),
        price: 0,
        size: 0,
        _reserved1: 0,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        _reserved2: 0,
        ts_recv: 1_000_000_001,
        _reserved3: [0; 8],
        levels: [cpair(20 * D, UNDEF_PRICE, 8, 8)],
    };
    let bytes = dbn(Schema::Cbbo1S, |e| e.encode_record(&rec).unwrap());
    let ev = events(&bytes);
    let [Event::Quote(q)] = ev[..] else {
        panic!("{ev:?}")
    };
    assert_eq!(
        (q.bid_px.raw(), q.ask_px, q.bid_sz, q.ask_sz),
        (20 * D, Px::ZERO, 8, 0)
    );
    assert_eq!(q.hdr.seq, 1_000_000_001);
}

fn status(action: StatusAction) -> StatusMsg {
    StatusMsg {
        action: action as u16,
        ..StatusMsg::default()
    }
}

#[test]
fn halts_resumes_and_short_sale_changes_become_status_and_the_rest_is_ignored() {
    let mk = |a: StatusAction, ts: u64| {
        let mut s = status(a);
        s.hd = RecordHeader::new::<StatusMsg>(rtype::STATUS, 81, 9, ts);
        s.ts_recv = ts + 1;
        s
    };
    let bytes = dbn(Schema::Status, |e| {
        for (a, ts) in [
            (StatusAction::Halt, 10),
            (StatusAction::Pause, 20),
            (StatusAction::Suspend, 30),
            (StatusAction::Trading, 40),
            (StatusAction::SsrChange, 50),
            (StatusAction::PreOpen, 60),
            (StatusAction::Quoting, 70),
            (StatusAction::Close, 80),
        ] {
            e.encode_record(&mk(a, ts)).unwrap();
        }
    });
    let mut d = Decoder::new(&bytes[..]).unwrap();
    let mut kinds = vec![];
    let mut ignored = 0;
    while let Some(i) = d.next_item().unwrap() {
        match i {
            Item::Event(Event::Status(s)) => {
                assert_eq!((s.lo, s.hi), (Px::ZERO, Px::ZERO));
                kinds.push((s.kind, s.hdr.ts_event, s.hdr.ts_recv));
            }
            Item::Ignored { rtype: r } => {
                assert_eq!(r, rtype::STATUS);
                ignored += 1;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        kinds,
        [
            (StatusKind::TradingHalt, 10, 11),
            (StatusKind::TradingHalt, 20, 21),
            (StatusKind::TradingHalt, 30, 31),
            (StatusKind::TradingResume, 40, 41),
            (StatusKind::ShortSaleRestriction, 50, 51),
        ]
    );
    assert_eq!(ignored, 3);
    assert_eq!((d.stats().statuses, d.stats().ignored), (5, 3));
}

#[test]
fn symbol_mappings_name_the_instruments_and_are_not_events() {
    let m = SymbolMappingMsg::new(
        20433,
        0,
        SType::RawSymbol,
        "AAPL",
        SType::RawSymbol,
        "AAPL",
        0,
        0,
    )
    .unwrap();
    let bytes = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(81, 20433, 1, 1, 5 * D, 1, 1))
            .unwrap();
        e.encode_record(&m).unwrap();
        e.encode_record(
            &SymbolMappingMsg::new(
                7,
                0,
                SType::RawSymbol,
                "MSFT",
                SType::RawSymbol,
                "MSFT",
                0,
                0,
            )
            .unwrap(),
        )
        .unwrap();
    });
    let mut d = Decoder::new(&bytes[..]).unwrap();
    let all: Vec<Item> = std::iter::from_fn(|| d.next_item().unwrap()).collect();
    assert!(matches!(&all[1], Item::Mapping { instrument: 0, symbol } if symbol == "AAPL"));
    assert!(
        matches!(&all[2], Item::Mapping { instrument: 1, symbol } if symbol == "MSFT"),
        "a mapping for an instrument not yet traded still gets an id"
    );
    assert_eq!(d.instruments().symbol(0), Some("AAPL"));
    assert_eq!(d.instruments().symbol(1), Some("MSFT"));
    assert_eq!(d.instruments().symbol(2), None);
    assert_eq!(d.stats().mappings, 2);
}

#[test]
fn the_gateways_own_messages_are_notices_and_a_skip_is_recognised() {
    let bytes = dbn(Schema::Cmbp1, |e| {
        e.encode_record(&SystemMsg::heartbeat(1)).unwrap();
        e.encode_record(
            &SystemMsg::new(2, Some(SystemCode::SlowReaderWarning), "behind real time").unwrap(),
        )
        .unwrap();
        e.encode_record(
            &SystemMsg::new(3, Some(SystemCode::ReplayCompleted), "replay done").unwrap(),
        )
        .unwrap();
        e.encode_record(&ErrorMsg::new(
            4,
            Some(ErrorCode::SkippedRecordsAfterSlowReading),
            "skipped 120 records",
            true,
        ))
        .unwrap();
        e.encode_record(&ErrorMsg::new(
            5,
            Some(ErrorCode::SymbolResolutionFailed),
            "no such symbol",
            false,
        ))
        .unwrap();
    });
    let n: Vec<Notice> = items(&bytes)
        .into_iter()
        .map(|i| match i {
            Item::Notice(n) => n,
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(n[0].is_heartbeat() && !n[0].is_skip());
    assert!(n[1].is_slow_reader_warning() && !n[1].is_heartbeat());
    assert!(n[2].is_replay_completed());
    assert!(n[3].is_skip() && !n[3].is_slow_reader_warning());
    assert!(matches!(&n[3], Notice::Error { code: 7, text } if text == "skipped 120 records"));
    assert!(matches!(&n[4], Notice::Error { code: 4, .. }) && !n[4].is_skip());
    let mut d = Decoder::new(&bytes[..]).unwrap();
    while d.next_item().unwrap().is_some() {}
    assert_eq!(d.stats().notices, 5);
}

#[test]
fn other_kinds_of_record_are_ignored_and_counted_not_mistaken_for_trades() {
    let bar = OhlcvMsg {
        hd: RecordHeader::new::<OhlcvMsg>(rtype::OHLCV_1S, 1, 1, 10),
        open: D,
        high: D,
        low: D,
        close: D,
        volume: 9,
    };
    let bytes = dbn(Schema::Ohlcv1S, |e| e.encode_record(&bar).unwrap());
    let all = items(&bytes);
    assert_eq!(
        all,
        [Item::Ignored {
            rtype: rtype::OHLCV_1S
        }]
    );
}

#[test]
fn plain_and_compressed_streams_read_the_same() {
    let plain = dbn(Schema::Trades, |e| {
        for i in 0..50u32 {
            e.encode_record(&trade(
                81,
                i % 7,
                u64::from(i),
                u64::from(i),
                (i64::from(i) + 1) * D,
                i + 1,
                i,
            ))
            .unwrap();
        }
    });
    let mut zbytes = Vec::new();
    {
        let mut enc = DbnEncoder::with_zstd(&mut zbytes, &metadata(Schema::Trades)).unwrap();
        for i in 0..50u32 {
            enc.encode_record(&trade(
                81,
                i % 7,
                u64::from(i),
                u64::from(i),
                (i64::from(i) + 1) * D,
                i + 1,
                i,
            ))
            .unwrap();
        }
    }
    let a = events(&plain);
    let b: Vec<Event> = Decoder::zstd(&zbytes[..])
        .unwrap()
        .filter_map(|i| match i.unwrap() {
            Item::Event(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(a.len(), 50);
    assert_eq!(a, b);
    assert!(zbytes.len() < plain.len());
    // And from a file.
    let path = std::env::temp_dir().join(format!("tf-databento-{}.dbn.zst", std::process::id()));
    std::fs::write(&path, &zbytes).unwrap();
    let c: Vec<Event> = Decoder::from_zstd_file(&path)
        .unwrap()
        .filter_map(|i| match i.unwrap() {
            Item::Event(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(a, c);
    let _ = std::fs::remove_file(&path);
    assert!(Decoder::from_zstd_file(path.with_extension("missing")).is_err());
}

#[test]
fn damaged_streams_are_errors_and_what_came_before_is_still_delivered() {
    assert!(Decoder::new(&b"this is not dbn at all, not even close"[..]).is_err());
    assert!(Decoder::new(&b""[..]).is_err());
    assert!(Decoder::zstd(&b"neither is this"[..]).is_err());
    let mut bytes = dbn(Schema::Trades, |e| {
        for i in 1..=3u32 {
            e.encode_record(&trade(81, 1, u64::from(i), u64::from(i), 5 * D, i, i))
                .unwrap();
        }
    });
    bytes.truncate(bytes.len() - 20); // the last record is cut short
    let mut d = Decoder::new(&bytes[..]).unwrap();
    let mut got = 0;
    let mut failed = false;
    loop {
        match d.next_item() {
            Ok(Some(_)) => got += 1,
            Ok(None) => break,
            Err(e) => {
                assert!(!e.to_string().is_empty());
                failed = true;
                break;
            }
        }
    }
    assert!(got >= 2, "the complete records were delivered: {got}");
    assert!(
        failed || got == 2,
        "a cut record is an error or the end, never a trade"
    );
}

#[test]
fn errors_say_what_and_where() {
    let e = DecodeError::Record {
        index: 12,
        why: "an unknown status action",
    };
    assert_eq!(e.to_string(), "record 12: an unknown status action");
    let d = DecodeError::from(dbn::Error::decode("boom"));
    assert!(d.to_string().starts_with("DBN: "), "{d}");
}

#[test]
fn a_mapper_maps_records_it_is_handed_and_keeps_its_ids_and_counts() {
    use dbn::decode::{DbnDecoder, DecodeRecordRef};
    let bytes = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(81, 500, 1, 1, 5 * D, 3, 1)).unwrap();
        e.encode_record(&trade(81, 600, 2, 2, 0, 3, 2)).unwrap();
        e.encode_record(&trade(81, 500, 3, 3, 6 * D, 0, 3)).unwrap();
    });
    let mut dec = DbnDecoder::new(&bytes[..]).unwrap();
    let mut m = Mapper::new();
    let mut out = Vec::new();
    while let Some(rec) = dec.decode_record_ref().unwrap() {
        m.map(&rec, &mut out).unwrap();
    }
    assert_eq!(out.len(), 1);
    let s = m.stats();
    assert_eq!(
        (s.records, s.trades, s.bad_trades, s.zero_size),
        (3, 1, 1, 1)
    );
    assert_eq!(m.instruments().dense(500), Some(0));
    assert_eq!(
        m.instruments().dense(600),
        None,
        "a dropped trade does not make an id"
    );
}

#[test]
fn instrument_ids_carry_from_one_file_to_the_next() {
    let first = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(81, 20, 1, 1, 5 * D, 1, 1)).unwrap();
        e.encode_record(&trade(81, 30, 2, 2, 5 * D, 1, 2)).unwrap();
    });
    let second = dbn(Schema::Trades, |e| {
        e.encode_record(&trade(81, 40, 3, 3, 5 * D, 1, 3)).unwrap();
        e.encode_record(&trade(81, 30, 4, 4, 5 * D, 1, 4)).unwrap();
    });
    let mut a = Decoder::new(&first[..]).unwrap();
    while a.next_item().unwrap().is_some() {}
    let ids = a.into_instruments();
    let ev: Vec<Event> = Decoder::new(&second[..])
        .unwrap()
        .with_instruments(ids)
        .filter_map(|i| {
            if let Item::Event(e) = i.unwrap() {
                Some(e)
            } else {
                None
            }
        })
        .collect();
    let dense: Vec<u32> = ev.iter().map(|e| e.hdr().instrument).collect();
    assert_eq!(
        dense,
        [2, 1],
        "30 keeps the id it had, 40 gets the next one"
    );
}
