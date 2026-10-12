//! T01, the opening range breakout, from scripted events: every decision of the rule has a case here. A day is played in seconds from the
//! 09:30 open (negative before it) with the shared bars fed as the host feeds them: only a trade moves bar time on, so the clock instrument
//! trades one share a second to close the candle's bar.

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_engine::{MtfConfig, SharedBars, Tier0};
use tf_universe::{HistInfo, RefInfo};

use crate::budget::{BudgetSource, BudgetView};
use crate::cross::{CrossRunner, Market, Members};
use crate::exits::{REASON_STOP, REASON_TIME};
use crate::intent::{Intent, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::opening_range::{OpeningRange, OpeningRangeParams, OpeningRangeStats, REASON_ENTRY};

const D: i64 = 1_000_000_000;
const SEC: u64 = 1_000_000_000;

fn cents(c: i64) -> i64 {
    c * D / 100
}

fn px(c: i64) -> Px {
    Px::from_raw(cents(c))
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

struct Fixed(BudgetView);

impl BudgetSource for Fixed {
    fn budget(&self, _: StrategyId) -> Option<BudgetView> {
        Some(self.0)
    }
}

struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    bars: SharedBars,
    runner: CrossRunner<OpeningRange>,
    day: SessionTimes,
    clock: u32,
    budget: Option<Fixed>,
    closes: Vec<tf_engine::BarClose>,
}

/// The paper's defaults, with a trigger that lives to minute 60 so a test can see it lapse.
fn params() -> OpeningRangeParams {
    OpeningRangeParams {
        entry_minutes: 60,
        ..OpeningRangeParams::default()
    }
}

fn with(f: impl FnOnce(&mut OpeningRangeParams)) -> OpeningRangeParams {
    let mut p = params();
    f(&mut p);
    p
}

/// A name with a prior close of $50, an ATR of $2, five-minute volume of 10,000 shares on average, 30,000 in the first quarter hour, easy to borrow.
fn reference() -> RefInfo {
    let mut hist = HistInfo {
        atr14: Some(2 * D),
        vol_first5: Some(10_000),
        ..HistInfo::default()
    };
    hist.profile.set_vol_quarter(22, Some(30_000));
    RefInfo {
        price: Some(50 * D),
        adv_shares: Some(2_000_000),
        easy_to_borrow: Some(true),
        hist,
    }
}

impl Rig {
    /// Names 0..n, the shared bars on (room for all), the first reviews in the premarket.
    fn new(p: OpeningRangeParams, names: u32) -> Rig {
        Rig::build(p, names, names as usize + 1, day(1))
    }

    fn build(p: OpeningRangeParams, names: u32, bars_room: usize, day: SessionTimes) -> Rig {
        Rig::custom(p, names, bars_room, day, |_| {})
    }

    /// As [`Rig::build`], with the references of the names changed by `tweak` before the first review (which claims on what it reads).
    fn custom(
        p: OpeningRangeParams,
        names: u32,
        bars_room: usize,
        day: SessionTimes,
        tweak: impl FnOnce(&mut [RefInfo]),
    ) -> Rig {
        let mut tier0 = Tier0::new(names as usize + 1);
        tier0.set_day(day);
        let mut refs: Vec<RefInfo> = (0..names).map(|_| reference()).collect();
        tweak(&mut refs);
        refs.push(RefInfo::default());
        let mut rig = Rig {
            tier0,
            refs,
            bars: SharedBars::new(MtfConfig::session(), names as usize + 1, bars_room),
            runner: CrossRunner::new(
                OpeningRange::new(1, p).unwrap(),
                Members::from_ids(0..names),
            ),
            day,
            clock: names,
            budget: None,
            closes: Vec::new(),
        };
        rig.tick(-3_600);
        rig.tick(-3_594);
        rig
    }

    fn at(&self, secs: i64) -> Nanos {
        (self.day.open as i64 + secs * SEC as i64) as Nanos
    }

    fn feed(&mut self, ev: Event) {
        self.tier0.on_event(&ev);
        self.closes.clear();
        self.bars.on_event(&ev, &mut self.closes);
        self.runner.on_event(
            Market {
                tier0: &self.tier0,
                refs: &self.refs,
                budget: self.budget.as_ref().map(|b| b as &dyn BudgetSource),
            },
            None,
            Some(&mut self.bars),
            &ev,
        );
    }

    /// Time passes: one share of the clock instrument at $1.00.
    fn tick(&mut self, secs: i64) {
        let ts = self.at(secs);
        self.feed(Event::Trade(Trade {
            hdr: hdr(self.clock, ts),
            px: px(100),
            size: 1,
            flags: TradeFlags::NONE,
        }));
    }

    fn quote(&mut self, id: u32, secs: i64, bid: i64, ask: i64) {
        let ts = self.at(secs);
        self.feed(Event::Quote(Quote {
            hdr: hdr(id, ts),
            bid_px: px(bid),
            ask_px: px(ask),
            bid_sz: 100,
            ask_sz: 100,
        }));
    }

    /// A trade of `id` at `c` cents with a quote a cent either side just before it.
    fn trade(&mut self, id: u32, secs: i64, c: i64, size: u32) {
        self.quote(id, secs, c - 1, c + 1);
        let ts = self.at(secs);
        self.feed(Event::Trade(Trade {
            hdr: hdr(id, ts + 1),
            px: px(c),
            size,
            flags: TradeFlags::NONE,
        }));
    }

    /// A five-minute candle of `id`: opens at `o`, reaches `h` and `l`, closes at `c`, with `vol` shares in all (three trades carry the
    /// extremes, the last the close). Then a tick closes the bar and a later one is the first review after the settling time.
    fn candle(&mut self, id: u32, o: i64, h: i64, l: i64, c: i64, vol: u32) {
        self.candle_of(id, 5, o, h, l, c, vol);
    }

    #[allow(clippy::too_many_arguments)]
    fn candle_of(&mut self, id: u32, minutes: i64, o: i64, h: i64, l: i64, c: i64, vol: u32) {
        let rest = vol - 3 * (vol / 4);
        self.trade(id, 1, o, vol / 4);
        self.trade(id, 60, h, vol / 4);
        self.trade(id, 120, l, vol / 4);
        self.trade(id, minutes * 60 - 5, c, rest);
    }

    /// The clock passes the end of the candle (closing its bar) and then the settling time, so the first review after it judges.
    fn judge(&mut self, minutes: i64) {
        self.tick(minutes * 60 + 1);
        self.tick(minutes * 60 + 6);
    }

    fn update(&mut self, u: OrderUpdate) {
        self.runner
            .on_order_update(&self.tier0, None, Some(&mut self.bars), &u);
    }

    fn out(&mut self) -> Vec<Intent> {
        self.runner.drain_intents()
    }

    fn stats(&self) -> OpeningRangeStats {
        self.runner.strategy().stats()
    }
}

fn fill(i: &Intent, qty: u32, c: i64, ts: Nanos) -> OrderUpdate {
    OrderUpdate {
        intent: i.id,
        order: None,
        state: OrderState::Filled,
        filled_qty: qty,
        avg_px: Some(px(c)),
        reject: None,
        ts,
    }
}

// ---- the rule ----

#[test]
fn a_green_candle_in_play_arms_a_buy_at_its_high_and_the_first_trade_through_it_buys_once() {
    let mut rig = Rig::new(params(), 1);
    // Opens 50.00, high 51.00, low 49.80, closes 50.80 on 15,000 shares: 150% of the 10,000 normal.
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    assert!(rig.out().is_empty(), "armed, nothing sent");
    let s = rig.stats();
    assert_eq!((s.claimed, s.armed, s.entries), (1, 1, 0));
    // A trade under the high does nothing.
    rig.trade(0, 400, 5099, 10);
    assert!(rig.out().is_empty());
    // The first trade at the high (51.00) buys: dollars 5,000 of the level is 98 shares, a collar of 1% around the print, the stop 10% of
    // the 2.00 ATR (0.20) under the level for the broker, the entry's reason.
    rig.trade(0, 410, 5100, 10);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let i = &out[0];
    assert_eq!(
        (i.instrument, i.side, i.purpose, i.tif, i.reason, i.qty),
        (0, Side::Buy, Purpose::Open, Tif::Day, REASON_ENTRY, 98)
    );
    assert_eq!(
        i.pricing,
        Pricing::Collar {
            reference: px(5100),
            collar_permille: 10
        }
    );
    assert_eq!(i.protect.map(|p| p.stop_trigger), Some(px(5080)));
    assert_eq!(rig.stats().entries, 1);
    // Once: a higher print buys nothing more.
    rig.trade(0, 420, 5200, 10);
    assert!(rig.out().is_empty());
}

#[test]
fn a_red_candle_arms_a_short_sale_at_its_low_with_the_stop_above() {
    let mut rig = Rig::new(params(), 1);
    // Opens 50.00, high 50.30, low 49.00, closes 49.20.
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 4901, 10);
    assert!(rig.out().is_empty());
    rig.trade(0, 410, 4900, 10);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let i = &out[0];
    assert_eq!(
        (i.side, i.purpose, i.qty),
        (Side::SellShort, Purpose::Open, 5_000 / 49)
    );
    // The stop is 0.20 over the level of 49.00.
    assert_eq!(i.protect.map(|p| p.stop_trigger), Some(px(4920)));
    assert!(
        i.limit_price() < px(4900),
        "a short sale's limit is under the print"
    );
}

