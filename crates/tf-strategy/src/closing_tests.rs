//! T04 from scripted events: every decision of the rule has a case here. A day is played relative to the regular close
//! (the strategy's times are minutes and seconds before it): trades at 14:58, a last trade at or before 15:00 that sets
//! the price, quotes at 15:29, and a decision at 15:30.

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::closing_reversal::{
    ClosingReversal, ClosingReversalParams, ParamError, REASON_ENTRY, return_ppm,
};
use crate::cross::{CrossRunner, CrossStrategy, Market, Members};
use crate::exits::REASON_TIME;
use crate::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};

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

/// A Friday in May: a regular day.
fn regular() -> SessionTimes {
    times(2026, 5, 1)
}

struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    runner: CrossRunner<ClosingReversal>,
    close: Nanos,
    /// An instrument that is not a member, whose quotes only carry the time.
    clock: u32,
}

impl Rig {
    /// `priors` are the prior closes, raw, of the members 0..n; the day is `day` (none: the host has not told Tier 0).
    fn new(p: ClosingReversalParams, priors: &[i64], day: Option<SessionTimes>) -> Rig {
        let n = priors.len();
        let mut tier0 = Tier0::new(n + 1);
        let close = day.map_or(20 * 3600 * SEC, |d| d.close);
        if let Some(d) = day {
            tier0.set_day(d);
        }
        let mut refs: Vec<RefInfo> = priors
            .iter()
            .map(|&price| RefInfo {
                price: (price != 0).then_some(price),
                adv_shares: Some(1000),
                ..RefInfo::default()
            })
            .collect();
        refs.push(RefInfo::default());
        let members = Members::from_ids(0..n as u32);
        let runner = CrossRunner::new(ClosingReversal::new(1, p).unwrap(), members);
        let mut rig = Rig {
            tier0,
            refs,
            runner,
            close,
            clock: n as u32,
        };
        // The first event arms the minute's review, the next one after it holds it: the strategy sets its timers.
        rig.tick(4 * 3600);
        rig.tick(4 * 3600 - 61);
        rig
    }

    /// `secs` before the regular close.
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

    /// Time passes: an event of the clock instrument, which changes no member's state.
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

    /// Play a day up to and including the decision. `px` are the prices of the members at 15:00, in cents; each trades once
    /// at 14:58 and quotes a cent either side at 15:29.
    fn play(&mut self, px: &[i64]) -> Vec<Intent> {
        self.tick(3850);
        for (i, &p) in px.iter().enumerate() {
            self.feed(trade(i as u32, self.t(3700) + i as u64, cents(p)));
        }
        self.tick(3590);
        for (i, &p) in px.iter().enumerate() {
            self.feed(quote(
                i as u32,
                self.t(1900) + i as u64,
                cents(p - 1),
                cents(p + 1),
            ));
        }
        assert!(
            self.out().is_empty(),
            "nothing is bought before the decision"
        );
        self.tick(1790);
        self.out()
    }
}

fn names(n: u32) -> ClosingReversalParams {
    ClosingReversalParams {
        names: n,
        ..ClosingReversalParams::default()
    }
}

fn priors(n: usize) -> Vec<i64> {
    vec![10 * D; n]
}

/// Six names with a prior close of $10: returns of -10%, -5%, -2%, +2%, -4% and 0.
const SIX: [i64; 6] = [900, 950, 980, 1020, 960, 1000];

fn picked(out: &[Intent]) -> Vec<u32> {
    out.iter().map(|i| i.instrument).collect()
}

// ---- the rule ----

#[test]
fn it_buys_the_most_negative_returns_to_the_price_at_three_most_negative_first() {
    let mut rig = Rig::new(names(3), &priors(6), Some(regular()));
    let out = rig.play(&SIX);
    // -10%, -5%, -4%: names 0, 1, 4, the most negative first.
    assert_eq!(picked(&out), [0, 1, 4]);
    for (i, ask) in out.iter().zip([cents(901), cents(951), cents(961)]) {
        assert_eq!(
            (i.side, i.purpose, i.tif),
            (Side::Buy, Purpose::Open, Tif::Day)
        );
        assert_eq!(
            i.pricing,
            Pricing::Collar {
                reference: Px::from_raw(ask),
                collar_permille: 5
            }
        );
        // An equal dollar amount of each: $2,000 over the ask, whole shares.
        assert_eq!(i64::from(i.qty), 2_000 * D / ask);
        assert_eq!(i.reason, REASON_ENTRY);
        // The disaster stop, 10% under the ask: not the rule's, the framework's.
        assert_eq!(
            i.protect,
            Some(Protective {
                stop_trigger: Px::from_raw(ask * 900 / 1000),
                stop_limit: None,
                take_profit: None
            })
        );
        assert_eq!(i.id.strategy, StrategyId(1));
    }
    assert_eq!(
        out.iter().map(|i| i.qty).collect::<Vec<_>>(),
        [221, 210, 208]
    );
    // Decided at 15:30 exactly, not at the event that found it due.
    assert!(out.iter().all(|i| i.ts == rig.t(1800)));
    // Numbered in the order sent.
    assert_eq!(out.iter().map(|i| i.id.seq).collect::<Vec<_>>(), [0, 1, 2]);
    let s = rig.runner.strategy().stats();
    assert_eq!((s.decisions, s.entries, s.entries_refused), (1, 3, 0));
}

