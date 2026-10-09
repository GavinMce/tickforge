//! The premarket volume spike and its first pullback, from scripted events: every decision of the rule has a case here. A day is
//! played in minutes of the premarket (minute 0 is 04:00 New York): twenty quiet minutes, five busy ones that run the price up, a
//! pullback, and a turn.

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::cross::{CrossRunner, Market, Members};
use crate::exits::{REASON_STOP, REASON_TIME};
use crate::intent::{Intent, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::premarket_pullback::{
    PremarketPullback, PremarketPullbackParams, PremarketPullbackStats, REASON_ENTRY,
};

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

fn times() -> SessionTimes {
    Calendar::us_equities()
        .times(Date::new(2026, 5, 1).unwrap())
        .unwrap()
        .unwrap()
}

struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    runner: CrossRunner<PremarketPullback>,
    day: SessionTimes,
    clock: u32,
}

impl Rig {
    fn new(p: PremarketPullbackParams, names: u32) -> Rig {
        let day = times();
        let mut tier0 = Tier0::new(names as usize + 1);
        tier0.set_day(day);
        let mut refs: Vec<RefInfo> = (0..names)
            .map(|_| RefInfo {
                adv_shares: Some(1000),
                ..RefInfo::default()
            })
            .collect();
        refs.push(RefInfo::default());
        let runner = CrossRunner::new(
            PremarketPullback::new(1, p).unwrap(),
            Members::from_ids(0..names),
        );
        let mut rig = Rig {
            tier0,
            refs,
            runner,
            day,
            clock: names,
        };
        // The first event arms the review, the next one after it holds it.
        rig.tick(0, 0);
        rig.tick(0, 6);
        rig
    }

