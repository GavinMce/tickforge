//! Putting broker events into the ledger.

use tf_ledger::{Journal, JournalError, LedgerStore};

use crate::events::{BrokerEvent, Kind};

/// What recording an event did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    Recorded,
    /// The ledger has no way to record this yet (a protective leg's fill, E09-S11). Nothing was
    /// written; reconciliation will see the difference.
    NotRecorded(&'static str),
    /// The order book would not take it (an order it does not know, or in a state that cannot
    /// receive it). Nothing was written.
    Refused(String),
}

/// Record one event. Only a ledger that cannot be written is an error: it stops everything.
pub fn apply<S: LedgerStore>(
    journal: &mut Journal<S>,
    e: &BrokerEvent,
) -> Result<Applied, JournalError> {
    let r = match e.kind {
        Kind::Ack => journal.ack(e.order, e.ts),
        Kind::Fill { qty, px } => journal.fill(e.order, qty, px, e.ts),
        Kind::Close(state) => journal.close(e.order, state, e.ts),
        Kind::LegFill { .. } => {
            return Ok(Applied::NotRecorded(
                "a protective leg filled: the ledger cannot record a fill for an order the gateway did not see",
            ));
        }
    };
    match r {
        Ok(()) => Ok(Applied::Recorded),
        Err(JournalError::Lifecycle(l)) => Ok(Applied::Refused(format!("{l:?}"))),
        Err(JournalError::UnknownOrder(o)) => {
            Ok(Applied::Refused(format!("no such order {}", o.0)))
        }
        Err(JournalError::BadClose(s)) => {
            Ok(Applied::Refused(format!("cannot be closed as {s:?}")))
        }
        Err(e) => Err(e),
    }
}