#[test]
fn a_short_is_taken_only_in_a_name_that_is_easy_to_borrow() {
    let mut rig = Rig::new(params(), 2);
    rig.refs[1].easy_to_borrow = Some(false);
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.candle(1, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    let s = rig.stats();
    assert_eq!((s.armed, s.not_shortable), (1, 1));
    rig.trade(1, 400, 4800, 10);
    assert!(rig.out().is_empty(), "name 1 has no trigger");
    rig.trade(0, 410, 4800, 10);
    assert_eq!(rig.out().len(), 1);
    // A name with no borrow flag at all is not shorted either.
    let mut rig = Rig::new(params(), 1);
    rig.refs[0].easy_to_borrow = None;
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    assert_eq!(rig.stats().not_shortable, 1);
    // A long needs no borrow.
    let mut rig = Rig::new(params(), 1);
    rig.refs[0].easy_to_borrow = Some(false);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    assert_eq!(rig.stats().armed, 1);
}

#[test]
fn a_doji_gets_no_trade() {
    let mut rig = Rig::new(params(), 1);
    rig.candle(0, 5000, 5100, 4900, 5000, 15_000);
    rig.judge(5);
    let s = rig.stats();
    assert_eq!((s.doji, s.armed), (1, 0));
    rig.trade(0, 400, 5200, 10);
    rig.trade(0, 410, 4800, 10);
    assert!(rig.out().is_empty());
}

#[test]
fn a_name_is_in_play_from_exactly_the_minimum_relative_volume() {
    // 10,000 normal; the minimum is 100%.
    for (vol, in_play) in [(9_999u32, false), (10_000, true), (10_001, true)] {
        let mut rig = Rig::new(params(), 1);
        rig.candle(0, 5000, 5100, 4980, 5080, vol);
        rig.judge(5);
        let s = rig.stats();
        assert_eq!(
            (s.armed, s.low_relvol),
            (u64::from(in_play), u64::from(!in_play)),
            "{vol}"
        );
    }
    // A higher minimum (300%) excludes 29,999 and keeps 30,000.
    for (vol, in_play) in [(29_999u32, false), (30_000, true)] {
        let mut rig = Rig::new(with(|p| p.min_relvol_pct = 300), 1);
        rig.candle(0, 5000, 5100, 4980, 5080, vol);
        rig.judge(5);
        assert_eq!(rig.stats().armed, u64::from(in_play), "{vol}");
    }
}

#[test]
fn only_the_names_with_the_highest_ratios_are_traded_and_ties_go_to_the_lower_number() {
    let mut rig = Rig::new(with(|p| p.names = 2), 4);
    // Ratios 1.5, 3.0, 3.0, 2.0: the two 3.0s (names 1 and 2) are traded; 3 and 0 are cut.
    for (id, vol) in [(0, 15_000), (1, 30_000), (2, 30_000), (3, 20_000)] {
        rig.candle(id, 5000, 5100, 4980, 5080, vol);
    }
    rig.judge(5);
    let s = rig.stats();
    assert_eq!((s.armed, s.cut), (2, 2));
    for id in [0, 3] {
        rig.trade(id, 400, 5200, 10);
    }
    assert!(rig.out().is_empty());
    rig.trade(2, 410, 5200, 10);
    rig.trade(1, 411, 5200, 10);
    let ids: Vec<u32> = rig.out().iter().map(|i| i.instrument).collect();
    assert_eq!(ids, [2, 1]);
    // Three at equal ratio and room for two: the lower two.
    let mut rig = Rig::new(with(|p| p.names = 2), 3);
    for id in 0..3 {
        rig.candle(id, 5000, 5100, 4980, 5080, 20_000);
    }
    rig.judge(5);
    for id in [2, 1, 0] {
        rig.trade(id, 400, 5200, 10);
    }
    let mut ids: Vec<u32> = rig.out().iter().map(|i| i.instrument).collect();
    ids.sort_unstable();
    assert_eq!(ids, [0, 1]);
}

#[test]
fn a_name_that_does_not_fit_is_not_counted_against_the_limit() {
    // Name 0 is not shortable and has the best ratio; with room for one it must not use the place.
    let mut rig = Rig::new(with(|p| p.names = 1), 2);
    rig.refs[0].easy_to_borrow = Some(false);
    rig.candle(0, 5000, 5030, 4900, 4920, 40_000);
    rig.candle(1, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    let s = rig.stats();
    assert_eq!((s.armed, s.not_shortable, s.cut), (1, 1, 0));
    rig.trade(1, 400, 5100, 10);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn a_fifteen_minute_candle_is_judged_against_the_first_quarter_hour_and_not_before_it_ends() {
    let mut rig = Rig::new(with(|p| p.range_minutes = 15), 1);
    // 30,000 normal in the first quarter hour: 29,999 is not in play, 30,000 is.
    rig.candle_of(0, 15, 5000, 5100, 4980, 5080, 30_000);
    // At five minutes nothing is judged.
    rig.tick(5 * 60 + 6);
    rig.tick(6 * 60);
    assert_eq!(rig.stats().armed, 0);
    rig.judge(15);
    assert_eq!(rig.stats().armed, 1);
    rig.trade(0, 1000, 5100, 10);
    assert_eq!(rig.out().len(), 1);
    let mut rig = Rig::new(with(|p| p.range_minutes = 15), 1);
    rig.candle_of(0, 15, 5000, 5100, 4980, 5080, 29_999);
    rig.judge(15);
    assert_eq!(rig.stats().low_relvol, 1);
}

#[test]
fn a_candle_is_judged_five_seconds_after_it_ends_and_a_bar_that_has_not_closed_is_skipped() {
    let mut rig = Rig::new(params(), 2);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    // Name 1 has no trade in the candle at all.
    // Just after the end of the candle (09:35:01) and before the settling time: not judged yet.
    rig.tick(5 * 60 + 1);
    rig.tick(5 * 60 + 4);
    assert_eq!(rig.stats().armed, 0);
    // The review at 09:35:05 judges: name 0 is armed, name 1 has no bar.
    rig.tick(5 * 60 + 5);
    let s = rig.stats();
    assert_eq!((s.armed, s.no_bar), (1, 1));
    // It is judged once: a candle that closes later is not looked at again.
    rig.trade(1, 5 * 60 + 30, 5000, 100);
    rig.tick(5 * 60 + 40);
    assert_eq!(rig.stats().armed, 1);
}

#[test]
fn names_without_an_atr_or_a_baseline_are_not_claimed_and_the_claims_are_bounded() {
    // No ATR, no five-minute baseline, an ATR of nothing, a baseline of nothing: none is claimed.
    let r = Rig::custom(params(), 4, 5, day(1), |refs| {
        refs[0].hist.atr14 = None;
        refs[1].hist.vol_first5 = None;
        refs[2].hist.atr14 = Some(0);
        refs[3].hist.vol_first5 = Some(0);
    });
    let s = r.stats();
    assert_eq!((s.claimed, s.no_reference), (0, 4));
    // The fifteen-minute rule reads the profile's first quarter hour instead, and a name without it is not claimed.
    let r = Rig::custom(with(|p| p.range_minutes = 15), 2, 3, day(1), |refs| {
        refs[1].hist.profile = tf_universe::Profile::default();
    });
    let s = r.stats();
    assert_eq!((s.claimed, s.no_reference), (1, 1));
    // max_tracked bounds the claims of the strategy.
    let r = Rig::new(with(|p| p.max_tracked = 2), 5);
    let s = r.stats();
    assert_eq!((s.claimed, s.refused_bars), (2, 3));
    // So does the room of the shared bars: three places for four names.
    let r = Rig::build(params(), 4, 3, day(1));
    let s = r.stats();
    assert_eq!((s.claimed, s.refused_bars), (3, 1));
}

#[test]
fn claims_are_let_go_when_the_candles_are_judged() {
    let mut rig = Rig::new(params(), 2);
    assert_eq!(rig.bars.tracked(), 2);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.candle(1, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    assert_eq!(
        rig.bars.tracked(),
        0,
        "the bars are not needed once the triggers are armed"
    );
}

/// A rig whose strategy has its first two events at `first` and `second` seconds from the open (its first review is at the second's).
fn late_rig(first: i64, second: i64) -> Rig {
    let mut tier0 = Tier0::new(2);
    tier0.set_day(day(1));
    let mut r = Rig {
        tier0,
        refs: vec![reference(), RefInfo::default()],
        bars: SharedBars::new(MtfConfig::session(), 2, 4),
        runner: CrossRunner::new(
            OpeningRange::new(1, params()).unwrap(),
            Members::from_ids(0..1),
        ),
        day: day(1),
        clock: 1,
        budget: None,
        closes: Vec::new(),
    };
    r.tick(first);
    r.tick(second);
    r
}

#[test]
fn the_premarket_claims_only_and_a_strategy_that_starts_after_the_open_trades_nothing() {
    // The first review is at 09:30:10, after the open: no claims, so the candle is not judged.
    let mut r = late_rig(5, 10);
    assert_eq!(r.stats().claimed, 0);
    r.candle(0, 5000, 5100, 4980, 5080, 15_000);
    r.judge(5);
    let s = r.stats();
    assert_eq!(
        (s.armed, s.no_bar),
        (0, 0),
        "nothing was claimed, so nothing is judged"
    );
    r.trade(0, 400, 5200, 10);
    assert!(r.out().is_empty());
    // A review at the very instant of the open is too late to claim for the first candle's bar as well (the bars start with the first trade).
    let r = late_rig(-6, 0);
    assert_eq!(r.stats().claimed, 0);
    // A second before it is not.
    let r = late_rig(-7, -1);
    assert_eq!(r.stats().claimed, 1);
}

// ---- the size ----

#[test]
fn with_a_budget_the_size_is_the_risk_over_the_stop_capped_by_the_position_limit_and_the_room() {
    let level_cents = 5100;
    // 100,000 dollars: 1% is 1,000 dollars over a 0.20 stop = 5,000 shares; a position holds 5% of the budget (5,000 dollars): 98 shares.
    let mut rig = Rig::new(params(), 1);
    rig.budget = Some(Fixed(BudgetView {
        total: 100_000 * D as u128,
        used: 0,
    }));
    rig.candle(0, 5000, level_cents, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, level_cents, 10);
    assert_eq!(rig.out()[0].qty, 98);
    // With a position limit of 100% the risk decides: 5,000 shares cost 255,000, more than the budget holds: 100,000 / 51 = 1,960.
    let mut rig = Rig::new(with(|p| p.max_position_permille = 1000), 1);
    rig.budget = Some(Fixed(BudgetView {
        total: 100_000 * D as u128,
        used: 0,
    }));
    rig.candle(0, 5000, level_cents, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, level_cents, 10);
    assert_eq!(rig.out()[0].qty, 1960);
    // With a smaller risk (0.1%): 100 dollars over 0.20 = 500 shares, inside the room.
    let mut rig = Rig::new(
        with(|p| {
            p.max_position_permille = 1000;
            p.risk_ppm = 1_000;
        }),
        1,
    );
    rig.budget = Some(Fixed(BudgetView {
        total: 100_000 * D as u128,
        used: 0,
    }));
    rig.candle(0, 5000, level_cents, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, level_cents, 10);
    assert_eq!(rig.out()[0].qty, 500);
    // Most of the budget used: the room is 5,100 dollars, 100 shares at 51.
    let mut rig = Rig::new(with(|p| p.max_position_permille = 1000), 1);
    rig.budget = Some(Fixed(BudgetView {
        total: 100_000 * D as u128,
        used: 94_900 * D as u128,
    }));
    rig.candle(0, 5000, level_cents, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, level_cents, 10);
    assert_eq!(rig.out()[0].qty, 100);
    // Nothing left: no trigger.
    let mut rig = Rig::new(params(), 1);
    rig.budget = Some(Fixed(BudgetView {
        total: 100_000 * D as u128,
        used: 100_000 * D as u128,
    }));
    rig.candle(0, 5000, level_cents, 4980, 5080, 15_000);
    rig.judge(5);
    assert_eq!(rig.stats().no_size, 1);
    rig.trade(0, 400, level_cents, 10);
    assert!(rig.out().is_empty());
}

#[test]
fn a_wider_stop_is_fewer_shares_for_the_same_risk() {
    let size = |stop_atr_pct: u32| {
        let mut rig = Rig::new(
            with(|p| {
                p.max_position_permille = 1000;
                p.stop_atr_pct = stop_atr_pct;
                p.risk_ppm = 1_000;
            }),
            1,
        );
        rig.budget = Some(Fixed(BudgetView {
            total: 1_000_000 * D as u128,
            used: 0,
        }));
        rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
        rig.judge(5);
        rig.trade(0, 400, 5100, 10);
        let i = rig.out()[0];
        (i.qty, i.protect.unwrap().stop_trigger)
    };
    // 1,000,000 dollars at 0.1% is 1,000 dollars: over a 0.20 stop (10% of the 2.00 ATR) 5,000 shares; over 0.40 (20%) 2,500.
    assert_eq!(size(10), (5_000, px(5080)));
    assert_eq!(size(20), (2_500, px(5060)));
}

// ---- the window and the checks at the fire ----

#[test]
fn a_trigger_lapses_at_entry_minutes_after_the_open() {
    // entry_minutes = 60: a trade through the level at 59:59 fires, at 60:00 does not.
    let mut rig = Rig::new(params(), 2);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.candle(1, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 3599, 5100, 10);
    assert_eq!(rig.out().len(), 1);
    rig.tick(3600);
    rig.trade(1, 3601, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().triggers.lapsed, 1);
}

#[test]
fn the_quoted_spread_is_checked_when_the_trigger_fires_and_a_trigger_that_is_refused_is_spent() {
    let mut rig = Rig::new(with(|p| p.spread_cap_bp = 20), 1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    // A quote 1.00 wide (about 196 bp of the mid) when the print comes: refused.
    rig.quote(0, 400, 5050, 5150);
    rig.feed(Event::Trade(Trade {
        hdr: hdr(0, rig.at(400) + 1),
        px: px(5100),
        size: 10,
        flags: TradeFlags::NONE,
    }));
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().triggers.refused, 1);
    // It is spent: a tight quote later does not revive it.
    rig.trade(0, 500, 5200, 10);
    assert!(rig.out().is_empty());
    // A spread of 10 cents at 51.00 is 19.6 bp: fine; 11 cents is 21.6 bp: not.
    for (spread, ok) in [(10, true), (11, false)] {
        let mut rig = Rig::new(with(|p| p.spread_cap_bp = 20), 1);
        rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
        rig.judge(5);
        rig.quote(0, 400, 5100 - spread / 2, 5100 + (spread - spread / 2));
        rig.feed(Event::Trade(Trade {
            hdr: hdr(0, rig.at(400) + 1),
            px: px(5100),
            size: 10,
            flags: TradeFlags::NONE,
        }));
        assert_eq!(rig.out().len(), usize::from(ok), "{spread} cents");
    }
    // Exactly the cap is allowed: a bid of 49.00 and an ask of 51.00 are 4.00 on a mid of 50.00, 400 bp; a cap of 399 refuses it.
    for (cap, ok) in [(400, true), (399, false)] {
        let mut rig = Rig::new(with(|p| p.spread_cap_bp = cap), 1);
        rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
        rig.judge(5);
        rig.quote(0, 400, 4900, 5100);
        rig.feed(Event::Trade(Trade {
            hdr: hdr(0, rig.at(400) + 1),
            px: px(5100),
            size: 10,
            flags: TradeFlags::NONE,
        }));
        assert_eq!(rig.out().len(), usize::from(ok), "cap {cap}");
    }
}

#[test]
fn every_name_armed_can_be_held_at_once() {
    let mut rig = Rig::new(with(|p| p.names = 2), 2);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.candle(1, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 5100, 10);
    let first = rig.out();
    assert_eq!(first.len(), 1);
    rig.trade(1, 410, 5100, 10);
    assert_eq!(rig.out().len(), 1, "two positions fit in names = 2");
}

// ---- the exits ----

fn held() -> (Rig, Intent) {
    let mut rig = Rig::new(params(), 1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 5100, 10);
    let i = rig.out()[0];
    let ts = rig.at(401);
    rig.update(fill(&i, i.qty, 5101, ts));
    (rig, i)
}

#[test]
fn the_stop_the_engine_holds_closes_the_position_at_the_first_print_through_it() {
    let (mut rig, i) = held();
    rig.trade(0, 500, 5085, 10);
    assert!(rig.out().is_empty(), "above the stop of 50.80");
    rig.trade(0, 510, 5080, 10);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].side, out[0].purpose, out[0].qty, out[0].reason),
        (Side::Sell, Purpose::Close, i.qty, REASON_STOP)
    );
}

#[test]
fn a_short_is_stopped_above_the_level_and_covered_with_a_buy() {
    let mut rig = Rig::new(params(), 1);
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 4900, 10);
    let i = rig.out()[0];
    let ts = rig.at(401);
    rig.update(fill(&i, i.qty, 4899, ts));
    rig.trade(0, 500, 4915, 10);
    assert!(rig.out().is_empty());
    rig.trade(0, 510, 4920, 10);
    let out = rig.out();
    assert_eq!(
        (out[0].side, out[0].purpose, out[0].reason),
        (Side::Buy, Purpose::Close, REASON_STOP)
    );
}