#[test]
fn the_number_bought_is_the_parameter_and_fewer_if_fewer_can_be_bought() {
    for (n, want) in [
        (1, vec![0]),
        (2, vec![0, 1]),
        (5, vec![0, 1, 4, 2, 5]),
        (6, vec![0, 1, 4, 2, 5, 3]),
        (40, vec![0, 1, 4, 2, 5, 3]),
    ] {
        let mut rig = Rig::new(names(n), &priors(6), Some(regular()));
        assert_eq!(picked(&rig.play(&SIX)), want, "{n} names");
    }
    // The most negative are bought whatever their sign: on a day the whole universe rose, the ones that rose least.
    let mut rig = Rig::new(names(2), &priors(3), Some(regular()));
    assert_eq!(picked(&rig.play(&[1100, 1010, 1050])), [1, 2]);
}

#[test]
fn equal_returns_go_to_the_lower_instrument_number() {
    let mut rig = Rig::new(names(2), &priors(4), Some(regular()));
    assert_eq!(picked(&rig.play(&[950, 900, 950, 950])), [1, 0]);
}

#[test]
fn a_halted_name_and_a_restricted_one_are_skipped_and_come_back_when_they_do() {
    for (kind, undo) in [
        (StatusKind::TradingHalt, StatusKind::TradingResume),
        (
            StatusKind::ShortSaleRestriction,
            StatusKind::ShortSaleRestrictionLifted,
        ),
    ] {
        let mut rig = Rig::new(names(2), &priors(6), Some(regular()));
        rig.feed(status(0, rig.t(3800), kind));
        // The most negative name is out: the next two are bought.
        assert_eq!(picked(&rig.play(&SIX)), [1, 4], "{kind:?}");
        // The same day, with the name back in good standing before the decision.
        let mut rig = Rig::new(names(2), &priors(6), Some(regular()));
        rig.feed(status(0, rig.t(3800), kind));
        rig.feed(status(0, rig.t(3750), undo));
        assert_eq!(picked(&rig.play(&SIX)), [0, 1], "{kind:?} then {undo:?}");
    }
    // A halt after the price was taken and before the decision counts: it is the state at 15:30 that matters.
    let mut rig = Rig::new(names(2), &priors(6), Some(regular()));
    rig.tick(3850);
    for (i, &p) in SIX.iter().enumerate() {
        rig.feed(trade(i as u32, rig.t(3700) + i as u64, cents(p)));
    }
    rig.tick(3590);
    rig.feed(status(0, rig.t(2000), StatusKind::TradingHalt));
    for (i, &p) in SIX.iter().enumerate() {
        rig.feed(quote(
            i as u32,
            rig.t(1900) + i as u64,
            cents(p - 1),
            cents(p + 1),
        ));
    }
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [1, 4]);
}

#[test]
fn a_name_with_nothing_to_go_on_is_skipped() {
    // Name 0: no prior close. Name 1: never traded. Name 2: no quote. Name 3: a crossed quote. Name 4 and 5: fine.
    let mut p = priors(6);
    p[0] = 0;
    let mut rig = Rig::new(names(6), &p, Some(regular()));
    rig.tick(3850);
    for i in [0u32, 2, 3, 4, 5] {
        rig.feed(trade(
            i,
            rig.t(3700) + u64::from(i),
            cents(900 + i64::from(i)),
        ));
    }
    rig.tick(3590);
    for (i, bid, ask) in [
        (0u32, 890, 910),
        (1, 890, 910),
        (3, 920, 910),
        (4, 890, 910),
        (5, 890, 910),
    ] {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(bid), cents(ask)));
    }
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [4, 5]);
    // A locked quote (the bid equal to the ask) is a quote; a bid of nothing is not.
    let mut rig = Rig::new(names(3), &priors(3), Some(regular()));
    rig.tick(3850);
    for i in 0..3u32 {
        rig.feed(trade(
            i,
            rig.t(3700) + u64::from(i),
            cents(900 + i64::from(i)),
        ));
    }
    rig.tick(3590);
    rig.feed(quote(0, rig.t(1900), cents(900), cents(900)));
    rig.feed(quote(1, rig.t(1900) + 1, 0, cents(901)));
    rig.feed(quote(2, rig.t(1900) + 2, cents(901), cents(903)));
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [0, 2]);
}

