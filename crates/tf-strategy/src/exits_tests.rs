use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::cross::{CrossRunner, CrossStrategy, Market, MemberView, Members};
use crate::exits::{
    ExitBook, ExitPlan, ExitReason, ExitStats, REASON_STOP, REASON_TARGET, REASON_TIME, flat_by,
};
use crate::intent::{Intent, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderState, OrderUpdate};
use crate::sim::{SimBroker, SimConfig};
use crate::strategy::{Ctx, Request, TimerId};
use crate::testing::T0;

const SEC: Nanos = 1_000_000_000;
/// 08:00 New York time on 2 October 2026: the premarket.
const PRE: Nanos = T0 - 3 * 3600 * SEC;

fn px(c: i64) -> Px {
    Px::from_cents(c)
}

fn hdr(sec: u64) -> Header {
    Header {
        ts_event: PRE + sec * SEC,
        ts_recv: PRE + sec * SEC,
        seq: sec,
        instrument: 0,
        provider: ProviderId::Synthetic,
    }
}

fn quote(sec: u64, bid: i64, ask: i64, sz: u32) -> Event {
    Event::Quote(Quote {
        hdr: hdr(sec),
        bid_px: px(bid),
        ask_px: px(ask),
        bid_sz: sz,
        ask_sz: sz,
    })
}

fn trade(sec: u64, c: i64) -> Event {
    Event::Trade(Trade {
        hdr: hdr(sec),
        px: px(c),
        size: 10,
        flags: TradeFlags::NONE,
    })
}

/// Opens at its first review (no protective orders: it is the premarket) and holds its exits from then on.
struct Holder {
    long: bool,
    plan: ExitPlan,
    qty: u32,
    book: ExitBook,
    entered: Option<crate::IntentId>,
    exits: Vec<ExitReason>,
    timers: Vec<ExitReason>,
    extra: Extra,
    /// For [`Extra::Raise`]: the stops to try to raise to, last first, and what came of each.
    raises: Vec<i64>,
    raised: Vec<bool>,
    /// For [`Extra::Signal`]: what each try to exit on a signal came to.
    signalled: Vec<bool>,
}

/// What else a test makes the strategy do with its book.
#[derive(Clone, Copy, PartialEq)]
enum Extra {
    Nothing,
    /// Arm zero shares of another instrument at the first review.
    ArmZero,
    /// Arm 50 more shares when the entry fills.
    ArmTwice,
    /// Set the book's own timer for the instrument itself when the entry fills, though the plan has no time exit.
    StrayTimer,
    /// At each review while the position is held, try to raise the stop to the next of `raises`.
    Raise,
    /// At each review while the position is held, exit on a signal at the last price.
    Signal,
}

impl Holder {
    fn new(long: bool, plan: ExitPlan) -> Holder {
        Holder {
            long,
            plan,
            qty: 100,
            book: ExitBook::new(1000),
            entered: None,
            exits: Vec::new(),
            timers: Vec::new(),
            extra: Extra::Nothing,
            raises: Vec::new(),
            raised: Vec::new(),
            signalled: Vec::new(),
        }
    }

    fn with(mut self, extra: Extra) -> Holder {
        self.extra = extra;
        self
    }
}

impl CrossStrategy for Holder {
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        StrategyId(1)
    }
    fn period(&self) -> Nanos {
        SEC
    }
    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        if self.extra == Extra::ArmZero {
            self.book.arm(ctx, 5, true, 0, self.plan);
        }
        if self.extra == Extra::Signal && self.book.is_held(0) {
            if let Some(last) = view.state(0).and_then(|s| s.last_px) {
                // Twice in one review: the second finds the first still out.
                self.signalled.push(self.book.exit_now(ctx, 0, last));
                self.signalled.push(self.book.exit_now(ctx, 0, last));
            }
        }
        if self.extra == Extra::Raise && self.book.is_held(0) {
            if let Some(c) = self.raises.pop() {
                self.raised.push(self.book.raise_stop(0, px(c)));
            }
        }
        if self.entered.is_some() {
            return;
        }
        let Some(last) = view.state(0).and_then(|s| s.last_px) else {
            return;
        };
        let req = Request {
            side: if self.long {
                Side::Buy
            } else {
                Side::SellShort
            },
            qty: self.qty,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(if self.long {
                Px::from_raw(last.raw() + 50_000_000)
            } else {
                Px::from_raw(last.raw() - 50_000_000)
            }),
            protect: None,
            tif: Tif::Day,
            reason: 1,
        };
        self.entered = Some(
            ctx.submit(0, req)
                .expect("an open needs no stop in the premarket"),
        );
    }
    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, ev: &Event) {
        if let Event::Trade(t) = ev {
            if let Some(r) = self.book.on_trade(ctx, t.hdr.instrument, t.px) {
                self.exits.push(r);
            }
        }
    }
    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, timer: TimerId) {
        if let Some((_, r)) = self.book.on_timer(ctx, timer) {
            self.timers.push(r);
        }
    }
    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, u: &OrderUpdate) {
        if self.book.on_order_update(ctx, u) {
            return;
        }
        if Some(u.intent) == self.entered && u.state == OrderState::Filled {
            self.book.arm(ctx, 0, self.long, u.filled_qty, self.plan);
            if self.extra == Extra::ArmTwice {
                self.book.arm(ctx, 0, self.long, 50, self.plan);
            }
            if self.extra == Extra::StrayTimer {
                ctx.set_timer_in(TimerId(1000), 2 * SEC);
            }
        }
    }
}