#[test]
fn what_is_held_is_closed_flat_minutes_before_the_close() {
    let (mut rig, i) = held();
    let close = rig.day.close;
    let before = close - 2 * 60 * SEC;
    // A tick a nanosecond before 15:58 does nothing; the time exit fires at it.
    rig.feed(Event::Trade(Trade {
        hdr: hdr(0, before - 1),
        px: px(5200),
        size: 1,
        flags: TradeFlags::NONE,
    }));
    assert!(rig.out().is_empty());
    rig.feed(Event::Trade(Trade {
        hdr: hdr(0, before + 1),
        px: px(5200),
        size: 1,
        flags: TradeFlags::NONE,
    }));
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].side, out[0].qty, out[0].reason),
        (Side::Sell, i.qty, REASON_TIME)
    );
}

#[test]
fn an_entry_that_never_fills_arms_no_exit() {
    let mut rig = Rig::new(with(|p| p.names = 1), 1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 5100, 10);
    let i = rig.out()[0];
    let ts = rig.at(401);
    rig.update(OrderUpdate {
        intent: i.id,
        order: None,
        state: OrderState::Cancelled,
        filled_qty: 0,
        avg_px: None,
        reject: None,
        ts,
    });
    assert_eq!(rig.stats().entries_refused, 1);
    // No exits for what was not bought.
    rig.trade(0, 500, 4000, 10);
    assert!(rig.out().is_empty());
}

