//! T14 from scripted events: the draw is of the seed and the day, it is spread over the window, a name that cannot be bought
//! is skipped, and the exits are the exit book's.

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::closing_reversal::ParamError;
use crate::cross::{CrossRunner, CrossStrategy, Market, Members};
use crate::exits::{REASON_STOP, REASON_TARGET, REASON_TIME};
use crate::intent::{Intent, Pricing, Protective, Purpose, Side, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::random_entries::{REASON_ENTRY, RandomEntries, RandomEntriesParams};

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

fn trade(id: u32, ts: Nanos, px: i64) -> Event {
    Event::Trade(Trade {
        hdr: hdr(id, ts),
        px: Px::from_raw(px),
        size: 100,
        flags: TradeFlags::NONE,
    })
}

fn quote(id: u32, ts: Nanos, bid: i64, ask: i64) -> Event {
    Event::Quote(Quote {
        hdr: hdr(id, ts),
        bid_px: Px::from_raw(bid),
        ask_px: Px::from_raw(ask),
        bid_sz: 100,
        ask_sz: 100,
    })
}

fn status(id: u32, ts: Nanos, kind: StatusKind) -> Event {
    Event::Status(Status {
        hdr: hdr(id, ts),
        kind,
        lo: Px::from_raw(0),
        hi: Px::from_raw(0),
    })
}

fn times(y: i32, m: u8, d: u8) -> SessionTimes {
    Calendar::us_equities()
        .times(Date::new(y, m, d).unwrap())
        .unwrap()
        .unwrap()
}

fn regular() -> SessionTimes {
    times(2026, 5, 1)
}

struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    runner: CrossRunner<RandomEntries>,
    close: Nanos,
    n: u32,
    clock: u32,
}

impl Rig {
    /// `n` members; the day is `day` (none: the host has not told Tier 0).
    fn new(p: RandomEntriesParams, n: u32, day: Option<SessionTimes>) -> Rig {
        Rig::with_tracing(p, n, day, false)
    }

    /// As [`Rig::new`], asking for traces from the start: the day is drawn while the rig warms up.
    fn with_tracing(
        p: RandomEntriesParams,
        n: u32,
        day: Option<SessionTimes>,
        tracing: bool,
    ) -> Rig {
        let mut tier0 = Tier0::new(n as usize + 1);
        let close = day.map_or(20 * 3600 * SEC, |d| d.close);
        if let Some(d) = day {
            tier0.set_day(d);
        }
        let mut rig = Rig {
            tier0,
            refs: vec![RefInfo::default(); n as usize + 1],
            runner: CrossRunner::new(RandomEntries::new(1, p).unwrap(), Members::from_ids(0..n)),
            close,
            n,
            clock: n,
        };
        if tracing {
            rig.runner.set_tracing(true);
        }
        // The first event arms the minute's review, the next one after it holds it: the strategy draws its day.
        rig.tick(4 * 3600);
        rig.tick(4 * 3600 - 61);
        rig
    }

