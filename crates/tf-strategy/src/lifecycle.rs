//! What happens to an intent after the risk gateway sees it.
//!
//! The gateway answers every intent with a [`Decision`]: accepted as an order, or
//! rejected for an explicit [`RejectReason`]. An order then moves through
//! [`OrderState`]s as the broker reports, tracked by [`Order`], which refuses
//! impossible transitions and overfills. Strategies hear about all of it as
//! [`OrderUpdate`]s.
//!
//! The model is deliberately strict. A fill is only valid on an order that has been
//! accepted, so an adapter that sees a fill first must report the acceptance
//! first. The persistent, event-sourced version of this is the order ledger
//! (E09-S04); these are the types it is built from.

use tf_core::{Nanos, Px};

use crate::intent::{Intent, IntentError, IntentId};

/// The gateway's identifier for an accepted order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OrderId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// Sent, not yet acknowledged.
    Pending,
    /// Working at the broker.
    Accepted,
    PartiallyFilled,
    Filled,
    /// Cancelled before filling completely.
    Cancelled,
    /// Refused by the broker.
    Rejected,
    /// Time in force ran out (an IOC remainder, or the end of the day).
    Expired,
}

impl OrderState {
    pub const ALL: [OrderState; 7] = [
        OrderState::Pending,
        OrderState::Accepted,
        OrderState::PartiallyFilled,
        OrderState::Filled,
        OrderState::Cancelled,
        OrderState::Rejected,
        OrderState::Expired,
    ];

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected | OrderState::Expired
        )
    }

    /// Whether the order can move from `self` to `next`.
    pub const fn can_transition_to(self, next: OrderState) -> bool {
        use OrderState::*;
        matches!(
            (self, next),
            (Pending, Accepted | Rejected | Cancelled)
                | (Accepted, PartiallyFilled | Filled | Cancelled | Expired)
                | (
                    PartiallyFilled,
                    PartiallyFilled | Filled | Cancelled | Expired
                )
        )
    }
}

/// Why the gateway or broker refused an intent. Rejections are always explicit
/// so they can be logged and counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectReason {
    /// The intent itself was malformed.
    Invalid(IntentError),
    KillSwitch,
    MaxNotional,
    MaxPosition,
    DailyLossLimit,
    OrderRate,
    NotShortable,
    ShortSaleRestricted,
    Halted,
    OutsideLuldBand,
    SpreadTooWide,
    RunUpTooLarge,
    /// An open against an existing position the other way (close it first).
    OpposingPosition,
    /// A close for more than is held, or with nothing held.
    NothingToClose,
    /// An instrument the gateway does not know.
    UnknownInstrument,
    /// The broker refused it.
    Broker,
}

/// The gateway's answer to an intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Accepted(OrderId),
    Rejected(RejectReason),
}

/// What a strategy is told about one of its orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderUpdate {
    pub intent: IntentId,
    /// `None` if the intent was rejected before becoming an order.
    pub order: Option<OrderId>,
    pub state: OrderState,
    /// Total shares filled so far.
    pub filled_qty: u32,
    /// Volume-weighted average fill price, once anything has filled.
    pub avg_px: Option<Px>,
    pub reject: Option<RejectReason>,
    pub ts: Nanos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    /// `from` cannot move to `to`.
    BadTransition { from: OrderState, to: OrderState },
    /// A fill on an order that is not working.
    NotWorking(OrderState),
    /// A fill for more shares than remain.
    Overfill { remaining: u32, fill: u32 },
    /// A fill of zero shares or at a non-positive price.
    BadFill,
}

/// An accepted order and its fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: OrderId,
    pub intent: Intent,
    state: OrderState,
    filled_qty: u32,
    /// Sum of price (raw) x shares over fills.
    notional: u128,
}

impl Order {
    /// A new order, sent but not yet acknowledged.
    pub fn new(id: OrderId, intent: Intent) -> Order {
        Order {
            id,
            intent,
            state: OrderState::Pending,
            filled_qty: 0,
            notional: 0,
        }
    }

    pub fn state(&self) -> OrderState {
        self.state
    }

    pub fn filled_qty(&self) -> u32 {
        self.filled_qty
    }

    pub fn remaining(&self) -> u32 {
        self.intent.qty - self.filled_qty
    }

    /// Average fill price, rounded down; `None` before any fill.
    pub fn avg_px(&self) -> Option<Px> {
        if self.filled_qty == 0 {
            return None;
        }
        i64::try_from(self.notional / u128::from(self.filled_qty))
            .ok()
            .map(Px::from_raw)
    }

    /// Move to `next` (acknowledgement, cancel, reject, expiry). Fills go
    /// through [`Order::fill`], which chooses between partial and complete.
    pub fn transition(&mut self, next: OrderState) -> Result<(), LifecycleError> {
        if matches!(next, OrderState::PartiallyFilled | OrderState::Filled) {
            return Err(LifecycleError::BadTransition {
                from: self.state,
                to: next,
            });
        }
        self.move_to(next)
    }

    fn move_to(&mut self, next: OrderState) -> Result<(), LifecycleError> {
        if !self.state.can_transition_to(next) {
            return Err(LifecycleError::BadTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }

    /// Apply a fill of `qty` shares at `px`. The order becomes partially or
    /// fully filled accordingly. Nothing changes if the fill is refused.
    pub fn fill(&mut self, qty: u32, px: Px) -> Result<(), LifecycleError> {
        if qty == 0 || px.raw() <= 0 {
            return Err(LifecycleError::BadFill);
        }
        if !matches!(
            self.state,
            OrderState::Accepted | OrderState::PartiallyFilled
        ) {
            return Err(LifecycleError::NotWorking(self.state));
        }
        let remaining = self.remaining();
        if qty > remaining {
            return Err(LifecycleError::Overfill {
                remaining,
                fill: qty,
            });
        }
        self.filled_qty += qty;
        self.notional += u128::try_from(px.raw()).unwrap_or(0) * u128::from(qty);
        let next = if self.filled_qty == self.intent.qty {
            OrderState::Filled
        } else {
            OrderState::PartiallyFilled
        };
        self.move_to(next)
    }

    /// The update to send the strategy about the order as it stands.
    pub fn update(&self, ts: Nanos) -> OrderUpdate {
        OrderUpdate {
            intent: self.intent.id,
            order: Some(self.id),
            state: self.state,
            filled_qty: self.filled_qty,
            avg_px: self.avg_px(),
            reject: None,
            ts,
        }
    }
}

impl OrderUpdate {
    /// The update for an intent refused before it became an order.
    pub fn rejected(intent: IntentId, reason: RejectReason, ts: Nanos) -> OrderUpdate {
        OrderUpdate {
            intent,
            order: None,
            state: OrderState::Rejected,
            filled_qty: 0,
            avg_px: None,
            reject: Some(reason),
            ts,
        }
    }
}

const _: () = {
    const fn is_copy<T: Copy>() {}
    is_copy::<Order>();
    is_copy::<OrderUpdate>();
};