#[test]
fn a_partly_filled_entry_exits_what_it_bought() {
    let mut rig = Rig::new(params(), 1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 5100, 10);
    let i = rig.out()[0];
    let ts = rig.at(401);
    rig.update(OrderUpdate {
        intent: i.id,
        order: None,
        state: OrderState::Cancelled,
        filled_qty: 40,
        avg_px: Some(px(5101)),
        reject: None,
        ts,
    });
    rig.trade(0, 500, 5080, 10);
    let out = rig.out();
    assert_eq!((out.len(), out[0].qty), (1, 40));
}

// ---- the parameters and the day ----

#[test]
fn the_parameters_render_parse_and_refuse_what_the_rule_cannot_run_with() {
    let p = OpeningRangeParams::default();
    assert_eq!(OpeningRangeParams::parse(&p.render()).unwrap(), p);
    assert!(
        p.render()
            .starts_with("names=20 range_minutes=5 min_relvol_pct=100 stop_atr_pct=10")
    );
    let e = |t: &str| OpeningRangeParams::parse(t).unwrap_err().0;
    assert!(e("names=20").contains("is missing"));
    assert!(e(&format!("{} wat=1", p.render())).contains("not a parameter"));
    assert!(e(&format!("{} names=1", p.render())).contains("twice"));
    assert!(e(&p.render().replace("names=20", "names=x")).contains("whole number"));
    assert!(e("names").contains("key=value"));
    let bad = |f: &dyn Fn(&mut OpeningRangeParams), why: &str| {
        let mut q = p;
        f(&mut q);
        let err = q.validate().unwrap_err().0;
        assert!(err.contains(why), "{err}");
        assert!(OpeningRange::new(1, q).is_err());
    };
    bad(&|q| q.names = 0, "names");
    bad(&|q| q.names = 1_001, "names");
    bad(&|q| q.range_minutes = 10, "range_minutes");
    bad(&|q| q.min_relvol_pct = 0, "min_relvol_pct");
    bad(&|q| q.stop_atr_pct = 0, "stop_atr_pct");
    bad(&|q| q.stop_atr_pct = 1_001, "stop_atr_pct");
    bad(&|q| q.risk_ppm = 0, "risk_ppm");
    bad(&|q| q.max_position_permille = 0, "max_position_permille");
    bad(
        &|q| q.max_position_permille = 1_001,
        "max_position_permille",
    );
    bad(&|q| q.dollars = 0, "dollars");
    bad(&|q| q.settle_seconds = 121, "settle_seconds");
    bad(&|q| q.flat_minutes = 0, "flat_minutes");
    bad(&|q| q.flat_minutes = 61, "flat_minutes");
    bad(&|q| q.entry_minutes = 7, "entry_minutes");
    bad(&|q| q.entry_minutes = 389, "entry_minutes");
    bad(&|q| q.collar_permille = 1000, "collar_permille");
    bad(&|q| q.spread_cap_bp = 10_001, "spread_cap_bp");
    bad(&|q| q.max_tracked = 0, "max_tracked");
    bad(&|q| q.max_tracked = 10_001, "max_tracked");
    // The edges that pass.
    let mut q = p;
    q.range_minutes = 15;
    q.entry_minutes = 18;
    q.flat_minutes = 60;
    q.names = 1_000;
    q.max_position_permille = 1_000;
    q.risk_ppm = 1_000_000;
    q.stop_atr_pct = 1_000;
    q.max_tracked = 10_000;
    q.settle_seconds = 120;
    q.spread_cap_bp = 10_000;
    q.collar_permille = 999;
    assert!(q.validate().is_ok());
    // entry_minutes + flat_minutes may be exactly the session.
    q.entry_minutes = 330;
    assert!(q.validate().is_ok());
    q.entry_minutes = 331;
    assert!(q.validate().is_err());
}