    fn t(&self, secs: u64) -> Nanos {
        self.close - secs * SEC
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

    fn tick(&mut self, secs_before_close: u64) {
        let ev = quote(self.clock, self.t(secs_before_close), cents(1), cents(2));
        self.feed(ev);
    }

    fn update(&mut self, u: OrderUpdate) {
        self.runner.on_order_update(&self.tier0, None, None, &u);
    }

    fn out(&mut self) -> Vec<Intent> {
        self.runner.drain_intents()
    }

    /// Every member quotes 19.99 / 20.01.
    fn quote_all(&mut self, secs_before_close: u64) {
        for i in 0..self.n {
            let ts = self.t(secs_before_close) + u64::from(i);
            self.feed(quote(i, ts, cents(1999), cents(2001)));
        }
    }

    /// Quote everyone and run only as far as the 15:30 entries: the clock stops just after them.
    fn enter_only(&mut self) -> Vec<Intent> {
        self.quote_all(5000);
        self.tick(1790);
        self.out()
    }

    /// Quote everyone, then let the whole window pass: the entries of the day, in the order they were sent.
    fn draw(&mut self) -> Vec<Intent> {
        self.quote_all(5000);
        self.tick(100);
        self.out()
    }
}

fn p(seed: u64, names: u32) -> RandomEntriesParams {
    RandomEntriesParams {
        seed,
        names,
        ..RandomEntriesParams::default()
    }
}

fn picks(out: &[Intent]) -> Vec<(u32, Nanos)> {
    out.iter().map(|i| (i.instrument, i.ts)).collect()
}

// ---- the draw ----

#[test]
fn a_seed_repeats_its_day_and_another_seed_or_another_day_is_another_draw() {
    let day = |seed, d| {
        let mut rig = Rig::new(p(seed, 8), 40, Some(d));
        picks(&rig.draw())
    };
    let a = day(7, regular());
    assert_eq!(a.len(), 8);
    assert_eq!(a, day(7, regular()), "the same seed on the same day");
    assert_ne!(a, day(8, regular()), "another seed");
    assert_ne!(
        a,
        day(7, times(2026, 5, 4))
            .iter()
            .map(|&(i, _)| (i, 0))
            .collect::<Vec<_>>()
    );
    // The draw of the next day, a Monday, is its own: the same seed, other names.
    let next: Vec<u32> = day(7, times(2026, 5, 4)).iter().map(|x| x.0).collect();
    let first: Vec<u32> = a.iter().map(|x| x.0).collect();
    assert_ne!(first, next);
}

#[test]
fn it_draws_distinct_members_and_only_members() {
    let mut rig = Rig::new(p(3, 20), 50, Some(regular()));
    let out = rig.draw();
    let mut ids: Vec<u32> = out.iter().map(|i| i.instrument).collect();
    assert_eq!(ids.len(), 20);
    assert!(ids.iter().all(|&i| i < 50), "{ids:?}");
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 20, "no name twice");
    // Fewer members than names: each member once.
    let mut rig = Rig::new(p(3, 20), 5, Some(regular()));
    let mut ids: Vec<u32> = rig.draw().iter().map(|i| i.instrument).collect();
    ids.sort();
    assert_eq!(ids, [0, 1, 2, 3, 4]);
    let s = rig.runner.strategy().stats();
    assert_eq!((s.days, s.drawn, s.entries, s.skipped), (1, 5, 5, 0));
    // Every member is as likely as any: across many seeds each of ten comes up about as often as a fifth of the time.
    let mut seen = [0u32; 10];
    for seed in 0..200 {
        let mut rig = Rig::new(p(seed, 2), 10, Some(regular()));
        for i in rig.draw() {
            seen[i.instrument as usize] += 1;
        }
    }
    assert!(seen.iter().all(|&c| (20..=60).contains(&c)), "{seen:?}");
}

#[test]
fn an_entry_time_is_a_second_in_the_window_and_the_window_is_used_to_its_ends() {
    // 31 to 30 minutes before the close: 61 possible seconds. 2,000 names are enough to meet them all.
    let params = RandomEntriesParams {
        window_start_minutes: 31,
        window_end_minutes: 30,
        ..p(5, 2_000)
    };
    let mut rig = Rig::new(params, 2_000, Some(regular()));
    let out = rig.draw();
    assert_eq!(out.len(), 2_000);
    let before: Vec<u64> = out.iter().map(|i| (rig.close - i.ts) / SEC).collect();
    assert!(out.iter().all(|i| (rig.close - i.ts) % SEC == 0));
    assert_eq!(
        (*before.iter().min().unwrap(), *before.iter().max().unwrap()),
        (1800, 1860)
    );
    let mut distinct: Vec<u64> = before.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 61);
    // Sent in the order of their times.
    assert!(out.windows(2).all(|w| w[0].ts <= w[1].ts));
    // One instant: no randomness in the time, only in the names.
    let mut rig = Rig::new(p(5, 10), 30, Some(regular()));
    assert!(rig.draw().iter().all(|i| i.ts == rig.close - 1800 * SEC));
    // A wide window is spread over: ten minutes in four quarters.
    let params = RandomEntriesParams {
        window_start_minutes: 60,
        window_end_minutes: 30,
        ..p(5, 400)
    };
    let mut rig = Rig::new(params, 400, Some(regular()));
    let mut quarters = [0u32; 4];
    for i in rig.draw() {
        let into = (rig.close - 1800 * SEC - i.ts) / SEC;
        assert!(into <= 1800);
        quarters[(into * 4 / 1801) as usize] += 1;
    }
    assert!(
        quarters.iter().all(|&c| (70..=130).contains(&c)),
        "{quarters:?}"
    );
}

