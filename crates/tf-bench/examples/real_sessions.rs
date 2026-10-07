//! Checks the session-by-session state of Tier 0 against Databento's own one-minute bars, on a real day.
//!
//! `cargo run --release -p tf-bench --example real_sessions -- DATE TRADES.csv[,TRADES2.csv] BARS.csv [keep-zero] [by-event]`
//!
//! `TRADES` are `schema=trades` CSVs (`pretty_px`, `pretty_ts`, `map_symbols`) of some symbols for one or
//! more windows of the day, `BARS` the `ohlcv-1m` CSV for the same symbols and day. The bars are
//! restricted to the windows the trades cover, so what is compared is what both sides saw: premarket,
//! the first minute, five and fifteen minutes after the open, the regular session in the windows given,
//! after-hours, and the open price. Prints each difference and exits non-zero if there is one.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

use tf_calendar::Calendar;
use tf_core::{Event, Header, Nanos, ProviderId, Px, Trade, TradeFlags};
use tf_engine::Tier0;

fn read(path: &str) -> (Vec<String>, Vec<Vec<String>>) {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (date, trade_files, bars_file) = (&args[1], &args[2], &args[3]);
    // Databento's bars count zero-share prints in their highs, lows and open; the decoder drops them.
    let keep_zero = args.iter().skip(4).any(|a| a == "keep-zero");
    // Place trades by event time instead of arrival time, to see which one the bars follow.
    let by_event = args.iter().skip(4).any(|a| a == "by-event");
    let p: Vec<i32> = date.split('-').map(|x| x.parse().unwrap()).collect();
    let day = tf_calendar::Date::new(p[0], p[1] as u8, p[2] as u8).expect("date");
    let times = Calendar::us_equities()
        .times(day)
        .unwrap()
        .expect("a trading day");

    let mut names: BTreeMap<String, u32> = BTreeMap::new();
    let mut events: Vec<Event> = Vec::new();
    let mut windows: Vec<(Nanos, Nanos)> = Vec::new();
    for file in trade_files.split(',') {
        let (head, rows) = read(file);
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
            let size: u32 = r[c_sz].parse().unwrap();
            let ts_event = tf_alpaca::events::parse_time(&r[c_ev]).unwrap();
            lo = lo.min(ts_event);
            hi = hi.max(ts_event);
            if size == 0 && !keep_zero {
                continue; // the decoder counts zero-share prints apart and drops them
            }
            let next = names.len() as u32;
            let inst = *names.entry(r[c_sym].clone()).or_insert(next);
            let ts_recv = tf_alpaca::events::parse_time(&r[c_rc]).unwrap();
            events.push(Event::Trade(Trade {
                hdr: Header {
                    ts_event,
                    ts_recv: if by_event { ts_event } else { ts_recv },
                    seq: r[c_seq].parse().unwrap_or(0),
                    instrument: inst,
                    provider: ProviderId::Databento,
                },
                px: Px::parse(&r[c_px]).unwrap(),
                size,
                flags: TradeFlags::NONE,
            }));
        }
        windows.push((lo, hi));
    }
    println!(
        "{} trades, {} symbols, windows {:?}",
        events.len(),
        names.len(),
        windows
    );

    let mut t0 = Tier0::new(names.len());
    t0.set_day(times);
    for e in &events {
        t0.on_event(e);
    }

    // The one-minute bars, kept when their minute lies inside a window the trades cover.
    let (head, rows) = read(bars_file);
    let (c_ts, c_o, c_h, c_l, c_v, c_s) = (
        col(&head, "ts_event"),
        col(&head, "open"),
        col(&head, "high"),
        col(&head, "low"),
        col(&head, "volume"),
        col(&head, "symbol"),
    );
    #[derive(Default, Clone, Copy)]
    struct Agg {
        vol: u64,
        hi: Option<Px>,
        lo: Option<Px>,
    }
    impl Agg {
        fn add(&mut self, v: u64, h: Px, l: Px) {
            self.vol += v;
            self.hi = Some(self.hi.map_or(h, |x| x.max(h)));
            self.lo = Some(self.lo.map_or(l, |x| x.min(l)));
        }
    }
    #[derive(Default)]
    struct Want {
        pm: Agg,
        reg: Agg,
        ah: Agg,
        first1: u64,
        first5: u64,
        or5: Agg,
        or15: Agg,
        open: Option<Px>,
    }
    let minute = 60 * 1_000_000_000u64;
    let mut want: BTreeMap<u32, Want> = BTreeMap::new();
    for r in rows {
        let Some(&inst) = names.get(&r[c_s]) else {
            continue;
        };
        let at = tf_alpaca::events::parse_time(&r[c_ts]).unwrap();
        // A bar counts if it starts and ends inside a window the trades covered.
        if !windows.iter().any(|&(lo, hi)| {
            at >= lo - lo % minute
                && at + minute <= hi - hi % minute + minute
                && at >= lo.saturating_sub(minute)
        }) {
            continue;
        }
        let v: u64 = r[c_v].parse().unwrap();
        if v == 0 {
            continue;
        }
        let (h, l) = (Px::parse(&r[c_h]).unwrap(), Px::parse(&r[c_l]).unwrap());
        let w = want.entry(inst).or_default();
        if at < times.open {
            w.pm.add(v, h, l);
        } else if at < times.close {
            w.reg.add(v, h, l);
            let since = at - times.open;
            if since == 0 {
                w.first1 = v;
                w.open = Some(Px::parse(&r[c_o]).unwrap());
            }
            if since < 5 * minute {
                w.first5 += v;
                w.or5.add(v, h, l);
            }
            if since < 15 * minute {
                w.or15.add(v, h, l);
            }
        } else {
            w.ah.add(v, h, l);
        }
    }

    let mut bad = 0;
    let mut checked = 0;
    for (name, &inst) in &names {
        let s = t0.session(inst).expect("state");
        let w = want.remove(&inst).unwrap_or_default();
        let mut check = |what: &str, got: String, exp: String| {
            checked += 1;
            if got != exp {
                bad += 1;
                println!("{name} {what}: ours {got}, bars {exp}");
            }
        };
        let hl =
            |h: Option<Px>, l: Option<Px>| format!("{:?}/{:?}", h.map(Px::raw), l.map(Px::raw));
        check(
            "premarket volume",
            s.premarket.volume.to_string(),
            w.pm.vol.to_string(),
        );
        check(
            "premarket hi/lo",
            hl(s.premarket.high, s.premarket.low),
            hl(w.pm.hi, w.pm.lo),
        );
        check(
            "regular volume",
            s.regular.volume.to_string(),
            w.reg.vol.to_string(),
        );
        check(
            "regular hi/lo",
            hl(s.regular.high, s.regular.low),
            hl(w.reg.hi, w.reg.lo),
        );
        check(
            "first minute",
            s.first_minute_volume.to_string(),
            w.first1.to_string(),
        );
        check(
            "first 5 minutes",
            s.first_5m_volume.to_string(),
            w.first5.to_string(),
        );
        check(
            "range 5m",
            hl(s.range_5m.high, s.range_5m.low),
            hl(w.or5.hi, w.or5.lo),
        );
        check(
            "range 15m",
            hl(s.range_15m.high, s.range_15m.low),
            hl(w.or15.hi, w.or15.lo),
        );
        check(
            "open",
            format!("{:?}", s.open.map(|o| o.0.raw())),
            format!("{:?}", w.open.map(Px::raw)),
        );
        check(
            "after-hours volume",
            s.after_hours.volume.to_string(),
            w.ah.vol.to_string(),
        );
        check(
            "after-hours hi/lo",
            hl(s.after_hours.high, s.after_hours.low),
            hl(w.ah.hi, w.ah.lo),
        );
    }
    println!("{checked} comparisons, {bad} differences");
    std::process::exit(i32::from(bad > 0));
}
