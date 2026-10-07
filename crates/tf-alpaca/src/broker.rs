//! The Alpaca adapter as a [`tf_strategy::broker::Broker`].
//!
//! Placement and cancel go over the [`Transport`]; what happens to an order afterwards arrives on the
//! `trade_updates` stream, which the caller feeds in as text frames ([`AlpacaBroker::on_stream_frame`]).
//! There is no network here: the stream client that delivers the frames is E09-S10.

use std::collections::{BTreeMap, BTreeSet};

use tf_core::{Event, Nanos};
use tf_strategy::broker::{Broker, BrokerEvent, CancelOutcome, Submission};
use tf_strategy::intent::Intent;
use tf_strategy::lifecycle::OrderId;
use tf_strategy::session_rules::ExtendedHoursRefusal;

use crate::client::{Alpaca, Cancel, Submission as Sent, SubmitError, Transport};
use crate::events::{Frame, Outcome, ParseError, parse_frame};
use crate::wire::RequestError;

pub struct AlpacaBroker<T: Transport> {
    alpaca: Alpaca<T>,
    /// The symbol Alpaca knows each instrument by, by instrument id; empty where there is none.
    symbols: Vec<String>,
    /// Alpaca's id for each order it accepted, for cancelling.
    broker_ids: BTreeMap<OrderId, String>,
    /// Placements that got no answer: they may exist, with an id we do not have.
    unsure: BTreeSet<OrderId>,
    events: Vec<BrokerEvent>,
    notes: Vec<String>,
    anomalies: Vec<String>,
}

impl<T: Transport> AlpacaBroker<T> {
    pub fn new(alpaca: Alpaca<T>, symbols: Vec<String>) -> Self {
        AlpacaBroker {
            alpaca,
            symbols,
            broker_ids: BTreeMap::new(),
            unsure: BTreeSet::new(),
            events: Vec::new(),
            notes: Vec::new(),
            anomalies: Vec::new(),
        }
    }

    pub fn alpaca(&self) -> &Alpaca<T> {
        &self.alpaca
    }

    /// Read one frame of the `trade_updates` stream. An update about an order becomes events for
    /// [`Broker::take_events`]; the frame is returned so the caller can see authorization and
    /// listening messages. What changes nothing is kept as notes, and what should not happen as
    /// anomalies, for a person or reconciliation.
    pub fn on_stream_frame(&mut self, text: &str) -> Result<Frame, ParseError> {
        let frame = parse_frame(text)?;
        if let Frame::Update(u) = &frame {
            for o in self.alpaca.tracker.translate(u) {
                match o {
                    Outcome::Event(e) => {
                        self.unsure.remove(&e.order);
                        self.events.push(e);
                    }
                    Outcome::Note(n) => self.notes.push(n),
                    Outcome::Anomaly(a) => self.anomalies.push(a),
                }
            }
        }
        Ok(frame)
    }

    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    pub fn anomalies(&self) -> &[String] {
        &self.anomalies
    }

    /// Orders whose placement got no answer and that nothing has been heard of since.
    pub fn unsure(&self) -> Vec<OrderId> {
        self.unsure.iter().copied().collect()
    }
}

impl<T: Transport> Broker for AlpacaBroker<T> {
    fn place(&mut self, intent: &Intent, order: OrderId) -> Submission {
        let symbol = match self.symbols.get(intent.instrument as usize) {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                return Submission::Refused {
                    code: 0,
                    message: format!("no symbol for instrument {}", intent.instrument),
                };
            }
        };
        match self.alpaca.submit(intent, order, &symbol) {
            Ok(Sent::Accepted(o)) => {
                self.broker_ids.insert(order, o.id);
                Submission::Accepted
            }
            Ok(Sent::Refused { status, message }) => Submission::Refused {
                code: status,
                message,
            },
            Ok(Sent::RateLimited { retry_after }) => Submission::RateLimited {
                retry_after: retry_after.saturating_mul(1_000_000_000),
            },
            Ok(Sent::Unknown) | Err(SubmitError::Answer(_)) => {
                self.unsure.insert(order);
                Submission::Unknown
            }
            // An order the broker's own rules would refuse is refused here with the status it would answer.
            Err(SubmitError::Request(e)) => Submission::Refused {
                code: match e {
                    RequestError::ExtendedHours(_) => ExtendedHoursRefusal::CODE,
                    _ => 0,
                },
                message: e.to_string(),
            },
        }
    }

    fn cancel_order(&mut self, order: OrderId, _ts: Nanos) -> CancelOutcome {
        let Some(id) = self.broker_ids.get(&order) else {
            // Never accepted: if the placement got no answer it may exist and we cannot name it.
            return if self.unsure.contains(&order) {
                CancelOutcome::Unknown
            } else {
                CancelOutcome::Finished
            };
        };
        let id = id.clone();
        match self.alpaca.cancel(&id) {
            Cancel::Requested => CancelOutcome::Requested,
            Cancel::Finished => CancelOutcome::Finished,
            Cancel::Unknown => CancelOutcome::Unknown,
        }
    }

    fn observe(&mut self, _ev: &Event) {}

    fn close_day(&mut self, _ts: Nanos) {}

    fn take_events(&mut self) -> Vec<BrokerEvent> {
        std::mem::take(&mut self.events)
    }
}
