//! The gateway and order book that write the ledger and recover from it.
//!
//! A [`Journal`] owns a risk [`Gateway`] and the order state machine ([`Order`] per accepted
//! order). Every change goes through it: the change is applied, then the record of it (the
//! input and, for a decision, the answer) is appended, and only then does the call return, so
//! nothing leaves the process (an order to a broker, a fill passed to a strategy) that the
//! ledger does not hold. If the append fails the journal is poisoned: its memory is ahead of
//! its ledger, so it refuses everything until restarted from the ledger.
//!
//! [`Journal::open`] replays the ledger through the same code, comparing each recorded record
//! with what the replay produces. A decision the replay would make differently (the limits
//! changed, the code changed behaviour, the ledger was edited) is a [`JournalError::Diverged`],
//! never a quiet difference.
//!
//! Marks (the last price of an instrument) are market data, not ledger events, with one
//! exception: decisions depend on the marks of instruments holding a position. Before each
//! decision, and each new day, the marks that have moved since they were last written are written
//! first ([`Journal::sync_marks`]). After a restart the marks are the last written ones until
//! market data refreshes them.

use std::collections::BTreeMap;

use tf_core::{InstrumentId, Nanos, Px};
use tf_risk::{Budgets, Gateway, GatewayError, GatewaySnapshot, Limits, LossEvent};
use tf_strategy::intent::Intent;
use tf_strategy::lifecycle::{Decision, LifecycleError, Order, OrderId, OrderState};