#[test]
fn a_second_day_starts_afresh() {
    let mut rig = Rig::new(params(), 1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 400, 5100, 10);
    assert_eq!(rig.out().len(), 1);
    // The next trading day (Monday 4 May): the premarket claims again, the candle is judged again, a trigger is armed again.
    let next = day(4);
    rig.tier0.set_day(next);
    rig.day = next;
    rig.tick(-3_600);
    rig.tick(-3_594);
    assert_eq!(rig.stats().claimed, 2, "claimed again");
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    assert_eq!(rig.stats().armed, 2);
    rig.trade(0, 400, 5100, 10);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn tracing_does_not_change_a_decision_and_says_what_was_decided() {
    let run = |tracing: bool| {
        let mut rig = Rig::new(params(), 2);
        rig.runner.set_tracing(tracing);
        rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
        rig.candle(1, 5000, 5030, 4900, 4920, 9_000);
        rig.judge(5);
        rig.trade(0, 400, 5100, 10);
        let out = rig.out();
        (out, rig.runner.drain_traces())
    };
    let (a, ta) = run(false);
    let (b, tb) = run(true);
    assert_eq!(a, b);
    assert!(ta.is_empty());
    let kinds: Vec<String> = tb.iter().map(|t| t.kind.clone()).collect();
    assert_eq!(kinds, ["armed", "fired"]);
}

#[test]
fn a_replay_of_the_same_events_sends_the_same_orders() {
    let run = || {
        let mut rig = Rig::new(with(|p| p.names = 3), 4);
        for (id, vol) in [(0, 12_000), (1, 30_000), (2, 20_000), (3, 5_000)] {
            rig.candle(id, 5000, 5100, 4980, 5080, vol);
        }
        rig.judge(5);
        for (s, id) in [(400, 2), (410, 1), (420, 0), (430, 3)] {
            rig.trade(id, s, 5100, 10);
        }
        rig.out()
    };
    let (a, b) = (run(), run());
    assert_eq!(a.len(), 3);
    assert_eq!(a, b);
}

#[test]
fn a_stop_that_is_not_a_price_arms_nothing() {
    // The stop distance is 10% of the ATR, whole raw units: an ATR of 5 raw units is a distance of 0.
    let mut rig = Rig::new(params(), 1);
    rig.refs[0].hist.atr14 = Some(5);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    let s = rig.stats();
    assert_eq!((s.armed, s.no_stop), (0, 1));
    // A stop at nothing (the distance is the whole level, 51.00 = 51e9 raw units, an ATR of 10 times that) is refused; a stop at one raw unit is not.
    for (atr, armed) in [(510_000_000_000i64, 0u64), (510_000_000_000 - 10, 1)] {
        let mut rig = Rig::new(params(), 1);
        rig.refs[0].hist.atr14 = Some(atr);
        rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
        rig.judge(5);
        assert_eq!(rig.stats().armed, armed, "atr {atr}");
        assert_eq!(rig.stats().no_stop, 1 - armed);
    }
    // For a short the stop is over the level, so no distance can make it nothing: only a distance of nothing refuses it.
    let mut rig = Rig::new(params(), 1);
    rig.refs[0].hist.atr14 = Some(510_000_000_000);
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    assert_eq!(rig.stats().armed, 1);
}

#[test]
fn the_quote_check_allows_a_locked_quote_and_refuses_a_crossed_one_or_none_at_all() {
    let fires = |bid: i64, ask: i64, cap: u32| {
        let mut rig = Rig::new(with(|p| p.spread_cap_bp = cap), 1);
        rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
        rig.judge(5);
        rig.quote(0, 400, bid, ask);
        rig.feed(Event::Trade(Trade {
            hdr: hdr(0, rig.at(400) + 1),
            px: px(5100),
            size: 10,
            flags: TradeFlags::NONE,
        }));
        rig.out().len()
    };
    assert_eq!(fires(5100, 5100, 10), 1, "locked: a spread of nothing");
    assert_eq!(fires(5101, 5100, 10), 0, "crossed");
    assert_eq!(fires(0, 5100, 10), 0, "no bid");
    // No quote at all before the trade: nothing to check, so nothing is bought under a cap.
    let mut rig = Rig::new(with(|p| p.spread_cap_bp = 10), 1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.feed(Event::Trade(Trade {
        hdr: hdr(0, rig.at(400)),
        px: px(5100),
        size: 10,
        flags: TradeFlags::NONE,
    }));
    // (The candle's own quotes are in Tier 0 already: this is the quote of the last trade of the candle, 1 cent either side, 3.9 bp.)
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn the_relative_volume_minimum_has_a_ceiling() {
    let mut p = OpeningRangeParams {
        min_relvol_pct: 1_000_000,
        ..OpeningRangeParams::default()
    };
    assert!(p.validate().is_ok());
    p.min_relvol_pct = 1_000_001;
    assert!(p.validate().is_err());
}

// ---- the hourly bias filter (T02, E19-S16) ----

/// The EMA(100) of the hourly closes at $50.00, as the snapshot would carry it.
fn put_ema(r: &mut RefInfo) {
    let mut e = tf_engine::Ema::new(100, tf_engine::Seed::Sma);
    for _ in 0..100 {
        e.update(50 * D);
    }
    let (s, c) = e.state().unwrap();
    r.hist.ema100h_state = Some(s);
    r.hist.ema100h_count = Some(i64::from(c));
}

/// The filter on, triggers living to minute 200, every name's average at $50.00.
fn bias_rig(names: u32) -> Rig {
    let p = with(|p| {
        p.bias = 1;
        p.entry_minutes = 200;
    });
    Rig::custom(p, names, names as usize + 1, day(1), |refs| {
        refs.iter_mut().for_each(put_ema);
    })
}

#[test]
fn with_the_bias_a_long_goes_when_the_last_hourly_close_is_over_the_average() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    // The name trades through the first hour and closes it at $50.90, over the $50.00 average.
    rig.trade(0, 3_000, 5090, 10);
    rig.trade(0, 3_660, 5100, 10);
    let out = rig.out();
    assert_eq!(out.len(), 1, "the break after 10:30 buys");
    assert_eq!(out[0].side, Side::Buy);
    assert_eq!(rig.stats().biased_out, 0);
}

#[test]
fn against_the_bias_the_trigger_fires_and_is_spent_with_nothing_sent() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    // The first hour closes at $49.50, under the average: a long is against it.
    rig.trade(0, 3_000, 4950, 10);
    rig.trade(0, 3_660, 5100, 10);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.biased_out, s.entries), (1, 0));
    rig.trade(0, 3_700, 5200, 10);
    assert!(rig.out().is_empty(), "spent");
}

