use std::collections::HashSet;

use tf_core::{Event, EventKind, Fnv1a64, Nanos, Px};

use crate::EventSink;

/// Hashes the stable encoding of every event: two runs are identical iff
/// their hashes are.
#[derive(Debug, Default)]
pub struct HashSink {
    hasher: Fnv1a64,
    buf: Vec<u8>,
    pub events: u64,
}

impl HashSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finish(&self) -> u64 {
        self.hasher.finish()
    }
}

impl EventSink for HashSink {
    fn on_event(&mut self, ev: &Event) {
        self.buf.clear();
        ev.encode(&mut self.buf);
        self.hasher.write(&self.buf);
        self.events += 1;
    }
}

#[derive(Debug, Default)]
pub struct StatsSink {
    pub trades: u64,
    pub quotes: u64,
    pub status: u64,
    pub shares: u64,
    pub first_ts_event: Option<Nanos>,
    pub last_ts_event: Option<Nanos>,
    pub per_instrument: Vec<u64>,
    pub max_trade_px: Option<Px>,
}

impl EventSink for StatsSink {
    fn on_event(&mut self, ev: &Event) {
        match ev.kind() {
            EventKind::Trade => self.trades += 1,
            EventKind::Quote => self.quotes += 1,
            EventKind::Status => self.status += 1,
        }
        if let Event::Trade(t) = ev {
            self.shares += u64::from(t.size);
            self.max_trade_px = Some(self.max_trade_px.map_or(t.px, |m| m.max(t.px)));
        }
        let ts = ev.ts_event();
        self.first_ts_event = Some(self.first_ts_event.map_or(ts, |f| f.min(ts)));
        self.last_ts_event = Some(self.last_ts_event.map_or(ts, |l| l.max(ts)));
        let i = ev.instrument() as usize;
        if self.per_instrument.len() <= i {
            self.per_instrument.resize(i + 1, 0);
        }
        self.per_instrument[i] += 1;
    }
}

#[derive(Debug, Default)]
pub struct VecSink(pub Vec<Event>);

impl EventSink for VecSink {
    fn on_event(&mut self, ev: &Event) {
        self.0.push(*ev);
    }
}

pub struct Tee<A, B>(pub A, pub B);

impl<A: EventSink, B: EventSink> EventSink for Tee<A, B> {
    fn on_event(&mut self, ev: &Event) {
        self.0.on_event(ev);
        self.1.on_event(ev);
    }
}

/// Drops events whose provider `seq` was already seen.
///
/// Dev/test utility: it remembers every `seq` forever. The production
/// engine needs a bounded window keyed on `(provider, instrument, ts_event, seq)`.
pub struct DedupeSink<S> {
    inner: S,
    seen: HashSet<u64>,
    pub dropped: u64,
}

impl<S> DedupeSink<S> {
    pub fn new(inner: S) -> Self {
        DedupeSink {
            inner,
            seen: HashSet::new(),
            dropped: 0,
        }
    }

    pub fn into_inner(self) -> S {
        self.inner
    }
}

impl<S: EventSink> EventSink for DedupeSink<S> {
    fn on_event(&mut self, ev: &Event) {
        if self.seen.insert(ev.seq()) {
            self.inner.on_event(ev);
        } else {
            self.dropped += 1;
        }
    }
}
