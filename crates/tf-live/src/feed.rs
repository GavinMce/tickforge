//! The reader thread: DBN off the socket, into the ingest queue.
//!
//! One thread decodes the gateway's stream (blocking reads, with a timeout that is the stall detector)
//! and offers each event to the [`tf_ingest::Producer`], which never blocks. It is the only thread that
//! touches the socket or the decoder, so nothing here needs a lock on the hot path. What it learns about
//! the session (names of instruments, notices) is published in [`FeedShared`] for anyone to read.

use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tf_core::Nanos;
use tf_databento::{DecodeError, Decoder, InstrumentMap, Item};
use tf_ingest::Producer;

use crate::protocol::LiveError;
use crate::session::{Config, Session, open};

/// Where the session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Streaming,
    /// The gateway closed the stream cleanly.
    Ended,
    /// It broke: see [`FeedShared::error`].
    Failed,
}

#[derive(Default)]
pub struct FeedShared {
    state: AtomicU8,
    error: Mutex<Option<String>>,
    /// The symbol of each dense instrument id, as the gateway has named them so far.
    names: Mutex<Vec<Option<String>>>,
    pub mappings: AtomicU64,
    pub records: AtomicU64,
    pub heartbeats: AtomicU64,
    pub skips: AtomicU64,
    pub slow_warnings: AtomicU64,
    pub replay_completed: AtomicBool,
    pub last_event_ts: AtomicU64,
}

impl FeedShared {
    pub fn state(&self) -> State {
        match self.state.load(Relaxed) {
            0 => State::Streaming,
            1 => State::Ended,
            _ => State::Failed,
        }
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// The names the gateway has given instruments so far, by dense id (`None` where one was seen in
    /// a record but not yet named).
    pub fn names(&self) -> Vec<Option<String>> {
        self.names.lock().map(|n| n.clone()).unwrap_or_default()
    }

    fn set_name(&self, id: u32, symbol: String) {
        if let Ok(mut n) = self.names.lock() {
            if n.len() <= id as usize {
                n.resize(id as usize + 1, None);
            }
            n[id as usize] = Some(symbol);
        }
    }

    fn end(&self, state: State, error: Option<String>) {
        if let (Some(e), Ok(mut slot)) = (error, self.error.lock()) {
            *slot = Some(e);
        }
        self.state
            .store(if state == State::Ended { 1 } else { 2 }, Relaxed);
    }
}

/// What the thread hands back when it stops, so a new session can carry on from it.
pub struct Returned {
    pub producer: Producer,
    pub instruments: InstrumentMap,
}

pub struct LiveFeed {
    shared: Arc<FeedShared>,
    stop: Arc<AtomicBool>,
    control: std::net::TcpStream,
    thread: Option<JoinHandle<Returned>>,
    pub session_id: String,
}

impl LiveFeed {
    /// Log in, subscribe, start the session and begin reading. `instruments` is what earlier sessions
    /// numbered, so an instrument keeps its id across a reconnect. With `start`, the gateway replays from
    /// that time first.
    pub fn start(
        cfg: &Config,
        producer: Producer,
        instruments: InstrumentMap,
        start: Option<Nanos>,
    ) -> Result<LiveFeed, Box<(LiveError, Returned)>> {
        let session = match open(cfg, start) {
            Ok(s) => s,
            Err(e) => {
                return Err(Box::new((
                    e,
                    Returned {
                        producer,
                        instruments,
                    },
                )));
            }
        };
        let Session {
            reader,
            control,
            session_id,
        } = session;
        // The gateway sends the DBN header as soon as the session starts; a decoder cannot be built
        // before it comes, and a gateway that never sends it is a stalled one.
        let decoder = match Decoder::new(reader) {
            Ok(d) => d.with_instruments(instruments),
            Err(e) => {
                return Err(Box::new((
                    LiveError::Protocol(format!("no DBN header from the gateway: {e}")),
                    Returned {
                        producer,
                        instruments,
                    },
                )));
            }
        };
        let shared = Arc::new(FeedShared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let stall = cfg.stall_secs;
        let (sh, st) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("tf-live-feed".to_owned())
            .spawn(move || read_loop(decoder, producer, &sh, &st, stall))
            .expect("a thread can be started");
        Ok(LiveFeed {
            shared,
            stop,
            control,
            thread: Some(thread),
            session_id,
        })
    }

    pub fn shared(&self) -> &Arc<FeedShared> {
        &self.shared
    }

    /// Stop reading and take back the producer and the numbering. Safe to call on a feed that has
    /// already ended.
    pub fn stop(mut self) -> Returned {
        self.stop.store(true, Relaxed);
        let _ = self.control.shutdown(Shutdown::Both);
        self.thread
            .take()
            .expect("joined once")
            .join()
            .expect("the feed thread does not panic")
    }
}

impl Drop for LiveFeed {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.stop.store(true, Relaxed);
            let _ = self.control.shutdown(Shutdown::Both);
            let _ = t.join();
        }
    }
}

fn stalled(e: &DecodeError) -> bool {
    matches!(e, DecodeError::Dbn(dbn::Error::Io { source, .. }) if matches!(source.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

fn read_loop(
    mut decoder: Decoder<'static>,
    mut producer: Producer,
    shared: &FeedShared,
    stop: &AtomicBool,
    stall_secs: u64,
) -> Returned {
    let mut since_tick = 0u32;
    loop {
        match decoder.next_item() {
            Ok(Some(item)) => {
                shared.records.fetch_add(1, Relaxed);
                match item {
                    Item::Event(e) => {
                        shared.last_event_ts.store(e.ts_recv(), Relaxed);
                        producer.push(e);
                        since_tick += 1;
                        if since_tick >= 256 {
                            since_tick = 0;
                            producer.tick();
                        }
                    }
                    Item::Mapping { instrument, symbol } => {
                        shared.mappings.fetch_add(1, Relaxed);
                        shared.set_name(instrument, symbol);
                    }
                    Item::Notice(n) => {
                        if n.is_skip() {
                            shared.skips.fetch_add(1, Relaxed);
                            producer.note_skip(shared.last_event_ts.load(Relaxed));
                        } else if n.is_heartbeat() {
                            shared.heartbeats.fetch_add(1, Relaxed);
                            producer.tick();
                        } else if n.is_slow_reader_warning() {
                            shared.slow_warnings.fetch_add(1, Relaxed);
                        } else if n.is_replay_completed() {
                            shared.replay_completed.store(true, Relaxed);
                        }
                    }
                    Item::Ignored { .. } => {}
                }
            }
            Ok(None) => {
                producer.tick();
                shared.end(State::Ended, None);
                break;
            }
            Err(e) => {
                producer.tick();
                let why = if stop.load(Relaxed) {
                    "stopped".to_owned()
                } else if stalled(&e) {
                    LiveError::Stalled(stall_secs).to_string()
                } else {
                    format!("the stream broke: {e}")
                };
                shared.end(State::Failed, Some(why));
                break;
            }
        }
        if stop.load(Relaxed) {
            shared.end(State::Ended, None);
            break;
        }
    }
    // Names the gateway gave are on the decoder's map too, which is what a new session needs.
    for id in 0..decoder.instruments().len() as u32 {
        if let Some(s) = decoder.instruments().symbol(id) {
            shared.set_name(id, s.to_owned());
        }
    }
    Returned {
        producer,
        instruments: decoder.into_instruments(),
    }
}
