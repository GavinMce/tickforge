use tf_core::{Event, Header, InstrumentId, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_engine::Tier0;
use tf_universe::RefInfo;

use crate::cross::{CrossRunner, CrossStrategy, Market, MemberView, Members};
use crate::entries::{ArmError, EntryBook, EntryStats, EntryTrigger, Fired};
use crate::intent::{Intent, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::strategy::{Ctx, TimerId};
use crate::testing::T0;

const SEC: Nanos = 1_000_000_000;
/// 08:00 New York time on 2 October 2026: the premarket, where an opening order needs no stop. `T0` is in the regular session.
const PRE: Nanos = T0 - 3 * 3600 * SEC;

fn px(c: i64) -> Px {
    Px::from_cents(c)
}

fn hdr(base: Nanos, sec: u64, inst: InstrumentId) -> Header {
    Header {
        ts_event: base + sec * SEC,
        ts_recv: base + sec * SEC,
        seq: sec,
        instrument: inst,
        provider: ProviderId::Synthetic,
    }
}

fn quote(base: Nanos, sec: u64, inst: InstrumentId, bid: i64, ask: i64) -> Event {
    Event::Quote(Quote {
        hdr: hdr(base, sec, inst),
        bid_px: px(bid),
        ask_px: px(ask),
        bid_sz: 500,
        ask_sz: 500,
    })
}

fn trade(base: Nanos, sec: u64, inst: InstrumentId, c: i64) -> Event {
    Event::Trade(Trade {
        hdr: hdr(base, sec, inst),
        px: px(c),
        size: 10,
        flags: TradeFlags::NONE,
    })
}

/// Arms its triggers at the first review and forwards trades and timers to the book.
struct Breaker {
    triggers: Vec<(InstrumentId, EntryTrigger)>,
    arm_results: Vec<Result<(), ArmError>>,
    book: EntryBook,
    fired: Vec<Fired>,
    lapsed: Vec<(InstrumentId, Side)>,
    armed: bool,
    /// When one side fires, disarm the other (the two sides of a range).
    oco: bool,
    /// The strategy's own check: refuse an entry in an instrument already in `held`.
    held: Vec<InstrumentId>,
    /// Arm the triggers at this review second instead of the first.
    arm_at: u64,
    review_seconds: u64,
    /// Disarmed at the review after the one that armed, and what each came to; then everything of `disarm_all`.
    disarm: Vec<(InstrumentId, Side)>,
    disarm_results: Vec<bool>,
    disarm_all: Vec<InstrumentId>,
    disarm_all_results: Vec<u32>,
    /// Clear the book at the review after the one that armed.
    clear_at_second_review: bool,
    cleared: Vec<u32>,
}

impl Breaker {
    fn new(triggers: Vec<(InstrumentId, EntryTrigger)>) -> Breaker {
        Breaker {
            triggers,
            arm_results: Vec::new(),
            book: EntryBook::new(1000),
            fired: Vec::new(),
            lapsed: Vec::new(),
            armed: false,
            oco: false,
            held: Vec::new(),
            arm_at: 0,
            review_seconds: 0,
            disarm: Vec::new(),
            disarm_results: Vec::new(),
            disarm_all: Vec::new(),
            disarm_all_results: Vec::new(),
            clear_at_second_review: false,
            cleared: Vec::new(),
        }
    }
}

impl CrossStrategy for Breaker {
    const WANTS_MEMBER_EVENTS: bool = true;
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }
    fn period(&self) -> Nanos {
        SEC
    }
    fn on_review(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>) {
        self.review_seconds += 1;
        if self.armed && self.review_seconds == self.arm_at + 2 {
            for (id, side) in self.disarm.clone() {
                self.disarm_results.push(self.book.disarm(ctx, id, side));
            }
            for id in self.disarm_all.clone() {
                self.disarm_all_results.push(self.book.disarm_all(ctx, id));
            }
            if self.clear_at_second_review {
                self.cleared.push(self.book.clear(ctx));
            }
        }
        if !self.armed && self.review_seconds > self.arm_at {
            self.armed = true;
            for (id, t) in self.triggers.clone() {
                self.arm_results.push(self.book.arm(ctx, id, t));
            }
        }
    }
    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, ev: &Event) {
        if let Event::Trade(t) = ev {
            let held = self.held.clone();
            let id = t.hdr.instrument;
            let sent = self
                .book
                .on_trade_checked(ctx, id, t.px, |_| !held.contains(&id));
            for f in sent {
                if self.oco {
                    let other = if f.side == Side::Buy {
                        Side::SellShort
                    } else {
                        Side::Buy
                    };
                    self.book.disarm(ctx, f.id, other);
                }
                self.fired.push(f);
            }
        }
    }
    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _: &MemberView<'_>, timer: TimerId) {
        if let Some(l) = self.book.on_timer(ctx, timer) {
            self.lapsed.push(l);
        }
    }
}

