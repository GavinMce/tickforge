//! What a strategy asks for.
//!
//! Strategies emit **intents**; a separate risk gateway owns all order state
//! and decides what reaches the broker. An intent says what is wanted (which
//! instrument, which side, how much, how far it may pay, how it is protected),
//! not how it is sent.
//!
//! Rules checked by [`Intent::validate`], so a malformed intent is refused where
//! it is created rather than discovered at the broker:
//! - an **open** carries broker-side protective orders (a stop, optionally a
//!   target); a **close** carries none;
//! - a short sale is its own side, [`Side::SellShort`], never inferred from the
//!   position, so the gateway applies the short checks to it;
//! - the stop and target sit on the correct side of where the order may fill: the
//!   stop beyond the *reference* price (the nearest the order can fill to the
//!   market, since price improvement is possible) and the target beyond the *worst*
//!   price (the farthest), so a stop never fires on the fill itself and a target is
//!   a profit at any allowed fill.

use tf_core::{InstrumentId, Nanos, Px};

/// Which strategy emitted an intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StrategyId(pub u16);

/// Unique across strategies: the emitting strategy and its own sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IntentId {
    pub strategy: StrategyId,
    pub seq: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    /// Sell shares that are held.
    Sell,
    /// Sell shares that are not held (opens or adds to a short).
    SellShort,
}

impl Side {
    pub const fn is_buy(self) -> bool {
        matches!(self, Side::Buy)
    }
}

/// Whether the intent opens exposure or reduces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    Open,
    Close,
}

/// How far an order may pay. Every order has a worst price; there is no
/// unbounded market order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pricing {
    /// At this price or better.
    Limit(Px),
    /// A marketable limit: cross the spread, but never trade worse than
    /// `reference` moved against us by `collar_permille` (0 to 999) of it.
    Collar { reference: Px, collar_permille: u32 },
}

impl Pricing {
    /// The worst price this pricing allows for `side`. A collar rounds toward the
    /// reference, so the bound is never looser than stated.
    pub fn worst_price(&self, side: Side) -> Px {
        match *self {
            Pricing::Limit(p) => p,
            Pricing::Collar {
                reference,
                collar_permille,
            } => {
                let (r, c) = (i128::from(reference.raw()), i128::from(collar_permille));
                let raw = if side.is_buy() {
                    r * (1000 + c) / 1000 // floor: never above the stated bound
                } else {
                    (r * (1000 - c) + 999) / 1000 // ceil: never below it
                };
                Px::from_raw(raw as i64)
            }
        }
    }

    /// The price nearest the market that the order can fill at: a limit's price,
    /// or a collar's reference.
    pub fn reference_price(&self) -> Px {
        self.base()
    }

    fn base(&self) -> Px {
        match *self {
            Pricing::Limit(p) => p,
            Pricing::Collar { reference, .. } => reference,
        }
    }
}

/// Broker-side protective orders sent with an opening order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Protective {
    /// The stop fires when the price reaches this.
    pub stop_trigger: Px,
    /// If set, the stop becomes a limit order at this price (otherwise market).
    pub stop_limit: Option<Px>,
    /// Optional profit target.
    pub take_profit: Option<Px>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tif {
    /// Good for the trading day.
    Day,
    /// Fill what is possible now, cancel the rest.
    Ioc,
}

/// A request from a strategy. `Copy`, so it moves through queues without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intent {
    pub id: IntentId,
    pub instrument: InstrumentId,
    pub side: Side,
    /// Shares.
    pub qty: u32,
    pub purpose: Purpose,
    pub pricing: Pricing,
    pub protect: Option<Protective>,
    pub tif: Tif,
    /// Event time the strategy decided at (never the wall clock).
    pub ts: Nanos,
    /// Strategy-defined code saying why, for the audit trail.
    pub reason: u16,
}

/// Why an intent is malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentError {
    ZeroQty,
    /// A price that is not positive.
    BadPrice,
    /// A collar outside 0..=999 permille.
    BadCollar,
    /// A side/purpose pair that does not make sense (selling short to close, selling to open).
    SideAndPurpose,
    /// An opening order without a stop.
    MissingProtection,
    /// A closing order with protective orders.
    UnexpectedProtection,
    /// The stop is not beyond the entry on the losing side.
    StopOnWrongSide,
    /// The target is not beyond the entry on the winning side.
    TargetOnWrongSide,
    /// The stop-limit is not on the far side of the stop trigger.
    StopLimitOnWrongSide,
}

impl Intent {
    /// The price this order may fill at, worst case.
    pub fn limit_price(&self) -> Px {
        self.pricing.worst_price(self.side)
    }

    /// The most this order can cost (buys) or the least it can raise (sells) at
    /// its limit, in raw price units times shares.
    pub fn notional_at_limit(&self) -> u128 {
        u128::try_from(self.limit_price().raw()).unwrap_or(0) * u128::from(self.qty)
    }

    /// Whether this opens a long (true) or a short (false); `None` for closes.
    pub fn opens_long(&self) -> Option<bool> {
        match (self.purpose, self.side) {
            (Purpose::Open, Side::Buy) => Some(true),
            (Purpose::Open, Side::SellShort) => Some(false),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), IntentError> {
        if self.qty == 0 {
            return Err(IntentError::ZeroQty);
        }
        if self.pricing.base().raw() <= 0 {
            return Err(IntentError::BadPrice);
        }
        if matches!(self.pricing, Pricing::Collar { collar_permille, .. } if collar_permille > 999)
        {
            return Err(IntentError::BadCollar);
        }
        // Open: Buy (long) or SellShort. Close: Sell (exit long) or Buy (cover).
        let sensible = matches!(
            (self.purpose, self.side),
            (Purpose::Open, Side::Buy | Side::SellShort) | (Purpose::Close, Side::Buy | Side::Sell)
        );
        if !sensible {
            return Err(IntentError::SideAndPurpose);
        }
        match (self.purpose, &self.protect) {
            (Purpose::Close, Some(_)) => return Err(IntentError::UnexpectedProtection),
            (Purpose::Open, None) => return Err(IntentError::MissingProtection),
            (Purpose::Close, None) => return Ok(()),
            (Purpose::Open, Some(_)) => {}
        }
        let prot = self.protect.expect("checked above");
        let long = self.side.is_buy();
        let (near, far) = (self.pricing.reference_price(), self.limit_price());
        let beyond = |a: Px, b: Px, want_below: bool| if want_below { a < b } else { a > b };
        // A long's stop is below the entry; a short's is above. Targets are the opposite.
        if prot.stop_trigger.raw() <= 0 || !beyond(prot.stop_trigger, near, long) {
            return Err(IntentError::StopOnWrongSide);
        }
        if prot
            .take_profit
            .is_some_and(|t| t.raw() <= 0 || !beyond(t, far, !long))
        {
            return Err(IntentError::TargetOnWrongSide);
        }
        // The stop sells (long) or buys (short): its limit is no better than the trigger.
        let limit_ok = |l: Px| {
            l.raw() > 0
                && if long {
                    l <= prot.stop_trigger
                } else {
                    l >= prot.stop_trigger
                }
        };
        if prot.stop_limit.is_some_and(|l| !limit_ok(l)) {
            return Err(IntentError::StopLimitOnWrongSide);
        }
        Ok(())
    }
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<Intent>();
};
