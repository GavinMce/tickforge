//! Shared multi-timeframe bars on real trades (E19-S03).
//!
//! `cargo run --release -p tf-bench --example real_bars -- check DATE TRADES.csv[,TRADES2.csv] BARS.csv`
//!
//! Builds session-aligned bars for every symbol in the trades files (`schema=trades`, `pretty_px`,
//! `pretty_ts`, `map_symbols`; zero-share prints kept, as Databento's bars count them, and trades placed
//! by arrival time) and compares them with Databento's own `ohlcv-1m` bars for the minutes that lie wholly
//! inside the windows the trades cover. Then, for every symbol and for the 15-minute and session-hourly
//! bars it built, the pure functions of `tf_engine::bar_fns` (EMA, RSI, ATR, VWAP) against the streaming
//! indicators fed the same closes. Exits non-zero on any difference.
//!
//! `cargo run --release -p tf-bench --example real_bars -- load DATE FILE.csv.zst [N]`
//!
//! Time and memory of the shared bars for the `N` (default 2,000) busiest symbols of a whole-market file
//! (a `trades` CSV with `instrument_id`, as `real_trades` reads), session alignment, against Tier 0 alone.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Instant;

use tf_calendar::{Calendar, Date};
use tf_core::{Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Trade, TradeFlags};
use tf_engine::bar_fns;
use tf_engine::{
    Atr, BarClose, Ema, MtfConfig, Rsi, Seed, SharedBars, SymbolBars, TfBar, Tier0, Timeframe,
};

fn date_arg(s: &str) -> Date {
    let p: Vec<i32> = s.split('-').map(|x| x.parse().expect("date")).collect();
    Date::new(p[0], p[1] as u8, p[2] as u8).expect("date")
}

fn trade(inst: u32, ts_event: Nanos, ts_recv: Nanos, seq: u64, px: Px, size: u32) -> Event {
    Event::Trade(Trade {
        hdr: Header {
            ts_event,
            ts_recv,
            seq,
            instrument: inst,
            provider: ProviderId::Databento,
        },
        px,
        size,
        flags: TradeFlags::NONE,
    })
}

fn read_csv(path: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let mut lines = BufReader::new(std::fs::File::open(path).expect("open")).lines();
    let head: Vec<String> = lines
        .next()
        .unwrap()
        .unwrap()
        .split(',')
        .map(str::to_owned)
        .collect();
    let rows = lines
        .map(|l| l.unwrap().split(',').map(str::to_owned).collect())
        .collect();
    (head, rows)
}

fn col(head: &[String], name: &str) -> usize {
    head.iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column {name}"))
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn closed_bars(s: &SymbolBars, tf: Timeframe) -> Vec<&TfBar> {
    (0..s.closed_len(tf))
        .rev()
        .map(|i| s.closed(tf, i).unwrap())
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("check") if args.len() >= 5 => check(&args[2], &args[3], &args[4]),
        Some("load") if args.len() >= 4 => load(
            &args[2],
            &args[3],
            args.get(4).map_or(2000, |n| n.parse().expect("N")),
        ),
        _ => {
            eprintln!(
                "usage: real_bars check DATE TRADES.csv[,..] BARS.csv | real_bars load DATE FILE.csv.zst [N]"
            );
            std::process::exit(2);
        }
    }
}