struct Rig {
    tier0: Tier0,
    refs: Vec<RefInfo>,
    runner: CrossRunner<Breaker>,
    intents: Vec<Intent>,
    base: Nanos,
}

impl Rig {
    fn new(b: Breaker, base: Nanos) -> Rig {
        Rig {
            tier0: Tier0::new(2),
            refs: vec![RefInfo::default(); 2],
            runner: CrossRunner::new(b, Members::from_ids([0, 1])),
            intents: Vec::new(),
            base,
        }
    }

    fn feed(&mut self, ev: &Event) {
        self.tier0.on_event(ev);
        let m = Market {
            tier0: &self.tier0,
            refs: &self.refs,
            budget: None,
        };
        self.runner.on_event(m, None, None, ev);
        self.intents.extend(self.runner.drain_intents());
    }

    fn quote(&mut self, sec: u64) {
        self.feed(&quote(self.base, sec, 0, 999, 1001));
    }

    fn trade(&mut self, sec: u64, inst: InstrumentId, c: i64) {
        self.feed(&trade(self.base, sec, inst, c));
    }

    fn b(&self) -> &Breaker {
        self.runner.strategy()
    }
}

fn buy(level: i64) -> EntryTrigger {
    EntryTrigger {
        side: Side::Buy,
        level: px(level),
        qty: 100,
        not_before: None,
        lapse_at: None,
        collar_permille: 5,
        tif: Tif::Day,
        protect: None,
        reason: 77,
    }
}

fn short(level: i64) -> EntryTrigger {
    EntryTrigger {
        side: Side::SellShort,
        ..buy(level)
    }
}

/// A rig in the premarket whose strategy has armed `triggers` by second 2 (the first review that sees an event is at second 1).
fn armed(triggers: Vec<(InstrumentId, EntryTrigger)>) -> Rig {
    let mut r = Rig::new(Breaker::new(triggers), PRE);
    r.quote(1);
    r.quote(2);
    assert!(r.b().armed, "the first review armed them");
    r
}

#[test]
fn a_buy_fires_once_at_the_first_trade_at_or_above_the_level() {
    let mut r = armed(vec![(0, buy(1000))]);
    r.trade(3, 0, 990);
    r.trade(3, 0, 999);
    assert!(r.intents.is_empty(), "below the level: nothing");
    assert!(r.b().book.is_armed(0, Side::Buy));
    // A trade exactly at the level fires it: a buy of 100, a collar of 0.5% above that trade's price, the trigger's reason.
    r.trade(4, 0, 1000);
    assert_eq!(r.intents.len(), 1);
    let i = r.intents[0];
    assert_eq!(
        (
            i.instrument,
            i.side,
            i.qty,
            i.purpose,
            i.tif,
            i.reason,
            i.protect
        ),
        (0, Side::Buy, 100, Purpose::Open, Tif::Day, 77, None)
    );
    assert_eq!(
        i.pricing,
        Pricing::Collar {
            reference: px(1000),
            collar_permille: 5
        }
    );
    assert_eq!(r.b().fired.len(), 1);
    assert_eq!(
        (
            r.b().fired[0].px,
            r.b().fired[0].level,
            r.b().fired[0].intent
        ),
        (px(1000), px(1000), i.id)
    );
    // Once: the trigger is gone, and a higher trade sends nothing more.
    assert!(!r.b().book.is_armed(0, Side::Buy));
    r.trade(5, 0, 1100);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(
        r.b().book.stats(),
        EntryStats {
            armed: 1,
            fired: 1,
            ..EntryStats::default()
        }
    );
    assert!(r.b().book.is_empty());
}

#[test]
fn a_gap_through_the_level_fires_at_the_first_print_beyond_it_with_the_collar_around_that_print() {
    let mut r = armed(vec![(0, buy(1000))]);
    r.trade(3, 0, 1040);
    let Pricing::Collar { reference, .. } = r.intents[0].pricing else {
        panic!("a collar")
    };
    assert_eq!(
        reference,
        px(1040),
        "the order follows the print, not the level"
    );
    assert_eq!(r.b().fired[0].level, px(1000));
}

