//! What 20 cross-sectional strategies cost next to the Tier 0 update.
//!
//! `cargo run --release -p tf-bench --example cross_load -- [SYMBOLS] [STRATEGIES] [EVENTS_PER_SEC] [SECS]`
//!
//! Defaults: 5,000 symbols, 20 strategies that each watch every symbol, 300,000 trades a second (about
//! the opening burst measured on 2026-10-02) for 8 seconds of event time. Each strategy reviews once a
//! second and ranks its members by gap, keeping the top 10. The same events are run twice: Tier 0 alone
//! (the engine's work) and Tier 0 plus the strategies, and the extra time is reported as a share of
//! the time the market took.

use std::time::Instant;

use tf_core::{Event, Header, ProviderId, Px, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_strategy::{CrossRunner, CrossStrategy, Ctx, Market, MemberView, Members, StrategyId};
use tf_universe::{LiveFeature, RefInfo};

const SEC: u64 = 1_000_000_000;

struct Ranker {
    id: u16,
    kept: u64,
    sink: i64,
}

impl CrossStrategy for Ranker {
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> u64 {
        SEC
    }

    fn on_review(&mut self, _ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let top = view.top_by(LiveFeature::GapPermille, 10, true);
        self.kept += top.len() as u64;
        self.sink = self.sink.wrapping_add(top.iter().map(|t| t.0).sum::<i64>());
    }
}

fn main() {
    let arg = |i: usize, d: u64| {
        std::env::args()
            .nth(i)
            .and_then(|s| s.parse().ok())
            .unwrap_or(d)
    };
    let (n, k, rate, secs) = (
        arg(1, 5000) as u32,
        arg(2, 20) as usize,
        arg(3, 300_000),
        arg(4, 8),
    );
    let total = (rate * secs) as usize;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let events: Vec<Event> = (0..total)
        .map(|i| {
            let ts = SEC + (i as u64) * SEC / rate;
            let id = (rnd() % u64::from(n)) as u32;
            Event::Trade(Trade {
                hdr: Header {
                    ts_event: ts,
                    ts_recv: ts,
                    seq: i as u64,
                    instrument: id,
                    provider: ProviderId::Synthetic,
                },
                px: Px::from_raw((10 + (rnd() % 100) as i64) * 1_000_000_000),
                size: 100,
                flags: TradeFlags::NONE,
            })
        })
        .collect();
    let refs = vec![
        RefInfo {
            price: Some(50_000_000_000),
            adv_shares: Some(1_000_000)
        };
        n as usize
    ];
    let market_secs = secs as f64;

    let t = Instant::now();
    let mut tier0 = Tier0::new(n as usize);
    for ev in &events {
        tier0.on_event(ev);
    }
    let base = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let mut tier0 = Tier0::new(n as usize);
    let mut runners: Vec<CrossRunner<Ranker>> = (0..k)
        .map(|i| {
            CrossRunner::new(
                Ranker {
                    id: i as u16,
                    kept: 0,
                    sink: 0,
                },
                Members::from_ids(0..n),
            )
        })
        .collect();
    for ev in &events {
        tier0.on_event(ev);
        let m = Market {
            tier0: &tier0,
            refs: &refs,
        };
        for r in &mut runners {
            r.on_event(m, None, None, ev);
        }
    }
    let with = t.elapsed().as_secs_f64();

    // One review alone, for scale.
    let m = Market {
        tier0: &tier0,
        refs: &refs,
    };
    let all = Members::from_ids(0..n);
    let view = MemberView::new(m, &all);
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..200 {
        sink += view.top_by(LiveFeature::GapPermille, 10, true).len();
    }
    let review = t.elapsed().as_secs_f64() / 200.0;

    let reviews: u64 = runners.iter().map(CrossRunner::reviews).sum();
    println!(
        "{k} strategies x {n} symbols, {rate} trades/s for {secs} s of event time ({total} events)"
    );
    println!(
        "tier 0 alone:            {:7.1} ms  = {:5.2}% of a core at market speed, {:5.1} ns/event",
        base * 1e3,
        base / market_secs * 100.0,
        base * 1e9 / total as f64
    );
    println!(
        "tier 0 + strategies:     {:7.1} ms  = {:5.2}% of a core, {:5.1} ns/event",
        with * 1e3,
        with / market_secs * 100.0,
        with * 1e9 / total as f64
    );
    println!(
        "strategies add:          {:7.1} ms  = {:5.2}% of a core, {:+.1}% over tier 0 alone",
        (with - base) * 1e3,
        (with - base) / market_secs * 100.0,
        (with - base) / base * 100.0
    );
    println!(
        "{reviews} reviews; one review of {n} members (top 10 by gap): {:.1} us (checksum {sink})",
        review * 1e6
    );
    println!(
        "routing: {:.1} ns per event for {k} bitset tests",
        ((with - base) - reviews as f64 * review).max(0.0) * 1e9 / total as f64
    );
}
