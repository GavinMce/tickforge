//! T03, the premarket VWAP reclaim, from scripted events: every decision of the rule has a case here. A day is played in minutes of the
//! premarket (minute 0 is 04:00 New York) with the shared bars fed as the host feeds them: only a trade moves bar time on, so the
//! clock instrument trades one share a second after the minute to close the bar before it.

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade, TradeFlags};
use tf_engine::{MtfConfig, SharedBars, Tier0};
use tf_universe::RefInfo;

use crate::cross::{CrossRunner, Market, Members};
use crate::exits::{REASON_SIGNAL, REASON_STOP, REASON_TARGET, REASON_TIME};
use crate::intent::{Intent, Pricing, Purpose, Side, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::vwap_reclaim::{REASON_ENTRY, VwapReclaim, VwapReclaimParams, VwapReclaimStats};

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
    bars: Option<SharedBars>,
    runner: CrossRunner<VwapReclaim>,
    day: SessionTimes,
    clock: u32,
    closes: Vec<tf_engine::BarClose>,
}

/// Fast tests: the window opens at minute 1, a name is a candidate with $20,000 traded, and the screen runs to minute 60.
fn params() -> VwapReclaimParams {
    VwapReclaimParams {
        start_minutes: 1,
        min_dollars: 20_000,
        screen_by_minutes: 60,
        ..VwapReclaimParams::default()
    }
}

fn p_with(f: impl FnOnce(&mut VwapReclaimParams)) -> VwapReclaimParams {
    let mut p = params();
    f(&mut p);
    p
}

impl Rig {
    /// Names 0..n with a prior close of $10.00, the shared bars on.
    fn new(p: VwapReclaimParams, names: u32) -> Rig {
        Rig::build(p, names, day(1), true)
    }

