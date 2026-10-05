//! The synthetic [`Provider`]: same trait, same limits and failure modes as
//! the real vendors, but fed from a deterministic [`SynthStream`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use tf_core::{Event, Nanos, ProviderId};
use tf_provider::{
    Capabilities, Poll, Provider, ProviderError, Subscription, SymbolSet, WireFormat,
};

use crate::config::SynthConfig;
use crate::rng::SplitMix64;
use crate::stream::SynthStream;

/// Connection slots shared by every [`SynthProvider`] built on it; models a
/// vendor account's concurrent-session limit (Alpaca: 1).
#[derive(Debug)]
pub struct Account {
    limit: u32,
    active: AtomicU32,
}

impl Account {
    pub fn new(limit: u32) -> Arc<Self> {
        Arc::new(Account {
            limit,
            active: AtomicU32::new(0),
        })
    }

    pub fn active(&self) -> u32 {
        self.active.load(Ordering::SeqCst)
    }

    fn acquire(&self) -> Result<(), ProviderError> {
        let mut cur = self.active.load(Ordering::SeqCst);
        loop {
            if cur >= self.limit {
                return Err(ProviderError::ConnectionLimitExceeded { limit: self.limit });
            }
            match self.active.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => cur = actual,
            }
        }
    }

    fn release(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// Drop the session (once) after delivering this many events.
    pub disconnect_after_events: Option<u64>,
    /// Events that occur while disconnected; lost unless replay is enabled.
    pub outage_events: u64,
    /// Chance, in permille, that an event is delivered twice.
    pub dup_permille: u32,
    /// Chance, in permille, that an event swaps places with its successor.
    pub reorder_permille: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SynthOptions {
    pub max_symbols: Option<usize>,
    /// Support `reconnect(Some(ts))` replay, like Databento's intraday replay.
    pub replay: bool,
    pub faults: Faults,
}

pub struct SynthProvider {
    cfg: SynthConfig,
    opts: SynthOptions,
    caps: Capabilities,
    account: Arc<Account>,
    fault_rng: SplitMix64,

    connected: bool,
    sub: Option<Subscription>,
    stream: SynthStream,
    /// Events owed to the caller (duplicates, reorder leftovers, replay head);
    /// delivered as-is, without further fault rolls.
    queue: VecDeque<Event>,
    delivered: u64,
    disconnect_armed: bool,
    exhausted: bool,
}

impl SynthProvider {
    pub fn new(cfg: SynthConfig, opts: SynthOptions, account: Arc<Account>) -> Self {
        let caps = Capabilities {
            provider: ProviderId::Synthetic,
            max_connections: account.limit,
            max_symbols_per_session: opts.max_symbols,
            wildcard: true,
            replay_window_secs: opts.replay.then_some(86_400),
            wire: WireFormat::Binary,
        };
        let stream = SynthStream::new(&cfg);
        let fault_rng = SplitMix64::fork(cfg.seed, 0xFA17);
        let disconnect_armed = opts.faults.disconnect_after_events.is_some();
        SynthProvider {
            cfg,
            opts,
            caps,
            account,
            fault_rng,
            connected: false,
            sub: None,
            stream,
            queue: VecDeque::new(),
            delivered: 0,
            disconnect_armed,
            exhausted: false,
        }
    }

    fn drop_session(&mut self) {
        if self.connected {
            self.connected = false;
            self.account.release();
        }
    }

    /// Next event after fault injection, before subscription filtering.
    fn pull(&mut self) -> Option<Event> {
        if let Some(ev) = self.queue.pop_front() {
            return Some(ev);
        }
        let mut ev = self.stream.next_event()?;
        let faults = &self.opts.faults;
        if self.fault_rng.permille(faults.reorder_permille) {
            if let Some(later) = self.stream.next_event() {
                self.queue.push_back(ev);
                ev = later;
            }
        }
        if self.fault_rng.permille(self.opts.faults.dup_permille) {
            self.queue.push_front(ev);
        }
        Some(ev)
    }
}

impl Drop for SynthProvider {
    fn drop(&mut self) {
        self.drop_session();
    }
}

impl Provider for SynthProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<(), ProviderError> {
        if self.connected {
            return Ok(());
        }
        self.account.acquire()?;
        self.connected = true;
        Ok(())
    }

    fn subscribe(&mut self, sub: &Subscription) -> Result<(), ProviderError> {
        if !self.connected {
            return Err(ProviderError::NotConnected);
        }
        match (&sub.symbols, self.caps.max_symbols_per_session) {
            (SymbolSet::All, _) if !self.caps.wildcard => {
                return Err(ProviderError::Unsupported("wildcard subscription"));
            }
            (SymbolSet::List(ids), Some(limit)) if ids.len() > limit => {
                return Err(ProviderError::SymbolLimitExceeded {
                    limit,
                    requested: ids.len(),
                });
            }
            (SymbolSet::All, Some(_)) => {
                return Err(ProviderError::Unsupported(
                    "wildcard on a symbol-limited plan",
                ));
            }
            _ => {}
        }
        self.sub = Some(sub.clone());
        Ok(())
    }

    fn reconnect(&mut self, resume_from: Option<Nanos>) -> Result<(), ProviderError> {
        self.connect()?;
        match resume_from {
            Some(from) if self.opts.replay => {
                // Deterministic replay: regenerate from the start and skip to the
                // resume point. `seq`s come out identical to the first delivery.
                self.stream = SynthStream::new(&self.cfg);
                self.queue.clear();
                while let Some(ev) = self.stream.next_event() {
                    if ev.ts_recv() >= from {
                        self.queue.push_back(ev);
                        break;
                    }
                }
            }
            _ => {
                for _ in 0..self.opts.faults.outage_events {
                    if self.stream.next_event().is_none() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        self.drop_session();
    }

    fn poll(&mut self, out: &mut Vec<Event>, max: usize) -> Poll {
        if !self.connected {
            return Poll::Disconnected;
        }
        let mut n = 0;
        while n < max {
            if self.disconnect_armed {
                if let Some(after) = self.opts.faults.disconnect_after_events {
                    if self.delivered >= after {
                        self.disconnect_armed = false;
                        self.drop_session();
                        break;
                    }
                }
            }
            let Some(ev) = self.pull() else {
                self.exhausted = true;
                break;
            };
            if self.sub.as_ref().is_some_and(|s| s.matches(&ev)) {
                out.push(ev);
                self.delivered += 1;
                n += 1;
            }
        }
        if n > 0 {
            Poll::Events(n)
        } else if !self.connected {
            Poll::Disconnected
        } else if self.exhausted {
            Poll::End
        } else {
            Poll::Idle
        }
    }
}