    /// The instant `sec` seconds into minute `min` of the premarket.
    fn at(&self, min: u64, sec: u64) -> Nanos {
        self.day.premarket + min * 60 * SEC + sec * SEC
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

    /// Time passes: an event of the clock instrument, which changes no member's state.
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

    /// A trade of `id` at `px` cents, with a quote a cent either side just before it.
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

    fn halt(&mut self, id: u32, min: u64, sec: u64) {
        let ts = self.at(min, sec);
        self.feed(Event::Status(Status {
            hdr: hdr(id, ts),
            kind: StatusKind::TradingHalt,
            lo: Px::from_raw(0),
            hi: Px::from_raw(0),
        }));
    }

    fn update(&mut self, u: OrderUpdate) {
        self.runner.on_order_update(&self.tier0, None, None, &u);
    }

    fn out(&mut self) -> Vec<Intent> {
        self.runner.drain_intents()
    }

    fn stats(&self) -> PremarketPullbackStats {
        self.runner.strategy().stats()
    }

    /// Name `id` is quiet for minutes 0..20 (a trade of `quiet` shares at $10.00 each) and busy for minutes 20..25: `trades` a
    /// minute of `size` shares each, at `burst[minute - 20]` cents. Time ends at minute 25, second 1, where the first review of
    /// that minute marks the spike.
    fn warm(&mut self, ids: &[u32], quiet: u32, burst: [i64; 5], trades: u64, size: u32) {
        for m in 0..20 {
            for &id in ids {
                self.trade(id, m, 30, 1000, quiet);
            }
            self.tick(m + 1, 1);
        }
        for (k, &px) in burst.iter().enumerate() {
            let m = 20 + k as u64;
            for t in 0..trades {
                for &id in ids {
                    self.trade(id, m, 10 + t * 10, px, size);
                }
            }
            self.tick(m + 1, 1);
        }
    }

    /// The usual story: quiet names that run from $10.00 to $10.70 in five busy minutes.
    fn story(&mut self, ids: &[u32]) {
        self.warm(ids, 100, [1010, 1025, 1040, 1055, 1070], 5, 2000);
    }

    /// A trade, then time passing a little, so the review sees it.
    fn print(&mut self, id: u32, min: u64, sec: u64, px: i64) {
        self.trade(id, min, sec, px, 100);
        self.tick(min, sec + 6);
    }
}

fn params() -> PremarketPullbackParams {
    PremarketPullbackParams::default()
}

fn p_with(f: impl FnOnce(&mut PremarketPullbackParams)) -> PremarketPullbackParams {
    let mut p = params();
    f(&mut p);
    p
}

/// The story to the turn that buys: the high $10.80, a pullback to $10.65, then $10.69.
fn to_the_turn(rig: &mut Rig) {
    rig.story(&[0]);
    rig.print(0, 25, 10, 1080);
    rig.print(0, 25, 30, 1070);
    rig.print(0, 25, 50, 1065);
    rig.quote(0, 26, 10, 1068, 1070);
    rig.print(0, 26, 10, 1069);
}

// ---- the rule ----

#[test]
fn it_buys_the_first_turn_of_a_small_pullback_in_a_run_that_started_with_a_spike() {
    let mut rig = Rig::new(params(), 1);
    rig.story(&[0]);
    assert_eq!(rig.stats().spikes, 1, "armed by the busy minutes");
    assert!(rig.out().is_empty());
    rig.print(0, 25, 10, 1080);
    rig.print(0, 25, 30, 1070);
    // Down 12.5% of the run (base 10.00 to high 10.80): a pullback, not yet a turn.
    assert!(rig.out().is_empty());
    rig.print(0, 25, 50, 1065);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().entries, 0);
    // Up 0.38% from the low of 10.65 with a quote 10.68 / 10.70.
    rig.quote(0, 26, 10, 1068, 1070);
    rig.print(0, 26, 10, 1069);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let i = &out[0];
    assert_eq!(
        (i.instrument, i.side, i.purpose, i.tif, i.reason),
        (0, Side::Buy, Purpose::Open, Tif::Day, REASON_ENTRY)
    );
    assert_eq!(
        i.pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(1070)),
            collar_permille: 10
        }
    );
    // $1,000 of whole shares at the ask, and no protective order: the broker takes none in the premarket.
    assert_eq!(i64::from(i.qty), 1_000 * D / cents(1070));
    assert_eq!(i.protect, None);
    assert_eq!(i.id.strategy, StrategyId(1));
    assert_eq!(i.ts, rig.at(26, 16));
    let s = rig.stats();
    assert_eq!((s.spikes, s.entries, s.failed_runs, s.missed), (1, 1, 0, 0));
    // Once a day for a name: a second pullback and turn buys nothing more.
    rig.print(0, 26, 30, 1100);
    rig.print(0, 26, 50, 1080);
    rig.print(0, 27, 10, 1070);
    rig.print(0, 27, 30, 1076);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().entries, 1);
}

// ---- the spike ----

/// Whether a story with these changes arms the name.
fn arms(p: PremarketPullbackParams, quiet: u32, burst: [i64; 5], trades: u64, size: u32) -> bool {
    let mut rig = Rig::new(p, 1);
    rig.warm(&[0], quiet, burst, trades, size);
    rig.stats().spikes == 1
}

const RUN: [i64; 5] = [1010, 1025, 1040, 1055, 1070];

#[test]
fn a_spike_is_the_windows_shares_at_least_the_multiple_of_the_average_window() {
    // A quiet minute of 1,000 shares: the average window is 5,000. Five busy minutes of five trades: 3.0 times is 15,000.
    let p = p_with(|p| p.min_dollars = 1);
    assert!(arms(p, 1000, RUN, 5, 600), "exactly three times");
    assert!(!arms(p, 1000, RUN, 5, 599), "just under");
    assert!(arms(
        p_with(|p| {
            p.min_dollars = 1;
            p.spike_x10 = 20
        }),
        1000,
        RUN,
        5,
        400
    ));
    assert!(!arms(
        p_with(|p| {
            p.min_dollars = 1;
            p.spike_x10 = 20
        }),
        1000,
        RUN,
        5,
        399
    ));
}