    fn build(p: VwapReclaimParams, names: u32, day: SessionTimes, with_bars: bool) -> Rig {
        let mut tier0 = Tier0::new(names as usize + 1);
        tier0.set_day(day);
        let mut refs: Vec<RefInfo> = (0..names)
            .map(|_| RefInfo {
                price: Some(10 * D),
                adv_shares: Some(1_000_000),
                ..RefInfo::default()
            })
            .collect();
        refs.push(RefInfo::default());
        let runner = CrossRunner::new(VwapReclaim::new(1, p).unwrap(), Members::from_ids(0..names));
        let mut rig = Rig {
            tier0,
            refs,
            bars: with_bars.then(|| SharedBars::new(MtfConfig::session(), names as usize + 1, 8)),
            runner,
            day,
            clock: names,
            closes: Vec::new(),
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
        if let Some(b) = self.bars.as_mut() {
            self.closes.clear();
            b.on_event(&ev, &mut self.closes);
        }
        self.runner.on_event(
            Market {
                tier0: &self.tier0,
                refs: &self.refs,
            },
            None,
            self.bars.as_mut(),
            &ev,
        );
    }

    /// Time passes: one share of the clock instrument at $1.00, which moves the bars on and changes no member.
    fn tick(&mut self, min: u64, sec: u64) {
        let ts = self.at(min, sec);
        self.feed(Event::Trade(Trade {
            hdr: hdr(self.clock, ts),
            px: Px::from_raw(cents(100)),
            size: 1,
            flags: TradeFlags::NONE,
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

    /// A trade of `id` at `px` cents with a quote a cent either side just before it.
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

    /// A trade at an exact raw price (fractional cents), with a quote a cent either side of it just before.
    fn trade_raw(&mut self, id: u32, min: u64, sec: u64, px: i64, size: u32) {
        let ts = self.at(min, sec);
        self.feed(Event::Quote(Quote {
            hdr: hdr(id, ts),
            bid_px: Px::from_raw(px - cents(1)),
            ask_px: Px::from_raw(px + cents(1)),
            bid_sz: 100,
            ask_sz: 100,
        }));
        self.feed(Event::Trade(Trade {
            hdr: hdr(id, ts + 1),
            px: Px::from_raw(px),
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
        self.runner
            .on_order_update(&self.tier0, None, self.bars.as_mut(), &u);
    }

    fn out(&mut self) -> Vec<Intent> {
        self.runner.drain_intents()
    }

    fn stats(&self) -> VwapReclaimStats {
        self.runner.strategy().stats()
    }

    /// $20,600 of name `id` traded at $10.30 in minute 0, a gap of exactly 300 basis points over the prior close of $10.00, and the
    /// review of the half minute after it makes the name a candidate. The premarket VWAP is $10.30 to the raw unit.
    fn candidate(&mut self, id: u32) {
        self.trade(id, 0, 10, 1030, 1000);
        self.trade(id, 0, 20, 1030, 1000);
        self.tick(0, 30);
    }

    /// Minute `m` of name `id`: these trades (second, price in cents, shares), then the tick that closes its bar.
    fn minute(&mut self, id: u32, m: u64, trades: &[(u64, i64, u32)]) {
        for &(sec, px, size) in trades {
            self.trade(id, m, sec, px, size);
        }
        self.tick(m + 1, 1);
    }
}

/// A one-share pair that leaves a VWAP of $10.30 where it is: 10.19 and 10.41 average to 10.30.
const DIP: [(u64, i64, u32); 2] = [(10, 1019, 1), (20, 1041, 1)];

// ---- the rule ----

#[test]
fn it_buys_the_first_bar_that_closes_above_the_vwap_after_a_dip() {
    let mut rig = Rig::new(params(), 1);
    rig.candidate(0);
    assert_eq!(rig.stats().candidates, 1);
    // Minute 1: a bar around the VWAP, no dip. Minute 2: the dip (a low of 10.19 against a VWAP of 10.30, 1.07% under).
    rig.minute(0, 1, &[(10, 1030, 1)]);
    assert!(rig.out().is_empty() && rig.stats().dips == 0);
    rig.minute(0, 2, &DIP);
    assert_eq!(rig.stats().dips, 1);
    assert!(rig.out().is_empty());
    // Minute 3: a bar that closes at 10.45, above the VWAP: the reclaim, seen when the bar closes.
    rig.minute(0, 3, &[(10, 1031, 1)]);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let i = &out[0];
    assert_eq!(
        (i.instrument, i.side, i.purpose, i.tif, i.reason, i.protect),
        (0, Side::Buy, Purpose::Open, Tif::Day, REASON_ENTRY, None)
    );
    // $1,000 of whole shares at the ask of $10.46, a collar of 1% around it.
    assert_eq!(i64::from(i.qty), 1_000 * D / cents(1032));
    assert_eq!(
        i.pricing,
        Pricing::Collar {
            reference: Px::from_raw(cents(1032)),
            collar_permille: 10
        }
    );
    assert_eq!(i.ts, rig.at(4, 1));
    let s = rig.stats();
    assert_eq!((s.candidates, s.dips, s.entries, s.missed), (1, 1, 1, 0));
    // Once a day: a second dip and reclaim buys nothing more.
    rig.minute(0, 4, &[(10, 1019, 1), (20, 1041, 1)]);
    rig.minute(0, 5, &[(10, 1050, 1)]);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().entries, 1);
}

#[test]
fn a_reclaim_is_a_later_bar_that_closes_strictly_above_the_vwap() {
    // A bar that dips and closes above in the same minute is not a reclaim: it is the dip. (The pair of 10.19 and 10.41 leaves the VWAP at 10.30.)
    let mut rig = Rig::new(params(), 1);
    rig.candidate(0);
    rig.minute(0, 1, &DIP);
    assert_eq!(rig.stats().dips, 1);
    assert!(rig.out().is_empty());
    // A later bar that closes exactly at the VWAP ($10.30: one share at 10.30 leaves it unchanged) is not above it.
    rig.minute(0, 2, &[(10, 1030, 1)]);
    assert!(rig.out().is_empty());
    // One that closes a cent above is.
    rig.minute(0, 3, &[(10, 1031, 1)]);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn a_dip_is_a_low_the_set_basis_points_under_the_vwap_and_no_nearer() {
    // A VWAP of $10.30: 1% under is 10.197. A low of 10.20 is not a dip and 10.19 is.
    let run = |low: i64| {
        let mut rig = Rig::new(params(), 1);
        rig.candidate(0);
        // The pair around the low leaves the VWAP at 10.30 exactly: low and (2 x 10.30 - low).
        rig.minute(0, 1, &[(10, low, 1), (20, 2060 - low, 1)]);
        rig.stats().dips
    };
    assert_eq!(run(1020), 0);
    assert_eq!(run(1019), 1);
    // And with the margin asked for: at 200 basis points 10.09 is a dip and 10.10 is not.
    let run2 = |low: i64| {
        let mut rig = Rig::new(p_with(|p| p.dip_bp = 200), 1);
        rig.candidate(0);
        rig.minute(0, 1, &[(10, low, 1), (20, 2060 - low, 1)]);
        rig.stats().dips
    };
    assert_eq!((run2(1010), run2(1009)), (0, 1));
}

#[test]
fn bars_before_the_window_opens_are_not_looked_at() {
    // The window opens at minute 3: a dip in minute 1 does not count, the same dip in minute 3 does.
    let mut rig = Rig::new(p_with(|p| p.start_minutes = 3), 1);
    rig.candidate(0);
    rig.minute(0, 1, &DIP);
    rig.minute(0, 2, &[(10, 1031, 1)]);
    assert_eq!(rig.stats().dips, 0);
    assert!(rig.out().is_empty());
    rig.minute(0, 3, &DIP);
    assert_eq!(rig.stats().dips, 1);
    rig.minute(0, 4, &[(10, 1031, 1)]);
    assert_eq!(rig.out().len(), 1);
}

// ---- the candidates ----

/// Whether name 0, traded as these say, becomes a candidate, with a prior close of `prior` cents.
fn candidate_with(p: VwapReclaimParams, prior: i64, px: i64, shares: u32) -> bool {
    let mut rig = Rig::new(p, 1);
    rig.refs[0].price = Some(cents(prior));
    rig.trade(0, 0, 10, px, shares);
    rig.tick(0, 30);
    rig.stats().candidates == 1
}

#[test]
fn a_candidate_has_the_dollars_the_gap_and_the_price_exactly() {
    let base = params();
    // $20,600 and a gap of exactly 300 basis points: in.
    assert!(candidate_with(base, 1000, 1030, 2000));
    // One share fewer is $20,589.70: under 20,600 only if the floor is 20,600; the floor is 20,000 here, so test the floor itself.
    let dollars = |d: u32| VwapReclaimParams {
        min_dollars: d,
        ..base
    };
    assert!(candidate_with(dollars(20_600), 1000, 1030, 2000));
    assert!(!candidate_with(dollars(20_601), 1000, 1030, 2000));
    // The gap: 10.30 over 10.00 is 300 basis points; over 10.01 it is 289.
    assert!(!candidate_with(base, 1001, 1030, 2000));
    let gap = |g: u32| VwapReclaimParams { gap_bp: g, ..base };
    assert!(candidate_with(gap(300), 1000, 1030, 2000));
    assert!(!candidate_with(gap(301), 1000, 1030, 2000));
    // A name below its prior close is not a gap up.
    assert!(!candidate_with(base, 1100, 1030, 2000));
    // The price band, in cents: 1030 is in [1030, 1030] and out of [1031, 5000] and [100, 1029].
    let band = |lo: u32, hi: u32| VwapReclaimParams {
        min_cents: lo,
        max_cents: hi,
        ..base
    };
    assert!(candidate_with(band(1030, 1030), 1000, 1030, 2000));
    assert!(!candidate_with(band(1031, 5000), 1000, 1030, 2000));
    assert!(!candidate_with(band(100, 1029), 1000, 1030, 2000));
    // Without a prior close there is no gap.
    let mut rig = Rig::new(base, 1);
    rig.refs[0].price = None;
    rig.trade(0, 0, 10, 1030, 2000);
    rig.tick(0, 30);
    assert_eq!(rig.stats().candidates, 0);
}

#[test]
fn a_name_that_qualifies_after_the_screen_closes_is_not_a_candidate() {
    // The screen runs to minute 5: qualifying in minute 5 is in, in minute 6 is out.
    let at = |m: u64| {
        let mut rig = Rig::new(p_with(|p| p.screen_by_minutes = 5), 1);
        rig.trade(0, m, 10, 1030, 2000);
        rig.tick(m, 30);
        rig.stats().candidates
    };
    assert_eq!((at(5), at(6)), (1, 0));
}

#[test]
fn the_bars_are_claimed_for_the_candidates_only_and_let_go_when_a_name_is_finished() {
    let mut rig = Rig::new(params(), 3);
    // Names 0 and 1 trade enough; name 2 does not.
    for id in [0, 1] {
        rig.trade(id, 0, 10, 1030, 1000);
        rig.trade(id, 0, 20, 1030, 1000);
    }
    rig.trade(2, 0, 10, 1030, 10);
    rig.tick(0, 30);
    assert_eq!(rig.stats().candidates, 2);
    assert_eq!(rig.bars.as_ref().unwrap().tracked(), 2);
    // Name 0 dips and is bought; name 1 never dips and stays watched. A name held keeps its bars for the exit.
    rig.trade(0, 1, 10, DIP[0].1, 1);
    rig.trade(0, 1, 20, DIP[1].1, 1);
    rig.tick(2, 1);
    rig.minute(0, 2, &[(10, 1031, 1)]);
    let entry = rig.out().remove(0);
    rig.update(fill(&entry, entry.qty, 1032, rig.at(3, 2)));
    assert_eq!(rig.bars.as_ref().unwrap().tracked(), 2);
    // Sold before the open and the sale filled: the name is let go, and the watched one still is claimed.
    rig.tick(325, 1);
    let sale = rig.out().remove(0);
    rig.update(fill(&sale, sale.qty, 1032, rig.at(325, 2)));
    rig.tick(326, 1);
    assert_eq!(rig.bars.as_ref().unwrap().tracked(), 1);
}

#[test]
fn a_name_dropped_as_too_late_lets_its_bars_go_at_once() {
    let mut rig = dipped(p_with(|p| p.last_entry_minutes = 330 - 3), 1);
    assert_eq!(rig.bars.as_ref().unwrap().tracked(), 1);
    rig.minute(0, 2, &[(10, 1031, 1)]);
    assert_eq!(rig.stats().missed, 1);
    assert_eq!(rig.bars.as_ref().unwrap().tracked(), 0);
}

#[test]
fn without_shared_bars_in_the_host_a_candidate_is_refused_and_counted_and_nothing_is_bought() {
    let mut rig = Rig::build(params(), 1, day(1), false);
    rig.candidate(0);
    let s = rig.stats();
    assert_eq!((s.candidates, s.refused_bars), (0, 1));
    rig.minute(0, 1, &DIP);
    rig.minute(0, 2, &[(10, 1031, 1)]);
    assert!(rig.out().is_empty());
}

#[test]
fn candidates_past_the_most_are_refused_and_counted() {
    let mut rig = Rig::new(p_with(|p| p.max_candidates = 1), 2);
    for id in [0, 1] {
        rig.trade(id, 0, 10, 1030, 2000);
    }
    rig.tick(0, 30);
    let s = rig.stats();
    assert_eq!((s.candidates, s.refused_cap), (1, 1));
}

// ---- the entry ----

fn dipped(p: VwapReclaimParams, names: u32) -> Rig {
    let mut rig = Rig::new(p, names);
    for id in 0..names {
        rig.trade(id, 0, 10, 1030, 1000);
        rig.trade(id, 0, 20, 1030, 1000);
    }
    rig.tick(0, 30);
    for id in 0..names {
        rig.trade(id, 1, 10, DIP[0].1, 1);
        rig.trade(id, 1, 20, DIP[1].1, 1);
    }
    rig.tick(2, 1);
    rig
}

#[test]
fn a_reclaim_after_the_last_entry_time_is_missed() {
    // The last entry is 310 minutes before the open: minute 20. The reclaim is seen at minute 4, and again with the limit at minute 3.
    let mut rig = dipped(p_with(|p| p.last_entry_minutes = 330 - 3), 1);
    rig.minute(0, 2, &[(10, 1031, 1)]);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (0, 1));
    // At the limit itself it is allowed: the review that sees the bar is at minute 3 second 1, and the last entry is minute 3 exactly.
    let mut rig = dipped(p_with(|p| p.last_entry_minutes = 330 - 3), 1);
    rig.trade(0, 2, 10, 1031, 1);
    rig.tick(3, 0);
    // (the tick is the first event of minute 3: its bar closes and the review runs at exactly the last entry time)
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn only_as_many_positions_as_names_and_the_rest_are_missed() {
    let mut rig = dipped(p_with(|p| p.names = 1), 2);
    for id in [0, 1] {
        rig.trade(id, 2, 10, 1031, 1);
    }
    rig.tick(3, 1);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (1, 1));
}

#[test]
fn a_wide_quote_or_a_halt_waits_for_the_next_bar() {
    let mut rig = dipped(params(), 1);
    // A reclaim bar with a spread of 5%: not bought, and not dropped.
    rig.trade(0, 2, 10, 1031, 1);
    rig.quote(0, 2, 12, 1020, 1070);
    rig.tick(3, 1);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (0, 0));
    // A halt: the same.
    rig.trade(0, 3, 10, 1032, 1);
    rig.halt(0, 3, 12);
    rig.tick(4, 1);
    assert!(rig.out().is_empty());
    assert_eq!(rig.stats().missed, 0);
    // A fair quote and the halt over: the next bar buys.
    rig.feed(Event::Status(Status {
        hdr: hdr(0, rig.at(4, 5)),
        kind: StatusKind::TradingResume,
        lo: Px::from_raw(0),
        hi: Px::from_raw(0),
    }));
    rig.trade(0, 4, 10, 1033, 1);
    rig.tick(5, 1);
    assert_eq!(rig.out().len(), 1);
}

#[test]
fn the_spread_cap_is_met_exactly_and_nothing_means_no_cap() {
    let at = |cap: u32| {
        let mut rig = dipped(p_with(|p| p.spread_cap_bp = cap), 1);
        rig.trade(0, 2, 10, 1031, 1);
        // 10 cents on a $10.00 mid is 100 basis points.
        rig.quote(0, 2, 12, 995, 1005);
        rig.tick(3, 1);
        rig.out().len()
    };
    assert_eq!((at(100), at(99), at(0)), (1, 0, 1));
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

/// A position bought at $10.32 after the dip and the reclaim, filled at that price.
fn held() -> (Rig, Intent) {
    let mut rig = dipped(params(), 1);
    rig.minute(0, 2, &[(10, 1031, 1)]);
    let entry = rig.out().remove(0);
    rig.update(fill(&entry, entry.qty, 1032, rig.at(3, 2)));
    (rig, entry)
}

#[test]
fn the_target_is_the_vwap_at_the_entry_plus_the_margin() {
    // The VWAP was 10.30; 1% over is 10.403: a trade at 10.40 is under it and 10.41 is at or through it.
    let (mut rig, entry) = held();
    rig.trade(0, 3, 10, 1040, 1);
    assert!(rig.out().is_empty());
    rig.trade(0, 3, 20, 1041, 1);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].side, out[0].purpose, out[0].qty, out[0].reason),
        (Side::Sell, Purpose::Close, entry.qty, REASON_TARGET)
    );
}

#[test]
fn a_bar_closing_under_the_vwap_by_the_margin_exits_and_a_price_touching_it_does_not() {
    let (mut rig, entry) = held();
    // The VWAP is about 10.30; 0.5% under is 10.2485. A low of 10.20 that closes back at 10.25 is not an exit... but the target is
    // over 10.40 and no trade there. Trades at 10.30 keep the VWAP.
    rig.minute(0, 3, &[(10, 1025, 1), (20, 1030, 1)]);
    assert!(rig.out().is_empty());
    // A bar that closes at 10.24: under 10.2485.
    rig.minute(0, 4, &[(10, 1024, 1)]);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].side, out[0].qty, out[0].reason),
        (Side::Sell, entry.qty, REASON_SIGNAL)
    );
    assert_eq!(rig.stats().exits.signal_exits, 1);
}

#[test]
fn the_disaster_stop_is_under_the_fill() {
    // 3% under $10.32 is $10.0104: a trade at 10.01 sells; the signal exit is not what sells it here (no bar has closed).
    let (mut rig, entry) = held();
    rig.trade(0, 3, 10, 1002, 1);
    assert!(rig.out().is_empty());
    rig.trade(0, 3, 20, 1001, 1);
    let out = rig.out();
    assert_eq!(out.len(), 1);
    assert_eq!((out[0].qty, out[0].reason), (entry.qty, REASON_STOP));
}

#[test]
fn a_position_is_sold_before_the_open() {
    let (mut rig, entry) = held();
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
    let p = VwapReclaimParams::default();
    assert_eq!(VwapReclaimParams::parse(&p.render()).unwrap(), p);
    assert_eq!(p.render().split_whitespace().count(), 17);
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
        VwapReclaimParams::parse(&text)
    };
    for (k, v) in [
        ("names", 9),
        ("gap_bp", 100_000),
        ("screen_by_minutes", 329),
        ("start_minutes", 0),
        ("start_minutes", 319),
        ("dip_bp", 5_000),
        ("target_bp", 10_000),
        ("exit_below_bp", 5_000),
        ("collar_permille", 999),
        ("stop_permille", 999),
        ("last_entry_minutes", 330 - 180 - 1),
        ("max_candidates", 10_000),
        ("spread_cap_bp", 0),
    ] {
        assert!(one(k, v).is_ok(), "{k}={v}");
    }
    for (k, v) in [
        ("names", 0),
        ("dollars", 0),
        ("min_dollars", 0),
        ("gap_bp", 0),
        ("gap_bp", 100_001),
        ("screen_by_minutes", 0),
        ("screen_by_minutes", 330),
        ("start_minutes", 320),
        ("dip_bp", 0),
        ("dip_bp", 5_001),
        ("target_bp", 0),
        ("target_bp", 10_001),
        ("exit_below_bp", 0),
        ("exit_below_bp", 5_001),
        ("collar_permille", 1000),
        ("stop_permille", 0),
        ("stop_permille", 1000),
        ("flat_minutes", 0),
        ("last_entry_minutes", 5),
        ("last_entry_minutes", 331),
        ("min_cents", 0),
        ("max_cents", 99),
        ("max_candidates", 0),
        ("max_candidates", 10_001),
    ] {
        assert!(one(k, v).is_err(), "{k}={v}");
    }
    let text = p.render();
    assert!(VwapReclaimParams::parse(&format!("{text} wat=1")).is_err());
    assert!(VwapReclaimParams::parse(&format!("{text} names=2")).is_err());
    assert!(VwapReclaimParams::parse(&text.replace("names=3 ", "")).is_err());
    assert!(VwapReclaimParams::parse(&text.replace("names=3", "names=x")).is_err());
    assert!(VwapReclaimParams::parse(&text.replace("names=3", "names")).is_err());
    assert!(VwapReclaim::new(1, VwapReclaimParams { names: 0, ..p }).is_err());
}

