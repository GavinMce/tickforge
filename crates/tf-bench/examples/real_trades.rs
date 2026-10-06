//! Feeds a real day's trades (a Databento CSV, zstd-compressed, as `tf`'s measurement notes
//! describe) through the Tier 0 state at full speed and reports how fast it goes, second by second.
//!
//! `cargo run --release -p tf-bench --example real_trades -- FILE.csv.zst`
//!
//! The file comes from the Databento historical API with `schema=trades`, `encoding=csv`,
//! `compression=zstd`, `pretty_px=true`, `pretty_ts=true`. Only Tier 0 is exercised: this
//! measures the engine's state update on real traffic, not decoding, queues or the scanner (which
//! needs quotes).

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::time::Instant;

use tf_core::{Event, Header, ProviderId, Trade, TradeFlags};
use tf_engine::Tier0;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: real_trades FILE.csv.zst");
    let file = std::fs::File::open(&path).expect("open");
    let dec = zstd::stream::read::Decoder::new(file).expect("zstd");
    let mut lines = BufReader::new(dec).lines();
    let head = lines.next().expect("header").expect("read");
    let col = |name: &str| {
        head.split(',')
            .position(|c| c == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    let (c_recv, c_event, c_inst, c_px, c_size, c_seq) = (
        col("ts_recv"),
        col("ts_event"),
        col("instrument_id"),
        col("price"),
        col("size"),
        col("sequence"),
    );

    let t0 = Instant::now();
    let mut ids: HashMap<u32, u32> = HashMap::new();
    let mut events: Vec<Event> = Vec::new();
    for line in lines {
        let line = line.expect("read");
        let f: Vec<&str> = line.split(',').collect();
        let raw: u32 = f[c_inst].parse().expect("instrument_id");
        let next = ids.len() as u32;
        let inst = *ids.entry(raw).or_insert(next);
        events.push(Event::Trade(Trade {
            hdr: Header {
                ts_event: tf_alpaca::events::parse_time(f[c_event]).expect("ts_event"),
                ts_recv: tf_alpaca::events::parse_time(f[c_recv]).expect("ts_recv"),
                seq: f[c_seq].parse().unwrap_or(0),
                instrument: inst,
                provider: ProviderId::Databento,
            },
            px: tf_alpaca::wire::parse_px(f[c_px]).expect("price"),
            size: f[c_size].parse().expect("size"),
            flags: TradeFlags::NONE,
        }));
    }
    println!(
        "read {} trades, {} symbols in {:.1}s",
        events.len(),
        ids.len(),
        t0.elapsed().as_secs_f64()
    );

    // Group by the second they arrived in, in arrival order.
    let mut buckets: BTreeMap<u64, std::ops::Range<usize>> = BTreeMap::new();
    for (i, e) in events.iter().enumerate() {
        let s = e.hdr().ts_recv / 1_000_000_000;
        buckets
            .entry(s)
            .and_modify(|r| r.end = i + 1)
            .or_insert(i..i + 1);
    }

    let mut tier0 = Tier0::new(ids.len());
    let whole = Instant::now();
    let mut per_second: Vec<(u64, usize, f64)> = Vec::with_capacity(buckets.len());
    for (sec, r) in &buckets {
        let t = Instant::now();
        for e in &events[r.clone()] {
            tier0.on_event(e);
        }
        per_second.push((*sec, r.len(), t.elapsed().as_secs_f64()));
    }
    let total = whole.elapsed().as_secs_f64();
    println!(
        "Tier 0: {} events in {:.3}s = {:.1}M events/s overall",
        events.len(),
        total,
        events.len() as f64 / total / 1e6
    );

    let busiest = per_second.iter().max_by_key(|p| p.1).expect("data");
    println!(
        "busiest second: {} events took {:.1} ms ({:.0}x faster than real time, {:.1}M events/s)",
        busiest.1,
        busiest.2 * 1e3,
        1.0 / busiest.2,
        busiest.1 as f64 / busiest.2 / 1e6
    );
    let slowest = per_second
        .iter()
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .expect("data");
    println!(
        "slowest second: {} events took {:.1} ms",
        slowest.1,
        slowest.2 * 1e3
    );
    let worst_ratio = per_second.iter().map(|p| p.2).fold(0.0, f64::max);
    println!(
        "no second took more than {:.1} ms to process",
        worst_ratio * 1e3
    );
    println!(
        "symbols with state: {}, unknown events: {}",
        ids.len(),
        tier0.unknown_events()
    );
}