#[test]
fn a_spike_needs_dollars_and_trades_in_the_window() {
    // 25 trades of 2,000 shares at about $10.40: $520,000 in the window.
    assert!(arms(params(), 100, RUN, 5, 2000));
    assert!(arms(p_with(|p| p.min_dollars = 520_000), 100, RUN, 5, 2000));
    assert!(!arms(
        p_with(|p| p.min_dollars = 530_000),
        100,
        RUN,
        5,
        2000
    ));
    // Twenty trades is enough, and twenty-one is more than there were.
    assert!(arms(p_with(|p| p.min_trades = 25), 100, RUN, 5, 2000));
    assert!(!arms(p_with(|p| p.min_trades = 26), 100, RUN, 5, 2000));
}

#[test]
fn a_spike_needs_a_price_between_the_bounds_and_a_run_of_enough_basis_points() {
    // The last price is $10.70.
    // (A spike arms as soon as it can, at the lower prices of the run, so the bounds are tried on a run that stays at one price.)
    let flat = [1030; 5];
    assert!(arms(p_with(|p| p.max_cents = 1030), 100, flat, 5, 2000));
    assert!(!arms(p_with(|p| p.max_cents = 1029), 100, flat, 5, 2000));
    assert!(arms(p_with(|p| p.min_cents = 1030), 100, flat, 5, 2000));
    assert!(!arms(p_with(|p| p.min_cents = 1031), 100, flat, 5, 2000));
    assert!(
        !arms(p_with(|p| p.max_cents = 1000), 100, RUN, 5, 2000),
        "no price of the run is in the band"
    );
    // From the low of the window ($10.00, the last price when it began) to $10.30 is 300 basis points.
    assert!(arms(p_with(|p| p.min_thrust_bp = 300), 100, flat, 5, 2000));
    assert!(!arms(p_with(|p| p.min_thrust_bp = 301), 100, flat, 5, 2000));
    // Volume without a run is not a spike: the price stays at $10.00.
    assert!(!arms(params(), 100, [1000; 5], 5, 2000));
}

#[test]
fn no_spike_before_the_minutes_to_compare_with_have_passed() {
    // Busy from minute 5 to 10, looked at in minute 10: fewer than the fifteen minutes the rule wants.
    let mut rig = Rig::new(params(), 1);
    for m in 0..5 {
        rig.trade(0, m, 30, 1000, 100);
        rig.tick(m + 1, 1);
    }
    for (k, px) in RUN.iter().enumerate() {
        for t in 0..5 {
            rig.trade(0, 5 + k as u64, 10 + t * 10, *px, 2000);
        }
        rig.tick(6 + k as u64, 1);
    }
    assert_eq!(rig.stats().spikes, 0);
    // The same with the rule asking for ten minutes.
    let mut rig = Rig::new(p_with(|p| p.min_history_minutes = 10), 1);
    for m in 0..5 {
        rig.trade(0, m, 30, 1000, 100);
        rig.tick(m + 1, 1);
    }
    for (k, px) in RUN.iter().enumerate() {
        for t in 0..5 {
            rig.trade(0, 5 + k as u64, 10 + t * 10, *px, 2000);
        }
        rig.tick(6 + k as u64, 1);
    }
    assert_eq!(rig.stats().spikes, 1);
}

// ---- the pullback ----

/// The story, then the high $11.00 (a run of $1.00 from $10.00) and these prices at intervals; the intents sent after.
fn after_the_high(p: PremarketPullbackParams, path: &[i64]) -> (Rig, Vec<Intent>) {
    let mut rig = Rig::new(p, 1);
    rig.story(&[0]);
    rig.print(0, 25, 10, 1100);
    let mut out = Vec::new();
    for (k, &px) in path.iter().enumerate() {
        rig.print(0, 25, 20 + 10 * k as u64, px);
        out.extend(rig.out());
    }
    (rig, out)
}