#[test]
fn recording_why_changes_nothing_it_decides_and_says_what_it_did() {
    let run = |tracing: bool| {
        let mut rig = Rig::build(params(), 1, day(1), true);
        rig.runner.set_tracing(tracing);
        rig.candidate(0);
        rig.minute(0, 1, &DIP);
        rig.minute(0, 2, &[(10, 1031, 1)]);
        (rig.out(), rig.runner.drain_traces())
    };
    let (a, ta) = run(false);
    let (b, tb) = run(true);
    assert_eq!(a, b);
    assert!(ta.is_empty());
    let kinds: Vec<&str> = tb.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, ["candidate", "dip", "entry"]);
    assert_eq!(tb[0].value("gap_bp"), Some("300"));
}

#[test]
fn a_new_day_forgets_the_last_one() {
    let mut rig = Rig::new(params(), 1);
    rig.candidate(0);
    assert_eq!(rig.stats().candidates, 1);
    let next = day(4);
    rig.tier0 = Tier0::new(2);
    rig.tier0.set_day(next);
    rig.day = next;
    rig.tick(0, 0);
    rig.tick(0, 6);
    rig.candidate(0);
    assert_eq!(
        rig.stats().candidates,
        2,
        "a candidate again on the new day"
    );
}

// ---- the edges ----