#[test]
fn a_name_that_cannot_be_bought_when_its_time_comes_is_skipped_and_counted() {
    let mut rig = Rig::new(p(1, 5), 5, Some(regular()));
    // Name 0 halted, 1 restricted, 2 never quoted, 3 crossed, 4 fine.
    rig.feed(status(0, rig.t(6000), StatusKind::TradingHalt));
    rig.feed(status(1, rig.t(6000), StatusKind::ShortSaleRestriction));
    for (i, bid, ask) in [
        (0u32, 1999, 2001),
        (1, 1999, 2001),
        (3, 2001, 1999),
        (4, 1999, 2001),
    ] {
        rig.feed(quote(i, rig.t(5000) + u64::from(i), cents(bid), cents(ask)));
    }
    rig.tick(100);
    let out = rig.out();
    assert_eq!(out.iter().map(|i| i.instrument).collect::<Vec<_>>(), [4]);
    let s = rig.runner.strategy().stats();
    assert_eq!((s.drawn, s.entries, s.skipped), (5, 1, 4));
    assert_eq!(rig.runner.invalid_intents(), 0);
    // A name whose dollars buy no share, and a bid of nothing.
    let params = RandomEntriesParams {
        dollars: 1,
        ..p(1, 3)
    };
    let mut rig = Rig::new(params, 3, Some(regular()));
    rig.feed(quote(0, rig.t(5000), cents(1999), cents(2001)));
    rig.feed(quote(1, rig.t(5000) + 1, cents(1999), cents(2001)));
    rig.feed(quote(2, rig.t(5000) + 2, cents(1999), cents(2001)));
    rig.tick(100);
    assert!(rig.out().is_empty());
    assert_eq!(rig.runner.strategy().stats().skipped, 3);
    // A price of a billionth of a dollar: a size that fits and a stop that rounds to nothing.
    let params = RandomEntriesParams {
        dollars: 1,
        ..p(1, 1)
    };
    let mut rig = Rig::new(params, 1, Some(regular()));
    rig.feed(quote(0, rig.t(5000), 1, 1));
    rig.tick(100);
    assert!(rig.out().is_empty());
    assert_eq!(rig.runner.strategy().stats().skipped, 1);
}

#[test]
fn an_entry_is_the_same_order_as_the_closing_reversals() {
    let mut rig = Rig::new(p(2, 1), 4, Some(regular()));
    let out = rig.draw();
    assert_eq!(out.len(), 1);
    let i = &out[0];
    let ask = cents(2001);
    assert_eq!(
        (i.side, i.purpose, i.tif, i.reason),
        (Side::Buy, Purpose::Open, Tif::Day, REASON_ENTRY)
    );
    assert_eq!(
        i.pricing,
        Pricing::Collar {
            reference: Px::from_raw(ask),
            collar_permille: 5
        }
    );
    // $2,000 over the ask, whole shares; the disaster stop ten percent under it.
    assert_eq!(i64::from(i.qty), 2_000 * D / ask);
    assert_eq!(i.qty, 99);
    assert_eq!(
        i.protect,
        Some(Protective {
            stop_trigger: Px::from_raw(ask * 900 / 1000),
            stop_limit: None,
            take_profit: None
        })
    );
}

// ---- the exits ----

fn filled(i: &Intent, qty: u32, ts: Nanos) -> OrderUpdate {
    OrderUpdate {
        intent: i.id,
        order: None,
        state: OrderState::Filled,
        filled_qty: qty,
        avg_px: Some(Px::from_raw(cents(2001))),
        reject: None,
        ts,
    }
}

