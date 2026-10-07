use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px, SymbolTable, TierChange};
use tf_engine::{BarClose, MtfConfig, Promoter, PromoterConfig, ScannerConfig, SharedBars, Tier0};
use tf_ledger::{Journal, JournalError, LedgerStore};
use tf_risk::{Budgets, Limits, LossTier};
use tf_strategy::broker::{Broker, BrokerEvent, CancelOutcome, Kind, Submission};
use tf_strategy::intent::{Intent, IntentId, Pricing, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{Decision, OrderId, OrderState, OrderUpdate};
use tf_strategy::sim::{Borrow, SimBroker, SimConfig};
use tf_strategy::{Market, Members};
use tf_universe::{RefInfo, Selection, Selector, Snapshot, StaticFeature, Tier0View, select};

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
    /// Bars shared by the strategies (E19-S03); `None` for a host whose strategies need none, in which case
    /// their `track_bars` says it is not configured.
    pub bars: Option<BarsConfig>,
    /// The session boundaries of the day the host starts on (`tf_calendar::Calendar::times`), given to Tier 0 at once;
    /// `None` leaves Tier 0 with its day-wide figures until [`Host::start_day`]. A replay applies the same configuration,
    /// so it sees the sessions the run it checks saw.
    pub day: Option<tf_calendar::SessionTimes>,
}

/// The engine's shared multi-timeframe bars: how they are aligned and how many symbols may be tracked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarsConfig {
    pub mtf: MtfConfig,
    /// A hard bound; about 40 KB a symbol.
    pub max_tracked: usize,
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
    universe_fp: u64,
    /// Every symbol a dynamic universe has ever held (empty for a static one).
    ever: BTreeSet<InstrumentId>,
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
    bars: Option<SharedBars>,
    /// Reused so that closing bars allocates nothing; the closes themselves are not used by the host.
    bar_closes: Vec<BarClose>,
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
    // What the daily report reads (E18-S07).
    reasons: BTreeMap<u16, BTreeMap<&'static str, u64>>,
    /// What the brokers refused, by strategy: whether it was a short sale, the reason, how many.
    broker_refusals: BTreeMap<u16, BTreeMap<(bool, String), u64>>,
    fills_by: BTreeMap<(u16, InstrumentId), (u64, u128)>,
    fill_counts: BTreeMap<u16, u64>,
    per_second: BTreeMap<u64, u32>,
    lag_hist: [u64; 65],
    first_ts: Option<Nanos>,
    gaps: Vec<GapNote>,
    fill_log: Option<FillLog>,
}

/// One execution and what the order it belongs to was for (E19-S13): what a research run needs to turn fills into
/// round trips. Prices are raw (1e-9 dollars).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FillNote {
    pub strategy: u16,
    pub instrument: InstrumentId,
    pub order: u64,
    /// The intent's number within its strategy.
    pub seq: u64,
    pub side: Side,
    pub purpose: Purpose,
    /// The strategy's own code for why (an exit's is one of `tf_strategy::exits`' `REASON_*`).
    pub reason: u16,
    pub qty: u32,
    pub px: i64,
    pub ts: Nanos,
    /// The price the intent was nearest to: its limit, or a collar's reference (`Pricing::reference_price`).
    pub reference: i64,
    /// The stop trigger of the intent's protective orders, if it had any.
    pub stop: Option<i64>,
}

#[derive(Default)]
struct FillLog {
    /// What each accepted order was for, until the day ends.
    meta: BTreeMap<OrderId, FillNote>,
    notes: Vec<FillNote>,
}