#[test]
fn a_dip_is_exactly_the_margin_under_the_vwap_down_to_the_raw_unit() {
    // A VWAP of exactly 10.30 and 100 basis points: 10.197 exactly is a dip, a raw unit above it is not. The partner share makes the pair average 10.30.
    let run = |low: i64| {
        let mut rig = Rig::new(params(), 1);
        rig.candidate(0);
        rig.trade_raw(0, 1, 10, low, 1);
        rig.trade_raw(0, 1, 20, 2 * 10_300_000_000 - low, 1);
        rig.tick(2, 1);
        rig.stats().dips
    };
    assert_eq!(run(10_197_000_000), 1);
    assert_eq!(run(10_197_000_001), 0);
}

#[test]
fn a_held_position_counts_against_the_names_as_a_pending_entry_does() {
    let mut rig = dipped(p_with(|p| p.names = 1), 2);
    rig.trade(0, 2, 10, 1031, 1);
    rig.tick(3, 1);
    let entry = rig.out().remove(0);
    // It fills: the entry is over and the position is the exit book's. A second name's reclaim finds no room.
    rig.update(fill(&entry, entry.qty, 1032, rig.at(3, 2)));
    rig.trade(1, 3, 10, 1031, 1);
    rig.tick(4, 1);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (1, 1));
}