/// A strategy, a simulated broker and one instrument, wired as the host wires them.
struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    sim: SimBroker,
    runner: CrossRunner<Holder>,
    intents: Vec<Intent>,
}

impl Rig {
    fn new(h: Holder) -> Rig {
        Rig {
            tier0: Tier0::new(1),
            refs: vec![RefInfo::default()],
            sim: SimBroker::new(
                SimConfig {
                    latency_ns: 0,
                    borrow_bps_per_year: 0,
                },
                1,
            ),
            runner: CrossRunner::new(h, Members::from_ids([0])),
            intents: Vec::new(),
        }
    }

    fn feed(&mut self, ev: &Event) {
        self.sim.on_event(ev);
        for u in self.sim.drain_updates() {
            self.runner.on_order_update(&self.tier0, None, None, &u);
            self.drain();
        }
        self.tier0.on_event(ev);
        let m = Market {
            tier0: &self.tier0,
            refs: &self.refs,
        };
        self.runner.on_event(m, None, None, ev);
        self.drain();
    }

    fn drain(&mut self) {
        for i in self.runner.drain_intents() {
            self.sim.submit(&i);
            self.intents.push(i);
        }
    }

    fn h(&self) -> &Holder {
        self.runner.strategy()
    }

    fn closes(&self) -> Vec<&Intent> {
        self.intents
            .iter()
            .filter(|i| i.purpose == Purpose::Close)
            .collect()
    }
}

/// The premarket: a quote, a trade so the strategy has a price, a review (one second later) that opens, and the quote
/// that fills it.
fn enter(r: &mut Rig, long: bool) {
    r.feed(&quote(1, 999, 1000, 500));
    r.feed(&trade(1, 1000));
    r.feed(&quote(2, 999, 1000, 500)); // the first event of the next second: the review runs and opens
    r.feed(&quote(3, 999, 1000, 500)); // the order is at the venue: it fills against this quote
    assert_eq!(r.sim.position(0), if long { 100 } else { -100 });
}