#[test]
fn the_extreme_variant_wants_a_return_of_at_most_minus_its_basis_points() {
    let p = ClosingReversalParams {
        extreme_bp: 300,
        ..names(10)
    };
    // -4.00%, -3.00% (exactly: kept), -2.99% (not), -10%, +1%.
    let mut rig = Rig::new(p, &priors(5), Some(regular()));
    assert_eq!(picked(&rig.play(&[960, 970, 971, 900, 1010])), [3, 0, 1]);
    // No floor at all with 0: everything is ranked.
    let mut rig = Rig::new(names(10), &priors(5), Some(regular()));
    assert_eq!(picked(&rig.play(&[960, 970, 971, 900, 1010])).len(), 5);
}

#[test]
fn the_spread_cap_variant_wants_a_quoted_spread_of_at_most_its_basis_points_of_the_mid() {
    let p = ClosingReversalParams {
        spread_cap_bp: 5,
        ..names(10)
    };
    let mut rig = Rig::new(p, &priors(4), Some(regular()));
    rig.tick(3850);
    for i in 0..4u32 {
        rig.feed(trade(
            i,
            rig.t(3700) + u64::from(i),
            cents(900 + i64::from(i)),
        ));
    }
    rig.tick(3590);
    // A mid of $20.00 and a spread of one cent is 5.0 basis points: kept. A hundredth of a cent more is not. A wide one is not.
    let raw = [
        (0u32, 19_995_000_000, 20_005_000_000),
        (1, 19_995_000_000, 20_005_100_000),
        (2, 19_000_000_000, 21_000_000_000),
        (3, 19_999_000_000, 20_001_000_000),
    ];
    for (i, bid, ask) in raw {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), bid, ask));
    }
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [0, 3]);
    // The cap is checked at the decision, whatever the quote was at 15:00: with 0 there is none.
    let mut rig = Rig::new(names(10), &priors(4), Some(regular()));
    rig.tick(3850);
    for i in 0..4u32 {
        rig.feed(trade(
            i,
            rig.t(3700) + u64::from(i),
            cents(900 + i64::from(i)),
        ));
    }
    rig.tick(3590);
    for (i, bid, ask) in raw {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), bid, ask));
    }
    rig.tick(1790);
    assert_eq!(picked(&rig.out()).len(), 4);
}

#[test]
fn a_name_the_money_does_not_buy_a_share_of_is_left_out_not_refused() {
    let mut rig = Rig::new(names(2), &priors(2), Some(regular()));
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(900)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(950)));
    rig.tick(3590);
    // $2,000 does not buy a share at $5,000.
    rig.feed(quote(0, rig.t(1900), 5_000 * D, 5_001 * D));
    rig.feed(quote(1, rig.t(1900) + 1, cents(949), cents(951)));
    rig.tick(1790);
    let out = rig.out();
    assert_eq!(picked(&out), [1]);
    assert_eq!(rig.runner.strategy().stats().entries_refused, 0);
}

// ---- the price at 15:00 ----

#[test]
fn the_price_is_the_last_trade_at_or_before_the_reference_time() {
    // Name 0 trades at 14:58 at 9.00 and again at 15:10 at 11.00: the second is after 15:00 and does not count. Name 1
    // trades at 14:58 at 9.50 and exactly at 15:00:00 at 9.40: the second counts. Name 2: 9.50 at 14:58, and at one nanosecond
    // after 15:00 at 8.00, which does not.
    let mut rig = Rig::new(names(1), &priors(3), Some(regular()));
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(900)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(950)));
    rig.feed(trade(2, rig.t(3700) + 2, cents(950)));
    rig.feed(trade(1, rig.t(3600), cents(940)));
    rig.tick(3590);
    rig.feed(trade(0, rig.t(3000), cents(1100)));
    rig.feed(trade(2, rig.t(3599), cents(800)));
    for i in 0..3u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1790);
    // Name 0 at 9.00 (-10%) is the most negative; name 2's 8.00 came after 15:00 and name 0's 11.00 too.
    assert_eq!(picked(&rig.out()), [0]);
    // Take name 0 away to see the other two: name 1 at 9.40 (-6%) against name 2 at 9.50 (-5%).
    let mut rig = Rig::new(names(1), &priors(3), Some(regular()));
    rig.tick(3850);
    rig.feed(trade(1, rig.t(3700) + 1, cents(950)));
    rig.feed(trade(2, rig.t(3700) + 2, cents(950)));
    rig.feed(trade(1, rig.t(3600), cents(940)));
    rig.tick(3590);
    rig.feed(trade(2, rig.t(3599), cents(800)));
    for i in 1..3u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [1]);
}

#[test]
fn when_the_event_that_fires_the_timer_is_the_names_own_later_trade_the_snapshot_is_used() {
    // The first event after 15:00 is a trade of name 0, so Tier 0 has already moved on: the price is the one five
    // minutes earlier, 9.00, not the 14:58 trade of 8.80 that the snapshot did not see, and not the new 12.00.
    let mut rig = Rig::new(names(1), &priors(2), Some(regular()));
    rig.feed(trade(0, rig.t(3950), cents(900)));
    rig.feed(trade(1, rig.t(3950) + 1, cents(990)));
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(880)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(990)));
    rig.feed(trade(0, rig.t(3590), cents(1200)));
    for i in 0..2u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1790);
    // The reference price of name 0 is 9.00 (-10%), the most negative.
    assert_eq!(picked(&rig.out()), [0]);
    // And with no snapshot to fall back on (no trade before it), the name has no price and is not bought.
    let mut rig = Rig::new(names(1), &priors(2), Some(regular()));
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(800)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(990)));
    rig.feed(trade(0, rig.t(3590), cents(1200)));
    for i in 0..2u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [1]);
}

