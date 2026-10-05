//! The seam between a market data vendor and the engine.
//!
//! The trait is deliberately synchronous and pull-based. A live adapter
//! (Databento, Alpaca) runs its own async tasks and exposes the result through
//! a bounded channel behind this trait; the synthetic provider is a plain
//! deterministic iterator. Either way the engine side stays free of I/O and
//! of any async runtime, which is what keeps runs reproducible.

use std::fmt;
use std::ops::BitOr;

use tf_core::{Event, EventKind, InstrumentId, Nanos, ProviderId};

/// Event channels a subscription can ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channels(u8);

impl Channels {
    pub const TRADES: Channels = Channels(1);
    pub const QUOTES: Channels = Channels(2);
    pub const STATUS: Channels = Channels(4);
    pub const NEWS: Channels = Channels(8);
    pub const ALL: Channels = Channels(15);

    pub const fn contains(self, other: Channels) -> bool {
        self.0 & other.0 == other.0
    }

    /// Corrections and cancel/errors ride on `TRADES`: a consumer of trades is
    /// wrong without the amendments to them.
    pub const fn admits(self, kind: EventKind) -> bool {
        match kind {
            EventKind::Trade | EventKind::Correction | EventKind::CancelError => {
                self.contains(Channels::TRADES)
            }
            EventKind::Quote => self.contains(Channels::QUOTES),
            EventKind::Status => self.contains(Channels::STATUS),
            EventKind::News => self.contains(Channels::NEWS),
        }
    }
}

impl BitOr for Channels {
    type Output = Channels;
    fn bitor(self, rhs: Channels) -> Channels {
        Channels(self.0 | rhs.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolSet {
    /// Wildcard (`*` on Alpaca, `ALL_SYMBOLS` on Databento).
    All,
    /// Sorted, deduplicated. Build through [`Subscription::list`].
    List(Vec<InstrumentId>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub channels: Channels,
    pub symbols: SymbolSet,
}

impl Subscription {
    pub fn all(channels: Channels) -> Self {
        Subscription {
            channels,
            symbols: SymbolSet::All,
        }
    }

    pub fn list(channels: Channels, mut ids: Vec<InstrumentId>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Subscription {
            channels,
            symbols: SymbolSet::List(ids),
        }
    }

    pub fn matches(&self, ev: &Event) -> bool {
        self.channels.admits(ev.kind())
            && match &self.symbols {
                SymbolSet::All => true,
                SymbolSet::List(ids) => ids.binary_search(&ev.instrument()).is_ok(),
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireFormat {
    Binary,
    Json,
    MsgPack,
}

/// What a provider will and won't let us do. These are the limits the real
/// vendors impose, so tests can reproduce them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub provider: ProviderId,
    /// Concurrent sessions allowed on the account/endpoint
    /// (Alpaca: usually 1; Databento: 10 per dataset on Standard).
    pub max_connections: u32,
    /// Max symbols per session; `None` = unlimited.
    pub max_symbols_per_session: Option<usize>,
    pub wildcard: bool,
    /// How far back a (re)connect can replay, if at all (Databento: 24 h).
    pub replay_window_secs: Option<u64>,
    pub wire: WireFormat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    /// Alpaca: 406 "connection limit exceeded".
    ConnectionLimitExceeded {
        limit: u32,
    },
    /// Alpaca: 405 "symbol limit exceeded".
    SymbolLimitExceeded {
        limit: usize,
        requested: usize,
    },
    NotConnected,
    Unsupported(&'static str),
    /// The underlying source failed in a way retrying will not fix (for example
    /// a damaged tape). Not transient: a run should stop and report it.
    Source(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::ConnectionLimitExceeded { limit } => {
                write!(f, "connection limit exceeded (limit {limit})")
            }
            ProviderError::SymbolLimitExceeded { limit, requested } => {
                write!(
                    f,
                    "symbol limit exceeded (limit {limit}, requested {requested})"
                )
            }
            ProviderError::NotConnected => write!(f, "not connected"),
            ProviderError::Unsupported(what) => write!(f, "unsupported: {what}"),
            ProviderError::Source(what) => write!(f, "source failed: {what}"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    /// This many events were appended to the output buffer.
    Events(usize),
    /// Connected, nothing available right now.
    Idle,
    /// The session dropped. Call [`Provider::reconnect`].
    Disconnected,
    /// The source is exhausted (replay/synthetic only; live feeds never end).
    End,
}

pub trait Provider: Send {
    fn capabilities(&self) -> &Capabilities;

    /// Open a session. Idempotent while connected.
    fn connect(&mut self) -> Result<(), ProviderError>;

    /// Set the subscription for the current session. A provider must restore
    /// the most recent subscription on [`Provider::reconnect`].
    fn subscribe(&mut self, sub: &Subscription) -> Result<(), ProviderError>;

    /// Re-open a dropped session. If the provider supports replay and
    /// `resume_from` is given, it re-delivers events with
    /// `ts_recv >= resume_from`, so the caller may see one boundary event
    /// twice and must dedupe on `(instrument, ts_event, seq)`. Without
    /// replay, whatever happened during the outage is lost.
    fn reconnect(&mut self, resume_from: Option<Nanos>) -> Result<(), ProviderError>;

    /// Drop the session and release any connection slot.
    fn disconnect(&mut self);

    /// Append up to `max` events to `out`.
    fn poll(&mut self, out: &mut Vec<Event>, max: usize) -> Poll;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_core::{Header, Px, Trade, TradeFlags};

    fn trade(instrument: InstrumentId) -> Event {
        Event::Trade(Trade {
            hdr: Header {
                ts_event: 1,
                ts_recv: 2,
                seq: 0,
                instrument,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(100),
            size: 100,
            flags: TradeFlags::NONE,
        })
    }

    #[test]
    fn channels_compose_and_filter() {
        let c = Channels::TRADES | Channels::STATUS;
        assert!(c.admits(EventKind::Trade));
        assert!(c.admits(EventKind::Status));
        assert!(!c.admits(EventKind::Quote));
        assert!(Channels::ALL.contains(c));
    }

    #[test]
    fn amendments_ride_on_trades_and_news_has_its_own_channel() {
        let trades = Channels::TRADES;
        assert!(trades.admits(EventKind::Correction));
        assert!(trades.admits(EventKind::CancelError));
        assert!(!trades.admits(EventKind::News));
        assert!(!Channels::NEWS.admits(EventKind::Trade));
        assert!(Channels::NEWS.admits(EventKind::News));
        for kind in [
            EventKind::Trade,
            EventKind::Quote,
            EventKind::Status,
            EventKind::Correction,
            EventKind::CancelError,
            EventKind::News,
        ] {
            assert!(Channels::ALL.admits(kind), "ALL does not admit {kind:?}");
        }
    }

    #[test]
    fn list_subscription_sorts_dedups_and_matches() {
        let s = Subscription::list(Channels::ALL, vec![5, 1, 5, 3]);
        assert_eq!(s.symbols, SymbolSet::List(vec![1, 3, 5]));
        assert!(s.matches(&trade(3)));
        assert!(!s.matches(&trade(4)));
        assert!(!Subscription::all(Channels::QUOTES).matches(&trade(4)));
    }
}