#[test]
fn a_pullback_is_a_tenth_to_three_tenths_of_the_run_given_back() {
    // High $11.00, base $10.00: 10% is $10.90, 30% is $10.70.
    // Never down 10%: still running, nothing to buy at any price on the way up.
    let (rig, out) = after_the_high(params(), &[1095, 1091, 1100, 1105]);
    assert!(out.is_empty() && rig.stats().entries == 0 && rig.stats().failed_runs == 0);
    // Exactly 10% down and then up: a pullback, bought on the turn.
    let (rig, out) = after_the_high(params(), &[1090, 1094]);
    assert_eq!(out.len(), 1, "{:?}", rig.stats());
    // Exactly 30% down is still a pullback; one tick more and the run has failed.
    let (rig, out) = after_the_high(params(), &[1070, 1074]);
    assert_eq!(out.len(), 1, "{:?}", rig.stats());
    let (rig, out) = after_the_high(params(), &[1069, 1080, 1090]);
    assert!(out.is_empty());
    assert_eq!(
        (rig.stats().failed_runs, rig.stats().entries),
        (1, 0),
        "dropped for the day, and a recovery buys nothing"
    );
}

#[test]
fn a_pullback_that_goes_past_the_bound_after_it_began_fails_the_run() {
    // Down 15% (a pullback), then down 35%: failed, then a rise of any size is not bought.
    let (rig, out) = after_the_high(params(), &[1085, 1065, 1075, 1090]);
    assert!(out.is_empty());
    assert_eq!((rig.stats().failed_runs, rig.stats().entries), (1, 0));
}

#[test]
fn a_new_high_before_the_turn_is_a_new_run_with_a_new_pullback() {
    // Down 15%, then a new high of $11.50 (the run is now $1.50): 10% of it is $11.35, so $11.40 is not yet a pullback.
    let (mut rig, out) = after_the_high(params(), &[1085, 1150, 1140]);
    assert!(out.is_empty() && rig.stats().entries == 0);
    // $11.30 is a pullback of 13%; $11.34 is up 0.35% from it: bought.
    rig.print(0, 26, 10, 1130);
    rig.print(0, 26, 30, 1134);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn the_turn_is_measured_from_the_lowest_price_of_the_pullback() {
    // Low $10.90 then $10.80 (the low): a rise to $10.82 is 0.19% and $10.84 is 0.37%.
    let (mut rig, out) = after_the_high(params(), &[1090, 1080, 1082]);
    assert!(out.is_empty());
    rig.print(0, 26, 10, 1084);
    assert_eq!(rig.out().len(), 1);
}

// ---- the entry ----

#[test]
fn a_turn_after_the_last_entry_time_is_not_bought() {
    // The last entry is 310 minutes before the open: minute 20 of the premarket. The turn is at minute 26.
    let mut rig = Rig::new(p_with(|p| p.last_entry_minutes = 310), 1);
    to_the_turn(&mut rig);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (0, 1));
}

#[test]
fn only_as_many_positions_as_names_and_the_rest_are_missed() {
    let mut rig = Rig::new(p_with(|p| p.names = 1), 2);
    rig.story(&[0, 1]);
    // Both are armed and both pull back together. Name 0 turns first.
    for (sec, px) in [(10, 1080), (30, 1070), (50, 1065)] {
        rig.trade(0, 25, sec, px, 100);
        rig.trade(1, 25, sec, px, 100);
        rig.tick(25, sec + 6);
    }
    rig.print(0, 26, 10, 1069);
    let first = rig.out();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].instrument, 0);
    rig.print(1, 26, 30, 1069);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.spikes, s.entries, s.missed), (2, 1, 1));
}

