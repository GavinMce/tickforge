//! The live feed as a [`tf_provider::Provider`].

use tf_core::{Event, Nanos, ProviderId};
use tf_databento::InstrumentMap;
use tf_ingest::{
    Config as IngestConfig, ConfigError, Consumer, Delivery, Gap, Producer, Stats, channel,
};
use tf_provider::{Capabilities, Poll, Provider, ProviderError, Subscription, WireFormat};

use crate::feed::{FeedShared, LiveFeed, State};
use crate::session::Config;

/// Sessions the Standard plan allows per dataset.
pub const MAX_SESSIONS: u32 = 10;

pub struct LiveProvider {
    cfg: Config,
    caps: Capabilities,
    consumer: Consumer,
    /// Held while no feed is running.
    spare: Option<(Producer, InstrumentMap)>,
    feed: Option<LiveFeed>,
    sub: Option<Subscription>,
    gaps: Vec<Gap>,
    /// The last event time delivered, to resume from.
    last_ts: Nanos,
}

fn source(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Source(e.to_string())
}

impl LiveProvider {
    pub fn new(cfg: Config, ingest: IngestConfig) -> Result<LiveProvider, ConfigError> {
        let (producer, consumer) = channel(ingest)?;
        Ok(LiveProvider {
            cfg,
            caps: Capabilities {
                provider: ProviderId::Databento,
                max_connections: MAX_SESSIONS,
                // The gateway takes 2,000 symbols a request and any number of requests; the limit that
                // matters is the dataset's, which ALL_SYMBOLS covers.
                max_symbols_per_session: None,
                wildcard: true,
                replay_window_secs: Some(24 * 3600),
                wire: WireFormat::Binary,
            },
            consumer,
            spare: Some((producer, InstrumentMap::default())),
            feed: None,
            sub: None,
            gaps: Vec::new(),
            last_ts: 0,
        })
    }

    fn start(&mut self, resume: Option<Nanos>) -> Result<(), ProviderError> {
        let (producer, instruments) = self.spare.take().ok_or(ProviderError::NotConnected)?;
        match LiveFeed::start(&self.cfg, producer, instruments, resume) {
            Ok(f) => {
                self.feed = Some(f);
                Ok(())
            }
            Err(b) => {
                let (e, back) = *b;
                self.spare = Some((back.producer, back.instruments));
                Err(source(e))
            }
        }
    }

    fn reclaim(&mut self) {
        if let Some(f) = self.feed.take() {
            let r = f.stop();
            self.spare = Some((r.producer, r.instruments));
        }
    }

    /// What the feed knows about the session, if there is one.
    pub fn shared(&self) -> Option<&std::sync::Arc<FeedShared>> {
        self.feed.as_ref().map(LiveFeed::shared)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.feed.as_ref().map(|f| f.session_id.as_str())
    }

    /// The ingest queue's counters.
    pub fn stats(&self) -> Stats {
        self.consumer.stats()
    }

    /// The gap markers delivered so far (the engine saw less than the feed sent in each).
    pub fn gaps(&self) -> &[Gap] {
        &self.gaps
    }

    /// The next thing off the queue, events and gap markers alike, for a driver that wants the gaps
    /// (the trait's `poll` keeps them aside).
    pub fn recv(&mut self) -> Option<Delivery> {
        let d = self.consumer.try_recv()?;
        match &d {
            Delivery::Event(e) => self.last_ts = self.last_ts.max(e.ts_recv()),
            Delivery::Gap(g) => self.gaps.push(*g),
        }
        Some(d)
    }

    /// The time of the last event handed out: what to resume from after a drop.
    pub fn last_event_ts(&self) -> Nanos {
        self.last_ts
    }
}

impl Provider for LiveProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<(), ProviderError> {
        if self.feed.is_some() {
            return Ok(());
        }
        self.start(None)
    }

    /// The symbols and schemas are in the [`Config`]; this sets what the caller sees of them.
    fn subscribe(&mut self, sub: &Subscription) -> Result<(), ProviderError> {
        if self.feed.is_none() {
            return Err(ProviderError::NotConnected);
        }
        self.sub = Some(sub.clone());
        Ok(())
    }

    /// A new session, replaying from `resume_from` if given (the gateway keeps 24 hours; an event at
    /// exactly that time can come twice). Instruments keep their ids.
    fn reconnect(&mut self, resume_from: Option<Nanos>) -> Result<(), ProviderError> {
        self.reclaim();
        self.start(resume_from)
    }

    fn disconnect(&mut self) {
        self.reclaim();
    }

    fn poll(&mut self, out: &mut Vec<Event>, max: usize) -> Poll {
        if self.feed.is_none() {
            return Poll::Disconnected;
        }
        let mut n = 0;
        while n < max {
            let Some(d) = self.recv() else { break };
            if let Delivery::Event(e) = d {
                if self.sub.as_ref().is_none_or(|s| s.matches(&e)) {
                    out.push(e);
                    n += 1;
                }
            }
        }
        if n > 0 {
            return Poll::Events(n);
        }
        let gone = self
            .feed
            .as_ref()
            .is_some_and(|f| f.shared().state() != State::Streaming);
        if gone && self.consumer.depth() == 0 {
            // Everything it said has been taken. The session is over.
            self.reclaim();
            return Poll::Disconnected;
        }
        Poll::Idle
    }
}
