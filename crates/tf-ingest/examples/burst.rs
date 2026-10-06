//! Replays the opening of a real session through the queue at the speed it happened, with the engine's
//! Tier 0 on the other end, and reports what the queue did.
//!
//! `cargo run --release -p tf-ingest --example burst -- FILE.csv.zst [SECS] [STALL_AT_MS STALL_MS] [CAPACITY]`
//!
//! `FILE` is a Databento `trades` CSV, zstd-compressed (see `tf-bench`'s `real_trades`). The first
//! SECS seconds of events (default 6) are offered at their recorded spacing by one thread; another
//! takes them off the queue into `Tier0::on_event`. With a stall, the consumer stops for STALL_MS
//! once STALL_AT_MS into the replay, as a pause in the engine would. `SPIN=1` makes the consumer poll
//! instead of sleeping between messages, as an engine loop would.

use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use tf_core::{Event, Header, ProviderId, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_ingest::{Config, Delivery, channel};

fn load(path: &str) -> (Vec<Event>, usize) {
    let file = std::fs::File::open(path).expect("open");
    let dec = zstd::stream::read::Decoder::new(file).expect("zstd");
    let mut lines = BufReader::new(dec).lines();
    let head = lines.next().expect("header").expect("read");
    let col = |n: &str| {
        head.split(',')
            .position(|c| c == n)
            .unwrap_or_else(|| panic!("no column {n}"))
    };
    let (c_recv, c_event, c_inst, c_px, c_size, c_seq) = (
        col("ts_recv"),
        col("ts_event"),
        col("instrument_id"),
        col("price"),
        col("size"),
        col("sequence"),
    );
    let mut ids = std::collections::HashMap::new();
    let mut events = Vec::new();
    for line in lines {
        let line = line.expect("read");
        let f: Vec<&str> = line.split(',').collect();
        let size: u32 = f[c_size].parse().expect("size");
        if size == 0 {
            continue; // a zero-share print: the decoder drops these by default
        }
        let next = ids.len() as u32;
        let inst = *ids
            .entry(f[c_inst].parse::<u32>().expect("id"))
            .or_insert(next);
        events.push(Event::Trade(Trade {
            hdr: Header {
                ts_event: tf_alpaca::events::parse_time(f[c_event]).expect("ts_event"),
                ts_recv: tf_alpaca::events::parse_time(f[c_recv]).expect("ts_recv"),
                seq: f[c_seq].parse().unwrap_or(0),
                instrument: inst,
                provider: ProviderId::Databento,
            },
            px: tf_alpaca::wire::parse_px(f[c_px]).expect("price"),
            size,
            flags: TradeFlags::NONE,
        }));
    }
    let n = ids.len();
    (events, n)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let path = a
        .first()
        .expect("usage: burst FILE.csv.zst [SECS] [STALL_AT_MS STALL_MS] [CAPACITY]");
    let secs: u64 = a.get(1).map_or(6, |s| s.parse().expect("SECS"));
    let stall_at: u64 = a.get(2).map_or(0, |s| s.parse().expect("STALL_AT_MS"));
    let stall_ms: u64 = a.get(3).map_or(0, |s| s.parse().expect("STALL_MS"));
    let capacity: usize = a
        .get(4)
        .map_or(Config::default().capacity, |s| s.parse().expect("CAPACITY"));

    let (mut events, instruments) = load(path);
    let base = events.first().expect("data").hdr().ts_recv;
    events.retain(|e| e.hdr().ts_recv - base < secs * 1_000_000_000);
    let busiest = {
        let mut per: std::collections::BTreeMap<u64, u64> = Default::default();
        for e in &events {
            *per.entry(e.hdr().ts_recv / 1_000_000_000).or_default() += 1;
        }
        per.values().copied().max().unwrap_or(0)
    };
    println!(
        "{} trades in the first {secs} s, busiest second {busiest}; queue of {capacity}; stall {stall_ms} ms at {stall_at} ms",
        events.len()
    );

    let cfg = Config {
        capacity,
        instruments,
        ..Config::default()
    };
    let spin = std::env::var_os("SPIN").is_some();
    println!(
        "consumer {}",
        if spin {
            "spins (SPIN=1)"
        } else {
            "sleeps between messages"
        }
    );
    let (mut producer, mut consumer) = channel(cfg).expect("config");
    let pushed_at: Arc<Vec<AtomicU64>> =
        Arc::new((0..events.len()).map(|_| AtomicU64::new(0)).collect());
    let done = Arc::new(AtomicBool::new(false));
    let start = Instant::now();

    let consumer_thread = {
        let (pushed_at, done) = (pushed_at.clone(), done.clone());
        std::thread::spawn(move || {
            let mut tier0 = Tier0::new(instruments);
            let mut lat: Vec<u64> = Vec::new();
            let (mut k, mut gaps, mut gap_lost) = (0usize, 0u64, 0u64);
            let mut stalled = stall_ms == 0;
            loop {
                if !stalled && start.elapsed() >= Duration::from_millis(stall_at) {
                    std::thread::sleep(Duration::from_millis(stall_ms));
                    stalled = true;
                }
                let next = if spin {
                    consumer.try_recv()
                } else {
                    consumer.recv_timeout(Duration::from_millis(20))
                };
                match next {
                    Some(Delivery::Event(e)) => {
                        tier0.on_event(&e);
                        // Only trades are offered here, so the k-th trade delivered is the k-th offered
                        // once drops are counted: latency is measured for runs without a gap.
                        if gaps == 0 && k % 16 == 0 {
                            let at = pushed_at[k].load(Relaxed);
                            lat.push(start.elapsed().as_nanos() as u64 - at);
                        }
                        k += 1;
                    }
                    Some(Delivery::Gap(g)) => {
                        gaps += 1;
                        gap_lost += g.count;
                    }
                    None if done.load(Relaxed) && consumer.depth() == 0 => break,
                    None => {}
                }
            }
            (
                k,
                gaps,
                gap_lost,
                lat,
                consumer.stats(),
                tier0.symbol(0).map(|s| s.trades),
            )
        })
    };

    // The feed: each trade at its recorded spacing.
    let t0 = events[0].hdr().ts_recv;
    for (i, e) in events.iter().enumerate() {
        let due = start + Duration::from_nanos(e.hdr().ts_recv - t0);
        while Instant::now() < due {
            std::hint::spin_loop();
        }
        pushed_at[i].store(start.elapsed().as_nanos() as u64, Relaxed);
        producer.push(*e);
        if i % 4096 == 0 {
            producer.tick();
        }
    }
    while !producer.is_settled() {
        producer.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
    done.store(true, Relaxed);
    let (k, gaps, gap_lost, mut lat, stats, _) = consumer_thread.join().expect("consumer");
    println!(
        "offered {} queued {} delivered {k} (events) | dropped trades {} | gaps {gaps} covering {gap_lost} | max depth {}",
        stats.offered, stats.queued, stats.dropped_trades, stats.max_depth
    );
    if !lat.is_empty() {
        lat.sort_unstable();
        let p = |q: f64| lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)] as f64 / 1e3;
        println!(
            "queue latency (offered to taken, {} samples): p50 {:.1} us  p99 {:.1} us  p99.9 {:.1} us  max {:.1} us",
            lat.len(),
            p(0.5),
            p(0.99),
            p(0.999),
            *lat.last().expect("samples") as f64 / 1e3
        );
    }
    println!(
        "{}",
        if stats.dropped_trades == 0 {
            "no trade was dropped"
        } else {
            "trades were dropped and marked"
        }
    );
}