use crate::codec::{CodecError, Input, Record};
use crate::store::{LedgerStore, StoreError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalError {
    Store(StoreError),
    /// A record that cannot be read.
    Codec {
        record: u64,
        why: CodecError,
    },
    /// Replaying record `record` did not give what was written.
    Diverged {
        record: u64,
        written: String,
        replayed: String,
    },
    /// The ledger is for different limits or a different universe than this journal was given.
    Mismatch(String),
    Lifecycle(LifecycleError),
    Gateway(GatewayError),
    UnknownOrder(OrderId),
    /// An order can only be closed as cancelled, rejected or expired.
    BadClose(OrderState),
    /// The ledger could not be written; restart from it.
    Poisoned,
    /// The first record is not a start record, or another one is.
    Structure(String),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Store(e) => write!(f, "{e}"),
            JournalError::Codec { record, why } => write!(f, "ledger record {record} cannot be read: {why}"),
            JournalError::Diverged { record, written, replayed } => write!(
                f,
                "replaying ledger record {record} gives a different result than was written\\n  written:  {written}\\n  replayed: {replayed}"
            ),
            JournalError::Mismatch(m) => write!(f, "{m}"),
            JournalError::Lifecycle(e) => write!(f, "order state: {e:?}"),
            JournalError::Gateway(e) => write!(f, "gateway: {e:?}"),
            JournalError::UnknownOrder(o) => write!(f, "no such order {}", o.0),
            JournalError::BadClose(s) => write!(f, "an order cannot be closed as {s:?}"),
            JournalError::Poisoned => f.write_str(
                "the ledger could not be written, so this journal is stopped; restart it from the ledger",
            ),
            JournalError::Structure(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<StoreError> for JournalError {
    fn from(e: StoreError) -> Self {
        JournalError::Store(e)
    }
}

/// What opening a ledger found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    /// Records replayed (including the start record).
    pub records: u64,
    /// A damaged last record that was removed, if any.
    pub repaired: Option<String>,
    pub open_orders: usize,
    pub positions: usize,
}

pub struct Journal<S: LedgerStore> {
    store: S,
    gw: Gateway,
    orders: BTreeMap<OrderId, Order>,
    seq: u64,
    /// The mark last written for each instrument.
    logged: Vec<i64>,
    poisoned: bool,
}

fn show(r: &Record) -> String {
    r.encode().unwrap_or_else(|e| format!("<{e}>"))
}

impl<S: LedgerStore> Journal<S> {
    /// Open the ledger in `store` for a universe of `instruments` under `limits`: a new ledger
    /// gets its start record; an existing one is replayed and checked.
    pub fn open(
        mut store: S,
        limits: Limits,
        instruments: usize,
    ) -> Result<(Journal<S>, Recovery), JournalError> {
        let loaded = store.load()?;
        Journal::from_loaded(store, loaded, limits, instruments)
    }

    /// Open an existing ledger using the limits and universe it was started with, as recorded in
    /// its first record (for tools that check or inspect a ledger). A ledger with no records is an
    /// error here: there is nothing to say what it is for.
    pub fn open_recorded(mut store: S) -> Result<(Journal<S>, Recovery), JournalError> {
        let loaded = store.load()?;
        let Some(first) = loaded.records.first() else {
            return Err(JournalError::Structure(
                "the ledger has no records".to_owned(),
            ));
        };
        match Record::decode(first) {
            Ok(Record::Start {
                instruments,
                limits,
            }) => {
                let limits = Limits::from_pairs(&limits).map_err(|why| {
                    JournalError::Structure(format!(
                        "the ledger's start record has bad limits: {why}"
                    ))
                })?;
                Journal::from_loaded(store, loaded, limits, instruments as usize)
            }
            Ok(_) => Err(JournalError::Structure(
                "the first record is not a start record".to_owned(),
            )),
            Err(why) => Err(JournalError::Codec { record: 1, why }),
        }
    }

    fn from_loaded(
        store: S,
        loaded: crate::store::Loaded,
        limits: Limits,
        instruments: usize,
    ) -> Result<(Journal<S>, Recovery), JournalError> {
        let pairs: Vec<(String, String)> = limits
            .pairs()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        let start = Record::Start {
            instruments: instruments as u32,
            limits: pairs,
        };
        let mut j = Journal {
            store,
            gw: Gateway::new(limits, instruments),
            orders: BTreeMap::new(),
            seq: 0,
            logged: vec![0; instruments],
            poisoned: false,
        };
        if loaded.records.is_empty() {
            j.append(&start)?;
        } else {
            for (k, line) in loaded.records.iter().enumerate() {
                let n = k as u64 + 1;
                let rec =
                    Record::decode(line).map_err(|why| JournalError::Codec { record: n, why })?;
                if k == 0 {
                    if rec != start {
                        return Err(JournalError::Mismatch(format!(
                            "this ledger is for a different universe or limits than the ones given\\n  ledger: {}\\n  given:  {}",
                            show(&rec),
                            show(&start)
                        )));
                    }
                } else {
                    let Record::Event { input, outcome } = rec.clone() else {
                        return Err(JournalError::Structure(format!(
                            "record {n} is a second start record"
                        )));
                    };
                    let got = j.step(input.clone())?;
                    if got != outcome {
                        return Err(JournalError::Diverged {
                            record: n,
                            written: line.clone(),
                            replayed: show(&Record::Event {
                                input,
                                outcome: got,
                            }),
                        });
                    }
                }
                j.seq = n;
            }
        }
        let rec = Recovery {
            records: j.seq,
            repaired: loaded.repaired,
            open_orders: j.open_orders().len(),
            positions: j.gw.snapshot().positions.len(),
        };
        Ok((j, rec))
    }

    fn append(&mut self, rec: &Record) -> Result<(), JournalError> {
        let line = rec.encode().map_err(|why| JournalError::Codec {
            record: self.seq + 1,
            why,
        })?;
        match self.store.append(self.seq + 1, &line) {
            Ok(()) => {
                self.seq += 1;
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                Err(e.into())
            }
        }
    }

    /// Apply one input to the gateway and order book; the decision's answer if it is a decision.
    /// All validation happens before anything changes, so a refused input changes nothing.
    fn step(&mut self, input: Input) -> Result<Option<Decision>, JournalError> {
        match input {
            Input::Mark { instrument, px } => {
                self.gw.mark(instrument, Px::from_raw(px));
                if let Some(l) = self.logged.get_mut(instrument as usize) {
                    *l = px;
                }
                Ok(None)
            }
            Input::Decide { intent, now } => {
                let d = self.gw.decide(&intent, now);
                if let Decision::Accepted(id) = d {
                    self.orders.insert(id, Order::new(id, intent));
                }
                Ok(Some(d))
            }
            Input::Ack { order, .. } => {
                let mut o = *self
                    .orders
                    .get(&order)
                    .ok_or(JournalError::UnknownOrder(order))?;
                o.transition(OrderState::Accepted)
                    .map_err(JournalError::Lifecycle)?;
                self.orders.insert(order, o);
                Ok(None)
            }
            Input::Fill { order, qty, px, .. } => {
                let mut o = *self
                    .orders
                    .get(&order)
                    .ok_or(JournalError::UnknownOrder(order))?;
                o.fill(qty, Px::from_raw(px))
                    .map_err(JournalError::Lifecycle)?;
                self.gw
                    .on_fill(order, qty, Px::from_raw(px))
                    .map_err(JournalError::Gateway)?;
                self.orders.insert(order, o);
                Ok(None)
            }
            Input::Close { order, state, .. } => {
                if !matches!(
                    state,
                    OrderState::Cancelled | OrderState::Rejected | OrderState::Expired
                ) {
                    return Err(JournalError::BadClose(state));
                }
                let mut o = *self
                    .orders
                    .get(&order)
                    .ok_or(JournalError::UnknownOrder(order))?;
                o.transition(state).map_err(JournalError::Lifecycle)?;
                self.gw.on_closed(order).map_err(JournalError::Gateway)?;
                self.orders.insert(order, o);
                Ok(None)
            }
            Input::Kill { .. } => {
                self.gw.engage_kill_switch();
                Ok(None)
            }
            Input::NewDay { .. } => {
                self.gw.new_day();
                Ok(None)
            }
            Input::LossCheck { .. } => {
                self.gw.check_loss_limits();
                Ok(None)
            }
            Input::Budgets { budgets, .. } => {
                self.gw.set_budgets(budgets);
                Ok(None)
            }
        }
    }

    /// Apply `input`, then write its record. Used for every live change.
    fn apply(&mut self, input: Input) -> Result<Option<Decision>, JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        let outcome = self.step(input.clone())?;
        self.append(&Record::Event { input, outcome })?;
        Ok(outcome)
    }

    /// Give the gateway the last price of `instrument`. Not written until a decision needs it.
    pub fn mark(&mut self, instrument: InstrumentId, px: Px) {
        self.gw.mark(instrument, px);
    }

    /// Write the marks of instruments holding a position that have moved since they were written.
    pub fn sync_marks(&mut self) -> Result<(), JournalError> {
        for i in 0..self.gw.instruments() as u32 {
            let m = self.gw.mark_of(i);
            if self.gw.position(i) != 0 && m != self.logged[i as usize] {
                self.apply(Input::Mark {
                    instrument: i,
                    px: m,
                })?;
            }
        }
        Ok(())
    }

    /// Put an intent to the gateway at event time `now`. The answer is in the ledger when this
    /// returns; an accepted order is `Pending` until [`Journal::ack`].
    pub fn decide(&mut self, intent: &Intent, now: Nanos) -> Result<Decision, JournalError> {
        self.sync_marks()?;
        Ok(self
            .apply(Input::Decide {
                intent: *intent,
                now,
            })?
            .expect("a decision has an answer"))
    }

    /// The broker acknowledged the order.
    pub fn ack(&mut self, order: OrderId, ts: Nanos) -> Result<(), JournalError> {
        self.apply(Input::Ack { order, ts }).map(|_| ())
    }

    /// A fill reported for the order (it must have been acknowledged).
    pub fn fill(
        &mut self,
        order: OrderId,
        qty: u32,
        px: Px,
        ts: Nanos,
    ) -> Result<(), JournalError> {
        self.apply(Input::Fill {
            order,
            qty,
            px: px.raw(),
            ts,
        })
        .map(|_| ())
    }

    /// The order ended without filling the rest: `Cancelled`, `Rejected` or `Expired`.
    pub fn close(
        &mut self,
        order: OrderId,
        state: OrderState,
        ts: Nanos,
    ) -> Result<(), JournalError> {
        self.apply(Input::Close { order, state, ts }).map(|_| ())
    }

    pub fn engage_kill_switch(&mut self, ts: Nanos) -> Result<(), JournalError> {
        self.apply(Input::Kill { ts }).map(|_| ())
    }

    /// Put budgets in force from here on, or take them away (`None`). Written to the ledger like any
    /// other input, so a replay enforces exactly what the live run did.
    pub fn set_budgets(&mut self, budgets: Option<Budgets>, ts: Nanos) -> Result<(), JournalError> {
        self.apply(Input::Budgets { budgets, ts }).map(|_| ())
    }

    /// Check every strategy's loss against its limits (after bringing the marks up to date) and
    /// return the limits newly crossed. A strategy past its soft limit has its opens refused; one
    /// past its hard limit should be flattened (`gateway().flatten_plan(strategy)`). The check is
    /// written to the ledger only when it crossed something, since otherwise it changed nothing.
    pub fn check_loss_limits(&mut self, ts: Nanos) -> Result<Vec<LossEvent>, JournalError> {
        if self.poisoned {
            return Err(JournalError::Poisoned);
        }
        self.sync_marks()?;
        let events = self.gw.check_loss_limits();
        if !events.is_empty() {
            self.append(&Record::Event {
                input: Input::LossCheck { ts },
                outcome: None,
            })?;
        }
        Ok(events)
    }

    pub fn new_day(&mut self, ts: Nanos) -> Result<(), JournalError> {
        self.sync_marks()?;
        self.apply(Input::NewDay { ts }).map(|_| ())
    }

    pub fn gateway(&self) -> &Gateway {
        &self.gw
    }

    /// The gateway's state as comparable data.
    pub fn snapshot(&self) -> GatewaySnapshot {
        self.gw.snapshot()
    }

    pub fn order(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Every order ever accepted, by id.
    pub fn orders(&self) -> impl Iterator<Item = &Order> {
        self.orders.values()
    }

    /// Orders not yet finished (pending, working or part filled), by id.
    pub fn open_orders(&self) -> Vec<Order> {
        self.orders
            .values()
            .filter(|o| !o.state().is_terminal())
            .copied()
            .collect()
    }

    /// Records written, including the start record.
    pub fn records(&self) -> u64 {
        self.seq
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn into_store(self) -> S {
        self.store
    }

    pub fn store(&self) -> &S {
        &self.store
    }
}