#[test]
fn a_wide_quote_or_no_quote_or_a_halt_waits_and_a_good_one_buys() {
    let mut rig = Rig::new(params(), 1);
    rig.story(&[0]);
    rig.print(0, 25, 10, 1080);
    rig.print(0, 25, 30, 1070);
    rig.print(0, 25, 50, 1065);
    // A spread of 5% at the turn: not bought. (`print` quotes a cent either side; this quote is later.)
    rig.trade(0, 26, 10, 1069, 100);
    rig.quote(0, 26, 12, 1040, 1100);
    rig.tick(26, 20);
    assert!(rig.out().is_empty());
    // A halt: not bought.
    rig.halt(0, 26, 30);
    rig.tick(26, 36);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().missed, 0, "waiting is not missing");
    // The quote is fair again and the halt is over: bought at the next look.
    rig.feed(Event::Status(Status {
        hdr: hdr(0, rig.at(26, 40)),
        kind: StatusKind::TradingResume,
        lo: Px::from_raw(0),
        hi: Px::from_raw(0),
    }));
    rig.quote(0, 26, 44, 1068, 1070);
    rig.tick(26, 50);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn a_spread_cap_of_nothing_means_no_cap() {
    let mut rig = Rig::new(p_with(|p| p.spread_cap_bp = 0), 1);
    rig.story(&[0]);
    rig.print(0, 25, 10, 1080);
    rig.print(0, 25, 30, 1070);
    rig.print(0, 25, 50, 1065);
    rig.trade(0, 26, 10, 1069, 100);
    rig.quote(0, 26, 12, 1040, 1100);
    rig.tick(26, 20);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn nothing_is_decided_after_the_open() {
    let mut rig = Rig::new(params(), 1);
    rig.story(&[0]);
    rig.print(0, 25, 10, 1080);
    rig.print(0, 25, 30, 1070);
    rig.print(0, 25, 50, 1065);
    // The same turn at 09:31.
    rig.quote(0, 331, 10, 1068, 1070);
    rig.print(0, 331, 10, 1069);
    assert!(rig.out().is_empty() && rig.stats().entries == 0);
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

#[test]
fn the_stop_trails_the_high_since_the_entry_and_is_held_by_the_strategy() {
    let mut rig = Rig::new(params(), 1);
    to_the_turn(&mut rig);
    let entry = rig.out().remove(0);
    let qty = entry.qty;
    rig.update(fill(&entry, qty, 1070, rig.at(26, 20)));
    // The initial stop is 0.5% under the pullback's low of 10.65: 10.5968. A print at 10.62 does not touch it.
    rig.print(0, 26, 30, 1062);
    assert!(rig.out().is_empty());
    // The price runs to $11.00; the stop follows it to 3% under, $10.67.
    rig.print(0, 26, 50, 1100);
    assert!(rig.out().is_empty());
    // A print at $10.67 is at the stop, and the position is sold.
    rig.print(0, 27, 10, 1067);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let e = &out[0];
    assert_eq!(
        (e.side, e.purpose, e.qty, e.reason),
        (Side::Sell, Purpose::Close, qty, REASON_STOP)
    );
    assert_eq!(
        e.pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(1067)),
            collar_permille: 10
        }
    );
    assert_eq!(rig.stats().exits.stops, 1);
}

#[test]
fn without_a_run_the_stop_is_under_the_pullback_low() {
    let mut rig = Rig::new(params(), 1);
    to_the_turn(&mut rig);
    let entry = rig.out().remove(0);
    let qty = entry.qty;
    rig.update(fill(&entry, qty, 1070, rig.at(26, 20)));
    // $10.60 is above 10.5968 and not sold; $10.59 is at or under it and is.
    rig.print(0, 26, 30, 1060);
    assert!(rig.out().is_empty());
    rig.print(0, 26, 50, 1059);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].reason, REASON_STOP);
}

#[test]
fn a_position_is_sold_before_the_open() {
    let mut rig = Rig::new(params(), 1);
    to_the_turn(&mut rig);
    let entry = rig.out().remove(0);
    let qty = entry.qty;
    rig.update(fill(&entry, qty, 1070, rig.at(26, 20)));
    rig.print(0, 26, 30, 1075);
    // Five minutes before the open, minute 325.
    rig.tick(324, 59);
    assert!(rig.out().is_empty());
    rig.tick(325, 1);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let e = &out[0];
    assert_eq!(
        (e.side, e.purpose, e.qty, e.reason, e.tif),
        (Side::Sell, Purpose::Close, qty, REASON_TIME, Tif::Day)
    );
    assert_eq!(e.ts, rig.at(325, 0));
}

