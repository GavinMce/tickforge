//! Replaying a strategy on a tape to earn the right to join a live day.

use tf_core::{Event, Nanos};
use tf_ledger::MemStore;
use tf_strategy::sim::SimBroker;

use crate::def::{Certificate, StrategyDef};
use crate::host::{AdmitError, Host, HostConfig, HostError, Reference, SlotState};

#[derive(Debug)]
pub enum CertifyError {
    /// The strategy could not even be set up on the host (no budget, universe, ...).
    Admit(AdmitError),
    NoEvents,
    /// Its code panicked during the replay.
    Panicked(String),
    /// It was stopped during the replay (a loss limit).
    Stopped(String),
    /// The ledger refused something it was told: the strategy or the host is inconsistent.
    LedgerRefused(u64),
    Host(HostError),
    /// The tape's files could not be read.
    Tape(String),
}

impl std::fmt::Display for CertifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertifyError::Admit(e) => write!(f, "cannot be set up: {e:?}"),
            CertifyError::NoEvents => write!(f, "the tape has no events"),
            CertifyError::Panicked(m) => write!(f, "the strategy panicked: {m}"),
            CertifyError::Stopped(m) => write!(f, "the strategy was stopped: {m}"),
            CertifyError::LedgerRefused(n) => {
                write!(f, "the ledger refused {n} things it was told")
            }
            CertifyError::Host(e) => write!(f, "{e}"),
            CertifyError::Tape(m) => write!(f, "the tape cannot be read: {m}"),
        }
    }
}

impl std::error::Error for CertifyError {}

/// Replay `events` (a captured tape, identified by `tape_id`, for a capture its manifest fingerprint)
/// through a scratch host that has only this strategy, with the same limits, budgets and brokers'
/// behaviour as `cfg`, and issue the certificate. The strategy must run to the end of the tape
/// without a panic and without being stopped, and everything it caused must have been taken by the ledger.
pub fn certify(
    def: &StrategyDef,
    cfg: &HostConfig,
    reference: &Reference,
    events: &[Event],
    tape_id: u64,
) -> Result<Certificate, CertifyError> {
    if events.is_empty() {
        return Err(CertifyError::NoEvents);
    }
    // A replay never reaches a real broker: the paper route is simulated too.
    let mut scratch = Host::new(
        cfg.clone(),
        reference.clone(),
        MemStore::from_records(vec![]),
    )
    .map_err(CertifyError::Host)?
    .with_paper(Box::new(SimBroker::new(cfg.sim, cfg.id_space)));
    scratch.install(def).map_err(CertifyError::Admit)?;
    for ev in events {
        scratch.on_event(ev).map_err(CertifyError::Host)?;
    }
    let last = events.last().map_or(0, Event::ts_recv);
    conclude(scratch, def, tape_id, last)
}

/// What the replay showed: the day ended, the strategy still running, the ledger content with everything it was told.
fn conclude(
    mut scratch: Host<MemStore>,
    def: &StrategyDef,
    tape_id: u64,
    last: Nanos,
) -> Result<Certificate, CertifyError> {
    scratch.end_of_day(last).map_err(CertifyError::Host)?;
    match scratch.state_of(def.id) {
        Some(SlotState::Running) => {}
        Some(other) => {
            return Err(match other {
                SlotState::Stopped(crate::host::StopReason::Panicked(m)) => {
                    CertifyError::Panicked(m.clone())
                }
                s => CertifyError::Stopped(format!("{s:?}")),
            });
        }
        None => return Err(CertifyError::NoEvents),
    }
    if scratch.ledger_refusals() > 0 {
        return Err(CertifyError::LedgerRefused(scratch.ledger_refusals()));
    }
    Ok(Certificate::new(
        def.fingerprint(),
        tape_id,
        scratch.events(),
        scratch.intents(),
        scratch.accepted(),
        scratch.outcome_hash(),
    ))
}

/// [`certify`] over the files of a stored day (zstd DBN, in the order given), streamed through the scratch host and not held in
/// memory: a whole market's day is tens of millions of events. The first pass names the day's instruments, as a replay does;
/// events the gateway sent twice are dropped by the one rule the live run applies. `cfg` should carry the session times of the
/// tape's date (`HostConfig::day`) and `snapshot` is the reference as it was before that day.
pub fn certify_files(
    def: &StrategyDef,
    cfg: &HostConfig,
    snapshot: tf_universe::Snapshot,
    files: &[std::path::PathBuf],
    tape_id: u64,
) -> Result<Certificate, CertifyError> {
    use tf_capture::CaptureReplay;
    use tf_provider::{Poll, Provider};
    let reference = Reference {
        symbols: crate::replay::learn_symbols(files),
        snapshot,
    };
    let mut scratch = Host::new(cfg.clone(), reference, MemStore::from_records(vec![]))
        .map_err(CertifyError::Host)?
        .with_paper(Box::new(SimBroker::new(cfg.sim, cfg.id_space)));
    scratch.install(def).map_err(CertifyError::Admit)?;
    let mut source = CaptureReplay::from_files(files.to_vec());
    let mut dedupe = tf_core::Dedupe::new();
    let mut buf: Vec<Event> = Vec::new();
    let mut last: Nanos = 0;
    loop {
        buf.clear();
        match source.poll(&mut buf, 4096) {
            Poll::Events(_) => {}
            Poll::Idle => continue,
            _ => break,
        }
        for ev in buf.iter().filter(|e| dedupe.admit(e)) {
            last = last.max(ev.ts_recv());
            scratch.on_event(ev).map_err(CertifyError::Host)?;
        }
    }
    if let Some(why) = source.failure() {
        return Err(CertifyError::Tape(why.to_string()));
    }
    if scratch.events() == 0 {
        return Err(CertifyError::NoEvents);
    }
    conclude(scratch, def, tape_id, last)
}