#[test]
fn a_short_goes_with_a_bias_down_and_not_with_one_up() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    rig.trade(0, 3_000, 4950, 10);
    rig.trade(0, 3_660, 4900, 10);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].side, Side::SellShort);
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    rig.trade(0, 3_000, 5050, 10);
    rig.trade(0, 3_660, 4900, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}

#[test]
fn a_break_before_the_first_hourly_close_has_no_bias_and_is_no_trade() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 1_200, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
    rig.trade(0, 3_660, 5200, 10);
    assert!(rig.out().is_empty(), "the trigger was spent at the break");
}

#[test]
fn the_bias_is_not_asked_of_a_trigger_before_the_close_has_settled() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 3_000, 5090, 10);
    // 10:30:03: the bucket ended but the five seconds have not passed.
    rig.trade(0, 3_603, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}

#[test]
fn a_quiet_last_quarter_hour_closes_the_hour_at_the_last_price_before_it() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    // The last trade before 10:30 is at 10:05; the bars of 10:15 and 10:30 are filled in by the trade that breaks the level.
    rig.trade(0, 2_100, 5090, 10);
    rig.trade(0, 3_660, 5100, 10);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn later_hours_move_the_bias_with_the_average_carried() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    // Over at 10:30, under at 11:30: a break at 11:40 is against the bias of the latest close.
    rig.trade(0, 3_000, 5090, 10);
    rig.trade(0, 6_000, 4950, 10);
    rig.trade(0, 7_800, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}