/// A break the ingest queue reported: events of one kind lost between two times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GapNote {
    pub lost: tf_ingest::Lost,
    pub count: u64,
    pub first_ts: Nanos,
    pub last_ts: Nanos,
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
        // What the snapshot says about borrowing, by instrument: the simulated broker applies the broker's rules for
        // short sales when the snapshot has the flags (from the broker's asset list), and otherwise asks nothing.
        let borrow_known = reference
            .snapshot
            .columns
            .contains(&StaticFeature::EasyToBorrow)
            || reference
                .snapshot
                .columns
                .contains(&StaticFeature::Shortable);
        let mut borrow = vec![Borrow::Unknown; cfg.id_space];
        for row in &reference.snapshot.rows {
            if let Some(id) = reference.symbols.get(&row.symbol) {
                if let Some(r) = refs.get_mut(id as usize) {
                    *r = RefInfo::from_row(row);
                }
                if let Some(b) = borrow.get_mut(id as usize) {
                    *b = match (row.shortable, row.easy_to_borrow) {
                        (Some(false), _) => Borrow::NotShortable,
                        (_, Some(true)) => Borrow::Easy,
                        (_, Some(false)) => Borrow::Hard,
                        _ => Borrow::Unknown,
                    };
                }
            }
        }
        let mut sim = SimBroker::new(cfg.sim, cfg.id_space);
        if borrow_known {
            sim = sim.with_borrow_table(borrow);
        }
        let promoter = Promoter::new(cfg.promoter, cfg.scanner, cfg.id_space)
            .map_err(|e| HostError::Ledger(JournalError::Structure(e.0.to_owned())))?;
        let mut tier0 = Tier0::new(cfg.id_space);
        if let Some(day) = cfg.day {
            tier0.set_day(day);
        }
        Ok(Host {
            tier0,
            bars: cfg
                .bars
                .map(|b| SharedBars::new(b.mtf, cfg.id_space, b.max_tracked)),
            bar_closes: Vec::new(),
            sim,
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
            reasons: BTreeMap::new(),
            broker_refusals: BTreeMap::new(),
            fills_by: BTreeMap::new(),
            fill_counts: BTreeMap::new(),
            per_second: BTreeMap::new(),
            lag_hist: [0; 65],
            first_ts: None,
            gaps: Vec::new(),
            fill_log: None,
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

    /// Keep a note of every execution with what its order was for ([`FillNote`]). Call before the first event.
    pub fn with_fill_log(mut self) -> Self {
        self.fill_log = Some(FillLog::default());
        self
    }

    /// The executions noted since the last call, in the order they were recorded.
    pub fn take_fill_notes(&mut self) -> Vec<FillNote> {
        self.fill_log
            .as_mut()
            .map(|l| std::mem::take(&mut l.notes))
            .unwrap_or_default()
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
            universe_fp: def.universe.fingerprint(),
            ever: BTreeSet::new(),
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
        self.first_ts.get_or_insert(ts);
        *self.per_second.entry(ts / NANOS_PER_SEC).or_insert(0) += 1;
        let lag = ts.saturating_sub(ev.hdr().ts_event);
        self.lag_hist[(64 - lag.leading_zeros()) as usize] += 1;
        self.sim.observe(ev);
        if let Some(p) = self.paper.as_mut() {
            p.observe(ev);
        }
        self.settle()?;
        if let Event::Trade(t) = ev {
            self.journal.mark(t.hdr.instrument, t.px);
        }
        self.tier0.on_event(ev);
        if let Some(b) = self.bars.as_mut() {
            self.bar_closes.clear();
            b.on_event(ev, &mut self.bar_closes);
        }
        let tier_before = self.tier_tape.len();
        self.promoter.on_event(&self.tier0, ev, &mut self.tier_tape);
        self.log_tiers(tier_before);
        for slot in self.slots.iter_mut() {
            if slot.state != SlotState::Running {
                continue;
            }
            let (tier0, refs) = (&self.tier0, &self.refs[..]);
            let promoter = &mut self.promoter;
            let bars = self.bars.as_mut();
            if guarded(slot, |r| {
                r.on_event(Market { tier0, refs }, Some(promoter), bars, ev)
            }) {
                self.broke.push(slot.id);
                continue;
            }
            if let Some(sel) = slot.selector.as_mut() {
                let view = Tier0View { tier0, refs };
                if let Some(change) = sel.update(ts, &view, &slot.candidates) {
                    slot.runner.members_mut().apply(&change);
                    slot.ever.extend(change.entered.iter().copied());
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
            let bars = self.bars.as_mut();
            let slot = &mut self.slots[k];
            if slot.state == SlotState::Running
                && guarded(slot, |r| {
                    r.on_tier1_revoked(Market { tier0, refs }, Some(promoter), bars, id)
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
                    self.release_bars(e.strategy);
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
                *self
                    .reasons
                    .entry(s)
                    .or_default()
                    .entry(tf_risk::reason_name(&r))
                    .or_insert(0) += 1;
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
                if let Some(l) = self.fill_log.as_mut() {
                    l.meta.insert(
                        order,
                        FillNote {
                            strategy: s,
                            instrument: intent.instrument,
                            order: order.0,
                            seq: intent.id.seq,
                            side: intent.side,
                            purpose: intent.purpose,
                            reason: intent.reason,
                            qty: 0,
                            px: 0,
                            ts: 0,
                            reference: intent.pricing.reference_price().raw(),
                            stop: intent.protect.map(|p| p.stop_trigger.raw()),
                        },
                    );
                }
                self.working.insert(order);
                *self.working_by.entry((s, intent.instrument)).or_insert(0) += 1;
                self.touched.insert((s, intent.instrument));
                match self.broker(route).place(&intent, order) {
                    Submission::Accepted => {}
                    Submission::Refused { message, .. } => {
                        self.slots[k].stats.refused_by_broker += 1;
                        *self
                            .broker_refusals
                            .entry(s)
                            .or_default()
                            .entry((intent.side == Side::SellShort, message))
                            .or_insert(0) += 1;
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
                    if let Some(l) = self.fill_log.as_mut() {
                        if let Some(m) = l.meta.get(&e.order) {
                            l.notes.push(FillNote {
                                qty,
                                px: px.raw(),
                                ts: e.ts,
                                ..*m
                            });
                        }
                    }
                    if let Some(&(s, instrument, _)) = self.orders.get(&e.order) {
                        let f = self.fills_by.entry((s, instrument)).or_insert((0, 0));
                        f.0 += u64::from(qty);
                        f.1 += u128::from(qty) * u128::try_from(px.raw()).unwrap_or(0);
                        *self.fill_counts.entry(s).or_insert(0) += 1;
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
        let bars = self.bars.as_mut();
        let slot = &mut self.slots[k];
        if guarded(slot, |r| r.on_order_update(tier0, Some(promoter), bars, u)) {
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

    /// A strategy that has stopped needs no bars: its claims go, and the bars of a symbol nobody else
    /// claims go with them.
    fn release_bars(&mut self, strategy: u16) {
        if let Some(b) = self.bars.as_mut() {
            b.release_all(strategy);
        }
    }

    fn begin_flatten(&mut self, strategy: u16, ts: Nanos) -> Result<(), HostError> {
        self.release_bars(strategy);
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

    /// The shared bars, if the host was configured with them ([`HostConfig::bars`]).
    pub fn bars(&self) -> Option<&SharedBars> {
        self.bars.as_ref()
    }

    /// Start the session-by-session state for a day (premarket, regular session, after-hours) with the
    /// boundaries `tf_calendar::Calendar::times` gives for its date. Until it is called, Tier 0 keeps only its
    /// day-wide figures. The driver calls it before the first event of a day.
    pub fn start_day(&mut self, times: tf_calendar::SessionTimes) {
        self.tier0.set_day(times);
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

    /// The ingest queue lost events between two times: noted for the report.
    pub fn on_gap(&mut self, lost: tf_ingest::Lost, count: u64, first_ts: Nanos, last_ts: Nanos) {
        self.gaps.push(GapNote {
            lost,
            count,
            first_ts,
            last_ts,
        });
    }

    pub fn gaps(&self) -> &[GapNote] {
        &self.gaps
    }

    /// Why the gateway refused a strategy's intents, by reason, with counts.
    pub fn rejections_of(&self, id: u16) -> Vec<(&'static str, u64)> {
        self.reasons
            .get(&id)
            .map(|m| m.iter().map(|(k, v)| (*k, *v)).collect())
            .unwrap_or_default()
    }

    /// What the brokers refused a strategy, with the reason they gave: `(was_a_short_sale, reason, count)`.
    pub fn broker_refusals_of(&self, id: u16) -> Vec<(bool, String, u64)> {
        self.broker_refusals
            .get(&id)
            .map(|m| {
                m.iter()
                    .map(|((short, why), n)| (*short, why.clone(), *n))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Fills a strategy had: how many, shares and dollars (raw price units x shares) traded.
    pub fn fills_of(&self, id: u16) -> (u64, u64, u128) {
        let (shares, notional) = self
            .fills_by
            .iter()
            .filter(|((s, _), _)| *s == id)
            .fold((0u64, 0u128), |a, (_, v)| (a.0 + v.0, a.1 + v.1));
        (
            self.fill_counts.get(&id).copied().unwrap_or(0),
            shares,
            notional,
        )
    }

    /// The symbols a strategy traded most (by shares), most first, ties to the lower id.
    pub fn top_symbols_of(&self, id: u16, k: usize) -> Vec<(InstrumentId, u64)> {
        let mut v: Vec<(InstrumentId, u64)> = self
            .fills_by
            .iter()
            .filter(|((s, _), _)| *s == id)
            .map(|((_, i), f)| (*i, f.0))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(k);
        v
    }

    /// What a strategy watched: its universe's fingerprint, how many symbols the static layer chose, how
    /// many a dynamic layer ever held, and how many it holds now.
    pub fn watched_by(&self, id: u16) -> Option<(u64, usize, usize, usize)> {
        let k = self.slot_index(id)?;
        let s = &self.slots[k];
        Some((
            s.universe_fp,
            s.candidates.len(),
            s.ever.len(),
            s.runner.members().len(),
        ))
    }

    pub fn reference(&self) -> &Reference {
        &self.reference
    }

    pub fn route_of(&self, id: u16) -> Option<Route> {
        self.slot_index(id).map(|k| self.slots[k].route)
    }

    /// Events handled, seconds of event time that had any, the busiest second and its time.
    pub fn rate(&self) -> (u64, u64, u32, Nanos) {
        let peak = self
            .per_second
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map_or((0, 0), |(s, n)| (*s, *n));
        (
            self.events,
            self.per_second.len() as u64,
            peak.1,
            peak.0 * NANOS_PER_SEC,
        )
    }

    /// An upper bound, in nanoseconds, on the `q` permille quantile of how long events took to reach us
    /// (receive time less event time): the top of the power-of-two bucket it falls in.
    pub fn feed_lag_quantile(&self, q_permille: u64) -> Nanos {
        let total: u64 = self.lag_hist.iter().sum();
        if total == 0 {
            return 0;
        }
        let want = (total * q_permille).div_ceil(1000).max(1);
        let mut seen = 0;
        for (b, n) in self.lag_hist.iter().enumerate() {
            seen += n;
            if seen >= want {
                return if b == 0 {
                    0
                } else if b >= 64 {
                    u64::MAX
                } else {
                    (1u64 << b) - 1
                };
            }
        }
        0
    }

    pub fn first_event_ts(&self) -> Option<Nanos> {
        self.first_ts
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