#[test]
fn what_filled_is_sold_after_the_hold_and_never_later_than_half_a_minute_before_the_close() {
    // Entries at 15:30; a hold of ten minutes after the fill.
    let params = RandomEntriesParams {
        hold_seconds: 600,
        ..p(1, 1)
    };
    let mut rig = Rig::new(params, 3, Some(regular()));
    let out = rig.enter_only();
    rig.feed(trade(out[0].instrument, rig.t(1795), cents(2000)));
    let at = rig.t(1800);
    rig.update(filled(&out[0], 99, at));
    // Nothing a moment before the hold is up, a sale of the shares that filled at it.
    rig.tick(1800 - 599);
    assert!(rig.out().is_empty());
    rig.tick(1800 - 600);
    let exits = rig.out();
    assert_eq!(exits.len(), 1);
    let e = &exits[0];
    assert_eq!(
        (e.instrument, e.side, e.purpose, e.qty, e.reason),
        (
            out[0].instrument,
            Side::Sell,
            Purpose::Close,
            99,
            REASON_TIME
        )
    );
    assert_eq!(e.ts, at + 600 * SEC);
    assert_eq!(
        e.pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(2000)),
            collar_permille: 5
        }
    );
    // The default hold from a 15:30 fill is up at 15:59:30.05: the exit is at 15:59:30, the margin.
    let mut rig = Rig::new(p(1, 1), 3, Some(regular()));
    let out = rig.enter_only();
    rig.feed(trade(out[0].instrument, rig.t(1795), cents(2000)));
    rig.update(filled(&out[0], 99, rig.t(1800) + 50_000_000));
    rig.tick(31);
    assert!(rig.out().is_empty());
    rig.tick(30);
    let exits = rig.out();
    assert_eq!((exits.len(), exits[0].ts), (1, rig.t(30)));
    // Nothing filled, nothing sold.
    let mut rig = Rig::new(p(1, 1), 3, Some(regular()));
    let out = rig.enter_only();
    rig.update(OrderUpdate {
        state: OrderState::Rejected,
        ..filled(&out[0], 0, rig.t(1799))
    });
    rig.tick(10);
    assert!(rig.out().is_empty());
}

#[test]
fn a_reference_that_has_a_stop_and_a_target_has_them_here_from_the_average_fill_price() {
    let params = RandomEntriesParams {
        exit_stop_permille: 20,
        target_permille: 30,
        ..p(1, 1)
    };
    // Fill at 20.01: the stop is 19.6098, the target 20.6103.
    let run = |px: i64| {
        let mut rig = Rig::new(params, 1, Some(regular()));
        let out = rig.draw();
        rig.update(filled(&out[0], 99, rig.t(1800)));
        rig.feed(trade(0, rig.t(1700), px));
        rig.out()
    };
    assert!(run(cents(2000)).is_empty(), "between them");
    assert!(run(cents(1962)).is_empty(), "above the stop");
    let s = run(cents(1960));
    assert_eq!(
        (s.len(), s[0].reason, s[0].side),
        (1, REASON_STOP, Side::Sell)
    );
    assert_eq!(s[0].qty, 99);
    assert!(run(cents(2061)).is_empty(), "under the target");
    let t = run(cents(2062));
    assert_eq!((t.len(), t[0].reason), (1, REASON_TARGET));
    // With none asked for, a price far under the entry is not an exit.
    let mut rig = Rig::new(p(1, 1), 1, Some(regular()));
    let out = rig.draw();
    rig.update(filled(&out[0], 99, rig.t(1800)));
    rig.feed(trade(0, rig.t(1700), cents(1500)));
    assert!(rig.out().is_empty());
}

// ---- days ----

#[test]
fn it_draws_again_the_next_day_and_not_at_all_when_it_is_not_told_the_day() {
    let mut rig = Rig::new(p(4, 3), 20, Some(regular()));
    let a = picks(&rig.draw());
    let d2 = times(2026, 5, 4);
    rig.tier0.set_day(d2);
    rig.close = d2.close;
    rig.tick(4 * 3600);
    rig.tick(4 * 3600 - 61);
    let b = picks(&rig.draw());
    assert_eq!((a.len(), b.len()), (3, 3));
    assert_ne!(
        a.iter().map(|x| x.0).collect::<Vec<_>>(),
        b.iter().map(|x| x.0).collect::<Vec<_>>()
    );
    assert_eq!(rig.runner.strategy().stats().days, 2);
    assert!(b.iter().all(|&(_, ts)| ts == d2.close - 1800 * SEC));
    // Not told the day: nothing is drawn.
    let mut rig = Rig::new(p(4, 3), 20, None);
    assert!(rig.draw().is_empty());
    assert_eq!(rig.runner.strategy().stats().days, 0);
    assert_eq!(rig.runner.strategy().period(), 60 * SEC);
}

// ---- parameters ----

