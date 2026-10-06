//! The queue between a feed and the engine.
//!
//! The feed must never wait for the engine: a vendor that is not read fast enough starts dropping
//! records of its own (Databento's gateway does, since April 2026), and a blocked reader is a
//! disconnect. So the producer side never blocks, and when the engine does fall behind, what is
//! given up is decided here, in the open, by what each event is worth:
//!
//! - **Quotes are state.** Only the latest matters, so under pressure they are *conflated*: a quote
//!   that cannot be queued replaces the one already waiting for its symbol, and what waits is queued
//!   as soon as there is room. Nothing is lost that a later quote did not supersede.
//! - **Trades are events.** Each one counts (volume, last sale). They are queued up to a high ceiling
//!   and only past it dropped; a drop is counted and an in-band [`Gap`] marker is delivered before
//!   the next event, so a consumer knows the stream is not continuous.
//! - **Control events** (halts, corrections, cancels, tier and parameter changes) are rare and
//!   decisive: they get the last of the room, and are dropped, loudly, only when the queue is
//!   completely full.
//!
//! One ring, first in first out, so events of one symbol reach the engine in the order they
//! arrived. Across symbols a conflated quote can arrive later than events that followed it; that is
//! the price of not blocking and is only seen under pressure.
//!
//! Because conflation depends on timing, what the engine consumed under pressure is not
//! reproducible from the raw capture. A replay-equivalence check (E18-S06) must replay what the
//! engine was given (its input tape), and a [`Gap`] or a non-zero `conflated` count in the day's
//! statistics says which days differ.

#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::time::Duration;

use tf_core::{Event, InstrumentId, Nanos, Quote};

/// What kind of loss a [`Gap`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lost {
    Trades,
    Control,
    /// The gateway skipped records because it could not send them as fast as they came (we read too
    /// slowly). Neither the kind nor the number is known; `count` is the number of such notices.
    Skipped,
}

/// A break in the stream: `count` events of one kind were dropped between `first_ts` and `last_ts`
/// (their `ts_recv`). Delivered in order, before the next event that follows the break.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub lost: Lost,
    pub count: u64,
    pub first_ts: Nanos,
    pub last_ts: Nanos,
}

/// What the consumer takes off the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Event(Event),
    Gap(Gap),
}

/// How big the queue is and where each kind of event stops being admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Messages the ring holds. At a measured 337,000 trades in the busiest second, two million is
    /// about six seconds of a stalled engine.
    pub capacity: usize,
    /// Quotes are queued while the ring is less full than this (permille); above it they conflate.
    pub quote_ceiling_permille: u32,
    /// Trades are queued while the ring is less full than this (permille); above it they drop.
    pub trade_ceiling_permille: u32,
    /// Instrument ids the conflation table covers (ids at or above it are never conflated: their
    /// quotes drop above the ceiling and are counted).
    pub instruments: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            capacity: 1 << 21,
            quote_ceiling_permille: 500,
            trade_ceiling_permille: 900,
            instruments: 16_384,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigError(pub &'static str);

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.capacity < 16 {
            return Err(ConfigError("capacity must be at least 16"));
        }
        if !(0 < self.quote_ceiling_permille
            && self.quote_ceiling_permille < self.trade_ceiling_permille
            && self.trade_ceiling_permille < 1000)
        {
            return Err(ConfigError(
                "need 0 < quote ceiling < trade ceiling < 1000 permille",
            ));
        }
        Ok(())
    }
}

/// Counters, shared between the two ends and readable from anywhere.
#[derive(Default)]
struct Shared {
    offered: AtomicU64,
    queued: AtomicU64,
    conflated: AtomicU64,
    flushed: AtomicU64,
    dropped_trades: AtomicU64,
    dropped_quotes: AtomicU64,
    dropped_control: AtomicU64,
    gaps: AtomicU64,
    delivered: AtomicU64,
    max_depth: AtomicUsize,
}

/// A reading of the counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Events the feed offered.
    pub offered: u64,
    /// Messages put on the ring (events and gap markers).
    pub queued: u64,
    /// Quotes that were not queued at once because the ring was busy (each replaced a waiting one or
    /// waited itself).
    pub conflated: u64,
    /// Waiting quotes later queued.
    pub flushed: u64,
    pub dropped_trades: u64,
    /// Quotes lost outright: waiting ones that could not be queued before a trade of their symbol,
    /// and quotes for instruments the conflation table does not cover.
    pub dropped_quotes: u64,
    pub dropped_control: u64,
    pub gaps: u64,
    /// Messages the consumer has taken.
    pub delivered: u64,
    /// The fullest the ring has been.
    pub max_depth: usize,
}

impl Stats {
    /// Whether anything but a superseded quote was lost.
    pub fn lossless(&self) -> bool {
        self.dropped_trades == 0 && self.dropped_control == 0 && self.dropped_quotes == 0
    }
}