fn check(date: &str, trade_files: &str, bars_file: &str) {
    let day = date_arg(date);
    let times = Calendar::us_equities()
        .times(day)
        .unwrap()
        .expect("a trading day");

    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    let mut events: Vec<Event> = Vec::new();
    let mut windows: Vec<(Nanos, Nanos)> = Vec::new();
    for file in trade_files.split(',') {
        let (head, rows) = read_csv(file);
        let (c_ev, c_rc, c_px, c_sz, c_sym, c_seq) = (
            col(&head, "ts_event"),
            col(&head, "ts_recv"),
            col(&head, "price"),
            col(&head, "size"),
            col(&head, "symbol"),
            col(&head, "sequence"),
        );
        let (mut lo, mut hi) = (Nanos::MAX, 0);
        for r in rows {
            let ts_recv = tf_alpaca::events::parse_time(&r[c_rc]).unwrap();
            lo = lo.min(ts_recv);
            hi = hi.max(ts_recv);
            let next = names.len() as u32;
            let inst = *names.entry(r[c_sym].clone()).or_insert(next);
            events.push(trade(
                inst,
                tf_alpaca::events::parse_time(&r[c_ev]).unwrap(),
                ts_recv,
                r[c_seq].parse().unwrap_or(0),
                Px::parse(&r[c_px]).unwrap(),
                r[c_sz].parse().unwrap(),
            ));
        }
        windows.push((lo, hi));
    }
    events.sort_by_key(Event::ts_recv); // stable: equal arrival times keep file order
    println!(
        "{} trades, {} symbols, windows {:?}",
        events.len(),
        names.len(),
        windows
    );

    let mut bars = SharedBars::new(MtfConfig::session(), names.len(), names.len());
    for id in 0..names.len() as u32 {
        bars.claim(1, id).unwrap();
    }
    let mut closes: Vec<BarClose> = Vec::new();
    let t = Instant::now();
    for e in &events {
        bars.on_event(e, &mut closes);
    }
    bars.advance_to(times.after_hours_end + 1, &mut closes);
    println!(
        "built in {:.2}s, {} closes, {} trades in no bar",
        t.elapsed().as_secs_f64(),
        closes.len(),
        bars.unplaced()
    );

    let mut bad = 0u64;

    // 1. One-minute bars against Databento's.
    let (head, rows) = read_csv(bars_file);
    let (c_ts, c_o, c_h, c_l, c_c, c_v, c_s) = (
        col(&head, "ts_event"),
        col(&head, "open"),
        col(&head, "high"),
        col(&head, "low"),
        col(&head, "close"),
        col(&head, "volume"),
        col(&head, "symbol"),
    );
    let minute = 60 * NANOS_PER_SEC;
    let inside = |start: Nanos| {
        windows.iter().any(|&(lo, hi)| {
            start >= lo.div_ceil(minute) * minute && start + minute <= hi / minute * minute
        })
    };
    type Ohlcv = (Px, Px, Px, Px, u64);
    let mut want: BTreeMap<(u32, u64), Ohlcv> = BTreeMap::new();
    for r in &rows {
        let start = tf_alpaca::events::parse_time(&r[c_ts]).unwrap();
        let Some(&inst) = names.get(&r[c_s]) else {
            continue;
        };
        if inside(start) {
            want.insert(
                (inst, start / NANOS_PER_SEC),
                (
                    Px::parse(&r[c_o]).unwrap(),
                    Px::parse(&r[c_h]).unwrap(),
                    Px::parse(&r[c_l]).unwrap(),
                    Px::parse(&r[c_c]).unwrap(),
                    r[c_v].parse().unwrap(),
                ),
            );
        }
    }
    let mut got: BTreeMap<(u32, u64), Ohlcv> = BTreeMap::new();
    for c in closes.iter().filter(|c| c.timeframe == Timeframe::M1) {
        let b = &c.bar;
        if inside(b.start_sec * NANOS_PER_SEC) {
            got.insert(
                (c.instrument, b.start_sec),
                (b.open, b.high, b.low, b.close, b.volume),
            );
        }
    }
    let (mut equal, mut differ, mut only_ours, mut only_theirs) = (0u64, 0u64, 0u64, 0u64);
    let mut by_field = [0u64; 5];
    for (k, g) in &got {
        match want.get(k) {
            None => only_ours += 1,
            Some(w) if w == g => equal += 1,
            Some(w) => {
                differ += 1;
                for (i, (a, b)) in [
                    (g.0.raw(), w.0.raw()),
                    (g.1.raw(), w.1.raw()),
                    (g.2.raw(), w.2.raw()),
                    (g.3.raw(), w.3.raw()),
                    (g.4 as i64, w.4 as i64),
                ]
                .into_iter()
                .enumerate()
                {
                    by_field[i] += u64::from(a != b);
                }
                if differ <= 5 {
                    let sym = names.iter().find(|(_, v)| **v == k.0).unwrap().0;
                    println!("  differs {sym} {}: ours {g:?} theirs {w:?}", k.1);
                }
            }
        }
    }
    for k in want.keys() {
        if !got.contains_key(k) {
            only_theirs += 1;
        }
    }
    println!(
        "one-minute bars inside the windows: {equal} equal, {differ} differ (open, high, low, close, volume: {by_field:?}), {only_ours} only ours, {only_theirs} only theirs"
    );
    bad += differ + only_ours + only_theirs;

    // 2. Session-hourly bars start where the calendar says.
    let (pre, open, close) = (
        times.premarket / NANOS_PER_SEC,
        times.open / NANOS_PER_SEC,
        times.close / NANOS_PER_SEC,
    );
    let mut hourly = 0u64;
    for c in closes.iter().filter(|c| c.timeframe == Timeframe::H1) {
        let s = c.bar.start_sec;
        let ok = if s < open {
            (s - pre) % 3600 == 0
        } else if s < close {
            (s - open) % 3600 == 0
        } else {
            (s - close) % 3600 == 0
        };
        hourly += 1;
        if !ok {
            bad += 1;
            println!("  hourly bar at {s} is not on its session's grid");
        }
    }
    println!(
        "{hourly} hourly bars, every one on its session's grid (04:00, 09:30, 16:00 + k hours)"
    );

    // 3. The pure functions against the streaming indicators, on the real closes.
    let mut compared = 0u64;
    for tf in [Timeframe::M15, Timeframe::H1] {
        for inst in 0..names.len() as u32 {
            let s = bars.symbol(1, inst).unwrap();
            let kept = closed_bars(s, tf);
            let fed: Vec<&TfBar> = closes
                .iter()
                .filter(|c| c.instrument == inst && c.timeframe == tf && c.bar.trades > 0)
                .map(|c| &c.bar)
                .collect();
            if s.closed_total(tf) as usize != kept.len() {
                continue; // outgrew the ring: not comparable by design
            }
            for period in [2u32, 5, 14, 20] {
                let (mut e, mut ef) = (
                    Ema::new(period, Seed::Sma),
                    Ema::new(period, Seed::FirstValue),
                );
                let (mut r, mut a) = (Rsi::new(period), Atr::new(period));
                for b in &fed {
                    e.update(b.close.raw());
                    ef.update(b.close.raw());
                    r.update(b.close.raw());
                    a.update_bar(b);
                }
                let it = || kept.iter().copied();
                for (name, got, want) in [
                    ("ema", bar_fns::ema(it(), period, Seed::Sma), e.value()),
                    (
                        "ema-first",
                        bar_fns::ema(it(), period, Seed::FirstValue),
                        ef.value(),
                    ),
                    ("rsi", bar_fns::rsi(it(), period), r.value()),
                    ("atr", bar_fns::atr(it(), period), a.value()),
                ] {
                    compared += 1;
                    if got != want {
                        bad += 1;
                        println!(
                            "  {name}({period}) {tf:?} symbol {inst}: function {got:?} streaming {want:?}"
                        );
                    }
                }
            }
            // VWAP over the closed bars against a trade-by-trade VWAP of the trades they hold.
            let mut v = tf_engine::Vwap::new();
            let (lo, hi) = (
                kept.first().map_or(0, |b| b.start_sec),
                kept.last().map_or(0, |b| b.start_sec + tf.secs()),
            );
            for e in &events {
                if let Event::Trade(t) = e {
                    let sec = t.hdr.ts_recv / NANOS_PER_SEC;
                    if t.hdr.instrument == inst && sec >= lo && sec < hi {
                        v.update(t.px.raw(), t.size);
                    }
                }
            }
            if !kept.is_empty() {
                compared += 1;
                let f = bar_fns::vwap(kept.iter().copied());
                if f != v.value() {
                    bad += 1;
                    println!(
                        "  vwap {tf:?} symbol {inst}: function {f:?} streaming {:?}",
                        v.value()
                    );
                }
            }
        }
    }
    println!(
        "{compared} indicator comparisons (EMA both seeds, RSI, ATR at 2, 5, 14, 20; VWAP) on 15-minute and hourly bars"
    );
    if bad > 0 {
        println!("{bad} DIFFERENCES");
        std::process::exit(1);
    }
    println!("all agree");
}

