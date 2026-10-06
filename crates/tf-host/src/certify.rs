//! Replaying a strategy on a tape to earn the right to join a live day.

use tf_core::Event;
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