// ---- time ----

#[test]
fn the_decision_is_at_half_past_three_on_a_regular_day_and_not_a_moment_before() {
    let d = regular();
    assert_eq!(d.close - d.open, (6 * 3600 + 1800) * SEC);
    let mut rig = Rig::new(names(1), &priors(2), Some(d));
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(900)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(950)));
    rig.tick(3590);
    for i in 0..2u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1801);
    assert!(rig.out().is_empty(), "not at 15:29:59");
    rig.tick(1800);
    let out = rig.out();
    assert_eq!(picked(&out), [0], "at 15:30:00");
    // 16:00 New York is 20:00 UTC in May.
    assert_eq!(
        rig.close,
        20 * 3600 * SEC + (d.close / (86_400 * SEC)) * 86_400 * SEC
    );
    assert_eq!(out[0].ts, rig.close - 1800 * SEC);
}

#[test]
fn on_an_early_close_day_every_time_moves_with_the_close() {
    // The day after Thanksgiving closes at 13:00: the decision is at 12:30, the price is as of 12:00.
    let d = times(2026, 11, 27);
    assert_eq!(d.close - d.open, (3 * 3600 + 1800) * SEC);
    let mut rig = Rig::new(names(1), &priors(2), Some(d));
    // 13:00 New York is 18:00 UTC in November.
    assert_eq!(rig.close % (86_400 * SEC), 18 * 3600 * SEC);
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(900)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(950)));
    rig.tick(3590);
    for i in 0..2u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1801);
    assert!(rig.out().is_empty());
    rig.tick(1800);
    let out = rig.out();
    assert_eq!(picked(&out), [0]);
    assert_eq!(out[0].ts % (86_400 * SEC), 17 * 3600 * SEC + 1800 * SEC);
}

#[test]
fn the_buy_at_three_variant_uses_the_price_taken_at_the_same_moment() {
    // Entry and price at the same time (60 minutes before the close): the price is taken first, then the decision.
    let p = ClosingReversalParams {
        entry_minutes: 60,
        ..names(2)
    };
    let mut rig = Rig::new(p, &priors(4), Some(regular()));
    rig.tick(3850);
    for (i, px) in [900, 950, 980, 1000].into_iter().enumerate() {
        rig.feed(trade(i as u32, rig.t(3700) + i as u64, cents(px)));
        rig.feed(quote(
            i as u32,
            rig.t(3650) + i as u64,
            cents(px - 1),
            cents(px + 1),
        ));
    }
    rig.tick(3601);
    assert!(rig.out().is_empty());
    rig.tick(3590);
    let out = rig.out();
    assert_eq!(picked(&out), [0, 1]);
    assert_eq!(out[0].ts, rig.t(3600));
}

#[test]
fn there_is_one_decision_a_day_and_another_the_next_day() {
    let mut rig = Rig::new(names(1), &priors(2), Some(regular()));
    let out = rig.play(&[900, 950]);
    assert_eq!(picked(&out), [0]);
    // Later events and reviews decide nothing more.
    for s in [1700, 1000, 500, 100] {
        rig.tick(s);
        assert!(rig.out().is_empty(), "{s}");
    }
    assert_eq!(rig.runner.strategy().stats().decisions, 1);
    // The next trading day (a Monday): the host tells Tier 0, the strategy sets its timers again at its next review.
    let d2 = times(2026, 5, 4);
    rig.tier0.set_day(d2);
    rig.close = d2.close;
    rig.tick(4 * 3600);
    rig.tick(4 * 3600 - 61);
    let out = rig.play(&[960, 900]);
    assert_eq!(picked(&out), [1]);
    assert_eq!(rig.runner.strategy().stats().decisions, 2);
    assert_eq!(out[0].ts, d2.close - 1800 * SEC);
}

#[test]
fn a_strategy_not_told_the_day_does_nothing() {
    let mut rig = Rig::new(names(2), &priors(2), None);
    assert!(rig.play(&[900, 950]).is_empty());
    rig.tick(100);
    assert!(rig.out().is_empty());
    assert_eq!(rig.runner.strategy().stats().decisions, 0);
}

// ---- the exit ----