fn snapshot(s: &Shared) -> Stats {
    Stats {
        offered: s.offered.load(Relaxed),
        queued: s.queued.load(Relaxed),
        conflated: s.conflated.load(Relaxed),
        flushed: s.flushed.load(Relaxed),
        dropped_trades: s.dropped_trades.load(Relaxed),
        dropped_quotes: s.dropped_quotes.load(Relaxed),
        dropped_control: s.dropped_control.load(Relaxed),
        gaps: s.gaps.load(Relaxed),
        delivered: s.delivered.load(Relaxed),
        max_depth: s.max_depth.load(Relaxed),
    }
}

/// What happened to an event the feed offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Queued,
    /// A quote that is waiting for room (or replaced the one that was).
    Conflated,
    Dropped,
}

#[derive(Clone, Copy)]
struct GapAcc {
    lost: Lost,
    count: u64,
    first_ts: Nanos,
    last_ts: Nanos,
}

pub struct Producer {
    tx: SyncSender<Delivery>,
    depth: Arc<AtomicUsize>,
    shared: Arc<Shared>,
    cfg: Config,
    quote_at: usize,
    trade_at: usize,
    low_water: usize,
    pending: Vec<Option<Quote>>,
    /// Symbols with a quote waiting, oldest first; each appears at most once.
    dirty: VecDeque<InstrumentId>,
    /// Quotes waiting.
    waiting: usize,
    /// Whether a symbol is in `dirty`.
    listed: Vec<bool>,
    /// One open marker per kind of loss: trades, control, then the gateway's skips.
    gaps: [Option<GapAcc>; 3],
}

pub struct Consumer {
    rx: Receiver<Delivery>,
    depth: Arc<AtomicUsize>,
    shared: Arc<Shared>,
}

/// A queue with its two ends.
pub fn channel(cfg: Config) -> Result<(Producer, Consumer), ConfigError> {
    cfg.validate()?;
    let (tx, rx) = sync_channel(cfg.capacity);
    let depth = Arc::new(AtomicUsize::new(0));
    let shared = Arc::new(Shared::default());
    let at = |permille: u32| cfg.capacity * permille as usize / 1000;
    let p = Producer {
        tx,
        depth: depth.clone(),
        shared: shared.clone(),
        cfg,
        quote_at: at(cfg.quote_ceiling_permille),
        trade_at: at(cfg.trade_ceiling_permille),
        low_water: at(cfg.quote_ceiling_permille) / 2,
        pending: vec![None; cfg.instruments],
        dirty: VecDeque::new(),
        waiting: 0,
        listed: vec![false; cfg.instruments],
        gaps: [None, None, None],
    };
    Ok((p, Consumer { rx, depth, shared }))
}

fn ts_of(e: &Event) -> Nanos {
    e.hdr().ts_recv
}

impl Producer {
    pub fn config(&self) -> Config {
        self.cfg
    }

    pub fn stats(&self) -> Stats {
        snapshot(&self.shared)
    }

    /// Put one message on the ring if there is room. `false` if the ring is full.
    fn put(&mut self, d: Delivery) -> bool {
        // Count first so the consumer cannot see the message before the depth includes it.
        let depth = self.depth.fetch_add(1, Relaxed) + 1;
        match self.tx.try_send(d) {
            Ok(()) => {
                self.shared.queued.fetch_add(1, Relaxed);
                self.shared.max_depth.fetch_max(depth, Relaxed);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.depth.fetch_sub(1, Relaxed);
                false
            }
        }
    }

    fn depth(&self) -> usize {
        self.depth.load(Relaxed)
    }

    /// Deliver the pending gap markers, if there is room for them.
    fn flush_gap(&mut self) {
        for i in 0..3 {
            let Some(g) = self.gaps[i] else { continue };
            let d = Delivery::Gap(Gap {
                lost: g.lost,
                count: g.count,
                first_ts: g.first_ts,
                last_ts: g.last_ts,
            });
            if self.put(d) {
                self.shared.gaps.fetch_add(1, Relaxed);
                self.gaps[i] = None;
            }
        }
    }

    fn note_loss(&mut self, lost: Lost, ts: Nanos) {
        let slot = match lost {
            Lost::Trades => 0,
            Lost::Control => 1,
            Lost::Skipped => 2,
        };
        match &mut self.gaps[slot] {
            Some(g) => {
                g.count += 1;
                g.last_ts = ts;
            }
            none => {
                *none = Some(GapAcc {
                    lost,
                    count: 1,
                    first_ts: ts,
                    last_ts: ts,
                })
            }
        }
    }

    /// The quote waiting for `id`, if any.
    fn take_pending(&mut self, id: InstrumentId) -> Option<Quote> {
        let q = self.pending.get_mut(id as usize)?.take()?;
        self.waiting -= 1;
        Some(q)
    }

