//! Replay a tape through the [`Provider`] trait, so the run loop, ingestors'
//! test harnesses and backtests consume recorded sessions exactly as they would
//! a live feed or the synthetic provider.
//!
//! Two speeds:
//!
//! - [`Speed::Max`] hands events over as fast as the consumer pulls them, for
//!   backtests.
//! - [`Speed::Paced`] holds each event until `(recorded offset) / speed` has
//!   passed on an injected [`Pacer`], so a tape can be replayed at 1x, 10x, ...
//!
//! Pacing happens by *waiting inside `poll`*, not by returning `Poll::Idle`: the
//! run loop treats a long run of idle polls as a stalled source. A poll returns
//! the events that are due; if none are, it waits for the next one. The event
//! stream is the same at every speed; only when events are released differs.
//!
//! Time comes from the injected [`Pacer`]: the real [`WallPacer`] sleeps, while
//! [`SimClock`] simply jumps to the target, so paced replay is deterministic and
//! instant in tests. [`WallPacer`] is the only code in this workspace that
//! sleeps. A tape with a long quiet gap (overnight, between sessions) is waited
//! out at its recorded length.

use std::io::{Read, Seek};
use std::sync::Arc;
use std::time::Duration;

use tf_core::{Clock, Event, Nanos, ProviderId, SimClock, SystemClock};
use tf_provider::{Capabilities, Poll, Provider, ProviderError, Subscription, WireFormat};

use crate::{Cursor, TapeReader};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// No pacing: deliver as fast as events are pulled.
    Max,
    /// `permille` of real time: 1000 is 1x, 10_000 is 10x, 500 is half speed.
    /// Zero is treated as 1 (the slowest speed) rather than "never".
    Paced { permille: u32 },
}

impl Speed {
    pub const REALTIME: Speed = Speed::Paced { permille: 1000 };
}

/// A time source that can also wait. Injected so replay speed never touches the
/// wall clock directly.
pub trait Pacer: Send {
    fn now(&self) -> Nanos;
    /// Block until `now() >= t` (returns at once if it already is).
    fn wait_until(&self, t: Nanos);
}

/// Simulated time: waiting is jumping to the target. Never moves backwards.
impl Pacer for SimClock {
    fn now(&self) -> Nanos {
        Clock::now(self)
    }

    fn wait_until(&self, t: Nanos) {
        self.advance_to(t);
    }
}

/// Lets a caller keep a handle to a clock it also gives to a provider.
impl<P: Pacer + Sync + ?Sized> Pacer for Arc<P> {
    fn now(&self) -> Nanos {
        (**self).now()
    }

    fn wait_until(&self, t: Nanos) {
        (**self).wait_until(t);
    }
}

/// Real time: the system clock, and `thread::sleep` to wait.
#[derive(Clone, Copy, Debug, Default)]
pub struct WallPacer;

impl Pacer for WallPacer {
    fn now(&self) -> Nanos {
        SystemClock.now()
    }

    fn wait_until(&self, t: Nanos) {
        // A sleep can return early; keep going until the time has come.
        loop {
            let now = SystemClock.now();
            if now >= t {
                return;
            }
            std::thread::sleep(Duration::from_nanos(t - now));
        }
    }
}

/// A recorded session as a [`Provider`].
///
/// Delivers the tape's events, filtered by the subscription, in tape order.
/// `reconnect(Some(t))` re-delivers from `ts_recv >= t`, like the vendors'
/// intraday replay; `reconnect(None)` starts again from the beginning (or from
/// [`TapeProvider::starting_at`]). A read error mid-tape is not a short tape: it
/// is reported as [`ProviderError::Source`] through `reconnect`, so a run fails
/// instead of quietly ending early.
pub struct TapeProvider<R> {
    reader: TapeReader<R>,
    caps: Capabilities,
    speed: Speed,
    pacer: Box<dyn Pacer>,
    start_at: Nanos,
    connected: bool,
    sub: Option<Subscription>,
    cursor: Cursor,
    pending: Option<Event>,
    exhausted: bool,
    /// `(pacer time, tape time)` of the first event delivered since (re)start.
    anchor: Option<(Nanos, Nanos)>,
    failed: Option<String>,
}