#[test]
fn what_filled_is_sold_at_the_exit_time_and_only_what_filled() {
    let mut rig = Rig::new(names(3), &priors(6), Some(regular()));
    let out = rig.play(&SIX);
    assert_eq!(picked(&out), [0, 1, 4]);
    let upd = |i: &Intent, filled, state, ts| OrderUpdate {
        intent: i.id,
        order: None,
        state,
        filled_qty: filled,
        avg_px: Some(Px::from_raw(cents(901))),
        reject: None,
        ts,
    };
    // Name 0 fills in two pieces, name 1 is refused, name 4 is not heard of again.
    rig.update(upd(&out[0], 100, OrderState::PartiallyFilled, rig.t(1799)));
    rig.update(upd(&out[0], 221, OrderState::Filled, rig.t(1798)));
    rig.update(upd(&out[1], 0, OrderState::Rejected, rig.t(1799)));
    // A trade of name 0 shortly before the exit.
    rig.feed(trade(0, rig.t(100), cents(905)));
    rig.tick(31);
    assert!(rig.out().is_empty(), "not before 15:59:30");
    rig.tick(30);
    let exits = rig.out();
    assert_eq!(exits.len(), 1);
    let e = &exits[0];
    assert_eq!(
        (e.instrument, e.side, e.purpose, e.qty),
        (0, Side::Sell, Purpose::Close, 221)
    );
    assert_eq!(
        e.pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(905)),
            collar_permille: 5
        }
    );
    assert_eq!((e.reason, e.tif, e.ts), (REASON_TIME, Tif::Day, rig.t(30)));
    // The exit fills: the book forgets the name, and the exit counted.
    let sold = OrderUpdate {
        intent: e.id,
        order: None,
        state: OrderState::Filled,
        filled_qty: 221,
        avg_px: Some(Px::from_raw(cents(905))),
        reject: None,
        ts: rig.t(29),
    };
    rig.update(sold);
    rig.tick(10);
    assert!(rig.out().is_empty());
    let s = rig.runner.strategy().stats();
    assert_eq!((s.exits.time_exits, s.exits.closed), (1, 1));
}

#[test]
fn the_exit_is_where_the_parameter_puts_it_and_only_what_has_filled_so_far_is_sold() {
    let p = ClosingReversalParams {
        exit_seconds: 45,
        collar_permille: 20,
        ..names(1)
    };
    let mut rig = Rig::new(p, &priors(2), Some(regular()));
    let out = rig.play(&[900, 950]);
    let fill = |filled, state, ts| OrderUpdate {
        intent: out[0].id,
        order: None,
        state,
        filled_qty: filled,
        avg_px: Some(Px::from_raw(cents(901))),
        reject: None,
        ts,
    };
    rig.update(fill(60, OrderState::PartiallyFilled, rig.t(1790)));
    rig.tick(46);
    assert!(rig.out().is_empty());
    rig.tick(45);
    let exits = rig.out();
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].qty, 60);
    assert_eq!(
        exits[0].pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(900)),
            collar_permille: 20
        }
    );
    // The entry was bought with the same collar.
    assert_eq!(
        out[0].pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(901)),
            collar_permille: 20
        }
    );
}

#[test]
fn a_fill_after_the_exit_time_is_sold_at_the_next_event_and_an_update_that_is_not_ours_is_ignored()
{
    let mut rig = Rig::new(names(1), &priors(2), Some(regular()));
    let out = rig.play(&[900, 950]);
    rig.tick(20);
    assert!(rig.out().is_empty());
    // It fills at last, after 15:59:30: the exit is due at once, at the next event.
    rig.update(OrderUpdate {
        intent: out[0].id,
        order: None,
        state: OrderState::Filled,
        filled_qty: 221,
        avg_px: Some(Px::from_raw(cents(901))),
        reject: None,
        ts: rig.t(19),
    });
    rig.tick(15);
    let exits = rig.out();
    assert_eq!((exits.len(), exits[0].qty), (1, 221));
    // An update for an intent the strategy never sent changes nothing.
    rig.update(OrderUpdate {
        intent: IntentId {
            strategy: StrategyId(9),
            seq: 77,
        },
        order: None,
        state: OrderState::Filled,
        filled_qty: 5,
        avg_px: None,
        reject: None,
        ts: rig.t(14),
    });
    rig.tick(10);
    assert!(rig.out().is_empty());
}

// ---- parameters and arithmetic ----

#[test]
fn the_return_in_parts_per_million_by_hand() {
    assert_eq!(return_ppm(10 * D, 9 * D), Some(-100_000));
    assert_eq!(return_ppm(10 * D, 11 * D), Some(100_000));
    assert_eq!(return_ppm(10 * D, 10 * D), Some(0));
    // Truncated toward zero: (2 - 3) / 3 is -333,333.33 and (4 - 3) / 3 is 333,333.33.
    assert_eq!(return_ppm(3, 2), Some(-333_333));
    assert_eq!(return_ppm(3, 4), Some(333_333));
    // No prior close, or no price, is no return.
    assert_eq!(return_ppm(0, 5), None);
    assert_eq!(return_ppm(-5, 5), None);
    assert_eq!(return_ppm(5, 0), None);
    assert_eq!(return_ppm(5, -1), None);
    assert_eq!(return_ppm(1, i64::MAX), None);
}

