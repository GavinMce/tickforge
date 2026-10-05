//! The run loop shared by live, replay and backtest.
//!
//! `run` is a function of (provider, subscription, sink): it owns no globals,
//! reads no wall clock, and does no I/O of its own. Time inside a run is the
//! [`SimClock`], advanced to each event's `ts_recv` as it is delivered. That
//! property is what lets thousands of runs execute in parallel as k8s Jobs and
//! still be reproducible.

mod sinks;

use std::fmt;

use tf_core::{Event, Nanos, SimClock};
use tf_provider::{Poll, Provider, ProviderError, Subscription};

pub use sinks::{DedupeSink, HashSink, StatsSink, Tee, VecSink};

pub trait EventSink {
    fn on_event(&mut self, ev: &Event);
}

impl<S: EventSink + ?Sized> EventSink for &mut S {
    fn on_event(&mut self, ev: &Event) {
        (**self).on_event(ev);
    }
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub batch: usize,
    /// Reconnects allowed before giving up.
    pub max_reconnects: u32,
    /// Consecutive `Idle` polls tolerated before declaring the source stalled.
    pub max_idle_polls: u32,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            batch: 4096,
            max_reconnects: 8,
            max_idle_polls: 1000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub events: u64,
    pub reconnects: u32,
    pub first_recv: Option<Nanos>,
    pub last_recv: Option<Nanos>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunError {
    Provider(ProviderError),
    TooManyReconnects,
    Stalled,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Provider(e) => write!(f, "provider error: {e}"),
            RunError::TooManyReconnects => write!(f, "gave up after too many reconnects"),
            RunError::Stalled => write!(f, "source stalled (idle too long)"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<ProviderError> for RunError {
    fn from(e: ProviderError) -> Self {
        RunError::Provider(e)
    }
}

/// Drive `provider` to exhaustion, feeding every event to `sink`.
///
/// On a dropped session it reconnects, asking the provider to resume from the
/// last `ts_recv` seen; wrap the sink in a [`DedupeSink`] if the provider may
/// replay boundary events.
pub fn run<P, S>(
    provider: &mut P,
    sub: &Subscription,
    clock: &SimClock,
    sink: &mut S,
    cfg: &RunConfig,
) -> Result<RunReport, RunError>
where
    P: Provider + ?Sized,
    S: EventSink + ?Sized,
{
    provider.connect()?;
    provider.subscribe(sub)?;

    let mut report = RunReport {
        events: 0,
        reconnects: 0,
        first_recv: None,
        last_recv: None,
    };
    let mut buf = Vec::with_capacity(cfg.batch);
    let mut idle = 0;

    loop {
        buf.clear();
        match provider.poll(&mut buf, cfg.batch) {
            Poll::Events(_) => {
                idle = 0;
                for ev in &buf {
                    clock.advance_to(ev.ts_recv());
                    sink.on_event(ev);
                    report.events += 1;
                    report.first_recv.get_or_insert(ev.ts_recv());
                    report.last_recv = Some(ev.ts_recv());
                }
            }
            Poll::Idle => {
                idle += 1;
                if idle > cfg.max_idle_polls {
                    return Err(RunError::Stalled);
                }
            }
            Poll::Disconnected => {
                if report.reconnects >= cfg.max_reconnects {
                    return Err(RunError::TooManyReconnects);
                }
                report.reconnects += 1;
                provider.reconnect(report.last_recv)?;
            }
            Poll::End => return Ok(report),
        }
    }
}
