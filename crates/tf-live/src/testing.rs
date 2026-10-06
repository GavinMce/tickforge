//! A fake Databento live gateway and DBN stream builders, for testing anything that reads the feed.
//!
//! Written from the official client's behaviour (see ADR 0049): it greets, sends a challenge, checks the
//! login against a fixed key, takes subscription lines until `start_session`, then sends what it was told
//! to and ends in one of a few ways. It records the lines each connection sent.

use std::ffi::c_char;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{
    FlagSet, MetadataBuilder, RecordHeader, SType, SymbolMappingMsg, SystemMsg, TradeMsg, rtype,
};

use crate::protocol::ApiKey;

pub const KEY: &str = "db-abcdefghijklmnopqrstuvwxyz123";
pub const CHALLENGE: &str = "G6EDS7OL0FZGYP4mNdhwTGptwJrMOrL2";
/// SHA-256 of `CHALLENGE|KEY`, from Python's hashlib.
pub const DIGEST: &str = "fe2c2793e5b328ff8c87036f8fd54a4862a106b4ae58f026b99703f1727161c5";

pub fn key() -> ApiKey {
    ApiKey::new(KEY).unwrap()
}

/// What a fake gateway does for one connection.
#[derive(Clone)]
pub struct Conn {
    /// Reply to the login with this error instead of success.
    pub refuse: Option<String>,
    /// What to send once the session starts, written in small pieces.
    pub stream: Vec<u8>,
    /// What to do when it has been sent.
    pub then: Then,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Then {
    Close,
    /// Keep the connection open and silent.
    Hang,
    /// Heartbeat every 200 ms for this many milliseconds, then close.
    Heartbeat(u64),
}

impl Conn {
    pub fn new(stream: Vec<u8>, then: Then) -> Conn {
        Conn {
            refuse: None,
            stream,
            then,
        }
    }
}

pub struct Gateway {
    pub addr: String,
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    thread: Option<JoinHandle<()>>,
}

pub fn gateway(conns: Vec<Conn>) -> Gateway {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let log = seen.clone();
    let thread = std::thread::spawn(move || {
        for conn in conns {
            let (sock, _) = listener.accept().unwrap();
            let mut lines = Vec::new();
            let mut w = sock.try_clone().unwrap();
            let mut r = BufReader::new(sock);
            w.write_all(format!("lsg_version=0.9.4\ncram={CHALLENGE}\n").as_bytes())
                .unwrap();
            let mut auth = String::new();
            r.read_line(&mut auth).unwrap();
            lines.push(auth.clone());
            let want = format!("auth={DIGEST}-yz123|dataset=XNAS.BASIC|");
            if let Some(e) = &conn.refuse {
                w.write_all(format!("success=0|error={e}\n").as_bytes())
                    .unwrap();
                log.lock().unwrap().push(lines);
                continue;
            }
            if !auth.starts_with(&want) {
                w.write_all(b"success=0|error=Authentication failed.\n")
                    .unwrap();
                log.lock().unwrap().push(lines);
                continue;
            }
            w.write_all(b"success=1|session_id=4242\n").unwrap();
            loop {
                let mut l = String::new();
                if r.read_line(&mut l).unwrap() == 0 {
                    break;
                }
                let done = l == "start_session\n";
                lines.push(l);
                if done {
                    break;
                }
            }
            log.lock().unwrap().push(lines);
            for piece in conn.stream.chunks(1000) {
                if w.write_all(piece).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            match conn.then {
                Then::Close => {}
                Then::Hang => {
                    // Until the client goes away.
                    let mut b = [0u8; 16];
                    let mut s = r.into_inner();
                    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    let _ = s.read(&mut b);
                }
                Then::Heartbeat(ms) => {
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_millis(ms) {
                        let mut hb = Vec::new();
                        hb.extend_from_slice(
                            dbn::RecordRef::from(&SystemMsg::heartbeat(1)).as_ref(),
                        );
                        if w.write_all(&hb).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        }
    });
    Gateway {
        addr,
        seen,
        thread: Some(thread),
    }
}

impl Gateway {
    pub fn lines(&self) -> Vec<Vec<String>> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        // A test that failed may leave the thread waiting; it is detached, not joined.
        let _ = self.thread.take();
    }
}

pub fn trade(raw: u32, ts: u64, cents: i64, size: u32, seq: u32) -> TradeMsg {
    TradeMsg {
        hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, 81, raw, ts - 1_000),
        price: cents * 10_000_000,
        size,
        action: b'T' as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        depth: 0,
        ts_recv: ts,
        ts_in_delta: 0,
        sequence: seq,
    }
}

pub fn mapping(raw: u32, name: &str) -> SymbolMappingMsg {
    SymbolMappingMsg::new(raw, 0, SType::RawSymbol, name, SType::RawSymbol, name, 0, 0).unwrap()
}

/// A DBN stream as the gateway sends it: the header, then whatever `write` puts in it.
pub fn stream(write: impl FnOnce(&mut DbnEncoder<&mut Vec<u8>>)) -> Vec<u8> {
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(0)
        .stype_in(Some(SType::RawSymbol))
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        write(&mut e);
    }
    bytes
}

/// Two symbols and `n` trades alternating between them.
pub fn day(n: u32, first_ts: u64) -> Vec<u8> {
    stream(|e| {
        e.encode_record(&mapping(20_001, "AAPL")).unwrap();
        e.encode_record(&mapping(20_002, "MSFT")).unwrap();
        for i in 0..n {
            e.encode_record(&trade(
                20_001 + i % 2,
                first_ts + u64::from(i) * 1_000,
                18_000 + i64::from(i),
                100,
                i,
            ))
            .unwrap();
        }
    })
}

/// One raw record, as bytes, with the time it was received.
pub type Raw = (u64, Vec<u8>);

fn bytes_of<R: dbn::Record + dbn::encode::DbnEncodable>(r: &R) -> Vec<u8> {
    dbn::RecordRef::from(r).as_ref().to_vec()
}

fn quote_rec(raw: u32, ts: u64, bid_cents: i64, ask_cents: i64) -> dbn::Cmbp1Msg {
    dbn::Cmbp1Msg {
        hd: RecordHeader::new::<dbn::Cmbp1Msg>(rtype::CMBP_1, 88, raw, ts - 500),
        price: 0,
        size: 0,
        action: b'A' as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        _reserved1: [0],
        ts_recv: ts,
        ts_in_delta: 0,
        _reserved2: [0; 4],
        levels: [dbn::ConsolidatedBidAskPair {
            bid_px: bid_cents * 10_000_000,
            ask_px: ask_cents * 10_000_000,
            bid_sz: 100_000,
            ask_sz: 100_000,
            bid_pb: 81,
            _reserved1: [0; 2],
            ask_pb: 82,
            _reserved2: [0; 2],
        }],
    }
}

/// `symbols` symbols named `S00`.. with raw ids `20_000 + i`: the mapping record of each, as raw bytes.
pub fn mappings(symbols: u32) -> Vec<Raw> {
    (0..symbols)
        .map(|i| (0, bytes_of(&mapping(20_000 + i, &format!("S{i:02}")))))
        .collect()
}

/// A day's market records: each second every symbol quotes at `2000 +- 1` cents and trades `1 + i % 3`
/// times at 2000 (S02 `bump` cents dearer from the third second), each record with a time of its own, in
/// the order received.
pub fn market_records(symbols: u32, secs: u64, t0: u64, bump: i64) -> Vec<Raw> {
    const SEC: u64 = 1_000_000_000;
    let mut v: Vec<Raw> = Vec::new();
    let mut seq = 0u32;
    for sec in 0..secs {
        for i in 0..symbols {
            let ts = t0 + sec * SEC + u64::from(i) * 1_000_000;
            let cents = 2_000 + if i == 2 && sec >= 2 { bump } else { 0 };
            v.push((
                ts,
                bytes_of(&quote_rec(20_000 + i, ts, cents - 1, cents + 1)),
            ));
            for k in 0..=u64::from(i % 3) {
                seq += 1;
                v.push((
                    ts + 1 + k,
                    bytes_of(&trade(20_000 + i, ts + 1 + k, cents, 100, seq)),
                ));
            }
        }
    }
    v.sort_by_key(|r| r.0);
    v
}

/// A DBN stream of the mappings and then these records.
pub fn stream_of(symbols: u32, records: &[Raw]) -> Vec<u8> {
    let mut bytes = stream(|_| {});
    for (_, b) in mappings(symbols) {
        bytes.extend_from_slice(&b);
    }
    for (_, b) in records {
        bytes.extend_from_slice(b);
    }
    bytes
}