fn load(date: &str, path: &str, n: usize) {
    let day = date_arg(date);
    let times = Calendar::us_equities()
        .times(day)
        .unwrap()
        .expect("a trading day");
    let file = std::fs::File::open(path).expect("open");
    let dec = zstd::stream::read::Decoder::new(file).expect("zstd");
    let mut lines = BufReader::new(dec).lines();
    let head: Vec<String> = lines
        .next()
        .unwrap()
        .unwrap()
        .split(',')
        .map(str::to_owned)
        .collect();
    let (c_recv, c_event, c_inst, c_px, c_size, c_seq) = (
        col(&head, "ts_recv"),
        col(&head, "ts_event"),
        col(&head, "instrument_id"),
        col(&head, "price"),
        col(&head, "size"),
        col(&head, "sequence"),
    );
    let mut ids: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut events: Vec<Event> = Vec::new();
    for line in lines {
        let line = line.unwrap();
        let f: Vec<&str> = line.split(',').collect();
        let raw: u32 = f[c_inst].parse().unwrap();
        let next = ids.len() as u32;
        let inst = *ids.entry(raw).or_insert(next);
        events.push(trade(
            inst,
            tf_alpaca::events::parse_time(f[c_event]).unwrap(),
            tf_alpaca::events::parse_time(f[c_recv]).unwrap(),
            f[c_seq].parse().unwrap_or(0),
            tf_alpaca::wire::parse_px(f[c_px]).unwrap(),
            f[c_size].parse().unwrap(),
        ));
    }
    events.sort_by_key(Event::ts_recv);
    let mut count = vec![0u64; ids.len()];
    for e in &events {
        count[e.instrument() as usize] += 1;
    }
    let mut order: Vec<u32> = (0..ids.len() as u32).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(count[i as usize]), i));
    order.truncate(n);
    let share: u64 = order.iter().map(|&i| count[i as usize]).sum();
    println!(
        "{} trades, {} symbols; tracking the {} busiest ({:.1}% of the trades)",
        events.len(),
        ids.len(),
        order.len(),
        100.0 * share as f64 / events.len() as f64
    );

    // Tier 0 alone, then Tier 0 and the shared bars; each timed per event.
    let timer_floor = {
        let t = Instant::now();
        let mut x = 0u128;
        for _ in 0..1_000_000 {
            x ^= Instant::now().elapsed().as_nanos();
        }
        std::hint::black_box(x);
        t.elapsed().as_nanos() as f64 / 1e6
    };
    let run = |with_bars: bool| {
        let mut t0 = Tier0::new(ids.len());
        t0.set_day(times);
        let rss0 = rss_kb();
        let mut bars = SharedBars::new(MtfConfig::session(), ids.len(), order.len());
        if with_bars {
            for &i in &order {
                bars.claim(1, i).unwrap();
            }
        }
        let rss1 = rss_kb();
        let mut closes: Vec<BarClose> = Vec::new();
        let mut per_event: Vec<u32> = Vec::with_capacity(events.len());
        let mut busiest: BTreeMap<u64, (u64, f64)> = BTreeMap::new();
        let whole = Instant::now();
        for e in &events {
            let t = Instant::now();
            t0.on_event(e);
            if with_bars {
                closes.clear();
                bars.on_event(e, &mut closes);
            }
            let ns = t.elapsed().as_nanos() as u32;
            per_event.push(ns);
            let b = busiest.entry(e.ts_recv() / NANOS_PER_SEC).or_default();
            b.0 += 1;
            b.1 += f64::from(ns) / 1e9;
        }
        let total = whole.elapsed().as_secs_f64();
        let rss2 = rss_kb();
        per_event.sort_unstable();
        let q = |p: f64| per_event[((per_event.len() - 1) as f64 * p) as usize];
        (
            total,
            per_event.len(),
            q(0.5),
            q(0.99),
            q(0.999),
            *per_event.last().unwrap(),
            busiest,
            bars,
            rss1 - rss0,
            rss2 - rss1,
        )
    };
    let (t_a, n, a50, a99, a999, amax, _, _, _, _) = run(false);
    let (t_b, _, b50, b99, b999, bmax, busiest, bars, rss_claims, rss_run) = run(true);
    println!("timer floor {timer_floor:.0} ns per reading (included in the per-event figures)");
    println!(
        "Tier 0 alone:        {:.3}s = {:.1}M events/s; per event p50 {a50} ns, p99 {a99} ns, p99.9 {a999} ns, max {amax} ns",
        t_a,
        n as f64 / t_a / 1e6
    );
    println!(
        "Tier 0 and bars:     {:.3}s = {:.1}M events/s; per event p50 {b50} ns, p99 {b99} ns, p99.9 {b999} ns, max {bmax} ns",
        t_b,
        n as f64 / t_b / 1e6
    );
    println!(
        "the bars add {:.0} ns an event on average over {} tracked symbols ({} events)",
        (t_b - t_a) / n as f64 * 1e9,
        bars.tracked(),
        n
    );
    let (sec, (cnt, secs)) = busiest.iter().max_by_key(|(_, v)| v.0).unwrap();
    println!(
        "busiest second ({sec}): {cnt} events took {:.1} ms with the bars, {:.0}x faster than real time",
        secs * 1e3,
        1.0 / secs
    );
    let size = std::mem::size_of::<SymbolBars>();
    println!(
        "memory: {} bytes a symbol, {:.1} MB for {} symbols (resident growth: {} MB on claiming, {} MB while running)",
        size,
        (size * bars.tracked()) as f64 / 1e6,
        bars.tracked(),
        rss_claims / 1024,
        rss_run / 1024
    );
    println!(
        "{} trades in no bar (session alignment), {} bars refused",
        bars.unplaced(),
        bars.stats()
            .iter()
            .map(|(_, s)| s.refused_full)
            .sum::<u64>()
    );
}