#[test]
fn only_a_fair_quote_is_bought() {
    // (bid, ask, spread cap in basis points) -> whether the reclaim is bought at that quote.
    let at = |bid: i64, ask: i64, cap: u32| {
        let mut rig = dipped(p_with(|p| p.spread_cap_bp = cap), 1);
        rig.trade(0, 2, 10, 1031, 1);
        rig.quote(0, 2, 12, bid, ask);
        rig.tick(3, 1);
        rig.out().len() == 1
    };
    assert!(at(1031, 1031, 0), "locked");
    assert!(!at(1032, 1031, 0), "crossed");
    assert!(!at(0, 1031, 0), "no bid");
}

#[test]
fn a_dollar_amount_that_buys_no_share_is_not_an_entry_and_not_a_refused_one() {
    let mut rig = dipped(p_with(|p| p.dollars = 5), 1);
    rig.trade(0, 2, 10, 1031, 1);
    rig.tick(3, 1);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.entries_refused), (0, 0));
}

#[test]
fn the_exit_bar_closes_under_the_vwap_by_exactly_the_margin_and_not_at_it() {
    // A VWAP of 10.30 exactly; 50 basis points under is 10.2485. A bar that closes there is not under it; a raw unit lower is.
    let run = |close: i64| {
        let mut rig = dipped(params(), 1);
        // The reclaim bar closes at 10.31 with a pair that keeps the VWAP at 10.30.
        rig.trade_raw(0, 2, 10, 10_290_000_000, 1);
        rig.trade_raw(0, 2, 20, 10_310_000_000, 1);
        rig.tick(3, 1);
        let entry = rig.out().remove(0);
        rig.update(fill(&entry, entry.qty, 1032, rig.at(3, 2)));
        // The exit bar: a pair whose last price is the close, around a VWAP that stays 10.30.
        rig.trade_raw(0, 3, 10, 2 * 10_300_000_000 - close, 1);
        rig.trade_raw(0, 3, 20, close, 1);
        rig.tick(4, 1);
        rig.out().len()
    };
    assert_eq!(run(10_248_500_000), 0);
    assert_eq!(run(10_248_499_999), 1);
}