#[test]
fn parameters_read_back_exactly_and_every_wrong_one_is_refused() {
    let p0 = RandomEntriesParams::default();
    assert_eq!(
        p0.render(),
        "seed=1 names=20 dollars=2000 collar_permille=5 stop_permille=100 window_start_minutes=30 window_end_minutes=30 hold_seconds=1770 exit_stop_permille=0 target_permille=0"
    );
    assert_eq!(RandomEntriesParams::parse(&p0.render()), Ok(p0));
    let q = RandomEntriesParams {
        seed: u64::MAX,
        names: 100_000,
        dollars: 1,
        collar_permille: 999,
        stop_permille: 999,
        window_start_minutes: 390,
        window_end_minutes: 1,
        hold_seconds: u32::MAX,
        exit_stop_permille: 999,
        target_permille: 1_000_000,
    };
    assert_eq!(RandomEntriesParams::parse(&q.render()), Ok(q));
    let with = |f: fn(&mut RandomEntriesParams)| {
        let mut x = p0;
        f(&mut x);
        x
    };
    for (bad, what) in [
        (with(|x| x.names = 0), "names"),
        (with(|x| x.names = 100_001), "names"),
        (with(|x| x.dollars = 0), "dollars"),
        (with(|x| x.collar_permille = 1000), "collar_permille"),
        (with(|x| x.stop_permille = 0), "stop_permille"),
        (with(|x| x.stop_permille = 1000), "stop_permille"),
        (with(|x| x.window_end_minutes = 0), "window_end_minutes"),
        (with(|x| x.window_end_minutes = 31), "window_end_minutes"),
        (
            with(|x| x.window_start_minutes = 391),
            "window_start_minutes",
        ),
        (with(|x| x.hold_seconds = 0), "hold_seconds"),
        (with(|x| x.exit_stop_permille = 1000), "exit_stop_permille"),
        (with(|x| x.target_permille = 1_000_001), "target_permille"),
    ] {
        let e = bad.validate().unwrap_err().0;
        assert!(e.contains(what), "{what}: {e}");
        assert!(RandomEntriesParams::parse(&bad.render()).is_err());
        assert!(RandomEntries::new(1, bad).is_err());
    }
    for (text, what) in [
        ("seed", "not key=value"),
        ("seed=1 colour=3", "not a parameter"),
        ("seed=one", "whole number"),
        ("seed=-1", "whole number"),
        ("seed=1 seed=1", "twice"),
        ("seed=1", "missing"),
        ("", "missing"),
        (
            "seed=1 names=99999999999 dollars=1 collar_permille=0 stop_permille=1 window_start_minutes=1 window_end_minutes=1 hold_seconds=1 exit_stop_permille=0 target_permille=0",
            "too large",
        ),
    ] {
        let ParamError(e) = RandomEntriesParams::parse(text).unwrap_err();
        assert!(e.contains(what), "{text}: {e}");
    }
}

#[test]
fn a_seeds_draw_is_pinned_so_that_a_seed_means_the_same_thing_in_every_version() {
    // Seed 7 on 1 May 2026, eight of forty names, entered between 15:00 and 15:30 (60 to 30 minutes before the close). The
    // draw is the first eight of a shuffle by SplitMix64 started from the seed plus the close times 0x9E3779B97F4A7C15,
    // then a second in the window for each; the numbers are from an independent implementation (scripts-style, in Python).
    let params = RandomEntriesParams {
        window_start_minutes: 60,
        window_end_minutes: 30,
        ..p(7, 8)
    };
    let mut rig = Rig::new(params, 40, Some(regular()));
    let out = rig.draw();
    let got: Vec<(u32, u64)> = out
        .iter()
        .map(|i| (i.instrument, (rig.close - i.ts) / SEC))
        .collect();
    assert_eq!(
        got,
        [
            (2, 3337),
            (1, 3249),
            (4, 3240),
            (20, 3236),
            (14, 2834),
            (11, 2581),
            (13, 1997),
            (22, 1843)
        ]
    );
}

