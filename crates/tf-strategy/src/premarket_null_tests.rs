//! T26 from scripted events: the draw, who may be bought, and the exits it shares with T25. A day is played in minutes of the
//! premarket (minute 0 is 04:00 New York).

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::cross::{CrossRunner, Market, Members};
use crate::exits::{REASON_STOP, REASON_TIME};
use crate::intent::{Intent, Pricing, Purpose, Side, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::premarket_null::{PremarketNull, PremarketNullParams, PremarketNullStats, REASON_ENTRY};

const D: i64 = 1_000_000_000;
const SEC: u64 = 1_000_000_000;

fn cents(c: i64) -> i64 {
    c * D / 100
}

fn hdr(id: u32, ts: Nanos) -> Header {
    Header {
        ts_event: ts,
        ts_recv: ts,
        seq: ts,
        instrument: id,
        provider: ProviderId::Synthetic,
    }
}

fn day(d: u8) -> SessionTimes {
    Calendar::us_equities()
        .times(Date::new(2026, 5, d).unwrap())
        .unwrap()
        .unwrap()
}

struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    runner: CrossRunner<PremarketNull>,
    day: SessionTimes,
    clock: u32,
}

impl Rig {
    fn new(p: PremarketNullParams, names: u32) -> Rig {
        Rig::on(p, names, day(1))
    }

    fn on(p: PremarketNullParams, names: u32, day: SessionTimes) -> Rig {
        Rig::build(p, names, day, false)
    }

    /// `tracing` is asked for before the first review, which is where the day's draw is made.
    fn build(p: PremarketNullParams, names: u32, day: SessionTimes, tracing: bool) -> Rig {
        let mut tier0 = Tier0::new(names as usize + 1);
        tier0.set_day(day);
        let mut refs: Vec<RefInfo> = (0..names).map(|_| RefInfo::default()).collect();
        refs.push(RefInfo::default());
        let mut runner = CrossRunner::new(
            PremarketNull::new(1, p).unwrap(),
            Members::from_ids(0..names),
        );
        runner.set_tracing(tracing);
        let mut rig = Rig {
            tier0,
            refs,
            runner,
            day,
            clock: names,
        };
        rig.tick(0, 0);
        rig.tick(0, 6);
        rig
    }

    fn at(&self, min: u64, sec: u64) -> Nanos {
        self.day.premarket + (min * 60 + sec) * SEC
    }

    fn feed(&mut self, ev: Event) {
        self.tier0.on_event(&ev);
        self.runner.on_event(
            Market {
                tier0: &self.tier0,
                refs: &self.refs,
            },
            None,
            None,
            &ev,
        );
    }

    fn tick(&mut self, min: u64, sec: u64) {
        let ts = self.at(min, sec);
        self.feed(Event::Quote(Quote {
            hdr: hdr(self.clock, ts),
            bid_px: Px::from_raw(cents(1)),
            ask_px: Px::from_raw(cents(2)),
            bid_sz: 1,
            ask_sz: 1,
        }));
    }

    fn quote(&mut self, id: u32, min: u64, sec: u64, bid: i64, ask: i64) {
        let ts = self.at(min, sec);
        self.feed(Event::Quote(Quote {
            hdr: hdr(id, ts),
            bid_px: Px::from_raw(cents(bid)),
            ask_px: Px::from_raw(cents(ask)),
            bid_sz: 100,
            ask_sz: 100,
        }));
    }

    fn trade(&mut self, id: u32, min: u64, sec: u64, px: i64, size: u32) {
        self.quote(id, min, sec, px - 1, px + 1);
        let ts = self.at(min, sec);
        self.feed(Event::Trade(Trade {
            hdr: hdr(id, ts + 1),
            px: Px::from_raw(cents(px)),
            size,
            flags: TradeFlags::NONE,
        }));
    }

    fn out(&mut self) -> Vec<Intent> {
        self.runner.drain_intents()
    }

    fn update(&mut self, u: OrderUpdate) {
        self.runner.on_order_update(&self.tier0, None, None, &u);
    }

    fn stats(&self) -> PremarketNullStats {
        self.runner.strategy().stats()
    }

    /// The names `ids` trade twice a minute for the first thirty minutes, 1,000 shares at $10.00 each: $20,000 a minute.
    fn active(&mut self, ids: &[u32]) {
        self.active_to(ids, 30);
    }

