//! Replays a stored schema of a history store through the Provider interface and reports what came out.
//!
//! `cargo run --release -p tf-history --example replay_real -- DIR DATASET SCHEMA [FROM [TO]]`
//!
//! Counts the events by kind, the instruments, whether arrival times never go backwards, and the speed.

use std::time::Instant;

use tf_core::Event;
use tf_provider::{Poll, Provider};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dir, dataset, schema) = (&a[1], &a[2], &a[3]);
    let mut replay = tf_history::replay(
        std::path::Path::new(dir),
        dataset,
        schema,
        a.get(4).map(String::as_str),
        a.get(5).map(String::as_str),
    )
    .expect("replay");
    let t = Instant::now();
    let (mut trades, mut quotes, mut other) = (0u64, 0u64, 0u64);
    let (mut last, mut backwards, mut max_inst) = (0u64, 0u64, 0u32);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match replay.poll(&mut buf, 65_536) {
            Poll::Events(_) => {}
            _ => break,
        }
        for e in &buf {
            match e {
                Event::Trade(_) => trades += 1,
                Event::Quote(_) => quotes += 1,
                _ => other += 1,
            }
            if e.ts_recv() < last {
                backwards += 1;
            }
            last = e.ts_recv();
            max_inst = max_inst.max(e.instrument());
        }
    }
    let n = trades + quotes + other;
    println!(
        "{n} events ({trades} trades, {quotes} quotes, {other} other) over {} instruments in {:.2}s = {:.1}M events/s; arrival times went backwards {backwards} times",
        max_inst + 1,
        t.elapsed().as_secs_f64(),
        n as f64 / t.elapsed().as_secs_f64() / 1e6
    );
    let ids = replay.instruments();
    for id in 0..ids.len() as u32 {
        println!("  instrument {id}: {:?}", ids.symbol(id));
    }
}