#[test]
fn an_entry_that_fills_nothing_leaves_nothing_to_sell() {
    let mut rig = Rig::new(params(), 1);
    to_the_turn(&mut rig);
    let entry = rig.out().remove(0);
    let mut u = fill(&entry, 0, 1070, rig.at(26, 20));
    u.state = OrderState::Rejected;
    rig.update(u);
    rig.print(0, 26, 30, 900);
    rig.tick(325, 1);
    assert!(rig.out().is_empty());
}

// ---- the parameters and the day ----

#[test]
fn parameters_read_back_exactly_and_every_bad_one_is_refused() {
    let p = params();
    assert_eq!(PremarketPullbackParams::parse(&p.render()).unwrap(), p);
    assert_eq!(p.render().split_whitespace().count(), 20);
    let one = |k: &str, v: u32| {
        let text = p
            .render()
            .split_whitespace()
            .map(|w| match w.split_once('=') {
                Some((key, _)) if key == k => format!("{k}={v}"),
                _ => w.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        PremarketPullbackParams::parse(&text)
    };
    assert!(one("names", 5).is_ok());
    for (k, v) in [
        ("names", 0),
        ("dollars", 0),
        ("window_minutes", 0),
        ("window_minutes", 31),
        ("min_history_minutes", 5),
        ("min_history_minutes", 301),
        ("spike_x10", 9),
        ("min_cents", 0),
        ("max_cents", 99),
        ("min_thrust_bp", 0),
        ("min_pullback_permille", 0),
        ("min_pullback_permille", 300),
        ("max_pullback_permille", 901),
        ("turn_bp", 0),
        ("collar_permille", 1000),
        ("stop_buffer_permille", 1000),
        ("trail_permille", 0),
        ("trail_permille", 1000),
        ("flat_minutes", 0),
        ("last_entry_minutes", 5),
        ("last_entry_minutes", 331),
        ("review_secs", 0),
        ("review_secs", 61),
    ] {
        assert!(one(k, v).is_err(), "{k}={v}");
    }
    let text = p.render();
    assert!(PremarketPullbackParams::parse(&format!("{text} wat=1")).is_err());
    assert!(PremarketPullbackParams::parse(&format!("{text} names=2")).is_err());
    assert!(PremarketPullbackParams::parse(&text.replace("names=3 ", "")).is_err());
    assert!(PremarketPullbackParams::parse(&text.replace("names=3", "names=x")).is_err());
    assert!(PremarketPullbackParams::parse(&text.replace("names=3", "names")).is_err());
    assert!(PremarketPullback::new(1, PremarketPullbackParams { names: 0, ..p }).is_err());
}

#[test]
fn recording_why_changes_nothing_it_decides_and_says_what_it_did() {
    let run = |tracing: bool| {
        let mut rig = Rig::new(params(), 1);
        rig.runner.set_tracing(tracing);
        to_the_turn(&mut rig);
        let out = rig.out();
        let traces = rig.runner.drain_traces();
        (out, traces)
    };
    let (a, ta) = run(false);
    let (b, tb) = run(true);
    assert_eq!(a, b);
    assert!(ta.is_empty());
    let kinds: Vec<&str> = tb.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, ["spike", "entry"]);
    assert_eq!(tb[1].value("ask"), Some("10700000000"));
    assert_eq!(tb[1].value("low"), Some("10650000000"));
}

#[test]
fn a_new_day_forgets_the_last_one() {
    let mut rig = Rig::new(params(), 1);
    rig.story(&[0]);
    assert_eq!(rig.stats().spikes, 1);
    // The next trading day: new session times, a fresh Tier 0, the same strategy.
    let next = Calendar::us_equities()
        .times(Date::new(2026, 5, 4).unwrap())
        .unwrap()
        .unwrap();
    rig.tier0 = Tier0::new(2);
    rig.tier0.set_day(next);
    rig.day = next;
    rig.tick(0, 0);
    rig.tick(0, 6);
    rig.story(&[0]);
    assert_eq!(rig.stats().spikes, 2, "armed again on the new day");
}