    /// The same for the first `minutes` minutes, ending with the tick that opens minute `minutes`.
    fn active_to(&mut self, ids: &[u32], minutes: u64) {
        for m in 0..minutes {
            for &id in ids {
                self.trade(id, m, 20, 1000, 1000);
                self.trade(id, m, 40, 1000, 1000);
            }
            self.tick(m + 1, 1);
        }
    }

    /// Time passes minute by minute to the end of minute `to`, collecting what is sent.
    fn run_to(&mut self, to: u64) -> Vec<Intent> {
        let mut out = self.out();
        for m in 30..=to {
            self.tick(m, 1);
            out.extend(self.out());
        }
        out
    }
}

/// Entries between minutes 25 and 30, so the test runs in half an hour of premarket.
fn params() -> PremarketNullParams {
    PremarketNullParams {
        window_start_minutes: 25,
        window_end_minutes: 30,
        ..PremarketNullParams::default()
    }
}

fn p_with(f: impl FnOnce(&mut PremarketNullParams)) -> PremarketNullParams {
    let mut p = params();
    f(&mut p);
    p
}

/// A day with these names active and what the null bought on it.
fn bought(p: PremarketNullParams, names: u32, active: &[u32]) -> (Rig, Vec<Intent>) {
    let mut rig = Rig::new(p, names);
    rig.active(active);
    let out = rig.run_to(31);
    (rig, out)
}

// ---- the draw ----

#[test]
fn it_buys_names_drawn_at_times_inside_the_window_one_each() {
    let all: Vec<u32> = (0..12).collect();
    let (rig, out) = bought(params(), 12, &all);
    assert_eq!(out.len(), 3);
    let mut ids: Vec<u32> = out.iter().map(|i| i.instrument).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "three different names");
    for i in &out {
        assert_eq!(
            (i.side, i.purpose, i.tif, i.reason, i.protect),
            (Side::Buy, Purpose::Open, Tif::Day, REASON_ENTRY, None)
        );
        // $1,000 of whole shares at the $10.01 ask, a collar of 1% around it.
        assert_eq!(i64::from(i.qty), 1_000 * D / cents(1001));
        assert_eq!(
            i.pricing,
            Pricing::Collar {
                reference: Px::from_raw(cents(1001)),
                collar_permille: 10
            }
        );
        // Decided at the time drawn: between 04:25:00 and 04:30:00.
        assert!(
            i.ts >= rig.at(25, 0) && i.ts <= rig.at(30, 0),
            "{} outside the window",
            i.ts
        );
    }
    let s = rig.stats();
    assert_eq!((s.days, s.drawn, s.entries, s.skipped), (1, 3, 3, 0));
}

#[test]
fn a_seed_repeats_its_day_and_another_seed_draws_again() {
    let all: Vec<u32> = (0..12).collect();
    let run = |seed: u64| {
        let (_, out) = bought(p_with(|p| p.seed = seed), 12, &all);
        out.iter().map(|i| (i.instrument, i.ts)).collect::<Vec<_>>()
    };
    assert_eq!(run(7), run(7));
    let draws: Vec<_> = (1..=6).map(run).collect();
    let mut distinct = draws.clone();
    distinct.sort();
    distinct.dedup();
    assert!(distinct.len() >= 5, "six seeds, {} draws", distinct.len());
}

#[test]
fn another_day_is_another_draw_with_the_same_seed() {
    let all: Vec<u32> = (0..12).collect();
    let on = |d: u8| {
        let mut rig = Rig::on(params(), 12, day(d));
        rig.active(&all);
        let out = rig.run_to(31);
        let start = rig.at(0, 0);
        out.iter()
            .map(|i| (i.instrument, i.ts - start))
            .collect::<Vec<_>>()
    };
    assert_ne!(on(1), on(4));
}

#[test]
fn the_window_can_be_one_instant() {
    let p = p_with(|p| {
        p.window_start_minutes = 28;
        p.window_end_minutes = 28;
    });
    let all: Vec<u32> = (0..12).collect();
    let (rig, out) = bought(p, 12, &all);
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|i| i.ts == rig.at(28, 0)));
}

// ---- who may be bought ----

#[test]
fn only_active_names_in_the_band_with_a_fair_quote_are_bought() {
    // Names 0..3 are active; 3..12 have nothing.
    let (rig, out) = bought(params(), 12, &[0, 1, 2]);
    let mut ids: Vec<u32> = out.iter().map(|i| i.instrument).collect();
    ids.sort_unstable();
    assert_eq!(ids, [0, 1, 2], "all three, and nobody else");
    assert_eq!(rig.stats().skipped, 0);
    // Two active and three draws: two bought, one time with no name left.
    let (rig, out) = bought(params(), 12, &[0, 1]);
    assert_eq!(out.len(), 2);
    assert_eq!((rig.stats().entries, rig.stats().skipped), (2, 1));
    // None active: nothing is bought, and every time says so.
    let (rig, out) = bought(params(), 12, &[]);
    assert!(out.is_empty());
    assert_eq!((rig.stats().entries, rig.stats().skipped), (0, 3));
}

