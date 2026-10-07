//! What the broker accepts in the extended hours (E19-S05).
//!
//! Outside the regular session (04:00 to 09:30 and 16:00 to 20:00 New York time) Alpaca accepts only plain limit
//! orders with time in force day or good-til-cancelled: no market or stop orders, no bracket, one-triggers-other or
//! one-cancels-other orders. Every intent is already a limit order (a limit or a collar), so what is left to refuse
//! is an intent that carries protective orders (which become a bracket or an OTO) and one that is immediate-or-cancel.
//! A strategy that wants an exit in the extended hours holds it itself ([`crate::exits`]).
//!
//! The simulated broker and the Alpaca order mapping both ask [`extended_hours_refusal`], so they refuse the same
//! intents for the same reasons. The session is that of the instant the strategy decided (`intent.ts`); an instant the
//! calendar cannot place (before 2007, or past its table) counts as the regular session, so a rule about the extended
//! hours never refuses an order for a date nothing is known about.
//!
//! The messages say the broker's rule in its own terms. They have not been checked against Alpaca's wire text (that
//! needs paper credentials, E18-S11).

use tf_calendar::{Calendar, Session};
use tf_core::Nanos;

use crate::intent::{Intent, Tif};

/// Why an order is not accepted in the extended hours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtendedHoursRefusal {
    /// It carries protective orders, which the broker would send as a bracket or an OTO order.
    ProtectiveOrders,
    /// It is immediate-or-cancel.
    TimeInForce,
}

impl ExtendedHoursRefusal {
    /// The HTTP status the broker answers with for an order it cannot take as asked.
    pub const CODE: u16 = 422;

    pub const fn message(self) -> &'static str {
        match self {
            ExtendedHoursRefusal::ProtectiveOrders => {
                "extended hours orders cannot be bracket, one-triggers-other or stop orders: only simple limit orders"
            }
            ExtendedHoursRefusal::TimeInForce => {
                "extended hours orders must have time in force day or good-til-cancelled"
            }
        }
    }
}

/// Whether `ts` falls in the premarket or after-hours; `None` if the calendar cannot place it.
pub fn is_extended_hours(ts: Nanos) -> Option<bool> {
    match Calendar::us_equities().session_at(ts).ok()? {
        Session::Premarket | Session::AfterHours => Some(true),
        Session::Regular | Session::Closed => Some(false),
    }
}

/// The reason an intent decided at `intent.ts` cannot be sent in the extended hours; `None` if it can, or if it is
/// not the extended hours.
pub fn extended_hours_refusal(intent: &Intent) -> Option<ExtendedHoursRefusal> {
    if is_extended_hours(intent.ts) != Some(true) {
        return None;
    }
    if intent.protect.is_some() {
        return Some(ExtendedHoursRefusal::ProtectiveOrders);
    }
    match intent.tif {
        Tif::Ioc => Some(ExtendedHoursRefusal::TimeInForce),
        Tif::Day | Tif::Gtc => None,
    }
}
