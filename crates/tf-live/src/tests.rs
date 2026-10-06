use std::io::Write;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbn::encode::EncodeRecord;
use dbn::{ErrorCode, ErrorMsg};
use tf_core::{Event, Px};
use tf_databento::Decoder;
use tf_ingest::{Config as IngestConfig, Lost};
use tf_provider::{Channels, Poll, Provider, ProviderError, Subscription};

use crate::protocol::{
    ApiKey, LiveError, MAX_SYMBOLS_PER_LINE, Sub, Symbols, auth_line, gateway_for,
    parse_auth_response, parse_challenge, sub_lines,
};
use crate::provider::LiveProvider;
use crate::session::Config;
use crate::sha256::{hex, sha256};
use crate::testing::*;

#[test]
fn sha256_matches_the_standards_vectors_and_every_padding_boundary() {
    let h = |b: &[u8]| hex(&sha256(b));
    assert_eq!(
        h(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        h(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        h(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    assert_eq!(
        h(&vec![b'a'; 1_000_000]),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );
    for (n, want) in [
        (
            55,
            "d5e285683cd4efc02d021a5c62014694958901005d6f71e89e0989fac77e4072",
        ),
        (
            56,
            "04c26261370ee7541549d16dee320c723e3fd14671e66a099afe0a377c16888e",
        ),
        (
            63,
            "75220b47218278e656f2013bb8f0c455a25eaf01e86c64924e9d48d89776d6f2",
        ),
        (
            64,
            "7ce100971f64e7001e8fe5a51973ecdfe1ced42befe7ee8d5fd6219506b5393c",
        ),
        (
            65,
            "9537c5fdf120482f7d58d25e9ed583f52c02b4e304ea814db1633ad565aed7e9",
        ),
        (
            119,
            "000b48d4edf0fa7bee3c6236ecd2785baa5db4eeb8bb54341b029e0d9fa5fb0c",
        ),
        (
            120,
            "13f05a0b594787f5ecd315edc96141bd3243203d1b7d4f0836f37308b276ba98",
        ),
    ] {
        assert_eq!(h(&vec![b'x'; n]), want, "{n} bytes");
    }
}

#[test]
fn the_login_line_is_the_digest_of_the_challenge_and_the_key_and_the_key_is_never_shown() {
    let k = key();
    assert_eq!(k.bucket_id(), "yz123");
    let line = auth_line(&k, "XNAS.BASIC", CHALLENGE, Some(5), "tickforge/0.1.0");
    assert_eq!(
        line,
        format!(
            "auth={DIGEST}-yz123|dataset=XNAS.BASIC|encoding=dbn|ts_out=0|client=tickforge/0.1.0|heartbeat_interval_s=5\n"
        )
    );
    assert!(!auth_line(&k, "D", CHALLENGE, None, "c").contains("heartbeat"));
    assert!(!format!("{k:?}").contains("abcdef") && format!("{k:?}").contains("yz123"));
    for (bad, why) in [
        ("", "32"),
        ("$YOUR_API_KEY", "placeholder"),
        ("db-short", "32"),
        ("db-abcdefghijklmnopqrstuvwxyz12\u{e9}", "32"),
        ("db-abcdefghijklmnopqrstuvwxyz1 3", "spaces"),
        ("db-abcdefghijklmnopqrstuvwxyz1|3", "bars"),
    ] {
        let e = ApiKey::new(bad).unwrap_err().to_string();
        assert!(e.contains(why), "{bad}: {e}");
    }
    // A key read from a file arrives with its newline.
    assert!(ApiKey::new(&format!("{KEY}\n")).is_ok());
    assert_eq!(
        gateway_for("XNAS.BASIC"),
        "xnas-basic.lsg.databento.com:13000"
    );
}

#[test]
fn the_gateways_lines_are_read_strictly() {
    assert_eq!(parse_challenge("cram=abc\n").unwrap(), "abc");
    for bad in ["", "cram=", "lsg_version=0.9.4", "CRAM=abc", "cram"] {
        assert!(
            matches!(parse_challenge(bad), Err(LiveError::Protocol(_))),
            "{bad:?}"
        );
    }
    assert_eq!(
        parse_auth_response("success=1|session_id=1234\n").unwrap(),
        "1234"
    );
    assert_eq!(parse_auth_response("success=1\n").unwrap(), "");
    for (bad, said) in [
        (
            "success=0|error=Authentication failed.\n",
            "Authentication failed.",
        ),
        ("error=nope|success=0", "nope"),
        ("garbage\n", "garbage"),
        ("success=2|error=x", "x"),
        ("", ""),
    ] {
        match parse_auth_response(bad) {
            Err(LiveError::Auth(m)) => assert_eq!(m, said, "{bad:?}"),
            other => panic!("{bad:?}: {other:?}"),
        }
    }
}

#[test]
fn a_subscription_is_one_line_or_one_per_five_hundred_symbols() {
    let all = sub_lines(&Sub::all("trades"), 1, None).unwrap();
    assert_eq!(
        all,
        ["schema=trades|stype_in=raw_symbol|symbols=ALL_SYMBOLS|snapshot=0|is_last=1|id=1\n"]
    );
    let names: Vec<String> = (0..1_200).map(|i| format!("S{i}")).collect();
    let sub = Sub {
        schema: "cmbp-1".into(),
        stype_in: "raw_symbol".into(),
        symbols: Symbols::List(names),
        start: Some(1_700_000_000_000_000_000),
        snapshot: false,
    };
    let lines = sub_lines(&sub, 7, None).unwrap();
    assert_eq!(lines.len(), 3);
    for (i, l) in lines.iter().enumerate() {
        assert!(
            l.ends_with("|id=7\n") && l.contains("|start=1700000000000000000|"),
            "{l}"
        );
        assert!(
            l.contains(&format!("|is_last={}|", u8::from(i == 2))),
            "{l}"
        );
    }
    let counts: Vec<usize> = lines
        .iter()
        .map(|l| {
            l.split("symbols=")
                .nth(1)
                .unwrap()
                .split('|')
                .next()
                .unwrap()
                .split(',')
                .count()
        })
        .collect();
    assert_eq!(counts, [MAX_SYMBOLS_PER_LINE, MAX_SYMBOLS_PER_LINE, 200]);
    // A resume overrides the start of every subscription.
    assert!(sub_lines(&sub, 1, Some(5)).unwrap()[0].contains("|start=5|"));
    assert!(sub_lines(&Sub::all("trades"), 1, Some(5)).unwrap()[0].contains("|start=5|"));
    // What cannot be sent is refused here, not left for the gateway to close the connection over.
    let mut snap = Sub::all("trades");
    snap.snapshot = true;
    assert!(sub_lines(&snap, 1, None).is_ok());
    assert!(sub_lines(&snap, 1, Some(1)).is_err());
    for bad in [
        vec![],
        vec!["A,B".to_owned()],
        vec!["A B".to_owned()],
        vec![String::new()],
        vec!["A|B".to_owned()],
    ] {
        let s = Sub {
            symbols: Symbols::List(bad),
            ..Sub::all("trades")
        };
        assert!(sub_lines(&s, 1, None).is_err());
    }
}

fn cfg(g: &Gateway) -> Config {
    let mut c = Config::new(
        key(),
        "XNAS.BASIC",
        vec![Sub::all("trades"), Sub::all("cmbp-1")],
    );
    c.addr = Some(g.addr.clone());
    c.stall_secs = 5;
    c
}

fn provider(g: &Gateway) -> LiveProvider {
    LiveProvider::new(
        cfg(g),
        IngestConfig {
            capacity: 1 << 12,
            instruments: 64,
            ..IngestConfig::default()
        },
    )
    .unwrap()
}

fn drain(p: &mut LiveProvider, secs: u64) -> (Vec<Event>, Poll) {
    let mut all = Vec::new();
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let mut out = Vec::new();
        match p.poll(&mut out, 1000) {
            Poll::Events(_) => all.extend(out),
            Poll::Idle => std::thread::sleep(Duration::from_millis(2)),
            other => return (all, other),
        }
        if Instant::now() > end {
            return (all, Poll::Idle);
        }
    }
}

fn direct(bytes: &[u8]) -> Vec<Event> {
    Decoder::new(bytes)
        .unwrap()
        .filter_map(|i| {
            if let tf_databento::Item::Event(e) = i.unwrap() {
                Some(e)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn a_session_logs_in_subscribes_starts_and_delivers_exactly_what_the_stream_carried() {
    let bytes = day(500, 1_000_000_000);
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Close)]);
    let mut p = provider(&g);
    p.connect().unwrap();
    p.subscribe(&Subscription::all(Channels::ALL)).unwrap();
    let (events, end) = drain(&mut p, 10);
    assert_eq!(end, Poll::Disconnected, "the stream ended");
    assert_eq!(events, direct(&bytes));
    assert_eq!(events.len(), 500);
    // Prices are the wire's, to the billionth: 180.00 and up by a cent.
    let Event::Trade(t) = events[3] else { panic!() };
    assert_eq!(t.px, Px::from_raw(18_003 * 10_000_000));
    // What the client sent, in order.
    let seen = g.lines();
    assert_eq!(seen.len(), 1);
    let l = &seen[0];
    assert!(
        l[0].starts_with(&format!(
            "auth={DIGEST}-yz123|dataset=XNAS.BASIC|encoding=dbn|ts_out=0|client=tickforge/"
        )) && l[0].ends_with("|heartbeat_interval_s=5\n"),
        "{}",
        l[0]
    );
    assert_eq!(
        l[1],
        "schema=trades|stype_in=raw_symbol|symbols=ALL_SYMBOLS|snapshot=0|is_last=1|id=1\n"
    );
    assert_eq!(
        l[2],
        "schema=cmbp-1|stype_in=raw_symbol|symbols=ALL_SYMBOLS|snapshot=0|is_last=1|id=2\n"
    );
    assert_eq!(l[3], "start_session\n");
    assert_eq!(l.len(), 4);
}

#[test]
fn the_names_the_gateway_gives_are_readable_while_the_session_runs() {
    let g = gateway(vec![Conn::new(day(4, 1_000_000_000), Then::Hang)]);
    let mut p = provider(&g);
    p.connect().unwrap();
    assert_eq!(p.session_id(), Some("4242"));
    let sh = p.shared().unwrap().clone();
    let end = Instant::now() + Duration::from_secs(5);
    while sh.mappings.load(std::sync::atomic::Ordering::Relaxed) < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        sh.names(),
        [Some("AAPL".to_owned()), Some("MSFT".to_owned())]
    );
    p.disconnect();
}

#[test]
fn a_refused_login_is_an_error_with_the_gateways_words_and_the_provider_can_try_again() {
    let g = gateway(vec![
        Conn {
            refuse: Some("Authentication failed.".into()),
            stream: vec![],
            then: Then::Close,
        },
        Conn::new(day(10, 1_000_000_000), Then::Close),
    ]);
    let mut p = provider(&g);
    match p.connect() {
        Err(ProviderError::Source(m)) => assert!(
            m.contains("refused the login") && m.contains("Authentication failed."),
            "{m}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(p.poll(&mut Vec::new(), 10), Poll::Disconnected);
    p.connect().unwrap();
    assert_eq!(drain(&mut p, 10).0.len(), 10);
    // A key the gateway does not know (the fake knows one).
    let g2 = gateway(vec![Conn::new(vec![], Then::Close)]);
    let mut c = cfg(&g2);
    c.key = ApiKey::new("db-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
    let mut q = LiveProvider::new(
        c,
        IngestConfig {
            capacity: 64,
            instruments: 8,
            ..IngestConfig::default()
        },
    )
    .unwrap();
    assert!(
        matches!(q.connect(), Err(ProviderError::Source(m)) if m.contains("Authentication failed"))
    );
}

#[test]
fn something_that_is_not_the_gateway_is_not_logged_into() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"HTTP/1.1 400 Bad Request\r\ncram=nope\n")
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
    });
    let mut c = Config::new(key(), "XNAS.BASIC", vec![Sub::all("trades")]);
    c.addr = Some(addr);
    let e = crate::login(&c).err().unwrap();
    assert!(
        matches!(&e, LiveError::Protocol(m) if m.contains("expected a greeting")),
        "{e}"
    );
    t.join().unwrap();
    // A greeting and then nothing is a gateway that went away.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.write_all(b"lsg_version=0.9.4\n").unwrap();
    });
    let mut c = Config::new(key(), "XNAS.BASIC", vec![]);
    c.addr = Some(addr);
    assert!(
        matches!(crate::login(&c).err().unwrap(), LiveError::Protocol(m) if m.contains("closed"))
    );
    t.join().unwrap();
}

