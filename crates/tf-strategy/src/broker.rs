//! What a broker looks like to the rest of the system (E09-S08).
//!
//! The gateway decides, then a [`Broker`] is asked to place the order the gateway accepted, under the
//! gateway's own [`OrderId`]. What the broker did next comes back as [`BrokerEvent`]s (acknowledged,
//! filled for so many shares at what price, ended), which is exactly what the ledger records. The
//! simulated broker and the Alpaca adapter both speak this, so the same host runs against either.
//!
//! A placement has one of four outcomes ([`Submission`]), and the fourth, [`Submission::Unknown`], is
//! why this is not simply "send, then wait": there was no usable answer, and the order may or may not
//! exist at the broker. The caller must keep treating it as working, never send it again under a
//! new id, and let the events (or a reconciliation) settle it.
//!
//! Events for one order obey the order of the state machine. [`check_events`] says whether a
//! sequence does, and is used on every broker that implements the trait.

use std::collections::BTreeMap;

use tf_core::{Event, Nanos, Px};

use crate::intent::Intent;
use crate::lifecycle::{OrderId, OrderState};

/// Which protective leg of a bracket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leg {
    /// The stop-loss.
    Stop,
    /// The profit target.
    Target,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ack,
    Fill {
        qty: u32,
        px: Px,
    },
    /// The order ended short of its size: `Cancelled`, `Expired`, or `Rejected` (a rejection by the
    /// venue arrives before any acknowledgement). A complete fill ends an order by itself.
    Close(OrderState),
    /// A protective leg of this order filled.
    LegFill {
        leg: Leg,
        qty: u32,
        px: Px,
    },
}

/// Something that happened to an order we placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrokerEvent {
    pub order: OrderId,
    pub ts: Nanos,
    pub kind: Kind,
}

/// How a placement ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submission {
    /// The broker has the order. Events follow.
    Accepted,
    /// The broker will not take it, and said why. The order did not happen.
    Refused { code: u16, message: String },
    /// Too many requests: nothing was done. Try again after this long.
    RateLimited { retry_after: Nanos },
    /// No usable answer. The order may still arrive: keep it working, do not send it again.
    Unknown,
}

/// How a cancel request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The broker will cancel it; a `Close(Cancelled)` follows (or a fill, if it got there first).
    Requested,
    /// The order is finished or unknown to the broker; nothing to cancel.
    Finished,
    Unknown,
}

/// A broker: simulated, or a real one behind an adapter.
pub trait Broker {
    /// Place `intent` as the gateway's `order`.
    fn place(&mut self, intent: &Intent, order: OrderId) -> Submission;

    fn cancel_order(&mut self, order: OrderId, ts: Nanos) -> CancelOutcome;

    /// A market event, in stream order. A simulated broker matches orders against it; a real one
    /// ignores it.
    fn observe(&mut self, ev: &Event);

    /// The session ended: what still works expires.
    fn close_day(&mut self, ts: Nanos);

    /// What happened since the last call, in order.
    fn take_events(&mut self) -> Vec<BrokerEvent>;
}

/// Why an event sequence is not one a broker may produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventFault {
    pub index: usize,
    pub order: OrderId,
    pub why: String,
}

#[derive(Default)]
struct Seen {
    acked: bool,
    filled: u64,
    ended: bool,
}

/// Check a broker's events: per order, an acknowledgement comes before any fill and before a
/// cancel or expiry, at most once; a rejection comes before any acknowledgement; nothing follows an
/// end; fills are of at least one share and never exceed `size(order)`; times do not go backwards.
/// `size` says how many shares each order asked for.
pub fn check_events(
    events: &[BrokerEvent],
    size: impl Fn(OrderId) -> Option<u32>,
) -> Result<(), EventFault> {
    let mut by: BTreeMap<OrderId, Seen> = BTreeMap::new();
    let mut last_ts: Nanos = 0;
    for (index, e) in events.iter().enumerate() {
        let fault = |why: &str| EventFault {
            index,
            order: e.order,
            why: why.to_owned(),
        };
        if e.ts < last_ts {
            return Err(fault("time went backwards"));
        }
        last_ts = e.ts;
        let s = by.entry(e.order).or_default();
        if s.ended {
            return Err(fault("an event after the order ended"));
        }
        match e.kind {
            Kind::Ack => {
                if s.acked {
                    return Err(fault("acknowledged twice"));
                }
                s.acked = true;
            }
            Kind::Fill { qty, px } => {
                if !s.acked {
                    return Err(fault("a fill before the acknowledgement"));
                }
                if qty == 0 || px.raw() <= 0 {
                    return Err(fault("a fill of no shares or at no price"));
                }
                s.filled += u64::from(qty);
                let asked = size(e.order).ok_or_else(|| fault("an order nobody placed"))?;
                if s.filled > u64::from(asked) {
                    return Err(fault("filled for more than was asked"));
                }
                if s.filled == u64::from(asked) {
                    s.ended = true;
                }
            }
            Kind::Close(state) => match state {
                OrderState::Rejected => {
                    if s.acked || s.filled > 0 {
                        return Err(fault("rejected after it was acknowledged"));
                    }
                    s.ended = true;
                }
                OrderState::Cancelled | OrderState::Expired => {
                    if state == OrderState::Expired && !s.acked {
                        return Err(fault("expired before it was acknowledged"));
                    }
                    s.ended = true;
                }
                _ => {
                    return Err(fault(
                        "an order is closed as cancelled, expired or rejected only",
                    ));
                }
            },
            Kind::LegFill { qty, .. } => {
                if qty == 0 {
                    return Err(fault("a leg fill of no shares"));
                }
            }
        }
    }
    Ok(())
}