#[test]
fn parameters_read_back_exactly_and_every_wrong_one_is_refused() {
    let p = ClosingReversalParams::default();
    assert_eq!(
        p.render(),
        "names=20 extreme_bp=0 spread_cap_bp=0 dollars=2000 collar_permille=5 stop_permille=100 ref_minutes=60 entry_minutes=30 exit_seconds=30"
    );
    assert_eq!(ClosingReversalParams::parse(&p.render()), Ok(p));
    let q = ClosingReversalParams {
        names: 40,
        extreme_bp: 300,
        spread_cap_bp: 5,
        dollars: 1,
        collar_permille: 999,
        stop_permille: 999,
        ref_minutes: 390,
        entry_minutes: 390,
        exit_seconds: 23_399,
    };
    assert_eq!(ClosingReversalParams::parse(&q.render()), Ok(q));
    // Spaces and the order of the keys are the writer's business.
    assert_eq!(
        ClosingReversalParams::parse(
            "  exit_seconds=30  entry_minutes=30 ref_minutes=60 stop_permille=100 collar_permille=5 dollars=2000 spread_cap_bp=0 extreme_bp=0 names=20 "
        ),
        Ok(p)
    );
    let with = |f: fn(&mut ClosingReversalParams)| {
        let mut x = p;
        f(&mut x);
        x
    };
    for (bad, what) in [
        (with(|x| x.names = 0), "names"),
        (with(|x| x.dollars = 0), "dollars"),
        (with(|x| x.collar_permille = 1000), "collar_permille"),
        (with(|x| x.stop_permille = 0), "stop_permille"),
        (with(|x| x.stop_permille = 1000), "stop_permille"),
        (with(|x| x.ref_minutes = 0), "ref_minutes"),
        (with(|x| x.ref_minutes = 391), "ref_minutes"),
        (with(|x| x.entry_minutes = 0), "entry_minutes"),
        (with(|x| x.entry_minutes = 61), "entry_minutes"),
        (with(|x| x.exit_seconds = 0), "exit_seconds"),
        (with(|x| x.exit_seconds = 1800), "exit_seconds"),
    ] {
        let e = bad.validate().unwrap_err().0;
        assert!(e.contains(what), "{what}: {e}");
        assert!(ClosingReversalParams::parse(&bad.render()).is_err());
        assert!(ClosingReversal::new(1, bad).is_err());
    }
    for (text, what) in [
        ("names", "not key=value"),
        ("names=20 colour=3", "not a parameter"),
        ("names=twenty", "whole number"),
        ("names=-1", "whole number"),
        ("names=20 names=20", "twice"),
        ("names=20", "missing"),
        ("", "missing"),
    ] {
        let ParamError(e) = ClosingReversalParams::parse(text).unwrap_err();
        assert!(e.contains(what), "{text}: {e}");
    }
}

#[test]
fn it_reviews_once_a_minute_and_leaves_a_name_whose_stop_would_be_nothing_to_the_side() {
    let p = ClosingReversalParams {
        dollars: 1,
        ..names(1)
    };
    let rig = Rig::new(p, &priors(1), Some(regular()));
    assert_eq!(rig.runner.strategy().period(), 60 * SEC);
    // A price of a billionth of a dollar: a size that fits, and a stop 10% under it that rounds to nothing. Skipped, and
    // not counted as an order the framework refused.
    let mut rig = Rig::new(p, &priors(2), Some(regular()));
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), 1));
    rig.feed(trade(1, rig.t(3700) + 1, cents(990)));
    rig.tick(3590);
    rig.feed(quote(0, rig.t(1900), 1, 1));
    rig.feed(quote(1, rig.t(1900) + 1, cents(989), cents(991)));
    rig.tick(1790);
    assert!(rig.out().is_empty());
    let s = rig.runner.strategy().stats();
    assert_eq!((s.decisions, s.entries, s.entries_refused), (1, 0, 0));
}

// ---- the trace of the decision (E19-S32) ----

fn status_of(t: &crate::trace::Trace, instrument: u32) -> String {
    let ids = t.column("instrument").expect("an instrument column");
    let st = t.column("status").expect("a status column");
    let i = ids
        .iter()
        .position(|&x| x == instrument.to_string())
        .unwrap_or_else(|| panic!("no row for {instrument}"));
    st[i].to_owned()
}