#[test]
fn a_short_sale_fires_at_the_first_trade_at_or_below_the_level() {
    let mut r = armed(vec![(0, short(1000))]);
    r.trade(3, 0, 1001);
    r.trade(3, 0, 1100);
    assert!(r.intents.is_empty());
    r.trade(4, 0, 1000);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(
        (r.intents[0].side, r.intents[0].purpose, r.intents[0].qty),
        (Side::SellShort, Purpose::Open, 100)
    );
    // A short sale's limit is below the print.
    assert!(r.intents[0].limit_price() < px(1000));
    r.trade(5, 0, 900);
    assert_eq!(r.intents.len(), 1, "once");
}

#[test]
fn the_two_sides_of_a_range_are_separate_and_the_strategy_disarms_the_other_when_one_fires() {
    let mut b = Breaker::new(vec![(0, buy(1100)), (0, short(900))]);
    b.oco = true;
    let mut r = Rig::new(b, PRE);
    r.quote(1);
    r.quote(2);
    assert_eq!(r.b().book.len(), 2);
    r.trade(3, 0, 1000);
    assert!(r.intents.is_empty(), "inside the range");
    r.trade(4, 0, 1100);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(r.intents[0].side, Side::Buy);
    assert!(
        !r.b().book.is_armed(0, Side::SellShort),
        "the other side was disarmed"
    );
    r.trade(5, 0, 800);
    assert_eq!(r.intents.len(), 1, "and cannot fire");
    assert_eq!(r.b().book.stats().disarmed, 1);
    // Without the strategy disarming it, a trade can fire a buy and a short sale in turn: both stay armed until a trade goes through them.
    let mut r = armed(vec![(0, buy(1100)), (0, short(900))]);
    r.trade(3, 0, 1100);
    r.trade(4, 0, 900);
    assert_eq!(
        r.intents.iter().map(|i| i.side).collect::<Vec<_>>(),
        [Side::Buy, Side::SellShort]
    );
}

#[test]
fn triggers_are_per_instrument() {
    let mut r = armed(vec![(0, buy(1000)), (1, buy(2000))]);
    r.trade(3, 1, 1500);
    assert!(r.intents.is_empty(), "instrument 1 is below its own level");
    r.trade(4, 0, 1500);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(r.intents[0].instrument, 0);
    assert!(r.b().book.is_armed(1, Side::Buy));
    r.trade(5, 1, 2000);
    assert_eq!(
        r.intents.iter().map(|i| i.instrument).collect::<Vec<_>>(),
        [0, 1]
    );
}

#[test]
fn a_trigger_is_not_in_force_before_not_before_and_fires_on_the_first_trade_through_the_level_after()
 {
    let t = EntryTrigger {
        not_before: Some(PRE + 10 * SEC),
        ..buy(1000)
    };
    let mut r = armed(vec![(0, t)]);
    r.trade(5, 0, 1200);
    r.trade(9, 0, 1300);
    assert!(r.intents.is_empty(), "through the level, but not yet armed");
    assert!(r.b().book.is_armed(0, Side::Buy), "and still held");
    // At the instant it takes effect a trade through the level fires.
    r.trade(10, 0, 1250);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(r.b().fired[0].px, px(1250));
}

#[test]
fn a_trigger_lapses_at_its_time_a_trade_at_that_time_fires_nothing_and_a_timer_cleans_up_without_trades()
 {
    let t = EntryTrigger {
        lapse_at: Some(PRE + 10 * SEC),
        ..buy(1000)
    };
    // One second before: in force.
    let mut r = armed(vec![(0, t)]);
    r.trade(9, 0, 1000);
    assert_eq!(r.intents.len(), 1, "a nanosecond before the lapse it fires");
    // At the lapse: nothing, and it is counted as lapsed, not fired.
    let mut r = armed(vec![(0, t)]);
    r.trade(10, 0, 1000);
    assert!(r.intents.is_empty());
    assert!(!r.b().book.is_armed(0, Side::Buy));
    assert_eq!(
        r.b().book.stats(),
        EntryStats {
            armed: 1,
            lapsed: 1,
            ..EntryStats::default()
        }
    );
    // With no trade at all the timer drops it, once, and says which.
    let mut r = armed(vec![
        (0, t),
        (
            1,
            EntryTrigger {
                lapse_at: None,
                ..buy(3000)
            },
        ),
    ]);
    r.quote(9);
    assert!(r.b().lapsed.is_empty());
    r.quote(10);
    r.quote(11);
    assert_eq!(r.b().lapsed, [(0, Side::Buy)]);
    assert_eq!(r.b().book.stats().lapsed, 1);
    assert!(r.b().book.is_armed(1, Side::Buy), "the other has no lapse");
    r.trade(12, 0, 5000);
    assert!(r.intents.is_empty(), "a lapsed trigger never fires");
}