    /// Queue what waits for `id`, ahead of whatever comes next for it (to keep its order). If there is
    /// no room even for that, it is lost (a later event for the symbol follows it anyway).
    fn flush_symbol(&mut self, id: InstrumentId) {
        let Some(q) = self.take_pending(id) else {
            return;
        };
        if self.depth() < self.trade_at && self.put(Delivery::Event(Event::Quote(q))) {
            self.shared.flushed.fetch_add(1, Relaxed);
        } else {
            self.shared.dropped_quotes.fetch_add(1, Relaxed);
        }
    }

    /// Queue waiting quotes, oldest first, while the ring is below its low-water mark.
    fn flush_some(&mut self, max: usize) {
        let mut n = 0;
        while n < max && self.depth() < self.low_water {
            let Some(id) = self.dirty.pop_front() else {
                break;
            };
            self.listed[id as usize] = false;
            if let Some(q) = self.take_pending(id) {
                if self.put(Delivery::Event(Event::Quote(q))) {
                    self.shared.flushed.fetch_add(1, Relaxed);
                } else {
                    self.pending[id as usize] = Some(q);
                    self.waiting += 1;
                    self.dirty.push_front(id);
                    self.listed[id as usize] = true;
                    break;
                }
            }
            n += 1;
        }
    }

    /// Offer one event. Never blocks.
    pub fn push(&mut self, e: Event) -> Admission {
        self.shared.offered.fetch_add(1, Relaxed);
        if self.waiting > 0 {
            self.flush_some(64);
        }
        match e {
            Event::Quote(q) => self.push_quote(q),
            Event::Trade(_) => self.push_trade(e),
            other => self.push_control(other),
        }
    }

    fn push_quote(&mut self, q: Quote) -> Admission {
        let id = q.hdr.instrument;
        let covered = (id as usize) < self.pending.len();
        if self.depth() < self.quote_at && self.put(Delivery::Event(Event::Quote(q))) {
            // Queued at once. Anything waiting for this symbol is older and superseded.
            if covered && self.take_pending(id).is_some() {
                self.shared.conflated.fetch_add(1, Relaxed);
            }
            return Admission::Queued;
        }
        if !covered {
            self.shared.dropped_quotes.fetch_add(1, Relaxed);
            return Admission::Dropped;
        }
        self.shared.conflated.fetch_add(1, Relaxed);
        if self.pending[id as usize].replace(q).is_none() {
            self.waiting += 1;
        }
        if !self.listed[id as usize] {
            self.listed[id as usize] = true;
            self.dirty.push_back(id);
        }
        Admission::Conflated
    }

    fn push_trade(&mut self, e: Event) -> Admission {
        self.flush_symbol(e.hdr().instrument);
        if self.depth() < self.trade_at {
            self.flush_gap();
            if self.put(Delivery::Event(e)) {
                return Admission::Queued;
            }
        }
        self.shared.dropped_trades.fetch_add(1, Relaxed);
        self.note_loss(Lost::Trades, ts_of(&e));
        Admission::Dropped
    }

    fn push_control(&mut self, e: Event) -> Admission {
        self.flush_symbol(e.hdr().instrument);
        self.flush_gap();
        if self.put(Delivery::Event(e)) {
            return Admission::Queued;
        }
        self.shared.dropped_control.fetch_add(1, Relaxed);
        self.note_loss(Lost::Control, ts_of(&e));
        Admission::Dropped
    }

    /// Call when the feed is idle (a read timed out, a heartbeat arrived) and at least once a second:
    /// queues waiting quotes and any gap marker, so nothing waits on the next event to arrive.
    pub fn tick(&mut self) {
        self.flush_gap();
        self.flush_some(usize::MAX);
        self.flush_gap();
    }

    /// The gateway told us it skipped records after we read too slowly. A marker is queued (now, or as
    /// soon as there is room) in order with the events.
    pub fn note_skip(&mut self, ts: Nanos) {
        self.note_loss(Lost::Skipped, ts);
        self.flush_gap();
    }

    /// Whether anything is still waiting to be queued.
    pub fn is_settled(&self) -> bool {
        self.waiting == 0 && self.gaps.iter().all(Option::is_none)
    }
}

impl Consumer {
    pub fn stats(&self) -> Stats {
        snapshot(&self.shared)
    }

    /// Messages on the ring now.
    pub fn depth(&self) -> usize {
        self.depth.load(Relaxed)
    }

    fn took(&self, d: Delivery) -> Delivery {
        self.depth.fetch_sub(1, Relaxed);
        self.shared.delivered.fetch_add(1, Relaxed);
        d
    }

    pub fn try_recv(&mut self) -> Option<Delivery> {
        match self.rx.try_recv() {
            Ok(d) => Some(self.took(d)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Wait up to `timeout` for the next message. `None` on a timeout or when the producer is gone
    /// and the ring is empty.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<Delivery> {
        match self.rx.recv_timeout(timeout) {
            Ok(d) => Some(self.took(d)),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }
}

#[cfg(test)]
mod tests;