#[test]
fn the_trace_of_a_decision_is_the_whole_cross_section_with_the_reason_for_each_name() {
    // Nine members, a prior close of $10 but for name 6 (none). Name 0 halted, 2 restricted, 4 a crossed quote, 7 no quote,
    // 8 never traded; 1, 3 and 5 can be bought, and two names are bought.
    let mut p = priors(9);
    p[6] = 0;
    let mut rig = Rig::new(names(2), &p, Some(regular()));
    rig.runner.set_tracing(true);
    rig.feed(status(0, rig.t(3800), StatusKind::TradingHalt));
    rig.feed(status(2, rig.t(3800) + 1, StatusKind::ShortSaleRestriction));
    rig.tick(3850);
    for (i, px) in [
        (0u32, 900),
        (1, 950),
        (2, 960),
        (3, 970),
        (4, 980),
        (5, 990),
        (6, 1000),
        (7, 1010),
    ] {
        rig.feed(trade(i, rig.t(3700) + u64::from(i), cents(px)));
    }
    rig.tick(3590);
    for (i, px) in [
        (0u32, 900),
        (1, 950),
        (2, 960),
        (3, 970),
        (5, 990),
        (6, 1000),
    ] {
        rig.feed(quote(
            i,
            rig.t(1900) + u64::from(i),
            cents(px - 1),
            cents(px + 1),
        ));
    }
    rig.feed(quote(4, rig.t(1900) + 4, cents(981), cents(979)));
    rig.tick(1790);
    assert_eq!(picked(&rig.out()), [1, 3]);
    let traces = rig.runner.drain_traces();
    assert_eq!(traces.len(), 1);
    let t = &traces[0];
    assert_eq!((t.kind.as_str(), t.ts), ("rank", rig.t(1800)));
    for (k, v) in [
        ("close", rig.close.to_string()),
        ("ref_minutes", "60".into()),
        ("entry_minutes", "30".into()),
        ("names", "2".into()),
        ("extreme_bp", "0".into()),
        ("spread_cap_bp", "0".into()),
        ("members", "9".into()),
        ("eligible", "3".into()),
        ("entered", "2".into()),
    ] {
        assert_eq!(t.value(k), Some(v.as_str()), "{k}");
    }
    assert_eq!(
        t.columns,
        [
            "rank",
            "instrument",
            "prior_close",
            "ref_px",
            "ref_src",
            "ret_ppm",
            "bid",
            "ask",
            "status"
        ]
    );
    // Those that may be bought, most negative first; then the rest by number.
    assert_eq!(
        t.column("instrument").unwrap(),
        ["1", "3", "5", "0", "2", "4", "6", "7", "8"]
    );
    assert_eq!(
        t.column("rank").unwrap(),
        ["1", "2", "3", "", "", "", "", "", ""]
    );
    assert_eq!(
        t.column("status").unwrap(),
        [
            "entered",
            "entered",
            "not_chosen",
            "skipped:halted",
            "skipped:restricted",
            "skipped:crossed_quote",
            "skipped:no_prior",
            "skipped:no_quote",
            "skipped:no_price",
        ]
    );
    // The first row in full: the numbers the decision was made on, raw.
    assert_eq!(
        t.rows[0],
        [
            "1",
            "1",
            "10000000000",
            "9500000000",
            "tier0",
            "-50000",
            "9490000000",
            "9510000000",
            "entered"
        ]
    );
    // A name without a prior close has no return but has its price; one that never traded has no price; one never quoted has
    // no quote.
    assert_eq!(t.rows[6][2..6], ["", "10000000000", "tier0", ""]);
    assert_eq!(
        t.rows[6][6..],
        ["9990000000", "10010000000", "skipped:no_prior"]
    );
    assert_eq!(
        t.rows[8][2..],
        ["10000000000", "", "none", "", "", "", "skipped:no_price"]
    );
    assert_eq!(
        t.rows[7][2..7],
        ["10000000000", "10100000000", "tier0", "10000", ""]
    );
    // The traces are taken once.
    assert!(rig.runner.drain_traces().is_empty());
}

#[test]
fn the_trace_says_where_the_price_came_from() {
    let mut rig = Rig::new(names(1), &priors(2), Some(regular()));
    rig.runner.set_tracing(true);
    rig.feed(trade(0, rig.t(3950), cents(900)));
    rig.feed(trade(1, rig.t(3950) + 1, cents(990)));
    rig.tick(3850);
    rig.feed(trade(1, rig.t(3700) + 1, cents(990)));
    // The first event after 15:00 is name 0's own later trade: its price is the snapshot's.
    rig.feed(trade(0, rig.t(3590), cents(1200)));
    for i in 0..2u32 {
        rig.feed(quote(i, rig.t(1900) + u64::from(i), cents(899), cents(901)));
    }
    rig.tick(1790);
    let t = rig.runner.drain_traces().remove(0);
    let src = t.column("ref_src").unwrap();
    let ids = t.column("instrument").unwrap();
    let of = |id: &str| src[ids.iter().position(|&x| x == id).unwrap()];
    assert_eq!((of("0"), of("1")), ("snapshot", "tier0"));
}