#[test]
fn a_name_the_snapshot_has_no_average_for_is_not_claimed_with_the_filter() {
    let p = with(|p| {
        p.bias = 1;
        p.entry_minutes = 200;
    });
    let mut rig = Rig::custom(p, 2, 3, day(1), |refs| {
        refs.iter_mut().for_each(put_ema);
        refs[1].hist.ema100h_state = None;
    });
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.candle(1, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    let s = rig.stats();
    assert_eq!((s.claimed, s.no_reference, s.armed), (1, 1, 1));
}

#[test]
fn the_armed_names_keep_their_bars_with_the_filter_and_the_others_let_go() {
    let mut rig = bias_rig(2);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    // Name 1 is not in play (volume under the normal).
    rig.candle(1, 5000, 5100, 4980, 5080, 5_000);
    rig.judge(5);
    assert_eq!(rig.stats().armed, 1);
    // Name 0 and the clock instrument have bars; name 1's claim is gone.
    assert!(rig.bars.is_claimed_by(1, 0) || rig.bars.tracked() >= 1);
    assert_eq!(rig.bars.tracked(), 1);
}

#[test]
fn the_filter_is_a_parameter_of_three_values() {
    let p = with(|p| p.bias = 1);
    assert!(p.render().ends_with("bias=1"));
    assert_eq!(OpeningRangeParams::parse(&p.render()).unwrap(), p);
    assert!(OpeningRangeParams::default().render().ends_with("bias=0"));
    let mut bad = p;
    bad.bias = 2;
    assert!(bad.validate().is_ok(), "2 carries the previous close");
    bad.bias = 3;
    assert!(bad.validate().is_err());
}

#[test]
fn a_second_day_follows_the_names_afresh() {
    let mut rig = bias_rig(1);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 3_000, 5090, 10);
    rig.trade(0, 3_660, 5100, 10);
    assert_eq!(rig.out().len(), 1);
    // The next day's first hour has not ended: no bias carried from yesterday.
    let next = day(4);
    rig.day = next;
    rig.tier0.set_day(next);
    rig.tick(-3_600);
    rig.tick(-3_594);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 1_200, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}