#[test]
fn a_disarmed_trigger_does_not_fire_and_disarming_says_whether_there_was_one() {
    let lapsing = EntryTrigger {
        lapse_at: Some(PRE + 20 * SEC),
        ..buy(1000)
    };
    let mut b = Breaker::new(vec![
        (0, lapsing),
        (0, short(500)),
        (1, buy(1000)),
        (1, short(500)),
    ]);
    // The second review disarms: 0's buy, 0's buy again (nothing left), and a short sale of 1 that is armed; then both sides of 1 (one is left).
    b.disarm = vec![(0, Side::Buy), (0, Side::Buy), (1, Side::SellShort)];
    b.disarm_all = vec![1, 1];
    let mut r = Rig::new(b, PRE);
    r.quote(1);
    r.quote(2);
    r.quote(3);
    assert_eq!(r.b().disarm_results, [true, false, true]);
    assert_eq!(r.b().disarm_all_results, [1, 0]);
    assert_eq!(r.b().book.len(), 1, "only the short sale of 0 is left");
    assert_eq!(r.b().book.stats().disarmed, 3);
    // None of the disarmed fires, and the one with a lapse had its timer cancelled: no lapse is reported at its time.
    r.trade(4, 0, 1200);
    r.trade(4, 1, 1200);
    r.trade(4, 1, 400);
    assert!(r.intents.is_empty());
    r.quote(21);
    r.quote(22);
    assert!(r.b().lapsed.is_empty());
    r.trade(23, 0, 500);
    assert_eq!(r.intents.len(), 1, "the one left still works");
}

#[test]
fn the_strategys_own_check_refuses_a_name_it_holds_and_the_trigger_is_spent() {
    let mut b = Breaker::new(vec![(0, buy(1000)), (1, buy(1000))]);
    b.held = vec![0];
    let mut r = Rig::new(b, PRE);
    r.quote(1);
    r.quote(2);
    r.trade(3, 0, 1100);
    assert!(r.intents.is_empty(), "held: refused, nothing sent");
    assert!(
        !r.b().book.is_armed(0, Side::Buy),
        "and the trigger is spent"
    );
    r.trade(4, 1, 1100);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(
        r.b().book.stats(),
        EntryStats {
            armed: 2,
            fired: 2,
            refused: 1,
            ..EntryStats::default()
        }
    );
    assert_eq!(r.b().fired.len(), 1);
}

#[test]
fn an_order_the_intent_validation_refuses_is_counted_and_the_trigger_is_spent() {
    // In the regular session an opening order needs a stop: a trigger armed without one fires into a refusal.
    let mut r = Rig::new(Breaker::new(vec![(0, buy(1000))]), T0);
    r.quote(1);
    r.quote(2);
    r.trade(3, 0, 1100);
    assert!(r.intents.is_empty());
    assert_eq!(r.b().book.stats().refused, 1);
    assert!(r.b().book.is_empty());
    assert_eq!(r.runner.invalid_intents(), 1);
    // With a stop under the trade it is sent, with the stop.
    let prot = Protective {
        stop_trigger: px(950),
        stop_limit: None,
        take_profit: None,
    };
    let t = EntryTrigger {
        protect: Some(prot),
        ..buy(1000)
    };
    let mut r = Rig::new(Breaker::new(vec![(0, t)]), T0);
    r.quote(1);
    r.quote(2);
    r.trade(3, 0, 1100);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(r.intents[0].protect, Some(prot));
    // A short sale with its stop above.
    let s = EntryTrigger {
        protect: Some(Protective {
            stop_trigger: px(1050),
            ..prot
        }),
        ..short(1000)
    };
    let mut r = Rig::new(Breaker::new(vec![(0, s)]), T0);
    r.quote(1);
    r.quote(2);
    r.trade(3, 0, 990);
    assert_eq!(r.intents.len(), 1);
    assert_eq!(r.intents[0].protect.map(|p| p.stop_trigger), Some(px(1050)));
}