impl<R: Read + Seek> TapeProvider<R> {
    /// `source` is the vendor the tape was recorded from; it only labels
    /// [`Capabilities`] (each event carries its own provider id).
    pub fn new(
        reader: TapeReader<R>,
        source: ProviderId,
        speed: Speed,
        pacer: Box<dyn Pacer>,
    ) -> Self {
        let window_secs = reader
            .time_range()
            .map_or(0, |(first, last)| (last - first) / 1_000_000_000);
        let caps = Capabilities {
            provider: source,
            max_connections: u32::MAX,
            max_symbols_per_session: None,
            wildcard: true,
            // A tape can be replayed from any point in it.
            replay_window_secs: Some(window_secs),
            wire: WireFormat::Binary,
        };
        let cursor = reader.cursor(0);
        TapeProvider {
            reader,
            caps,
            speed,
            pacer,
            start_at: 0,
            connected: false,
            sub: None,
            cursor,
            pending: None,
            exhausted: false,
            anchor: None,
            failed: None,
        }
    }

    /// Start at the first event with `ts_recv >= t` instead of the beginning.
    pub fn starting_at(mut self, t: Nanos) -> Self {
        self.start_at = t;
        self.restart(t);
        self
    }

    fn restart(&mut self, from: Nanos) {
        self.cursor = self.reader.cursor(from);
        self.pending = None;
        self.exhausted = false;
        self.anchor = None;
    }

    /// Make `pending` the next event the subscription wants, or mark the tape
    /// exhausted.
    fn fill(&mut self) -> Result<(), String> {
        while self.pending.is_none() && !self.exhausted {
            match self.reader.next_event(&mut self.cursor) {
                Ok(Some(ev)) => {
                    if self.sub.as_ref().is_some_and(|s| s.matches(&ev)) {
                        self.pending = Some(ev);
                    }
                }
                Ok(None) => self.exhausted = true,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    /// When the pacer's clock should reach for `ev` to be released.
    fn due(&mut self, permille: u32, ev: &Event) -> Nanos {
        let (wall0, tape0) = *self.anchor.get_or_insert((self.pacer.now(), ev.ts_recv()));
        let offset = u128::from(ev.ts_recv().saturating_sub(tape0));
        let scaled = offset * 1000 / u128::from(permille.max(1));
        wall0.saturating_add(Nanos::try_from(scaled).unwrap_or(Nanos::MAX))
    }
}

impl<R: Read + Seek + Send> Provider for TapeProvider<R> {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<(), ProviderError> {
        self.connected = true;
        Ok(())
    }

    fn subscribe(&mut self, sub: &Subscription) -> Result<(), ProviderError> {
        if !self.connected {
            return Err(ProviderError::NotConnected);
        }
        self.sub = Some(sub.clone());
        // An event already held was chosen under the old subscription.
        self.pending = None;
        Ok(())
    }

    fn reconnect(&mut self, resume_from: Option<Nanos>) -> Result<(), ProviderError> {
        if let Some(what) = &self.failed {
            return Err(ProviderError::Source(what.clone()));
        }
        self.connected = true;
        self.restart(resume_from.unwrap_or(self.start_at));
        Ok(())
    }

    fn disconnect(&mut self) {
        self.connected = false;
        self.pending = None;
    }

    fn poll(&mut self, out: &mut Vec<Event>, max: usize) -> Poll {
        if !self.connected {
            return Poll::Disconnected;
        }
        if max == 0 {
            return Poll::Idle;
        }
        let mut n = 0;
        while n < max {
            if let Err(what) = self.fill() {
                self.failed = Some(what);
                self.connected = false;
                break;
            }
            let Some(ev) = self.pending else { break };
            if let Speed::Paced { permille } = self.speed {
                let due = self.due(permille, &ev);
                if self.pacer.now() < due {
                    if n > 0 {
                        break; // hand over what is due; wait on the next poll
                    }
                    self.pacer.wait_until(due);
                }
            }
            out.push(ev);
            self.pending = None;
            n += 1;
        }
        if n > 0 {
            Poll::Events(n)
        } else if self.failed.is_some() {
            Poll::Disconnected
        } else if self.sub.is_none() {
            Poll::Idle
        } else {
            Poll::End
        }
    }
}