#[test]
fn every_trigger_the_bias_refuses_is_counted_once() {
    let mut rig = bias_rig(2);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.candle(1, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    // Both close the first hour under the average, so both longs are against the bias.
    rig.trade(0, 3_000, 4950, 10);
    rig.trade(1, 3_000, 4950, 10);
    rig.trade(0, 3_660, 5100, 10);
    assert_eq!(rig.stats().biased_out, 1);
    rig.trade(1, 3_670, 5100, 10);
    assert_eq!(rig.stats().biased_out, 2);
    assert!(rig.out().is_empty());
}

/// As [`bias_rig`] with the filter at 2 and a previous close of `prev` cents for every name.
fn carry_rig(prev: i64) -> Rig {
    let p = with(|p| {
        p.bias = 2;
        p.entry_minutes = 200;
    });
    Rig::custom(p, 1, 2, day(1), |refs| {
        refs.iter_mut().for_each(|r| {
            put_ema(r);
            r.hist.prev_close = Some(cents(prev));
        });
    })
}

#[test]
fn with_the_previous_close_carried_a_break_in_the_first_hour_goes_with_it() {
    // The previous close $51.00 is over the $50.00 average: a long in the first hour goes.
    let mut rig = carry_rig(5100);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 1_200, 5100, 10);
    assert_eq!(rig.out().len(), 1);
    assert_eq!(rig.stats().biased_out, 0);
    // At $49.00 it is under: the long is against the bias and is spent.
    let mut rig = carry_rig(4900);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 1_200, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}

#[test]
fn a_short_in_the_first_hour_goes_with_a_previous_close_under_the_average() {
    let mut rig = carry_rig(4900);
    rig.candle(0, 5000, 5030, 4900, 4920, 15_000);
    rig.judge(5);
    rig.trade(0, 1_200, 4900, 10);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].side, Side::SellShort);
}

#[test]
fn after_the_first_hour_the_days_close_decides_not_the_previous_one() {
    // Over yesterday, but the first hour closed under the average: a long after 10:30 is against the bias.
    let mut rig = carry_rig(5100);
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 3_000, 4950, 10);
    rig.trade(0, 3_660, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}

#[test]
fn a_name_with_no_previous_close_has_no_bias_in_the_first_hour_even_with_the_carry() {
    let p = with(|p| {
        p.bias = 2;
        p.entry_minutes = 200;
    });
    let mut rig = Rig::custom(p, 1, 2, day(1), |refs| refs.iter_mut().for_each(put_ema));
    rig.candle(0, 5000, 5100, 4980, 5080, 15_000);
    rig.judge(5);
    rig.trade(0, 1_200, 5100, 10);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().biased_out, 1);
}