#[test]
fn a_locked_quote_is_bought_and_a_bid_of_nothing_is_not() {
    let mut rig = Rig::new(p(1, 2), 2, Some(regular()));
    // Name 0 locked (bid equal to ask): a quote, bought: $2,000 at 20.00 is 100 shares. Name 1 a bid of nothing.
    rig.feed(quote(0, rig.t(5000), cents(2000), cents(2000)));
    rig.feed(quote(1, rig.t(5000) + 1, 0, cents(2001)));
    rig.tick(100);
    let out = rig.out();
    assert_eq!(picks(&out).iter().map(|x| x.0).collect::<Vec<_>>(), [0]);
    assert_eq!(out[0].qty, 100);
    assert_eq!(rig.runner.strategy().stats().skipped, 1);
    assert_eq!(rig.runner.invalid_intents(), 0);
}

#[test]
fn a_size_too_big_for_a_share_count_and_a_stop_of_nothing_are_skipped_before_they_are_sent() {
    let big = RandomEntriesParams {
        dollars: u32::MAX,
        ..p(1, 1)
    };
    let mut rig = Rig::new(big, 1, Some(regular()));
    rig.feed(quote(0, rig.t(5000), 1, 1));
    rig.tick(100);
    assert!(rig.out().is_empty());
    assert_eq!(rig.runner.strategy().stats().skipped, 1);
    assert_eq!(rig.runner.invalid_intents(), 0);
    // A price of a billionth with one dollar: 1e9 shares fit, and a stop 10% under rounds to nothing.
    let one = RandomEntriesParams {
        dollars: 1,
        ..p(1, 1)
    };
    let mut rig = Rig::new(one, 1, Some(regular()));
    rig.feed(quote(0, rig.t(5000), 1, 1));
    rig.tick(100);
    assert!(rig.out().is_empty());
    assert_eq!(
        (
            rig.runner.strategy().stats().skipped,
            rig.runner.invalid_intents()
        ),
        (1, 0)
    );
    // And one dollar at $20 buys no share.
    let mut rig = Rig::new(one, 1, Some(regular()));
    rig.quote_all(5000);
    rig.tick(100);
    assert!(rig.out().is_empty());
    assert_eq!(
        (
            rig.runner.strategy().stats().skipped,
            rig.runner.invalid_intents()
        ),
        (1, 0)
    );
}

#[test]
fn the_null_of_a_closing_reversal_is_its_own_names_dollars_collar_stop_entry_and_exit() {
    use crate::closing_reversal::ClosingReversalParams;
    // The defaults of the two are one another's null.
    let d = RandomEntriesParams::from(&ClosingReversalParams::default());
    assert_eq!(d, RandomEntriesParams::default());
    assert_eq!(d.seed, 1);
    // Another variant: ten names, $500, a 2 permille collar, a 7 permille stop, bought at 15:00 and sold 45 seconds before
    // the close: an hour less 45 seconds held.
    let c = ClosingReversalParams {
        names: 10,
        dollars: 500,
        collar_permille: 2,
        stop_permille: 7,
        entry_minutes: 60,
        exit_seconds: 45,
        extreme_bp: 300,
        spread_cap_bp: 5,
        ..ClosingReversalParams::default()
    };
    let n = RandomEntriesParams::from(&c);
    assert_eq!(
        (n.names, n.dollars, n.collar_permille, n.stop_permille),
        (10, 500, 2, 7)
    );
    assert_eq!((n.window_start_minutes, n.window_end_minutes), (60, 60));
    assert_eq!(n.hold_seconds, 3_555);
    // Its own filters have no null of their own; and the result is a valid null.
    assert_eq!((n.exit_stop_permille, n.target_permille), (0, 0));
    assert_eq!(n.validate(), Ok(()));
}

// ---- the traces (E19-S32) ----

