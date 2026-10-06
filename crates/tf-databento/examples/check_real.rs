//! Checks the decoder against real Databento files, locally (the files are not in the repository).
//!
//! `check_real trades FILE.dbn.zst FILE.csv.zst`
//!     decodes the trades and compares each with the same row of the CSV Databento produced for the
//!     same request: instrument, price, size, both timestamps and the sequence, row by row.
//! `check_real summary FILE.dbn.zst`
//!     decodes any schema and reports what came out, and checks that no quote has its bid above its
//!     ask.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

use tf_core::Event;
use tf_databento::{Decoder, Item};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("trades") => trades(&args[1], &args[2]),
        Some("summary") => summary(&args[1]),
        _ => eprintln!("usage: check_real trades DBN CSV | summary DBN"),
    }
}

fn trades(dbn: &str, csv: &str) {
    let mut dec = Decoder::from_zstd_file(dbn).expect("open dbn");
    let file = std::fs::File::open(csv).expect("open csv");
    let mut lines = BufReader::new(zstd::stream::read::Decoder::new(file).expect("zstd")).lines();
    let head = lines.next().expect("header").expect("read");
    let col = |n: &str| {
        head.split(',')
            .position(|c| c == n)
            .unwrap_or_else(|| panic!("no column {n}"))
    };
    let (c_recv, c_event, c_pub, c_inst, c_px, c_size, c_seq) = (
        col("ts_recv"),
        col("ts_event"),
        col("publisher_id"),
        col("instrument_id"),
        col("price"),
        col("size"),
        col("sequence"),
    );
    let (mut rows, mut bad, mut shown, mut zero) = (0u64, 0u64, 0, 0u64);
    for line in lines {
        let line = line.expect("read");
        let f: Vec<&str> = line.split(',').collect();
        if f[c_size] == "0" {
            zero += 1; // dropped by the decoder on purpose, and counted there
            continue;
        }
        let item = loop {
            match dec.next_item().expect("decode") {
                Some(Item::Event(Event::Trade(t))) => break Some(t),
                Some(_) => continue,
                None => break None,
            }
        };
        let Some(t) = item else {
            println!("the DBN ended after {rows} rows but the CSV goes on");
            std::process::exit(1);
        };
        let raw_inst = dec.instruments().raw_of(t.hdr.instrument).expect("raw id");
        let want_seq = (f[c_pub].parse::<u64>().expect("publisher") << 32)
            | f[c_seq].parse::<u64>().expect("sequence");
        let checks = [
            ("instrument", raw_inst.to_string() == f[c_inst]),
            ("price", Some(t.px) == tf_alpaca::wire::parse_px(f[c_px])),
            ("size", t.size.to_string() == f[c_size]),
            (
                "ts_event",
                Some(t.hdr.ts_event) == tf_alpaca::events::parse_time(f[c_event]),
            ),
            (
                "ts_recv",
                Some(t.hdr.ts_recv) == tf_alpaca::events::parse_time(f[c_recv]),
            ),
            ("seq", t.hdr.seq == want_seq),
        ];
        rows += 1;
        let wrong: Vec<&str> = checks.iter().filter(|c| !c.1).map(|c| c.0).collect();
        if !wrong.is_empty() {
            bad += 1;
            if shown < 5 {
                shown += 1;
                println!("row {rows}: differs in {wrong:?}: {line}");
            }
        }
    }
    let extra = std::iter::from_fn(|| dec.next_item().expect("decode"))
        .filter(|i| matches!(i, Item::Event(_)))
        .count();
    println!(
        "{rows} CSV rows compared with the decoded trades: {bad} rows differ; {extra} decoded events left over"
    );
    println!(
        "stats: {:?}, {} instruments",
        dec.stats(),
        dec.instruments().len()
    );
    if bad > 0 || extra > 0 || zero != dec.stats().zero_size {
        std::process::exit(1);
    }
}

fn summary(dbn: &str) {
    let started = std::time::Instant::now();
    let mut dec = Decoder::from_zstd_file(dbn).expect("open dbn");
    let (mut quotes, mut crossed, mut one_sided, mut empty) = (0u64, 0u64, 0u64, 0u64);
    let mut kinds: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut notices = Vec::new();
    let mut ts_back = 0u64;
    let mut last_recv = 0u64;
    while let Some(item) = dec.next_item().expect("decode") {
        match item {
            Item::Event(e) => {
                let name = match e {
                    Event::Trade(_) => "trade",
                    Event::Quote(q) => {
                        quotes += 1;
                        match (q.bid_px.raw() > 0, q.ask_px.raw() > 0) {
                            (true, true) if q.bid_px > q.ask_px => crossed += 1,
                            (true, true) => {}
                            (false, false) => empty += 1,
                            _ => one_sided += 1,
                        }
                        "quote"
                    }
                    Event::Status(s) => match s.kind {
                        tf_core::StatusKind::TradingHalt => "status:halt",
                        tf_core::StatusKind::TradingResume => "status:resume",
                        tf_core::StatusKind::ShortSaleRestriction => "status:ssr",
                        tf_core::StatusKind::LuldBand => "status:luld",
                    },
                    _ => "other",
                };
                *kinds.entry(name).or_default() += 1;
                let r = e.hdr().ts_recv;
                if r < last_recv {
                    ts_back += 1;
                }
                last_recv = r;
            }
            Item::Notice(n) => notices.push(n),
            Item::Ignored { .. } | Item::Mapping { .. } => {}
        }
    }
    let secs = started.elapsed().as_secs_f64();
    println!("{:?}", dec.stats());
    println!(
        "decoded {} records in {:.3}s = {:.2}M records/s (including zstd)",
        dec.stats().records,
        secs,
        dec.stats().records as f64 / secs / 1e6
    );
    println!("events by kind: {kinds:?}");
    println!(
        "quotes: {quotes} (crossed {crossed}, one-sided {one_sided}, empty {empty}); ts_recv going backwards {ts_back} times"
    );
    println!(
        "notices: {} {:?}",
        notices.len(),
        notices.iter().take(3).collect::<Vec<_>>()
    );
    println!("{} instruments", dec.instruments().len());
}
