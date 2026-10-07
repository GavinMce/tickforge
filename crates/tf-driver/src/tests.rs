use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbn::encode::EncodeRecord;
use dbn::{ErrorCode, ErrorMsg};
use tf_budget::{Group, LossLimits, Strategy as BudgetStrategy, Tree};
use tf_core::{Event, Px};
use tf_engine::{PromoterConfig, ScannerConfig};
use tf_host::{Certificate, HostConfig, Reference, Route, StrategyDef, certify, compare, runner};
use tf_ingest::Config as IngestConfig;
use tf_ledger::MemStore;
use tf_live::testing::{Conn, Then, gateway, key, market_records, stream, stream_of, trade};
use tf_live::{Config as LiveConfig, RawSink, Sub};
use tf_risk::{Budgets, Limits};
use tf_strategy::intent::{Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::sim::SimConfig;
use tf_strategy::{CrossStrategy, Ctx, MemberView, Request};
use tf_universe::{LiveFeature, Snapshot, Spec};

use crate::{DriverConfig, DriverError, Outcome, Reconnect, Warmup, run, run_with_sink};

const SEC: u64 = 1_000_000_000;
const T0: u64 = 100 * SEC;
const D: u128 = 1_000_000_000;
const SYMBOLS: u32 = 12;

struct Buyer {
    id: u16,
    qty: u32,
    n: u32,
    reviews: u32,
}

impl CrossStrategy for Buyer {
    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> u64 {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.reviews += 1;
        if self.reviews > self.n {
            return;
        }
        let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first().copied() else {
            return;
        };
        let Some(last) = view.state(id).and_then(|s| s.last_px) else {
            return;
        };
        let req = Request {
            side: Side::Buy,
            qty: self.qty,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(Px::from_raw(last.raw() + 50_000_000)),
            protect: Some(Protective {
                stop_trigger: Px::from_raw(last.raw() / 2),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            reason: 1,
        };
        let _ = ctx.submit(id, req);
    }
}

fn def(id: u16, universe: &str, qty: u32) -> StrategyDef {
    StrategyDef {
        id,
        name: format!("buyer{id}"),
        params: format!("{qty}"),
        universe: Spec::parse(universe).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(Buyer {
                id,
                qty,
                n: 8,
                reviews: 0,
            })
        }),
    }
}

const LOW: &str = "universe v1\nstatic adv_shares <= 600\n";
const HIGH: &str = "universe v1\nstatic adv_shares >= 700\n";

fn snapshot() -> Snapshot {
    let mut text = String::from("# as_of 2026-10-02\nsymbol,price,adv_shares\n");
    for i in 0..SYMBOLS {
        text.push_str(&format!("S{i:02},20.00,{}\n", (i + 1) * 100));
    }
    Snapshot::parse(&text).unwrap()
}

fn host_cfg() -> HostConfig {
    let tree = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits {
            soft: 300,
            hard: 600,
        },
        strategies: vec![
            BudgetStrategy {
                id: "s1".into(),
                share: 5_000,
            },
            BudgetStrategy {
                id: "s2".into(),
                share: 5_000,
            },
        ],
    }])
    .unwrap();
    HostConfig {
        id_space: 64,
        limits: Limits::new(50_000 * D, 100_000, 5_000_000 * D, 90_000 * D, 10_000, SEC).unwrap(),
        budgets: Some(
            Budgets::new(
                tree,
                100_000 * D,
                [(1u16, "s1".to_owned()), (2, "s2".to_owned())],
            )
            .unwrap(),
        ),
        promoter: PromoterConfig {
            max_tier1: 4,
            min_dwell_secs: 0,
            ..PromoterConfig::default()
        },
        scanner: ScannerConfig {
            min_volume: 1_000_000,
            ..ScannerConfig::default()
        },
        sim: SimConfig {
            latency_ns: 0,
            borrow_bps_per_year: 0,
        },
        min_certified_events: 10,
        start_ts: 0,
        bars: None,
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-driver-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// The events and names a stream carries, as the host will see them.
fn events_of(bytes: &[u8]) -> (Vec<Event>, tf_core::SymbolTable) {
    let mut d = tf_databento::Decoder::new(bytes).unwrap();
    let mut ev = Vec::new();
    while let Some(i) = d.next_item().unwrap() {
        if let tf_databento::Item::Event(e) = i {
            ev.push(e);
        }
    }
    let names: Vec<Option<String>> = (0..d.instruments().len() as u32)
        .map(|i| d.instruments().symbol(i).map(str::to_owned))
        .collect();
    (ev, tf_host::symbol_table(&names))
}

fn defs() -> Vec<StrategyDef> {
    vec![def(1, LOW, 100), def(2, HIGH, 50)]
}

fn certified(bytes: &[u8]) -> Vec<(StrategyDef, Certificate)> {
    let (events, symbols) = events_of(bytes);
    let reference = Reference {
        symbols,
        snapshot: snapshot(),
    };
    defs()
        .into_iter()
        .map(|d| {
            let c = certify(&d, &host_cfg(), &reference, &events, 1).unwrap();
            (d, c)
        })
        .collect()
}

fn driver_cfg(addr: &str, dir: PathBuf, close_secs: Option<u64>) -> DriverConfig {
    let mut live = LiveConfig::new(
        key(),
        "XNAS.BASIC",
        vec![Sub::all("trades"), Sub::all("cmbp-1")],
    );
    live.addr = Some(addr.to_owned());
    live.stall_secs = 5;
    DriverConfig {
        live,
        ingest: IngestConfig {
            capacity: 1 << 14,
            instruments: 64,
            ..IngestConfig::default()
        },
        host: host_cfg(),
        snapshot: snapshot(),
        dir,
        segment_secs: 3,
        warmup: Warmup {
            quiet: Duration::from_millis(100),
            max: Duration::from_secs(5),
        },
        reconnect: Reconnect {
            max: 3,
            first_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
        },
        close_ts: close_secs.map(|s| T0 + s * SEC),
        label: "test day".to_owned(),
        replay_check: true,
    }
}

fn stop() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn a_day_runs_from_the_first_name_to_the_close_and_leaves_a_capture_a_log_and_a_report() {
    let records = market_records(SYMBOLS, 14, T0, 0);
    let bytes = stream_of(SYMBOLS, &records);
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Hang)]);
    let dir = scratch("day");
    let r = run(
        driver_cfg(&g.addr, dir.clone(), Some(11)),
        certified(&bytes),
        MemStore::from_records(vec![]),
        stop(),
    )
    .unwrap();
    assert_eq!(r.outcome, Outcome::Closed);
    // The universes were resolved against the names the gateway gave: S00..S05 and S06..S11.
    assert_eq!(r.host.members_of(1).unwrap(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(r.host.members_of(2).unwrap(), [6, 7, 8, 9, 10, 11]);
    let (s1, s2) = (r.host.stats_of(1).unwrap(), r.host.stats_of(2).unwrap());
    assert_eq!((s1.accepted, s2.accepted), (8, 8));
    // Every event before the close time and none at or after it.
    let before_close = records.iter().filter(|r| r.0 < T0 + 11 * SEC).count();
    assert_eq!(r.host.events() as usize, before_close);
    assert_eq!(r.host.journal().gateway().strategy_position(1, 2), 800);
    assert_eq!(r.host.journal().gateway().strategy_position(2, 8), 400);
    // The log on disk is the host's, closed.
    let text = std::fs::read_to_string(&r.log_path).unwrap();
    let (on_disk, complete) = tf_host::Log::parse_partial(&text).unwrap();
    assert!(complete);
    assert_eq!(&on_disk, r.host.log().unwrap());
    // The capture is finished and clean, and holds what the gateway sent: all of it.
    let v = tf_capture::verify(&r.capture_dir).unwrap();
    assert!(v.is_clean(), "{:?}", v.problems);
    // It holds what was read before the day closed: every mapping, and at least every event the host
    // was given (the feed may have read a little further).
    assert!(
        v.records >= SYMBOLS as u64 + r.host.events(),
        "{} records, {} events",
        v.records,
        r.host.events()
    );
    // And replaying it through a fresh host reproduces the day.
    assert_eq!(r.replay_equal, Some(true), "{}", r.text);
    // The report has the driver's own measurements.
    assert!(
        r.text.starts_with(
            "THE DAY CLOSED at its close time.\n0 reconnects, 0 repeated events dropped.\n"
        ),
        "{}",
        r.text
    );
    assert!(
        r.text.contains("engine lag: 99% within") && !r.text.contains("engine lag: not measured"),
        "{}",
        r.text
    );
    assert!(
        r.text.contains("ingest queue: ") && r.text.contains("capture:    "),
        "{}",
        r.text
    );
    assert!(r.text.contains("the replay reproduced all"), "{}", r.text);
    assert!(
        r.text
            .contains("POSITIONS OPEN AT THE END:\n  strategy 1: 800 shares of S02"),
        "{}",
        r.text
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("report.txt")).unwrap(),
        r.text
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_forced_disconnect_resumes_from_the_last_event_and_the_day_is_the_same_as_without_it() {
    let records = market_records(SYMBOLS, 14, T0, 0);
    let bytes = stream_of(SYMBOLS, &records);
    // The uninterrupted day.
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Hang)]);
    let whole = run(
        driver_cfg(&g.addr, scratch("whole"), Some(11)),
        certified(&bytes),
        MemStore::from_records(vec![]),
        stop(),
    )
    .unwrap();
    // The same day, the gateway dropping the session a third of the way in and replaying from the last
    // event it had sent (the boundary one twice).
    let m = records.len() / 3;
    let first = stream_of(SYMBOLS, &records[..m]);
    let second = stream_of(SYMBOLS, &records[m - 1..]);
    let g2 = gateway(vec![
        Conn::new(first, Then::Close),
        Conn::new(second, Then::Hang),
    ]);
    let dir = scratch("resume");
    let cut = run(
        driver_cfg(&g2.addr, dir.clone(), Some(11)),
        certified(&bytes),
        MemStore::from_records(vec![]),
        stop(),
    )
    .unwrap();
    assert_eq!(cut.outcome, Outcome::Closed);
    assert_eq!(cut.reconnects, 1);
    assert_eq!(cut.repeats_dropped, 1, "the boundary event came twice");
    // The same decisions, answers and fills as the day that was not interrupted.
    let names = whole.host.reference().symbols.clone();
    let v = compare(whole.host.log().unwrap(), cut.host.log().unwrap(), &names);
    assert!(v.is_equal(), "{}", v.report());
    // It asked for the replay from the last event it had.
    let resume = records[m - 1].0;
    let seen = g2.lines();
    assert!(!seen[0][1].contains("start="));
    assert!(
        seen[1][1].contains(&format!("|start={resume}|"))
            && seen[1][2].contains(&format!("|start={resume}|")),
        "{:?}",
        seen[1]
    );
    // The capture holds the repeat (it is what the gateway sent) and the replay of it drops it too.
    assert!(tf_capture::verify(&cut.capture_dir).unwrap().is_clean());
    assert_eq!(cut.replay_equal, Some(true), "{}", cut.text);
    assert!(
        cut.text.contains("1 reconnects, 1 repeated events dropped"),
        "{}",
        cut.text
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_gateway_that_does_not_come_back_ends_the_day_and_says_what_was_left_open() {
    let records = market_records(SYMBOLS, 14, T0, 0);
    let bytes = stream_of(SYMBOLS, &records);
    // The session dies five seconds in and nothing answers again.
    let m = records.iter().position(|r| r.0 >= T0 + 5 * SEC).unwrap();
    let g = gateway(vec![Conn::new(
        stream_of(SYMBOLS, &records[..m]),
        Then::Close,
    )]);
    let dir = scratch("down");
    let r = run(
        driver_cfg(&g.addr, dir.clone(), Some(11)),
        certified(&bytes),
        MemStore::from_records(vec![]),
        stop(),
    )
    .unwrap();
    assert!(
        matches!(&r.outcome, Outcome::GaveUp(w) if w.contains("3 reconnects were not enough")),
        "{:?}",
        r.outcome
    );
    assert_eq!(r.reconnects, 3);
    assert!(
        r.text
            .contains("THE GATEWAY WAS LOST and did not come back"),
        "{}",
        r.text
    );
    assert!(
        r.text.contains("reconnect 1 failed") && r.text.contains("reconnect 3 failed"),
        "{}",
        r.text
    );
    assert!(r.text.contains("POSITIONS OPEN AT THE END:"), "{}", r.text);
    // The host's day was ended and its books agree: nothing is left working, and the log is complete.
    assert!(r.host.working_orders().is_empty());
    let (_, complete) =
        tf_host::Log::parse_partial(&std::fs::read_to_string(&r.log_path).unwrap()).unwrap();
    assert!(complete);
    assert!(tf_capture::verify(&r.capture_dir).unwrap().is_clean());
    assert_eq!(
        r.replay_equal,
        Some(true),
        "what was captured replays to what was decided: {}",
        r.text
    );
    let _ = std::fs::remove_dir_all(&dir);
}

struct FailAfter(usize, Arc<Mutex<usize>>);

impl RawSink for FailAfter {
    fn record(&mut self, _rec: &dbn::RecordRef<'_>) -> Result<(), String> {
        let mut n = self.1.lock().unwrap();
        *n += 1;
        if *n > self.0 {
            Err("no space left on device".to_owned())
        } else {
            Ok(())
        }
    }
}

#[test]
fn when_the_capture_cannot_be_written_the_kill_switch_is_thrown_and_the_day_ends() {
    let records = market_records(SYMBOLS, 14, T0, 0);
    let bytes = stream_of(SYMBOLS, &records);
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Hang)]);
    let dir = scratch("capfail");
    let taken = Arc::new(Mutex::new(0));
    let sink: tf_live::SharedSink = Arc::new(Mutex::new(FailAfter(
        SYMBOLS as usize + records.len() / 2,
        taken.clone(),
    )));
    let r = run_with_sink(
        driver_cfg(&g.addr, dir.clone(), Some(11)),
        certified(&bytes),
        MemStore::from_records(vec![]),
        stop(),
        sink,
        None,
        dir.join("capture"),
    )
    .unwrap();
    assert!(
        matches!(&r.outcome, Outcome::CaptureFailed(w) if w.contains("no space left on device")),
        "{:?}",
        r.outcome
    );
    assert!(r.host.journal().gateway().kill_switch_engaged());
    assert!(
        r.text.contains("THE CAPTURE FAILED")
            && r.text.contains("a day that cannot be kept is not traded"),
        "{}",
        r.text
    );
    assert_eq!(r.replay_equal, None, "there is no capture to replay");
    // What was seen before the failure was decided on; nothing after it.
    assert!(r.host.events() > 0 && (r.host.events() as usize) < records.len());
    assert!(r.host.stats_of(1).unwrap().accepted <= 8);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_operator_can_stop_the_day_and_everything_the_queue_held_is_taken_first() {
    let records = market_records(SYMBOLS, 14, T0, 0);
    let bytes = stream_of(SYMBOLS, &records);
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Hang)]);
    let dir = scratch("stop");
    let flag = stop();
    let f2 = flag.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2_000));
        f2.store(true, Ordering::Relaxed);
    });
    let r = run(
        driver_cfg(&g.addr, dir.clone(), None),
        certified(&bytes),
        MemStore::from_records(vec![]),
        flag,
    )
    .unwrap();
    t.join().unwrap();
    assert_eq!(r.outcome, Outcome::Stopped);
    assert_eq!(
        r.host.events() as usize,
        records.len(),
        "all of it was taken before the day ended"
    );
    assert!(
        r.text.starts_with("THE DAY WAS STOPPED by an operator."),
        "{}",
        r.text
    );
    assert_eq!(r.replay_equal, Some(true), "{}", r.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_skip_the_gateway_reports_reaches_the_report_as_a_gap() {
    let records = market_records(SYMBOLS, 6, T0, 0);
    let half = records.len() / 2;
    let last_ts = records[half - 1].0;
    let mut bytes = stream_of(SYMBOLS, &records[..half]);
    // After the half of the day: the gateway says it skipped records, and a trade at the close time ends it.
    let tail = stream(|e| {
        e.encode_record(&ErrorMsg::new(
            last_ts + 1,
            Some(ErrorCode::SkippedRecordsAfterSlowReading),
            "skipped 50 records",
            true,
        ))
        .unwrap();
        e.encode_record(&trade(20_000, T0 + 9 * SEC, 2_000, 1, 99_999))
            .unwrap();
    });
    let header_len = stream(|_| {}).len();
    bytes.extend_from_slice(&tail[header_len..]);
    let g = gateway(vec![Conn::new(bytes, Then::Hang)]);
    let dir = scratch("skip");
    let r = run(
        driver_cfg(&g.addr, dir.clone(), Some(9)),
        certified(&stream_of(SYMBOLS, &records)),
        MemStore::from_records(vec![]),
        stop(),
    )
    .unwrap();
    assert_eq!(r.outcome, Outcome::Closed);
    assert_eq!(r.host.gaps().len(), 1);
    assert_eq!(r.host.gaps()[0].lost, tf_ingest::Lost::Skipped);
    assert!(r.text.contains("gateway skip notices"), "{}", r.text);
    assert!(r.text.contains("1 gaps"), "{}", r.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_strategy_without_a_matching_certificate_is_not_admitted_and_nothing_is_traded() {
    let records = market_records(SYMBOLS, 4, T0, 0);
    let bytes = stream_of(SYMBOLS, &records);
    let g = gateway(vec![Conn::new(bytes.clone(), Then::Hang)]);
    let mut s = certified(&bytes);
    let other = def(1, LOW, 999);
    let cert = s.remove(0).1;
    let dir = scratch("admit");
    let e = run(
        driver_cfg(&g.addr, dir.clone(), Some(3)),
        vec![(other, cert)],
        MemStore::from_records(vec![]),
        stop(),
    )
    .err()
    .unwrap();
    assert!(
        matches!(
            e,
            DriverError::Admit(1, tf_host::AdmitError::NotCertified(_))
        ),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