#[test]
fn a_stop_the_strategy_holds_closes_a_long_in_the_premarket_with_a_limit_order() {
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            target: Some(px(1100)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    // The open went out with no protective orders, as the premarket needs.
    let open = r.intents[0];
    assert_eq!((open.purpose, open.protect), (Purpose::Open, None));
    assert!(r.h().book.is_held(0));
    assert_eq!(r.h().book.left(0), 100);
    // A trade above the stop does nothing; the market falls.
    r.feed(&trade(4, 960));
    assert!(r.closes().is_empty());
    r.feed(&quote(5, 940, 941, 500));
    r.feed(&trade(5, 945));
    // The trade through the stop sent one closing order: a sell, 100 shares, a collar of 0.5% below the 9.45 print.
    let c = r.closes();
    assert_eq!(c.len(), 1);
    assert_eq!(
        (c[0].side, c[0].qty, c[0].reason, c[0].tif, c[0].protect),
        (Side::Sell, 100, REASON_STOP, Tif::Day, None)
    );
    let Pricing::Collar {
        reference,
        collar_permille,
    } = c[0].pricing
    else {
        panic!("a collar")
    };
    assert_eq!((reference, collar_permille), (px(945), 5));
    assert_eq!(
        c[0].limit_price(),
        Px::from_raw(945 * 10_000_000 * 995 / 1000 + 1).min(c[0].limit_price())
    );
    // It filled against the bid, 9.40 (inside the collar's 9.40275: no, below it)... the position is what the book says.
    r.feed(&quote(6, 941, 942, 500));
    assert_eq!(r.sim.position(0), 0);
    assert!(!r.h().book.is_held(0), "a closed position is forgotten");
    assert_eq!(r.h().exits, [ExitReason::Stop]);
    assert_eq!(
        r.h().book.stats(),
        ExitStats {
            stops: 1,
            closed: 1,
            ..ExitStats::default()
        }
    );
}

#[test]
fn a_target_closes_a_long_and_a_short_exits_the_other_way() {
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            target: Some(px(1100)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    r.feed(&quote(4, 1101, 1102, 500));
    r.feed(&trade(4, 1100));
    assert_eq!(r.closes().len(), 1);
    assert_eq!(
        (r.closes()[0].side, r.closes()[0].reason),
        (Side::Sell, REASON_TARGET)
    );
    r.feed(&quote(5, 1101, 1102, 500));
    assert_eq!(r.sim.position(0), 0);

    // A short: the stop is above, it is hit by a trade at or above, and the exit buys.
    let mut r = Rig::new(Holder::new(
        false,
        ExitPlan {
            stop: Some(px(1050)),
            target: Some(px(900)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, false);
    r.feed(&trade(4, 1049));
    assert!(r.closes().is_empty(), "one tick under the stop");
    r.feed(&quote(5, 1050, 1051, 500));
    r.feed(&trade(5, 1050));
    let c = r.closes();
    assert_eq!(
        (c.len(), c[0].side, c[0].reason),
        (1, Side::Buy, REASON_STOP)
    );
    r.feed(&quote(6, 1050, 1051, 500));
    assert_eq!(r.sim.position(0), 0);

    // A gap past the collar does not fill: the order rests, the position stays, and the book still holds it.
    let mut r = Rig::new(Holder::new(
        false,
        ExitPlan {
            stop: Some(px(1050)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, false);
    r.feed(&quote(5, 1059, 1060, 500));
    r.feed(&trade(5, 1050));
    r.feed(&quote(6, 1059, 1060, 500));
    assert_eq!(r.closes().len(), 1);
    assert_eq!(
        r.sim.position(0),
        -100,
        "10.60 is beyond the collar of 10.5525"
    );
    assert_eq!(r.h().book.left(0), 100);
    // When the market comes back inside the collar the resting order fills.
    r.feed(&quote(7, 1054, 1055, 500));
    assert_eq!(r.sim.position(0), 0);
    assert!(r.h().book.is_empty());
}

#[test]
fn only_one_exit_is_out_at_a_time_and_a_partial_fill_leaves_the_rest_to_finish() {
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    // The bid shows only 40 shares: the exit fills 40 and rests for the other 60.
    r.feed(&quote(4, 944, 945, 40));
    r.feed(&trade(4, 945));
    // The exit reaches the venue with the next event and takes the 40 shown; nothing else is shown yet.
    r.feed(&trade(5, 945));
    assert_eq!(r.sim.position(0), 60);
    // More prints at the stop while the exit is out send nothing more.
    for s in 6..10 {
        r.feed(&trade(s, 944));
    }
    assert_eq!(r.closes().len(), 1);
    assert_eq!(r.h().book.left(0), 60, "40 are closed");
    // The market shows size: the rest fills and the position is forgotten.
    r.feed(&quote(10, 944, 945, 500));
    assert_eq!(r.sim.position(0), 0);
    assert!(r.h().book.is_empty());
}

#[test]
fn an_exit_that_ends_unfilled_is_tried_again_after_the_wait() {
    // Immediate-or-cancel exits are not for the premarket; the point here is only the retry, so the test runs the
    // same story in the regular session's hours with the intent decided later: an order that expires unfilled.
    let plan = ExitPlan {
        stop: Some(px(950)),
        tif: Tif::Ioc,
        retry_after: 3 * SEC,
        ..ExitPlan::new()
    };
    let mut r = Rig::new(Holder::new(true, plan));
    enter(&mut r, true);
    // The premarket refuses an immediate-or-cancel order: the sim rejects it at once, and the book keeps the position.
    r.feed(&quote(4, 940, 941, 500));
    r.feed(&trade(4, 945));
    assert_eq!(r.closes().len(), 1);
    assert_eq!(
        r.sim.position(0),
        100,
        "refused in the premarket: still held"
    );
    assert_eq!(r.h().book.left(0), 100);
    // Within the wait nothing is sent again; after it, the next print at the stop tries once more.
    r.feed(&trade(5, 944));
    r.feed(&trade(6, 944));
    assert_eq!(r.closes().len(), 1, "waiting");
    r.feed(&trade(7, 944));
    assert_eq!(r.closes().len(), 2, "tried again after 3 s");
    assert_eq!(r.h().book.stats().stops, 2);
}

#[test]
fn a_time_exit_fires_from_its_timer_at_the_last_price() {
    let at = PRE + 10 * SEC;
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            flat_by: Some(at),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    r.feed(&quote(5, 1004, 1005, 500));
    r.feed(&trade(5, 1004));
    assert!(r.closes().is_empty());
    // The first event at or after the instant fires the timer, with time at that instant.
    r.feed(&quote(11, 1004, 1005, 500));
    let c = r.closes();
    assert_eq!(c.len(), 1);
    assert_eq!(
        (c[0].reason, c[0].side, c[0].ts),
        (REASON_TIME, Side::Sell, at)
    );
    let Pricing::Collar { reference, .. } = c[0].pricing else {
        panic!()
    };
    assert_eq!(reference, px(1004), "the last trade");
    r.feed(&quote(12, 1003, 1004, 500));
    assert_eq!(r.sim.position(0), 0);
    assert_eq!(r.h().timers, [ExitReason::Time]);
    assert_eq!(r.h().book.stats().time_exits, 1);
}

#[test]
fn the_flat_by_instant_comes_from_the_calendar_including_an_early_close() {
    let cal = tf_calendar::Calendar::us_equities();
    let times = |y, m, d| {
        cal.times(tf_calendar::Date::new(y, m, d).unwrap())
            .unwrap()
            .unwrap()
    };
    // Friday 2 October 2026: closes at 16:00 (20:00 UTC); five minutes before is 15:55.
    let t = times(2026, 10, 2);
    assert_eq!(flat_by(t.open + SEC, 5), Some(t.close - 5 * 60 * SEC));
    assert_eq!(
        flat_by(t.premarket, 30),
        Some(t.close - 30 * 60 * SEC),
        "from any time of that New York day"
    );
    // The Friday after Thanksgiving closes at 13:00.
    let e = times(2026, 11, 27);
    assert_eq!(flat_by(e.open, 5), Some(e.close - 300 * SEC));
    assert!(e.close < t.close - 3 * 3600 * SEC + (e.open - t.open) + 24 * 3600 * SEC);
    assert_eq!(
        flat_by(e.open, 5).map(|x| (x - e.open) / SEC),
        Some(3 * 3600 + 25 * 60),
        "12:55 is 3 h 25 min in"
    );
    // A Saturday, and a year the table does not cover, have no close to be flat before.
    let saturday =
        (tf_calendar::Date::new(2026, 10, 3).unwrap().days() as u64 * 86_400 + 15 * 3600) * SEC;
    assert_eq!(flat_by(saturday, 5), None);
    let far =
        (tf_calendar::Date::new(2031, 1, 6).unwrap().days() as u64 * 86_400 + 15 * 3600) * SEC;
    assert_eq!(flat_by(far, 5), None);
}

#[test]
fn an_open_needs_its_protective_orders_except_in_the_extended_hours() {
    let mut open = Intent {
        id: crate::IntentId {
            strategy: StrategyId(1),
            seq: 1,
        },
        instrument: 0,
        side: Side::Buy,
        qty: 10,
        purpose: Purpose::Open,
        pricing: Pricing::Limit(px(1000)),
        protect: None,
        tif: Tif::Day,
        ts: T0, // 11:00 New York
        reason: 1,
    };
    assert_eq!(open.validate(), Err(crate::IntentError::MissingProtection));
    open.ts = PRE; // 08:00
    assert_eq!(open.validate(), Ok(()));
    open.ts = T0 + 6 * 3600 * SEC; // 17:00
    assert_eq!(open.validate(), Ok(()));
    open.ts = 0; // a time the calendar cannot place is not the extended hours
    assert_eq!(open.validate(), Err(crate::IntentError::MissingProtection));
}

#[test]
fn a_print_exactly_at_the_stop_or_the_target_triggers_it_for_both_sides() {
    // A long: a print exactly at the stop; a short: a print exactly at the target.
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    r.feed(&trade(4, 951));
    assert!(r.closes().is_empty());
    r.feed(&quote(5, 949, 951, 500));
    r.feed(&trade(5, 950));
    assert_eq!(r.closes().len(), 1);
    assert_eq!(r.closes()[0].reason, REASON_STOP);

    let mut r = Rig::new(Holder::new(
        false,
        ExitPlan {
            target: Some(px(900)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, false);
    r.feed(&trade(4, 901));
    assert!(r.closes().is_empty());
    r.feed(&quote(5, 899, 900, 500));
    r.feed(&trade(5, 900));
    assert_eq!(r.closes().len(), 1);
    assert_eq!(
        (r.closes()[0].side, r.closes()[0].reason),
        (Side::Buy, REASON_TARGET)
    );
}

#[test]
fn a_time_exit_that_finds_an_exit_already_out_waits_and_does_not_send_a_second() {
    let at = PRE + 8 * SEC;
    let mut r = Rig::new(Holder::new(
        false,
        ExitPlan {
            stop: Some(px(1050)),
            flat_by: Some(at),
            retry_after: 2 * SEC,
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, false);
    // The stop sends an exit that cannot fill (the ask is past its collar) and rests.
    r.feed(&quote(5, 1059, 1060, 500));
    r.feed(&trade(5, 1050));
    r.feed(&quote(6, 1059, 1060, 500));
    assert_eq!(r.closes().len(), 1);
    // The clock reaches the time exit: an exit is out, so nothing more is sent; the timer is set again.
    r.feed(&quote(9, 1059, 1060, 500));
    assert_eq!(r.closes().len(), 1, "no second exit while one is out");
    assert!(r.h().timers.is_empty());
    // When the market comes back, the resting exit fills and the book is done; the later timer finds nothing.
    r.feed(&quote(10, 1050, 1051, 500));
    assert_eq!(r.sim.position(0), 0);
    r.feed(&quote(14, 1050, 1051, 500));
    assert_eq!(r.closes().len(), 1);
    assert!(r.h().book.is_empty());
}

#[test]
fn a_cumulative_fill_count_is_applied_as_the_difference_and_an_exit_that_ends_short_leaves_the_rest()
 {
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    r.feed(&quote(4, 944, 945, 40));
    r.feed(&trade(4, 945));
    r.feed(&trade(5, 945));
    assert_eq!((r.sim.position(0), r.h().book.left(0)), (60, 60));
    // The day ends with the exit still resting: it expires, an update that says 40 filled again. The 40 are not
    // closed a second time, and the book is free to try again for the 60.
    r.sim.end_of_day(PRE + 6 * SEC);
    for u in r.sim.drain_updates() {
        r.runner.on_order_update(&r.tier0, None, None, &u);
    }
    assert_eq!(r.h().book.left(0), 60);
    assert!(r.h().book.is_held(0));
}

#[test]
fn arming_adds_to_a_held_position_arming_nothing_holds_nothing_and_a_foreign_timer_exits_nothing() {
    // Arming 100 and then 50 more holds 150.
    let mut r = Rig::new(
        Holder::new(
            true,
            ExitPlan {
                stop: Some(px(950)),
                ..ExitPlan::new()
            },
        )
        .with(Extra::ArmTwice),
    );
    enter(&mut r, true);
    assert_eq!(r.h().book.left(0), 150);
    r.feed(&quote(4, 944, 945, 500));
    r.feed(&trade(4, 945));
    assert_eq!(r.closes()[0].qty, 150, "the exit is for all that is held");
    // Arming no shares holds nothing.
    let mut r = Rig::new(Holder::new(true, ExitPlan::new()).with(Extra::ArmZero));
    enter(&mut r, true);
    assert!(!r.h().book.is_held(5) && r.h().book.len() == 1);
    // A timer in the book's range that is not a time exit (the plan has none) sends nothing when it fires.
    let mut r = Rig::new(
        Holder::new(
            true,
            ExitPlan {
                stop: Some(px(950)),
                ..ExitPlan::new()
            },
        )
        .with(Extra::StrayTimer),
    );
    enter(&mut r, true);
    r.feed(&trade(4, 990));
    r.feed(&quote(8, 990, 991, 500));
    assert!(r.closes().is_empty(), "no time exit was planned");
    assert_eq!(r.h().timers, []);
}

#[test]
fn a_longs_stop_can_be_raised_and_never_lowered_and_a_shorts_is_not_moved() {
    // A long with a stop at 9.50: trying 9.40 (lower) is refused, 9.60 is taken, 9.60 again (not above) is refused.
    let mut h = Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            ..ExitPlan::new()
        },
    )
    .with(Extra::Raise);
    h.raises = vec![960, 960, 940];
    let mut r = Rig::new(h);
    enter(&mut r, true);
    r.feed(&quote(4, 999, 1000, 500));
    r.feed(&quote(5, 999, 1000, 500));
    r.feed(&quote(6, 999, 1000, 500));
    assert_eq!(r.h().raised, [false, true, false]);
    // 9.55 is above the old stop and under the raised one: sold. Unraised it would not have been.
    r.feed(&quote(7, 954, 956, 500));
    r.feed(&trade(7, 955));
    assert_eq!(r.closes().len(), 1);
    assert_eq!(r.h().exits, [ExitReason::Stop]);

    // A stop at 9.50 and no raise at all: the same print sells nothing.
    let mut r = Rig::new(Holder::new(
        true,
        ExitPlan {
            stop: Some(px(950)),
            ..ExitPlan::new()
        },
    ));
    enter(&mut r, true);
    r.feed(&quote(7, 954, 956, 500));
    r.feed(&trade(7, 955));
    assert!(r.closes().is_empty());

    // A long with no stop at all is given one by a raise.
    let mut h = Holder::new(true, ExitPlan::new()).with(Extra::Raise);
    h.raises = vec![960];
    let mut r = Rig::new(h);
    enter(&mut r, true);
    r.feed(&quote(4, 999, 1000, 500));
    assert_eq!(r.h().raised, [true]);
    r.feed(&quote(7, 954, 956, 500));
    r.feed(&trade(7, 955));
    assert_eq!(r.closes().len(), 1);

    // A short's stop is never moved by it.
    let mut h = Holder::new(
        false,
        ExitPlan {
            stop: Some(px(1050)),
            ..ExitPlan::new()
        },
    )
    .with(Extra::Raise);
    h.raises = vec![1060];
    let mut r = Rig::new(h);
    enter(&mut r, false);
    r.feed(&quote(4, 999, 1000, 500));
    assert_eq!(r.h().raised, [false]);
    r.feed(&quote(7, 1054, 1056, 500));
    r.feed(&trade(7, 1055));
    assert_eq!(
        r.closes().len(),
        1,
        "the original stop of 10.50 still holds"
    );
}

#[test]
fn an_exit_on_the_strategys_own_signal_is_sent_once_at_a_time_with_its_own_reason() {
    use crate::exits::REASON_SIGNAL;
    let mut r = Rig::new(Holder::new(true, ExitPlan::new()).with(Extra::Signal));
    enter(&mut r, true);
    // The first review that finds the position held sends the exit, at the last price with the plan's collar.
    r.feed(&quote(4, 999, 1000, 500));
    assert_eq!(
        r.h().signalled,
        [true, false],
        "the second try found the first still out"
    );
    let c = r.closes();
    assert_eq!(c.len(), 1);
    assert_eq!(
        (c[0].side, c[0].qty, c[0].reason, c[0].tif),
        (Side::Sell, 100, REASON_SIGNAL, Tif::Day)
    );
    // It fills: the position is closed, forgotten, and counted as a signal exit; nothing more is tried.
    r.feed(&quote(5, 999, 1000, 500));
    assert_eq!(r.closes().len(), 1, "no second order went");
    assert_eq!(r.sim.position(0), 0);
    assert!(!r.h().book.is_held(0));
    let tries = r.h().signalled.len();
    r.feed(&quote(7, 999, 1000, 500));
    assert_eq!(r.h().signalled.len(), tries);
    let s = r.h().book.stats();
    assert_eq!(
        (s.signal_exits, s.stops, s.time_exits, s.closed),
        (1, 0, 0, 1)
    );
    // A name that is not held cannot be exited.
    let mut r = Rig::new(Holder::new(true, ExitPlan::new()).with(Extra::Signal));
    r.feed(&quote(1, 999, 1000, 500));
    assert!(r.h().signalled.is_empty());
}