#[test]
fn nothing_is_decided_after_the_open() {
    // A dipped name whose reclaim bar closes in the regular session is not bought and not counted missed: the premarket is over.
    let mut rig = dipped(params(), 1);
    rig.trade(0, 331, 10, 1031, 1);
    rig.tick(332, 1);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (0, 0));
}

#[test]
fn a_reclaim_that_has_already_run_past_the_target_is_dropped_and_one_just_inside_it_is_bought() {
    // A bar that closes at 10.45 on a VWAP of 10.30 has an ask of 10.46, over the target of 10.403: the exit would fire on the first trade,
    // below the entry. Not bought, counted missed, and the name's bars are let go.
    let mut rig = dipped(params(), 1);
    rig.minute(0, 2, &[(10, 1045, 1)]);
    assert!(rig.out().is_empty());
    let s = rig.stats();
    assert_eq!((s.entries, s.missed), (0, 1));
    assert_eq!(rig.bars.as_ref().unwrap().tracked(), 0);
    // To the raw unit, with a VWAP of exactly 10.30 (a pair around the close): an ask of exactly the target is out, a raw unit under is in.
    let ask = |close: i64| {
        let mut rig = dipped(params(), 1);
        rig.trade_raw(0, 2, 10, 2 * 10_300_000_000 - close, 1);
        rig.trade_raw(0, 2, 20, close, 1);
        rig.tick(3, 1);
        rig.out().len()
    };
    // The quote is a cent either side of the last trade: an ask of close + 0.01. The target is 10.403.
    assert_eq!(ask(10_393_000_000), 0, "ask 10.403 is the target");
    assert_eq!(ask(10_392_999_999), 1, "a raw unit under it");
}