#[test]
fn the_filters_and_the_names_the_money_does_not_buy_are_in_the_trace_with_their_reasons() {
    // The extreme floor: -3%, so the names above it say so.
    let p = ClosingReversalParams {
        extreme_bp: 300,
        ..names(6)
    };
    let mut rig = Rig::new(p, &priors(6), Some(regular()));
    rig.runner.set_tracing(true);
    let out = rig.play(&SIX);
    assert_eq!(picked(&out), [0, 1, 4]);
    let t = rig.runner.drain_traces().remove(0);
    assert_eq!(t.value("eligible"), Some("3"));
    for (id, want) in [
        (0, "entered"),
        (1, "entered"),
        (4, "entered"),
        (2, "skipped:not_extreme"),
        (3, "skipped:not_extreme"),
        (5, "skipped:not_extreme"),
    ] {
        assert_eq!(status_of(&t, id), want, "name {id}");
    }
    // The spread cap: a mid of $20 and a cent is 5.0 basis points (kept); a bit more is not.
    let p = ClosingReversalParams {
        spread_cap_bp: 5,
        ..names(10)
    };
    let mut rig = Rig::new(p, &priors(2), Some(regular()));
    rig.runner.set_tracing(true);
    rig.tick(3850);
    rig.feed(trade(0, rig.t(3700), cents(900)));
    rig.feed(trade(1, rig.t(3700) + 1, cents(901)));
    rig.tick(3590);
    rig.feed(quote(0, rig.t(1900), 19_995_000_000, 20_005_000_000));
    rig.feed(quote(1, rig.t(1900) + 1, 19_995_000_000, 20_005_100_000));
    rig.tick(1790);
    let t = rig.runner.drain_traces().remove(0);
    assert_eq!(
        (status_of(&t, 0).as_str(), status_of(&t, 1).as_str()),
        ("entered", "skipped:wide_spread")
    );
    // Names chosen that the dollars do not buy, or whose stop would be nothing: chosen and not entered.
    let p = ClosingReversalParams {
        dollars: 1,
        ..names(2)
    };
    let mut rig = Rig::new(p, &priors(3), Some(regular()));
    rig.runner.set_tracing(true);
    rig.tick(3850);
    for i in 0..3u32 {
        rig.feed(trade(
            i,
            rig.t(3700) + u64::from(i),
            cents(900 + i64::from(i)),
        ));
    }
    rig.tick(3590);
    rig.feed(quote(0, rig.t(1900), cents(2000), cents(2002)));
    rig.feed(quote(1, rig.t(1900) + 1, 1, 1));
    rig.feed(quote(2, rig.t(1900) + 2, cents(2000), cents(2002)));
    rig.tick(1790);
    assert!(rig.out().is_empty());
    let t = rig.runner.drain_traces().remove(0);
    assert_eq!(status_of(&t, 0), "skipped:no_share");
    assert_eq!(status_of(&t, 1), "skipped:no_stop");
    assert_eq!(status_of(&t, 2), "not_chosen");
    assert_eq!(
        (t.value("entered"), t.value("eligible")),
        (Some("0"), Some("3"))
    );
}

#[test]
fn a_day_traced_decides_exactly_as_a_day_untraced_and_a_strategy_not_asked_records_nothing() {
    let run = |traced: bool| {
        let mut rig = Rig::new(names(3), &priors(6), Some(regular()));
        rig.runner.set_tracing(traced);
        let out = rig.play(&SIX);
        let fills = [100u32, 80, 60];
        for (i, f) in out.iter().zip(fills) {
            rig.update(OrderUpdate {
                intent: i.id,
                order: None,
                state: OrderState::Filled,
                filled_qty: f.min(i.qty),
                avg_px: Some(Px::from_raw(cents(901))),
                reject: None,
                ts: rig.t(1799),
            });
        }
        rig.tick(30);
        let exits = rig.out();
        (
            format!("{out:?}"),
            format!("{exits:?}"),
            rig.runner.strategy().stats(),
            rig.runner.drain_traces().len(),
        )
    };
    let (a, b) = (run(false), run(true));
    assert_eq!((&a.0, &a.1, a.2), (&b.0, &b.1, b.2));
    assert_eq!((a.3, b.3), (0, 1));
    // Turning tracing off drops what was kept.
    let mut rig = Rig::new(names(3), &priors(6), Some(regular()));
    rig.runner.set_tracing(true);
    rig.play(&SIX);
    rig.runner.set_tracing(false);
    assert!(rig.runner.drain_traces().is_empty());
}

#[test]
fn a_strategy_never_asked_to_trace_records_nothing_and_a_prior_close_of_nothing_is_no_prior() {
    let mut rig = Rig::new(names(3), &priors(6), Some(regular()));
    rig.play(&SIX);
    assert!(rig.runner.drain_traces().is_empty());
    // A prior close of exactly nothing (not unknown) is no prior close, whatever the price: and a negative one too.
    let mut rig = Rig::new(names(6), &priors(3), Some(regular()));
    rig.runner.set_tracing(true);
    rig.refs[0].price = Some(0);
    rig.refs[1].price = Some(-5);
    rig.play(&[900, 950, 980]);
    let t = rig.runner.drain_traces().remove(0);
    assert_eq!(status_of(&t, 0), "skipped:no_prior");
    assert_eq!(status_of(&t, 1), "skipped:no_prior");
    assert_eq!(status_of(&t, 2), "entered");
}
