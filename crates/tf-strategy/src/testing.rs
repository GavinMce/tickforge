//! Scripted scenarios shared by the simulated broker's tests and the Alpaca order mapping's (E19-S05).
//!
//! A [`BracketScenario`] is an opening intent with protective orders, a scripted market (quotes and trades) and the
//! broker events that must come out of it. The simulated broker is run on the market and must produce exactly
//! `expected`; the Alpaca mapping is given the `trade_updates` frames of the same story (built from `expected`, in
//! the shape of the documented fixtures) and must translate them to exactly `expected` too. So the two agree on what
//! a stop, a target, a gap and a partial fill are, because both are held to one oracle.

use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};

use crate::broker::Kind;
use crate::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};

/// 2026-10-02 15:00:00 UTC, 11:00 in New York: the regular session.
pub const T0: Nanos = 1_790_953_200_000_000_000;
const SEC: Nanos = 1_000_000_000;

pub struct BracketScenario {
    pub name: &'static str,
    pub intent: Intent,
    pub market: Vec<Event>,
    /// What the broker reports, in order, for the order and its legs.
    pub expected: Vec<Kind>,
    /// The position left after the market has played, in shares (negative for a short).
    pub position: i64,
}

fn cents(c: i64) -> Px {
    Px::from_cents(c)
}

fn hdr(ts: Nanos) -> Header {
    Header {
        ts_event: ts,
        ts_recv: ts,
        seq: ts,
        instrument: 0,
        provider: ProviderId::Synthetic,
    }
}

fn quote(sec: u64, bid: i64, ask: i64, bid_sz: u32, ask_sz: u32) -> Event {
    Event::Quote(Quote {
        hdr: hdr(T0 + sec * SEC),
        bid_px: cents(bid),
        ask_px: cents(ask),
        bid_sz,
        ask_sz,
    })
}

fn trade(sec: u64, px: i64, size: u32) -> Event {
    Event::Trade(Trade {
        hdr: hdr(T0 + sec * SEC),
        px: cents(px),
        size,
        flags: TradeFlags::NONE,
    })
}

fn intent(side: Side, limit: i64, stop: i64, stop_limit: Option<i64>, target: i64) -> Intent {
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq: 1,
        },
        instrument: 0,
        side,
        qty: 100,
        purpose: Purpose::Open,
        pricing: Pricing::Limit(cents(limit)),
        protect: Some(Protective {
            stop_trigger: cents(stop),
            stop_limit: stop_limit.map(cents),
            take_profit: Some(cents(target)),
        }),
        tif: Tif::Day,
        ts: T0,
        reason: 1,
    }
}

fn fill(qty: u32, c: i64) -> Kind {
    Kind::Fill { qty, px: cents(c) }
}

fn leg(leg: crate::broker::Leg, qty: u32, c: i64) -> Kind {
    Kind::LegFill {
        leg,
        qty,
        px: cents(c),
    }
}

/// The scenarios, for a broker with no delay.
pub fn bracket_scenarios() -> Vec<BracketScenario> {
    use crate::broker::Leg::{Stop, Target};
    let long = intent(Side::Buy, 1000, 950, None, 1100);
    vec![
        BracketScenario {
            name: "a long's market stop is gapped through and fills at the gapped price",
            intent: long,
            market: vec![
                quote(1, 999, 1000, 500, 500),
                // The market gaps down: the quote moves first, then a trade prints at the new level, through
                // the stop (9.50), which triggers it; it sells at the bid, 9.40, not at 9.50.
                quote(3, 940, 941, 500, 500),
                trade(4, 941, 50),
            ],
            expected: vec![Kind::Ack, fill(100, 1000), leg(Stop, 100, 940)],
            position: 0,
        },
        BracketScenario {
            name: "a target fills at the bid and the stop is cancelled with it",
            intent: long,
            market: vec![
                quote(1, 999, 1000, 500, 500),
                quote(2, 1102, 1103, 500, 500),
                // Afterwards the price falls through the stop: nothing is left to sell.
                trade(3, 900, 100),
                quote(4, 890, 891, 500, 500),
            ],
            expected: vec![Kind::Ack, fill(100, 1000), leg(Target, 100, 1102)],
            position: 0,
        },
        BracketScenario {
            name: "the legs protect what has filled so far and share their shares",
            intent: long,
            market: vec![
                quote(1, 999, 1000, 500, 40),
                quote(2, 999, 1000, 500, 70),
                // 40 shares are protected at first, 100 after the second fill; the target fills 30 of them, then the
                // rest, and the stop's share of the pool shrinks with it.
                quote(3, 1105, 1106, 30, 500),
                quote(4, 1105, 1106, 100, 500),
            ],
            expected: vec![
                Kind::Ack,
                fill(40, 1000),
                fill(60, 1000),
                leg(Target, 30, 1105),
                leg(Target, 70, 1105),
            ],
            position: 0,
        },
        BracketScenario {
            name: "a stop-limit that is gapped through waits for its price",
            intent: intent(Side::Buy, 1000, 950, Some(940), 1100),
            market: vec![
                quote(1, 999, 1000, 500, 500),
                // The trade at 9.45 triggers the stop, but the bid, 9.35, is below its limit, 9.40.
                quote(2, 935, 936, 500, 500),
                trade(3, 945, 100),
                // The market comes back to the limit: now it fills, at the bid.
                quote(4, 940, 941, 500, 500),
            ],
            expected: vec![Kind::Ack, fill(100, 1000), leg(Stop, 100, 940)],
            position: 0,
        },
        BracketScenario {
            name: "a short's stop buys at the ask",
            intent: intent(Side::SellShort, 1000, 1050, None, 900),
            market: vec![
                quote(1, 1000, 1001, 500, 500),
                quote(3, 1059, 1060, 500, 500),
                trade(4, 1055, 50),
            ],
            expected: vec![Kind::Ack, fill(100, 1000), leg(Stop, 100, 1060)],
            position: 0,
        },
        BracketScenario {
            name: "a stop that has not been reached leaves the position open",
            intent: long,
            market: vec![
                quote(1, 999, 1000, 500, 500),
                trade(2, 960, 100),
                quote(3, 955, 956, 500, 500),
            ],
            expected: vec![Kind::Ack, fill(100, 1000)],
            position: 100,
        },
    ]
}