#[test]
fn arming_checks_the_trigger_and_arming_again_replaces_it() {
    let mut b = Breaker::new(vec![
        (
            0,
            EntryTrigger {
                side: Side::Sell,
                ..buy(1000)
            },
        ),
        (
            0,
            EntryTrigger {
                qty: 0,
                ..buy(1000)
            },
        ),
        (0, buy(0)),
        (
            0,
            EntryTrigger {
                collar_permille: 1000,
                ..buy(1000)
            },
        ),
        (
            0,
            EntryTrigger {
                lapse_at: Some(PRE + 2 * SEC),
                ..buy(1000)
            },
        ),
        (
            0,
            EntryTrigger {
                not_before: Some(PRE + 50 * SEC),
                lapse_at: Some(PRE + 50 * SEC),
                ..buy(1000)
            },
        ),
        (u32::MAX / 2 + 1, buy(1000)),
        (0, buy(1000)),
        (
            0,
            EntryTrigger {
                qty: 40,
                ..buy(1200)
            },
        ),
    ]);
    b.arm_at = 0;
    let mut r = Rig::new(b, PRE);
    r.quote(1);
    r.quote(2);
    assert_eq!(
        r.b().arm_results,
        [
            Err(ArmError::Side),
            Err(ArmError::Qty),
            Err(ArmError::Level),
            Err(ArmError::Collar),
            Err(ArmError::Lapsed),
            Err(ArmError::Lapsed),
            Err(ArmError::Timer),
            Ok(()),
            Ok(())
        ]
    );
    // Only the last good one is held: it replaced the one before.
    assert_eq!(r.b().book.len(), 1);
    r.trade(3, 0, 1100);
    assert!(r.intents.is_empty(), "the replacement's level is 12.00");
    r.trade(4, 0, 1200);
    assert_eq!((r.intents.len(), r.intents[0].qty), (1, 40));
}

#[test]
fn a_replay_of_the_same_events_sends_the_same_orders() {
    let run = || {
        let mut b = Breaker::new(vec![(0, buy(1100)), (0, short(900)), (1, buy(2000))]);
        b.oco = true;
        let mut r = Rig::new(b, PRE);
        r.quote(1);
        r.quote(2);
        for (s, i, c) in [
            (3, 0, 1000),
            (4, 1, 1999),
            (5, 0, 890),
            (6, 1, 2001),
            (7, 0, 1200),
        ] {
            r.trade(s, i, c);
        }
        r.intents
    };
    let (a, b) = (run(), run());
    assert_eq!(a.len(), 2);
    assert_eq!(a, b);
}

#[test]
fn every_instrument_and_side_has_its_own_lapse_timer() {
    let lapse = |sec: u64, t: EntryTrigger| EntryTrigger {
        lapse_at: Some(PRE + sec * SEC),
        ..t
    };
    // Four triggers lapsing at four different seconds: each timer drops its own and says which.
    let mut r = armed(vec![
        (0, lapse(10, buy(1000))),
        (0, lapse(11, short(500))),
        (1, lapse(12, buy(1000))),
        (1, lapse(13, short(500))),
    ]);
    for sec in 9..=14 {
        r.quote(sec);
    }
    assert_eq!(
        r.b().lapsed,
        [
            (0, Side::Buy),
            (0, Side::SellShort),
            (1, Side::Buy),
            (1, Side::SellShort)
        ]
    );
    assert!(r.b().book.is_empty());
}

#[test]
fn a_collar_of_999_permille_is_the_widest_there_is() {
    let mut r = Rig::new(
        Breaker::new(vec![
            (
                0,
                EntryTrigger {
                    collar_permille: 999,
                    ..buy(1000)
                },
            ),
            (
                1,
                EntryTrigger {
                    collar_permille: 1000,
                    ..buy(1000)
                },
            ),
        ]),
        PRE,
    );
    r.quote(1);
    r.quote(2);
    assert_eq!(r.b().arm_results, [Ok(()), Err(ArmError::Collar)]);
}

#[test]
fn disarming_all_counts_both_sides() {
    let mut b = Breaker::new(vec![(0, buy(1000)), (0, short(500)), (1, buy(1000))]);
    b.disarm_all = vec![0, 0, 1];
    let mut r = Rig::new(b, PRE);
    r.quote(1);
    r.quote(2);
    r.quote(3);
    assert_eq!(r.b().disarm_all_results, [2, 0, 1]);
    assert!(r.b().book.is_empty());
}

#[test]
fn clearing_drops_every_trigger_and_its_lapse() {
    let t = EntryTrigger {
        lapse_at: Some(PRE + 20 * SEC),
        ..buy(1000)
    };
    let mut b = Breaker::new(vec![(0, t), (0, short(500)), (1, t)]);
    b.clear_at_second_review = true;
    let mut r = Rig::new(b, PRE);
    r.quote(1);
    r.quote(2);
    r.quote(3);
    assert_eq!(r.b().cleared, [3]);
    assert!(r.b().book.is_empty());
    r.trade(4, 0, 1200);
    r.trade(4, 0, 400);
    assert!(r.intents.is_empty());
    r.quote(21);
    r.quote(22);
    assert!(
        r.b().lapsed.is_empty(),
        "the lapse timers went with the triggers"
    );
    assert_eq!(r.b().book.stats().disarmed, 0, "clearing is not disarming");
}