#[test]
fn the_draw_and_each_entry_are_traced_and_a_strategy_not_asked_records_nothing() {
    let params = RandomEntriesParams {
        window_start_minutes: 31,
        window_end_minutes: 30,
        ..p(5, 3)
    };
    let mut rig = Rig::with_tracing(params, 10, Some(regular()), true);
    let out = rig.draw();
    let traces = rig.runner.drain_traces();
    // The draw, first, then an entry for each name.
    assert_eq!(
        traces.iter().map(|t| t.kind.as_str()).collect::<Vec<_>>(),
        ["draw", "entry", "entry", "entry"]
    );
    let d = &traces[0];
    for (k, v) in [
        ("seed", "5"),
        ("names", "3"),
        ("members", "10"),
        ("drawn", "3"),
        ("window_start_minutes", "31"),
        ("window_end_minutes", "30"),
    ] {
        assert_eq!(d.value(k), Some(v), "{k}");
    }
    assert_eq!(d.value("close"), Some(rig.close.to_string().as_str()));
    assert_eq!(d.columns, ["k", "instrument", "secs_before_close"]);
    assert_eq!(d.column("k").unwrap(), ["0", "1", "2"]);
    // What was drawn is what was bought, at the times drawn (the entries went out in the order of their times).
    let mut drawn: Vec<(u32, u64)> = d
        .column("instrument")
        .unwrap()
        .iter()
        .zip(d.column("secs_before_close").unwrap())
        .map(|(i, s)| (i.parse().unwrap(), s.parse().unwrap()))
        .collect();
    drawn.sort_by_key(|&(i, s)| (std::cmp::Reverse(s), i));
    let bought: Vec<(u32, u64)> = out
        .iter()
        .map(|i| (i.instrument, (rig.close - i.ts) / SEC))
        .collect();
    assert_eq!(bought, drawn);
    // Each entry: what it was and what it was judged on.
    for t in &traces[1..] {
        assert_eq!(
            t.columns,
            ["k", "instrument", "result", "bid", "ask", "qty"]
        );
        assert_eq!(t.rows.len(), 1);
        assert_eq!(
            t.rows[0][2..],
            ["entered", "19990000000", "20010000000", "99"]
        );
    }
    // Not asked, nothing recorded; and the same day decides the same.
    let mut quiet = Rig::new(params, 10, Some(regular()));
    let out2 = quiet.draw();
    assert!(quiet.runner.drain_traces().is_empty());
    assert_eq!(format!("{out:?}"), format!("{out2:?}"));
    assert_eq!(
        rig.runner.strategy().stats(),
        quiet.runner.strategy().stats()
    );
}

#[test]
fn an_entry_that_was_not_made_says_why() {
    let mut rig = Rig::with_tracing(p(1, 6), 6, Some(regular()), true);
    // 0 halted, 1 restricted, 2 never quoted, 3 crossed, 4 a bid of nothing, 5 fine.
    rig.feed(status(0, rig.t(6000), StatusKind::TradingHalt));
    rig.feed(status(1, rig.t(6000), StatusKind::ShortSaleRestriction));
    for (i, bid, ask) in [
        (0u32, 1999, 2001),
        (1, 1999, 2001),
        (3, 2001, 1999),
        (4, 0, 2001),
        (5, 1999, 2001),
    ] {
        rig.feed(quote(i, rig.t(5000) + u64::from(i), cents(bid), cents(ask)));
    }
    rig.tick(100);
    let traces = rig.runner.drain_traces();
    let mut by_instrument = std::collections::BTreeMap::new();
    for t in traces.iter().filter(|t| t.kind == "entry") {
        by_instrument.insert(t.rows[0][1].clone(), t.rows[0][2].clone());
    }
    let want: Vec<(String, String)> = [
        ("0", "halted"),
        ("1", "restricted"),
        ("2", "no_quote"),
        ("3", "crossed_quote"),
        ("4", "no_quote"),
        ("5", "entered"),
    ]
    .iter()
    .map(|&(a, b)| (a.to_owned(), b.to_owned()))
    .collect();
    assert_eq!(by_instrument.into_iter().collect::<Vec<_>>(), want);
    // The size and the stop.
    let big = RandomEntriesParams {
        dollars: u32::MAX,
        ..p(1, 1)
    };
    let mut rig = Rig::with_tracing(big, 1, Some(regular()), true);
    rig.feed(quote(0, rig.t(5000), 1, 1));
    rig.tick(100);
    let t = rig.runner.drain_traces();
    assert_eq!(t[1].rows[0][2], "no_share");
    let one = RandomEntriesParams {
        dollars: 1,
        ..p(1, 1)
    };
    let mut rig = Rig::with_tracing(one, 1, Some(regular()), true);
    rig.feed(quote(0, rig.t(5000), 1, 1));
    rig.tick(100);
    let t = rig.runner.drain_traces();
    assert_eq!(
        (t[1].rows[0][2].as_str(), t[1].rows[0][5].as_str()),
        ("no_stop", "1000000000")
    );
    // Turning tracing off drops what was kept.
    rig.runner.set_tracing(false);
    assert!(rig.runner.drain_traces().is_empty());
}