/// Whether the only name, active as these say, is bought at its one entry time.
fn one(p: PremarketNullParams, minutes: u64, trades_a_min: u64, size: u32, px: i64) -> bool {
    let p = p_with(|q| {
        *q = PremarketNullParams {
            names: 1,
            window_start_minutes: 28,
            window_end_minutes: 28,
            ..p
        }
    });
    let mut rig = Rig::new(p, 1);
    for m in 0..minutes {
        for t in 0..trades_a_min {
            rig.trade(0, m, 5 + t, px, size);
        }
        rig.tick(m + 1, 1);
    }
    for m in minutes + 1..=29 {
        rig.tick(m, 1);
    }
    rig.stats().entries == 1
}

#[test]
fn the_dollar_and_trade_floors_and_the_price_band_are_met_exactly() {
    let base = params();
    // 25 minutes of two trades of 1,000 shares at $10.00 is 50 trades and $500,000.
    assert!(one(base, 25, 2, 1000, 1000));
    let dollars = |d: u32| PremarketNullParams {
        min_dollars: d,
        ..base
    };
    assert!(one(dollars(500_000), 25, 2, 1000, 1000));
    assert!(!one(dollars(500_001), 25, 2, 1000, 1000));
    let trades = |t: u32| PremarketNullParams {
        min_trades: t,
        ..base
    };
    assert!(one(trades(50), 25, 2, 1000, 1000));
    assert!(!one(trades(51), 25, 2, 1000, 1000));
    let band = |lo: u32, hi: u32| PremarketNullParams {
        min_cents: lo,
        max_cents: hi,
        ..base
    };
    assert!(one(band(1000, 1000), 25, 2, 1000, 1000));
    assert!(!one(band(1001, 3000), 25, 2, 1000, 1000));
    assert!(!one(band(100, 999), 25, 2, 1000, 1000));
}

#[test]
fn a_wide_or_crossed_or_missing_quote_is_not_bought() {
    // The entry time is minute 29, judged on the last quote of minute 28.
    let judge = |bid: i64, ask: i64, cap: u32| {
        let p = p_with(|p| {
            p.names = 1;
            p.window_start_minutes = 29;
            p.window_end_minutes = 29;
            p.spread_cap_bp = cap;
        });
        let mut rig = Rig::new(p, 1);
        rig.active_to(&[0], 28);
        rig.quote(0, 28, 59, bid, ask);
        rig.tick(29, 1);
        rig.out().len() == 1
    };
    // A spread of exactly the cap (10 cents on a $10.00 mid is 100 basis points) is bought; a basis point less is not.
    assert!(judge(995, 1005, 100));
    assert!(!judge(995, 1005, 99));
    assert!(judge(1000, 1000, 0), "locked");
    assert!(!judge(1001, 1000, 0), "crossed");
    assert!(!judge(0, 1000, 0), "no bid");
}

#[test]
fn a_halted_name_is_not_bought() {
    let p = p_with(|p| {
        p.names = 1;
        p.window_start_minutes = 29;
        p.window_end_minutes = 29;
    });
    let mut rig = Rig::new(p, 1);
    rig.active_to(&[0], 28);
    let ts = rig.at(28, 59);
    rig.feed(Event::Status(Status {
        hdr: hdr(0, ts),
        kind: StatusKind::TradingHalt,
        lo: Px::from_raw(0),
        hi: Px::from_raw(0),
    }));
    rig.tick(29, 1);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().skipped, 1);
}

// ---- the exits ----

fn fill(i: &Intent, qty: u32, px: i64, ts: Nanos) -> OrderUpdate {
    OrderUpdate {
        intent: i.id,
        order: None,
        state: OrderState::Filled,
        filled_qty: qty,
        avg_px: Some(Px::from_raw(cents(px))),
        reject: None,
        ts,
    }
}

/// One name bought and filled at $10.01, at its entry time of minute 28.
fn held() -> (Rig, Intent) {
    let p = p_with(|p| {
        p.names = 1;
        p.window_start_minutes = 28;
        p.window_end_minutes = 28;
    });
    let mut rig = Rig::new(p, 1);
    rig.active_to(&[0], 28);
    let entry = rig.out().remove(0);
    rig.update(fill(&entry, entry.qty, 1001, rig.at(28, 2)));
    (rig, entry)
}