#[test]
fn a_login_alone_asks_for_nothing() {
    let g = gateway(vec![Conn::new(vec![], Then::Close)]);
    let l = crate::login(&cfg(&g)).unwrap();
    assert_eq!(l.session_id, "4242");
    drop(l);
    std::thread::sleep(Duration::from_millis(50));
    // All the fake saw was the login: no subscription, no start.
    let seen = g.lines();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].len(), 1);
    assert!(seen[0][0].starts_with("auth="));
}

#[test]
fn the_gateway_not_being_there_is_an_error_not_a_hang() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    let mut c = Config::new(key(), "XNAS.BASIC", vec![Sub::all("trades")]);
    c.addr = Some(addr);
    c.connect_timeout = Duration::from_secs(2);
    let mut p = LiveProvider::new(
        c,
        IngestConfig {
            capacity: 64,
            instruments: 8,
            ..IngestConfig::default()
        },
    )
    .unwrap();
    let start = Instant::now();
    assert!(matches!(p.connect(), Err(ProviderError::Source(m)) if m.contains("cannot connect")));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_provider_keeps_the_contract_of_the_others() {
    let g = gateway(vec![Conn::new(day(40, 1_000_000_000), Then::Hang)]);
    let mut p = provider(&g);
    assert_eq!(p.capabilities().provider, tf_core::ProviderId::Databento);
    assert!(p.capabilities().wildcard);
    // Before a session: nothing to poll, nothing to subscribe on.
    assert_eq!(p.poll(&mut Vec::new(), 10), Poll::Disconnected);
    assert_eq!(
        p.subscribe(&Subscription::all(Channels::ALL)),
        Err(ProviderError::NotConnected)
    );
    // Connecting twice is one session (the fake would not accept a second).
    p.connect().unwrap();
    p.connect().unwrap();
    // Only what is subscribed is delivered: trades of the second symbol (dense id 1).
    p.subscribe(&Subscription::list(Channels::ALL, vec![1]))
        .unwrap();
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while got.len() < 20 && Instant::now() < end {
        let mut out = Vec::new();
        if let Poll::Events(_) = p.poll(&mut out, 100) {
            got.extend(out);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(got.len(), 20);
    assert!(got.iter().all(|e| e.instrument() == 1));
    let mut none = Vec::new();
    assert_eq!(p.poll(&mut none, 0), Poll::Idle);
    // Disconnecting releases the session, and polling after it says so.
    p.disconnect();
    assert_eq!(p.poll(&mut Vec::new(), 10), Poll::Disconnected);
    p.disconnect();
}

#[test]
fn after_a_drop_a_new_session_replays_from_the_resume_time_and_instruments_keep_their_ids() {
    // The first session dies after a few trades; the second is asked to replay from the last one seen,
    // and sends that boundary trade again (it is the gateway's right to), then more, naming the same
    // instruments by the same raw ids but in the other order.
    let first = day(10, 1_000_000_000);
    let second = stream(|e| {
        e.encode_record(&mapping(20_002, "MSFT")).unwrap();
        e.encode_record(&mapping(20_001, "AAPL")).unwrap();
        for i in 9..14u32 {
            e.encode_record(&trade(
                20_001 + i % 2,
                1_000_000_000 + u64::from(i) * 1_000,
                18_000 + i64::from(i),
                100,
                i,
            ))
            .unwrap();
        }
    });
    let g = gateway(vec![
        Conn::new(first, Then::Close),
        Conn::new(second, Then::Close),
    ]);
    let mut p = provider(&g);
    p.connect().unwrap();
    let (events, end) = drain(&mut p, 10);
    assert_eq!((events.len(), end), (10, Poll::Disconnected));
    let resume = p.last_event_ts();
    assert_eq!(resume, 1_000_000_000 + 9_000);
    p.reconnect(Some(resume)).unwrap();
    let (more, end) = drain(&mut p, 10);
    assert_eq!((more.len(), end), (5, Poll::Disconnected));
    // The replay's first trade is the boundary one: same ts_recv, same sequence.
    assert_eq!(
        (more[0].ts_recv(), more[0].seq()),
        (resume, events[9].seq())
    );
    // AAPL is raw 20,001 and was id 0 in the first session; it is id 0 in the second, though the
    // gateway named MSFT first this time.
    let ids: Vec<u32> = events.iter().chain(&more).map(Event::instrument).collect();
    let expect: Vec<u32> = (0..10u32).chain(9..14).map(|i| i % 2).collect();
    assert_eq!(ids, expect);
    // The second session asked for the replay on every subscription.
    let seen = g.lines();
    assert_eq!(seen.len(), 2);
    assert!(!seen[0][1].contains("start="));
    assert!(
        seen[1][1].contains(&format!("|start={resume}|"))
            && seen[1][2].contains(&format!("|start={resume}|")),
        "{:?}",
        seen[1]
    );
}

#[test]
fn a_session_that_goes_silent_is_called_dead_and_one_that_sends_heartbeats_is_not() {
    let hang = gateway(vec![Conn::new(day(3, 1_000_000_000), Then::Hang)]);
    let mut c = cfg(&hang);
    c.stall_secs = 1;
    let mut p = LiveProvider::new(
        c,
        IngestConfig {
            capacity: 256,
            instruments: 8,
            ..IngestConfig::default()
        },
    )
    .unwrap();
    p.connect().unwrap();
    let shared = p.shared().unwrap().clone();
    let t = Instant::now();
    let (events, end) = drain(&mut p, 10);
    assert_eq!((events.len(), end), (3, Poll::Disconnected));
    assert_eq!(shared.state(), crate::State::Failed);
    assert!(
        shared
            .error()
            .is_some_and(|e| e.contains("not even a heartbeat, for 1 seconds")),
        "{:?}",
        shared.error()
    );
    assert!(
        t.elapsed() >= Duration::from_millis(900) && t.elapsed() < Duration::from_secs(6),
        "{:?}",
        t.elapsed()
    );
    // A gateway that talks only in heartbeats keeps the session alive past the stall time.
    let live = gateway(vec![Conn::new(
        day(3, 1_000_000_000),
        Then::Heartbeat(2_400),
    )]);
    let mut c = cfg(&live);
    c.stall_secs = 1;
    let mut q = LiveProvider::new(
        c,
        IngestConfig {
            capacity: 256,
            instruments: 8,
            ..IngestConfig::default()
        },
    )
    .unwrap();
    q.connect().unwrap();
    let t = Instant::now();
    let (events, end) = drain(&mut q, 10);
    assert_eq!((events.len(), end), (3, Poll::Disconnected));
    assert!(
        t.elapsed() >= Duration::from_millis(2_000),
        "it stayed up while heartbeats came: {:?}",
        t.elapsed()
    );
    assert!(q.stats().offered == 3);
}

#[test]
fn what_the_gateway_says_about_skipped_records_becomes_a_gap_in_order_with_the_events() {
    let bytes = stream(|e| {
        e.encode_record(&mapping(20_001, "AAPL")).unwrap();
        e.encode_record(&trade(20_001, 1_000_000_000, 18_000, 100, 1))
            .unwrap();
        e.encode_record(&trade(20_001, 1_000_001_000, 18_001, 100, 2))
            .unwrap();
        e.encode_record(&ErrorMsg::new(
            1_000_002_000,
            Some(ErrorCode::SkippedRecordsAfterSlowReading),
            "skipped 120 records",
            true,
        ))
        .unwrap();
        e.encode_record(&trade(20_001, 1_000_003_000, 18_002, 100, 3))
            .unwrap();
    });
    let g = gateway(vec![Conn::new(bytes, Then::Close)]);
    let mut p = provider(&g);
    p.connect().unwrap();
    let mut order = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        match p.recv() {
            Some(tf_ingest::Delivery::Event(e)) => order.push(format!("trade {}", e.ts_recv())),
            Some(tf_ingest::Delivery::Gap(g)) => {
                order.push(format!("gap {:?} {}", g.lost, g.count))
            }
            None if p
                .shared()
                .is_some_and(|s| s.state() != crate::State::Streaming)
                && order.len() >= 4 =>
            {
                break;
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    assert_eq!(
        order,
        [
            "trade 1000000000",
            "trade 1000001000",
            "gap Skipped 1",
            "trade 1000003000"
        ]
    );
    assert_eq!(p.gaps().len(), 1);
    assert_eq!(p.gaps()[0].lost, Lost::Skipped);
    assert_eq!(p.stats().gaps, 1);
    assert_eq!(
        p.shared()
            .unwrap()
            .skips
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn an_engine_that_does_not_read_never_blocks_the_feed() {
    // 200,000 trades into a queue of 64 that nobody reads: the reader thread must finish, with the rest
    // counted as dropped, not wait for room.
    let bytes = day(200_000, 1_000_000_000);
    let g = gateway(vec![Conn::new(bytes, Then::Close)]);
    let mut p = LiveProvider::new(
        cfg(&g),
        IngestConfig {
            capacity: 64,
            instruments: 8,
            ..IngestConfig::default()
        },
    )
    .unwrap();
    p.connect().unwrap();
    let sh = p.shared().unwrap().clone();
    let end = Instant::now() + Duration::from_secs(20);
    while sh.state() == crate::State::Streaming && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(sh.state(), crate::State::Ended, "{:?}", sh.error());
    let s = p.stats();
    assert_eq!(s.offered, 200_000);
    assert!(s.dropped_trades > 100_000, "{s:?}");
    assert_eq!(
        s.offered,
        s.queued - s.gaps + s.dropped_trades + s.conflated - s.flushed
            + s.dropped_quotes
            + s.dropped_control,
        "every event is accounted for: {s:?}"
    );
}

struct Collect(Arc<Mutex<Vec<u8>>>, usize);

impl crate::RawSink for Collect {
    fn record(&mut self, rec: &dbn::RecordRef<'_>) -> Result<(), String> {
        let mut v = self.0.lock().unwrap();
        if v.len() >= self.1 {
            return Err("the disk is full".to_owned());
        }
        v.push(rec.header().rtype);
        Ok(())
    }
}

#[test]
fn every_record_goes_to_the_raw_sink_and_a_sink_that_refuses_stops_the_session() {
    let bytes = day(100, 1_000_000_000);
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Close)]);
    let seen: Arc<Mutex<Vec<u8>>> = Arc::default();
    let mut p = provider(&g).with_sink(Arc::new(Mutex::new(Collect(seen.clone(), usize::MAX))));
    p.connect().unwrap();
    let (events, end) = drain(&mut p, 10);
    assert_eq!((events.len(), end), (100, Poll::Disconnected));
    // Two mappings and a hundred trades, as the gateway sent them.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 102);
    assert_eq!(
        seen.iter()
            .filter(|r| **r == dbn::rtype::SYMBOL_MAPPING)
            .count(),
        2
    );
    drop(seen);
    // A sink that stops taking records ends the session, with its words, and the events before it
    // are still delivered.
    let g = gateway(vec![Conn::new(bytes, Then::Hang)]);
    let seen: Arc<Mutex<Vec<u8>>> = Arc::default();
    let mut p = provider(&g).with_sink(Arc::new(Mutex::new(Collect(seen.clone(), 50))));
    p.connect().unwrap();
    let sh = p.shared().unwrap().clone();
    let (events, end) = drain(&mut p, 10);
    assert_eq!(end, Poll::Disconnected);
    assert_eq!(
        events.len(),
        48,
        "50 records taken: two mappings and 48 trades"
    );
    assert_eq!(sh.state(), crate::State::Failed);
    assert!(
        sh.error()
            .is_some_and(|e| e.contains("tap: the disk is full")),
        "{:?}",
        sh.error()
    );
}
