use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px, SymbolTable, TierChange};
use tf_engine::{Promoter, PromoterConfig, ScannerConfig, Tier0};
use tf_ledger::{Journal, JournalError, LedgerStore};
use tf_risk::{Budgets, Limits, LossTier};
use tf_strategy::broker::{Broker, BrokerEvent, CancelOutcome, Kind, Submission};
use tf_strategy::intent::{Intent, IntentId, Pricing, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{Decision, OrderId, OrderState, OrderUpdate};
use tf_strategy::sim::{SimBroker, SimConfig};
use tf_strategy::{Market, Members};
use tf_universe::{RefInfo, Selection, Selector, Snapshot, Tier0View, select};

use crate::def::{Certificate, Route, StrategyDef};
use crate::equiv::{Answer, Log, Rec};
use crate::runner::DynRunner;

/// Intents the host makes itself (flattening) are numbered from here, apart from the strategies' own.
const HOST_SEQ_BASE: u64 = 1 << 48;
/// The `reason` code on the intents the host makes to flatten a strategy.
pub const REASON_FLATTEN: u16 = 0xF1A7;

#[derive(Clone)]
pub struct HostConfig {
    pub id_space: usize,
    pub limits: Limits,
    /// Each strategy's sub-account: a node of this tree. A strategy not in it is refused.
    pub budgets: Option<Budgets>,
    pub promoter: PromoterConfig,
    pub scanner: ScannerConfig,
    pub sim: SimConfig,
    /// A certificate must be for a replay of at least this many events.
    pub min_certified_events: u64,
    /// Event time the ledger starts at.
    pub start_ts: Nanos,
}

/// What is known about each symbol before the session: the names of the instrument ids and the snapshot
/// the universes are judged from.
#[derive(Clone)]
pub struct Reference {
    pub symbols: SymbolTable,
    pub snapshot: Snapshot,
}

#[derive(Debug)]
pub enum HostError {
    /// The ledger could not be written or no longer agrees with itself: stop and restart from it.
    Ledger(JournalError),
    /// The host was stopped by an earlier ledger failure.
    Stopped,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Ledger(e) => write!(f, "ledger: {e}"),
            HostError::Stopped => write!(f, "the host stopped after a ledger failure"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<JournalError> for HostError {
    fn from(e: JournalError) -> Self {
        HostError::Ledger(e)
    }
}

/// Why a strategy was not admitted.
#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    /// The certificate is missing the proof: not made by `certify`, altered, for another strategy
    /// (or the same one with other parameters or universe), or from too short a replay.
    NotCertified(String),
    /// Budgets are in force and the strategy is not in the tree: it has no sub-account.
    NoBudget(u16),
    Duplicate(u16),
    /// Its universe cannot be judged from the reference snapshot (a column it needs is absent).
    Universe(String),
    /// Names in its selected universe that the host's symbol table does not know.
    UnknownSymbols(Vec<String>),
    /// The route is the paper broker and the host has none.
    NoPaperBroker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Its code panicked. The message is what it said.
    Panicked(String),
    /// It crossed its soft loss limit: no more events or reviews; the gateway refuses its opens.
    SoftLoss,
    /// It crossed its hard loss limit: flattened.
    HardLoss,
    /// An operator killed it: flattened.
    Killed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotState {
    Running,
    Stopped(StopReason),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StrategyStats {
    pub intents: u64,
    pub accepted: u64,
    pub rejected_by_gateway: u64,
    pub refused_by_broker: u64,
    pub rate_limited: u64,
    pub unanswered: u64,
    pub filled_shares: u64,
    pub flatten_orders: u64,
    pub panics: u64,
}

struct Slot {
    id: u16,
    name: String,
    runner: Box<dyn DynRunner>,
    selector: Option<Selector>,
    candidates: Vec<InstrumentId>,
    state: SlotState,
    route: Route,
    /// Being flattened: re-checked every second until nothing is left.
    flattening: bool,
    stats: StrategyStats,
}

impl Slot {
    fn alive(&self) -> bool {
        !matches!(self.state, SlotState::Stopped(StopReason::Panicked(_)))
    }
}

/// Run `f` on a slot's runner. A panic stops that strategy and nothing else.
fn guarded(slot: &mut Slot, f: impl FnOnce(&mut dyn DynRunner)) -> bool {
    if !slot.alive() {
        return false;
    }
    match catch_unwind(AssertUnwindSafe(|| f(slot.runner.as_mut()))) {
        Ok(()) => false,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_owned());
            slot.state = SlotState::Stopped(StopReason::Panicked(msg));
            slot.stats.panics += 1;
            true
        }
    }
}

struct Fnv(u64);

impl Fnv {
    fn put(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

pub struct Host<S: LedgerStore> {
    cfg: HostConfig,
    reference: Reference,
    refs: Vec<RefInfo>,
    tier0: Tier0,
    promoter: Promoter,
    journal: Journal<S>,
    sim: SimBroker,
    paper: Option<Box<dyn Broker>>,
    slots: Vec<Slot>,
    /// Strategy, instrument and route of each order the gateway accepted.
    orders: BTreeMap<OrderId, (u16, InstrumentId, Route)>,
    /// Orders not yet ended, and how many each (strategy, instrument) has.
    working: BTreeSet<OrderId>,
    working_by: BTreeMap<(u16, InstrumentId), u32>,
    /// What the promoter is told each strategy holds, to change only what changed.
    pinned: BTreeSet<(u16, InstrumentId)>,
    touched: BTreeSet<(u16, InstrumentId)>,
    /// Strategies whose code panicked and that are not yet being flattened.
    broke: Vec<u16>,
    tier_tape: Vec<TierChange>,
    host_seq: u64,
    now: Nanos,
    last_check_sec: u64,
    events: u64,
    intents: u64,
    accepted: u64,
    ledger_refusals: u64,
    anomalies: Vec<String>,
    hash: Fnv,
    failed: bool,
    log: Option<Log>,
}

impl<S: LedgerStore> Host<S> {
    /// A host over `store` (a new ledger, or one to continue). Strategies are added with
    /// [`Host::add_strategy`].
    pub fn new(cfg: HostConfig, reference: Reference, store: S) -> Result<Host<S>, HostError> {
        let (mut journal, _) = Journal::open(store, cfg.limits, cfg.id_space)?;
        if let Some(b) = &cfg.budgets {
            journal.set_budgets(Some(b.clone()), cfg.start_ts)?;
        }
        let mut refs = vec![RefInfo::default(); cfg.id_space];
        for row in &reference.snapshot.rows {
            if let Some(id) = reference.symbols.get(&row.symbol) {
                if let Some(r) = refs.get_mut(id as usize) {
                    *r = RefInfo {
                        price: row.price,
                        adv_shares: row.adv_shares,
                    };
                }
            }
        }
        let promoter = Promoter::new(cfg.promoter, cfg.scanner, cfg.id_space)
            .map_err(|e| HostError::Ledger(JournalError::Structure(e.0.to_owned())))?;
        Ok(Host {
            tier0: Tier0::new(cfg.id_space),
            sim: SimBroker::new(cfg.sim, cfg.id_space),
            refs,
            promoter,
            journal,
            paper: None,
            slots: Vec::new(),
            orders: BTreeMap::new(),
            working: BTreeSet::new(),
            working_by: BTreeMap::new(),
            pinned: BTreeSet::new(),
            touched: BTreeSet::new(),
            broke: Vec::new(),
            tier_tape: Vec::new(),
            host_seq: 0,
            now: cfg.start_ts,
            last_check_sec: 0,
            events: 0,
            intents: 0,
            accepted: 0,
            ledger_refusals: 0,
            anomalies: Vec::new(),
            hash: Fnv(0xcbf2_9ce4_8422_2325),
            failed: false,
            log: None,
            reference,
            cfg,
        })
    }

    /// Keep a log of every decision, tier change, fill and action (see [`crate::equiv`]). Call before
    /// adding strategies.
    pub fn record(mut self) -> Self {
        self.log = Some(Log::new(self.cfg.id_space, &self.reference.symbols));
        self
    }

    /// The decision log so far, if recording.
    pub fn log(&self) -> Option<&Log> {
        self.log.as_ref()
    }

    fn note(&mut self, ts: Nanos, rec: impl FnOnce(u64, Nanos) -> Rec) {
        let idx = self.events;
        if let Some(l) = self.log.as_mut() {
            l.recs.push(rec(idx, ts));
        }
    }

    fn note_action(&mut self, ts: Nanos, what: String) {
        self.note(ts, |idx, ts| Rec::Action { idx, ts, what });
    }

    fn log_tiers(&mut self, from: usize) {
        if self.log.is_none() {
            return;
        }
        let idx = self.events;
        let new: Vec<Rec> = self.tier_tape[from.min(self.tier_tape.len())..]
            .iter()
            .map(|c| Rec::Tier {
                idx,
                ts: c.hdr.ts_recv,
                instrument: c.hdr.instrument,
                promote: c.action == tf_core::TierAction::Promote,
                reason: c.reason,
                score: c.score,
            })
            .collect();
        if let Some(l) = self.log.as_mut() {
            l.recs.extend(new);
        }
    }

    /// The broker for strategies routed to [`Route::Paper`].
    pub fn with_paper(mut self, broker: Box<dyn Broker>) -> Self {
        self.paper = Some(broker);
        self
    }

    /// Add a strategy. It must come with a certificate that a replay on a tape was made of exactly
    /// this strategy (see [`crate::certify`]); the host refuses one that has not.
    pub fn add_strategy(
        &mut self,
        def: &StrategyDef,
        cert: &Certificate,
    ) -> Result<(), AdmitError> {
        let why = if !cert.is_intact() {
            Some("the certificate has been altered".to_owned())
        } else if cert.strategy_fp != def.fingerprint() {
            Some("the certificate is for another strategy, or this one with other parameters or universe".to_owned())
        } else if cert.events < self.cfg.min_certified_events.max(1) {
            Some(format!(
                "the replay had {} events, the host wants at least {}",
                cert.events,
                self.cfg.min_certified_events.max(1)
            ))
        } else {
            None
        };
        if let Some(w) = why {
            return Err(AdmitError::NotCertified(w));
        }
        self.install(def)
    }

    /// Add without a certificate: for the replay that makes one.
    pub(crate) fn install(&mut self, def: &StrategyDef) -> Result<(), AdmitError> {
        if self.slots.iter().any(|s| s.id == def.id) {
            return Err(AdmitError::Duplicate(def.id));
        }
        if let Some(b) = self.journal.gateway().budgets() {
            if !b.ids().contains_key(&def.id) {
                return Err(AdmitError::NoBudget(def.id));
            }
        }
        if def.route == Route::Paper && self.paper.is_none() {
            return Err(AdmitError::NoPaperBroker);
        }
        let selection: Selection = select(&def.universe, &self.reference.snapshot)
            .map_err(|e| AdmitError::Universe(e.to_string()))?;
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for s in &selection.symbols {
            match self.reference.symbols.get(s) {
                Some(id) if (id as usize) < self.cfg.id_space => candidates.push(id),
                _ => unknown.push(s.clone()),
            }
        }
        if !unknown.is_empty() {
            return Err(AdmitError::UnknownSymbols(unknown));
        }
        candidates.sort_unstable();
        let mut runner = (def.build)();
        let selector = Selector::new(&def.universe);
        if selector.is_none() {
            *runner.members_mut() = Members::from_ids(candidates.iter().copied());
        }
        self.promoter.set_priority(def.id, def.priority);
        let (now, fp) = (self.now, def.fingerprint());
        self.note_action(now, format!("add {} {fp:016x}", def.id));
        self.slots.push(Slot {
            id: def.id,
            name: def.name.clone(),
            runner,
            selector,
            candidates,
            state: SlotState::Running,
            route: def.route,
            flattening: false,
            stats: StrategyStats::default(),
        });
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn install_for_test(&mut self, def: &StrategyDef) {
        self.install(def).unwrap();
    }

    fn slot_index(&self, id: u16) -> Option<usize> {
        self.slots.iter().position(|s| s.id == id)
    }

    fn broker(&mut self, route: Route) -> &mut dyn Broker {
        match route {
            Route::Sim => &mut self.sim,
            Route::Paper => self
                .paper
                .as_mut()
                .expect("a paper route needs a paper broker")
                .as_mut(),
        }
    }

    /// One market event. In order: the brokers see it and what they did is recorded and told to the
    /// strategies; marks; Tier 0 and the promoter; each running strategy; dynamic universes;
    /// what the strategies asked for goes through the gateway to its broker; once a second the loss
    /// limits are looked at and strategies being flattened are pushed on.
    pub fn on_event(&mut self, ev: &Event) -> Result<(), HostError> {
        if self.failed {
            return Err(HostError::Stopped);
        }
        let r = self.step(ev);
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    fn step(&mut self, ev: &Event) -> Result<(), HostError> {
        if matches!(ev, Event::ParamChange(_) | Event::TierChange(_)) {
            return Ok(());
        }
        let ts = ev.ts_recv();
        self.now = self.now.max(ts);
        self.events += 1;
        self.sim.observe(ev);
        if let Some(p) = self.paper.as_mut() {
            p.observe(ev);
        }
        self.settle()?;
        if let Event::Trade(t) = ev {
            self.journal.mark(t.hdr.instrument, t.px);
        }
        self.tier0.on_event(ev);
        let tier_before = self.tier_tape.len();
        self.promoter.on_event(&self.tier0, ev, &mut self.tier_tape);
        self.log_tiers(tier_before);
        for slot in self.slots.iter_mut() {
            if slot.state != SlotState::Running {
                continue;
            }
            let (tier0, refs) = (&self.tier0, &self.refs[..]);
            let promoter = &mut self.promoter;
            if guarded(slot, |r| {
                r.on_event(Market { tier0, refs }, Some(promoter), ev)
            }) {
                self.broke.push(slot.id);
                continue;
            }
            if let Some(sel) = slot.selector.as_mut() {
                let view = Tier0View { tier0, refs };
                if let Some(change) = sel.update(ts, &view, &slot.candidates) {
                    slot.runner.members_mut().apply(&change);
                }
            }
        }
        self.collect(ts)?;
        self.revocations(ts);
        self.flatten_the_broken(ts)?;
        let sec = ts / NANOS_PER_SEC;
        if sec > self.last_check_sec {
            self.last_check_sec = sec;
            self.per_second(ts)?;
        }
        self.update_holds();
        Ok(())
    }

    /// A strategy whose code panicked is stopped alone, and what it held is closed.
    fn flatten_the_broken(&mut self, ts: Nanos) -> Result<(), HostError> {
        while let Some(id) = self.broke.pop() {
            if let Some(k) = self.slot_index(id) {
                self.anomalies
                    .push(format!("strategy {id} stopped: {:?}", self.slots[k].state));
            }
            self.begin_flatten(id, ts)?;
        }
        Ok(())
    }

    /// What strategies asked for, in strategy order, goes through the gateway.
    fn collect(&mut self, ts: Nanos) -> Result<(), HostError> {
        let tier_before = self.tier_tape.len();
        let mut batch: Vec<Intent> = Vec::new();
        for slot in &mut self.slots {
            if slot.alive() {
                batch.extend(slot.runner.drain_intents());
                self.tier_tape.extend(slot.runner.drain_tier_events());
            }
        }
        self.log_tiers(tier_before);
        for i in batch {
            // A strategy that has been stopped cannot get an order out by way of what it left behind.
            let running = self
                .slot_index(i.id.strategy.0)
                .is_some_and(|k| self.slots[k].state == SlotState::Running);
            if running {
                self.submit(i, ts)?;
            }
        }
        Ok(())
    }

    /// Interests lost to an eviction are told to the strategies that had them.
    fn revocations(&mut self, _ts: Nanos) {
        for (owner, id) in self.promoter.drain_revoked() {
            let Some(k) = self.slot_index(owner) else {
                continue;
            };
            let (tier0, refs) = (&self.tier0, &self.refs[..]);
            let promoter = &mut self.promoter;
            let slot = &mut self.slots[k];
            if slot.state == SlotState::Running
                && guarded(slot, |r| {
                    r.on_tier1_revoked(Market { tier0, refs }, Some(promoter), id)
                })
            {
                self.broke.push(owner);
            }
        }
    }

    fn per_second(&mut self, ts: Nanos) -> Result<(), HostError> {
        for e in self.journal.check_loss_limits(ts)? {
            let Some(k) = self.slot_index(e.strategy) else {
                continue;
            };
            let slot = &mut self.slots[k];
            match e.tier {
                LossTier::Soft => {
                    if slot.state == SlotState::Running {
                        slot.state = SlotState::Stopped(StopReason::SoftLoss);
                    }
                    self.anomalies.push(format!(
                        "strategy {} crossed its soft loss limit",
                        e.strategy
                    ));
                    self.cancel_opens(e.strategy, ts)?;
                }
                LossTier::Hard => {
                    if slot.alive() {
                        slot.state = SlotState::Stopped(StopReason::HardLoss);
                    }
                    self.anomalies.push(format!(
                        "strategy {} crossed its hard loss limit",
                        e.strategy
                    ));
                    self.begin_flatten(e.strategy, ts)?;
                }
            }
        }
        let flattening: Vec<u16> = self
            .slots
            .iter()
            .filter(|s| s.flattening)
            .map(|s| s.id)
            .collect();
        for id in flattening {
            self.flatten(id, ts)?;
        }
        self.journal.sync_marks()?;
        Ok(())
    }

    /// Send an intent through the gateway to its broker.
    fn submit(&mut self, intent: Intent, ts: Nanos) -> Result<(), HostError> {
        let s = intent.id.strategy.0;
        let Some(k) = self.slot_index(s) else {
            return Ok(());
        };
        self.intents += 1;
        self.slots[k].stats.intents += 1;
        let route = self.slots[k].route;
        let decision = self.journal.decide(&intent, intent.ts)?;
        self.hash.put(u64::from(s));
        self.hash.put(intent.id.seq);
        let answer = match decision {
            Decision::Accepted(o) => Answer::Accepted(o.0),
            Decision::Rejected(r) => Answer::Rejected(tf_risk::reason_name(&r).to_owned()),
        };
        self.note(intent.ts, |idx, ts| Rec::Decision {
            idx,
            ts,
            strategy: s,
            seq: intent.id.seq,
            instrument: intent.instrument,
            side: intent.side,
            qty: intent.qty,
            purpose: intent.purpose,
            limit: intent.limit_price().raw(),
            reason: intent.reason,
            answer,
        });
        match decision {
            Decision::Rejected(r) => {
                self.hash.put(0);
                self.slots[k].stats.rejected_by_gateway += 1;
                let u = OrderUpdate::rejected(intent.id, r, intent.ts);
                self.deliver(k, &u);
            }
            Decision::Accepted(order) => {
                self.hash.put(1 + order.0);
                self.accepted += 1;
                self.slots[k].stats.accepted += 1;
                self.orders.insert(order, (s, intent.instrument, route));
                self.working.insert(order);
                *self.working_by.entry((s, intent.instrument)).or_insert(0) += 1;
                self.touched.insert((s, intent.instrument));
                match self.broker(route).place(&intent, order) {
                    Submission::Accepted => {}
                    Submission::Refused { .. } => {
                        self.slots[k].stats.refused_by_broker += 1;
                        self.close(order, OrderState::Rejected, ts)?;
                    }
                    Submission::RateLimited { .. } => {
                        self.slots[k].stats.rate_limited += 1;
                        self.close(order, OrderState::Rejected, ts)?;
                    }
                    // It may exist. It stays working, and is neither closed nor sent again.
                    Submission::Unknown => self.slots[k].stats.unanswered += 1,
                }
            }
        }
        Ok(())
    }

    fn close(&mut self, order: OrderId, state: OrderState, ts: Nanos) -> Result<(), HostError> {
        match self.journal.close(order, state, ts) {
            Ok(()) => self.after_change(order, ts),
            Err(e) => self.refused(e, order),
        }
    }

    /// A ledger error that is about one order (the book would not take it) is counted and noted; one
    /// about the ledger itself stops the host.
    fn refused(&mut self, e: JournalError, order: OrderId) -> Result<(), HostError> {
        match e {
            JournalError::Lifecycle(_)
            | JournalError::UnknownOrder(_)
            | JournalError::BadClose(_)
            | JournalError::Gateway(_) => {
                self.ledger_refusals += 1;
                self.anomalies.push(format!(
                    "the ledger would not take what happened to order {}: {e}",
                    order.0
                ));
                Ok(())
            }
            other => Err(HostError::Ledger(other)),
        }
    }

    /// Everything the brokers did since the last call is recorded and told to the strategy.
    fn settle(&mut self) -> Result<(), HostError> {
        let mut evs: Vec<BrokerEvent> = self.sim.take_events();
        if let Some(p) = self.paper.as_mut() {
            evs.extend(p.take_events());
        }
        for e in evs {
            let r = match e.kind {
                Kind::Ack => self.journal.ack(e.order, e.ts),
                Kind::Fill { qty, px } => {
                    self.hash.put(e.order.0);
                    self.hash.put(u64::from(qty));
                    self.hash.put(px.raw() as u64);
                    self.hash.put(e.ts);
                    if let Some(&(_, instrument, _)) = self.orders.get(&e.order) {
                        self.note(e.ts, |idx, ts| Rec::Fill {
                            idx,
                            ts,
                            order: e.order.0,
                            instrument,
                            qty,
                            px: px.raw(),
                        });
                    }
                    if let Some((s, _, _)) = self.orders.get(&e.order) {
                        if let Some(k) = self.slot_index(*s) {
                            self.slots[k].stats.filled_shares += u64::from(qty);
                        }
                    }
                    self.journal.fill(e.order, qty, px, e.ts)
                }
                Kind::Close(state) => self.journal.close(e.order, state, e.ts),
                Kind::LegFill { .. } => {
                    self.anomalies.push(format!(
                        "a protective leg of order {} filled; the ledger cannot record it yet",
                        e.order.0
                    ));
                    continue;
                }
            };
            match r {
                Ok(()) => self.after_change(e.order, e.ts)?,
                Err(err) => self.refused(err, e.order)?,
            }
        }
        Ok(())
    }

    /// An order changed in the ledger: tell its strategy, and keep the books of what is working.
    fn after_change(&mut self, order: OrderId, ts: Nanos) -> Result<(), HostError> {
        let Some(o) = self.journal.order(order).copied() else {
            return Ok(());
        };
        let Some(&(s, inst, _)) = self.orders.get(&order) else {
            return Ok(());
        };
        self.touched.insert((s, inst));
        if o.state().is_terminal() && self.working.remove(&order) {
            if let Some(n) = self.working_by.get_mut(&(s, inst)) {
                *n -= 1;
                if *n == 0 {
                    self.working_by.remove(&(s, inst));
                }
            }
        }
        if let Some(k) = self.slot_index(s) {
            let mut u = o.update(ts);
            if o.state() == OrderState::Rejected {
                u.reject = Some(tf_strategy::lifecycle::RejectReason::Broker);
            }
            self.deliver(k, &u);
        }
        Ok(())
    }

    fn deliver(&mut self, k: usize, u: &OrderUpdate) {
        let (tier0, promoter) = (&self.tier0, &mut self.promoter);
        let slot = &mut self.slots[k];
        if guarded(slot, |r| r.on_order_update(tier0, Some(promoter), u)) {
            self.broke.push(slot.id);
        }
    }

    /// A strategy holds a symbol in Tier 1 while it has a position or an order working in it.
    fn update_holds(&mut self) {
        let touched = std::mem::take(&mut self.touched);
        for (s, i) in touched {
            let held = self.working_by.contains_key(&(s, i))
                || self.journal.gateway().strategy_position(s, i) != 0;
            let was = self.pinned.contains(&(s, i));
            if held && !was {
                self.promoter.pin(s, i);
                self.pinned.insert((s, i));
            } else if !held && was {
                self.promoter.unpin(s, i);
                self.pinned.remove(&(s, i));
            }
        }
    }

    // ---- stopping and flattening ----

    fn cancel_opens(&mut self, strategy: u16, ts: Nanos) -> Result<(), HostError> {
        let plan = self.journal.gateway().flatten_plan(strategy);
        for order in plan.cancels {
            self.cancel(order, ts);
        }
        Ok(())
    }

    fn cancel(&mut self, order: OrderId, ts: Nanos) {
        let Some(&(_, _, route)) = self.orders.get(&order) else {
            return;
        };
        match self.broker(route).cancel_order(order, ts) {
            CancelOutcome::Requested | CancelOutcome::Finished => {}
            CancelOutcome::Unknown => self
                .anomalies
                .push(format!("a cancel of order {} got no answer", order.0)),
        }
    }

    fn begin_flatten(&mut self, strategy: u16, ts: Nanos) -> Result<(), HostError> {
        if let Some(k) = self.slot_index(strategy) {
            self.slots[k].flattening = true;
        }
        self.flatten(strategy, ts)
    }

    /// Cancel what a strategy has working to open, and close what it holds: marketable limits within
    /// 10% of the last price, which pass the gateway whatever the strategy's state. Run again every
    /// second until nothing is left, since one attempt may fill in part.
    fn flatten(&mut self, strategy: u16, ts: Nanos) -> Result<(), HostError> {
        let plan = self.journal.gateway().flatten_plan(strategy);
        if plan.closes.is_empty() && plan.cancels.is_empty() {
            if let Some(k) = self.slot_index(strategy) {
                if self.slots[k].flattening && !self.working_by.keys().any(|(s, _)| *s == strategy)
                {
                    self.slots[k].flattening = false;
                }
            }
            return Ok(());
        }
        for order in plan.cancels {
            self.cancel(order, ts);
        }
        for (inst, side, qty) in plan.closes {
            let mark = self.journal.gateway().mark_of(inst);
            if mark <= 0 {
                self.anomalies.push(format!(
                    "cannot flatten strategy {strategy} in instrument {inst}: no price"
                ));
                continue;
            }
            self.host_seq += 1;
            let intent = Intent {
                id: IntentId {
                    strategy: StrategyId(strategy),
                    seq: HOST_SEQ_BASE + self.host_seq,
                },
                instrument: inst,
                side,
                qty,
                purpose: Purpose::Close,
                pricing: Pricing::Collar {
                    reference: Px::from_raw(mark),
                    collar_permille: 100,
                },
                protect: None,
                tif: Tif::Day,
                ts,
                reason: REASON_FLATTEN,
            };
            if let Some(k) = self.slot_index(strategy) {
                self.slots[k].stats.flatten_orders += 1;
            }
            let _ = Side::Buy;
            self.submit(intent, ts)?;
        }
        Ok(())
    }

    /// An operator stops one strategy: its working opens are cancelled and what it holds is closed.
    /// The others are not touched.
    pub fn kill_strategy(&mut self, id: u16, ts: Nanos) -> Result<bool, HostError> {
        let Some(k) = self.slot_index(id) else {
            return Ok(false);
        };
        self.note_action(ts, format!("kill_strategy {id}"));
        if self.slots[k].alive() {
            self.slots[k].state = SlotState::Stopped(StopReason::Killed);
        }
        self.begin_flatten(id, ts)?;
        self.update_holds();
        Ok(true)
    }

    /// The kill switch: the gateway refuses every opening order from now on, and what is working to
    /// open is cancelled. Positions are left (closes still pass) for the strategies to exit.
    pub fn kill_switch(&mut self, ts: Nanos) -> Result<(), HostError> {
        self.note_action(ts, "kill_switch".to_owned());
        self.journal.engage_kill_switch(ts)?;
        let ids: Vec<u16> = self.slots.iter().map(|s| s.id).collect();
        for id in ids {
            self.cancel_opens(id, ts)?;
        }
        Ok(())
    }

    /// The session ended: what is still working expires.
    pub fn end_of_day(&mut self, ts: Nanos) -> Result<(), HostError> {
        self.note_action(ts, "end_of_day".to_owned());
        self.now = self.now.max(ts);
        self.sim.close_day(ts);
        if let Some(p) = self.paper.as_mut() {
            p.close_day(ts);
        }
        self.settle()?;
        self.update_holds();
        Ok(())
    }

    // ---- reading the host ----

    pub fn strategies(&self) -> Vec<(u16, &str, &SlotState, StrategyStats)> {
        self.slots
            .iter()
            .map(|s| (s.id, s.name.as_str(), &s.state, s.stats))
            .collect()
    }

    pub fn state_of(&self, id: u16) -> Option<&SlotState> {
        self.slot_index(id).map(|k| &self.slots[k].state)
    }

    pub fn stats_of(&self, id: u16) -> Option<StrategyStats> {
        self.slot_index(id).map(|k| self.slots[k].stats)
    }

    /// The members of a strategy now.
    pub fn members_of(&self, id: u16) -> Option<Vec<InstrumentId>> {
        self.slot_index(id)
            .map(|k| self.slots[k].runner.members().iter().collect())
    }

    pub fn reviews_of(&self, id: u16) -> Option<u64> {
        self.slot_index(id).map(|k| self.slots[k].runner.reviews())
    }

    pub fn journal(&self) -> &Journal<S> {
        &self.journal
    }

    pub fn promoter(&self) -> &Promoter {
        &self.promoter
    }

    pub fn tier0(&self) -> &Tier0 {
        &self.tier0
    }

    pub fn sim(&self) -> &SimBroker {
        &self.sim
    }

    /// The tier changes made since the last call, in order: what goes on the tape.
    pub fn drain_tier_events(&mut self) -> Vec<TierChange> {
        std::mem::take(&mut self.tier_tape)
    }

    pub fn anomalies(&self) -> &[String] {
        &self.anomalies
    }

    /// Things the ledger would not take (it is told, never forced).
    pub fn ledger_refusals(&self) -> u64 {
        self.ledger_refusals
    }

    pub fn events(&self) -> u64 {
        self.events
    }

    pub fn intents(&self) -> u64 {
        self.intents
    }

    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// A hash of every decision and fill so far.
    pub fn outcome_hash(&self) -> u64 {
        self.hash.0
    }

    pub fn now(&self) -> Nanos {
        self.now
    }

    /// Orders accepted by the gateway and not yet ended.
    pub fn working_orders(&self) -> Vec<OrderId> {
        self.working.iter().copied().collect()
    }
}