#[test]
fn the_first_stop_is_under_the_fill_and_the_trail_follows_the_high() {
    // A stop 3% under $10.01 is $9.7097. $9.75 is above it.
    let (mut rig, entry) = held();
    rig.trade(0, 29, 10, 975, 100);
    rig.tick(29, 20);
    assert!(rig.out().is_empty());
    // $9.70 is through it: sold.
    rig.trade(0, 29, 30, 970, 100);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].side, out[0].purpose, out[0].qty, out[0].reason),
        (Side::Sell, Purpose::Close, entry.qty, REASON_STOP)
    );

    // The price runs to $11.00 and the stop follows it to $10.67; $10.70 holds and $10.66 sells.
    let (mut rig, entry) = held();
    rig.trade(0, 29, 10, 1100, 100);
    rig.tick(29, 20);
    rig.trade(0, 29, 30, 1070, 100);
    rig.tick(29, 40);
    assert!(rig.out().is_empty());
    rig.trade(0, 29, 50, 1066, 100);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].qty, out[0].reason), (entry.qty, REASON_STOP));
}

#[test]
fn a_position_is_sold_before_the_open() {
    let (mut rig, entry) = held();
    rig.trade(0, 29, 10, 1010, 100);
    rig.tick(324, 59);
    assert!(rig.out().is_empty());
    rig.tick(325, 1);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].qty, out[0].reason, out[0].ts),
        (entry.qty, REASON_TIME, rig.at(325, 0))
    );
}

// ---- the parameters and the record ----

#[test]
fn parameters_read_back_exactly_and_every_bad_one_is_refused() {
    let p = PremarketNullParams::default();
    assert_eq!(PremarketNullParams::parse(&p.render()).unwrap(), p);
    assert_eq!(p.render().split_whitespace().count(), 14);
    let one = |k: &str, v: u64| {
        let text = p
            .render()
            .split_whitespace()
            .map(|w| match w.split_once('=') {
                Some((key, _)) if key == k => format!("{k}={v}"),
                _ => w.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        PremarketNullParams::parse(&text)
    };
    assert!(one("seed", 0).is_ok() && one("seed", u64::MAX).is_ok());
    assert!(one("names", 10_000).is_ok());
    assert!(one("window_end_minutes", 324).is_ok());
    assert!(one("stop_permille", 999).is_ok() && one("trail_permille", 999).is_ok());
    assert!(one("collar_permille", 999).is_ok());
    for (k, v) in [
        ("names", 0),
        ("names", 10_001),
        ("dollars", 0),
        ("window_start_minutes", 0),
        ("window_start_minutes", 316),
        ("window_end_minutes", 325),
        ("flat_minutes", 0),
        ("min_cents", 0),
        ("max_cents", 99),
        ("collar_permille", 1000),
        ("stop_permille", 0),
        ("stop_permille", 1000),
        ("trail_permille", 0),
        ("trail_permille", 1000),
        ("names", 4_294_967_296),
    ] {
        assert!(one(k, v).is_err(), "{k}={v}");
    }
    let text = p.render();
    assert!(PremarketNullParams::parse(&format!("{text} wat=1")).is_err());
    assert!(PremarketNullParams::parse(&format!("{text} seed=2")).is_err());
    assert!(PremarketNullParams::parse(&text.replace("seed=1 ", "")).is_err());
    assert!(PremarketNullParams::parse(&text.replace("seed=1", "seed=x")).is_err());
    assert!(PremarketNullParams::parse(&text.replace("seed=1", "seed")).is_err());
    assert!(PremarketNull::new(1, PremarketNullParams { names: 0, ..p }).is_err());
}

#[test]
fn recording_the_draw_changes_nothing_it_decides() {
    let all: Vec<u32> = (0..12).collect();
    let run = |tracing: bool| {
        let mut rig = Rig::build(params(), 12, day(1), tracing);
        rig.active(&all);
        let out = rig.run_to(31);
        (out, rig.runner.drain_traces())
    };
    let (a, ta) = run(false);
    let (b, tb) = run(true);
    assert_eq!(a, b);
    assert!(ta.is_empty());
    let kinds: Vec<&str> = tb.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, ["draw", "entry", "entry", "entry"]);
    assert_eq!(tb[0].rows.len(), 3);
    assert_eq!(tb[1].value("result"), Some("entered"));
    assert_eq!(tb[1].value("pool"), Some("12"));
}
