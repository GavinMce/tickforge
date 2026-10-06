use std::path::PathBuf;

use tf_core::{InstrumentId, Nanos, Px};
use tf_risk::{GapRule, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{Decision, OrderId, OrderState, RejectReason};
use tf_synth::SplitMix64;

use crate::codec::{Input, Record};
use crate::journal::{Journal, JournalError};
use crate::store::*;

const P: i64 = 1_000_000_000;
const SEC: Nanos = 1_000_000_000;

fn limits() -> Limits {
    Limits::new(
        5_000 * 1_000_000_000u128,
        1_000,
        20_000 * 1_000_000_000u128,
        400 * 1_000_000_000u128,
        6,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(100_000 * 1_000_000_000u128, 20_000, 1000).unwrap())
}

fn intent(seq: u64, inst: InstrumentId, side: Side, purpose: Purpose, qty: u32, px: i64) -> Intent {
    let protect = (purpose == Purpose::Open).then(|| Protective {
        stop_trigger: Px::from_raw(if side.is_buy() {
            px * 9 / 10
        } else {
            px * 11 / 10
        }),
        stop_limit: None,
        take_profit: None,
    });
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq,
        },
        instrument: inst,
        side,
        qty,
        purpose,
        pricing: Pricing::Limit(Px::from_raw(px)),
        protect,
        tif: Tif::Day,
        ts: seq * SEC,
        reason: 7,
    }
}

fn buy(seq: u64, inst: InstrumentId, qty: u32, px: i64) -> Intent {
    intent(seq, inst, Side::Buy, Purpose::Open, qty, px)
}

fn sell(seq: u64, inst: InstrumentId, qty: u32, px: i64) -> Intent {
    intent(seq, inst, Side::Sell, Purpose::Close, qty, px)
}

// ---------------------------------------------------------------- codec

fn rt(r: &Record) -> Record {
    let line = r.encode().unwrap();
    assert!(!line.contains('\n'), "{line}");
    Record::decode(&line).unwrap_or_else(|e| panic!("{line}: {e}"))
}

#[test]
fn every_kind_of_record_survives_the_text_form_exactly() {
    let mut recs = vec![
        Record::Start {
            instruments: 3,
            limits: limits()
                .pairs()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        },
        Record::Start {
            instruments: 0,
            limits: vec![],
        },
    ];
    let ev = |input| Record::Event {
        input,
        outcome: None,
    };
    recs.extend([
        ev(Input::Mark {
            instrument: 4,
            px: 12_340_000_000,
        }),
        ev(Input::Mark {
            instrument: 0,
            px: -1,
        }),
        ev(Input::Ack {
            order: OrderId(9),
            ts: 5,
        }),
        ev(Input::Fill {
            order: OrderId(9),
            qty: 100,
            px: 1,
            ts: u64::MAX,
        }),
        ev(Input::Close {
            order: OrderId(1),
            state: OrderState::Cancelled,
            ts: 1,
        }),
        ev(Input::Close {
            order: OrderId(2),
            state: OrderState::Rejected,
            ts: 1,
        }),
        ev(Input::Close {
            order: OrderId(3),
            state: OrderState::Expired,
            ts: 1,
        }),
        ev(Input::Kill { ts: 77 }),
        ev(Input::NewDay { ts: 0 }),
    ]);
    // Every shape of intent, and every possible answer.
    let mut seq = 0;
    for side in [Side::Buy, Side::Sell, Side::SellShort] {
        for purpose in [Purpose::Open, Purpose::Close] {
            for pricing in [
                Pricing::Limit(Px::from_raw(5 * P)),
                Pricing::Collar {
                    reference: Px::from_raw(7 * P),
                    collar_permille: 20,
                },
            ] {
                for protect in [
                    None,
                    Some(Protective {
                        stop_trigger: Px::from_raw(P),
                        stop_limit: None,
                        take_profit: None,
                    }),
                    Some(Protective {
                        stop_trigger: Px::from_raw(P),
                        stop_limit: Some(Px::from_raw(P - 1)),
                        take_profit: Some(Px::from_raw(9 * P)),
                    }),
                ] {
                    for tif in [Tif::Day, Tif::Ioc] {
                        seq += 1;
                        let i = Intent {
                            id: IntentId {
                                strategy: StrategyId(65_535),
                                seq,
                            },
                            instrument: 4_000_000,
                            side,
                            qty: u32::MAX,
                            purpose,
                            pricing,
                            protect,
                            tif,
                            ts: u64::MAX,
                            reason: 65_535,
                        };
                        recs.push(Record::Event {
                            input: Input::Decide {
                                intent: i,
                                now: 123,
                            },
                            outcome: Some(Decision::Accepted(OrderId(seq))),
                        });
                    }
                }
            }
        }
    }
    let i = buy(1, 0, 10, 5 * P);
    let mut reasons = vec![
        RejectReason::KillSwitch,
        RejectReason::MaxNotional,
        RejectReason::MaxPosition,
        RejectReason::DailyLossLimit,
        RejectReason::OrderRate,
        RejectReason::NotShortable,
        RejectReason::ShortSaleRestricted,
        RejectReason::Halted,
        RejectReason::OutsideLuldBand,
        RejectReason::SpreadTooWide,
        RejectReason::RunUpTooLarge,
        RejectReason::GapRisk,
        RejectReason::OpposingPosition,
        RejectReason::NothingToClose,
        RejectReason::UnknownInstrument,
        RejectReason::Broker,
        RejectReason::NoBudget,
        RejectReason::StrategyBudget,
        RejectReason::GroupBudget,
    ];
    use tf_strategy::intent::IntentError as E;
    reasons.extend(
        [
            E::ZeroQty,
            E::BadPrice,
            E::BadCollar,
            E::SideAndPurpose,
            E::MissingProtection,
            E::UnexpectedProtection,
            E::StopOnWrongSide,
            E::TargetOnWrongSide,
            E::StopLimitOnWrongSide,
        ]
        .map(RejectReason::Invalid),
    );
    for r in reasons {
        recs.push(Record::Event {
            input: Input::Decide { intent: i, now: 1 },
            outcome: Some(Decision::Rejected(r)),
        });
    }
    assert!(recs.len() > 100);
    for r in &recs {
        assert_eq!(&rt(r), r);
    }
}

#[test]
fn the_text_form_is_the_documented_one() {
    let i = buy(3, 2, 100, 5 * P);
    let r = Record::Event {
        input: Input::Decide { intent: i, now: 9 },
        outcome: Some(Decision::Accepted(OrderId(0))),
    };
    assert_eq!(
        r.encode().unwrap(),
        "decide 9 1 3 2 buy 100 open limit:5000000000 stop:4500000000:-:- day 3000000000 7 => ok:0"
    );
    let s = Record::Start {
        instruments: 2,
        limits: vec![("a".into(), "1".into()), ("b".into(), "x".into())],
    };
    assert_eq!(s.encode().unwrap(), "start 2 a=1 b=x");
    assert_eq!(
        Record::Event {
            input: Input::Fill {
                order: OrderId(4),
                qty: 5,
                px: 6,
                ts: 7
            },
            outcome: None
        }
        .encode()
        .unwrap(),
        "fill 4 5 6 7"
    );
}

#[test]
fn records_that_make_no_sense_are_refused_both_ways() {
    let i = buy(1, 0, 10, 5 * P);
    assert!(
        Record::Event {
            input: Input::Decide { intent: i, now: 1 },
            outcome: None
        }
        .encode()
        .is_err()
    );
    assert!(
        Record::Event {
            input: Input::Kill { ts: 1 },
            outcome: Some(Decision::Accepted(OrderId(0)))
        }
        .encode()
        .is_err()
    );
    for bad in [
        OrderState::Pending,
        OrderState::Accepted,
        OrderState::PartiallyFilled,
        OrderState::Filled,
    ] {
        assert!(
            Record::Event {
                input: Input::Close {
                    order: OrderId(1),
                    state: bad,
                    ts: 1
                },
                outcome: None
            }
            .encode()
            .is_err(),
            "{bad:?}"
        );
    }
    assert!(
        Record::Start {
            instruments: 1,
            limits: vec![("a b".into(), "1".into())]
        }
        .encode()
        .is_err()
    );
    assert!(
        Record::Start {
            instruments: 1,
            limits: vec![("a".into(), "1 2".into())]
        }
        .encode()
        .is_err()
    );
    let good =
        "decide 9 1 3 2 buy 100 open limit:5000000000 stop:4500000000:-:- day 3000000000 7 => ok:0";
    assert!(Record::decode(good).is_ok());
    for (bad, want) in [
        ("", "not a record"),
        ("nonsense 1", "not a record"),
        ("mark 1", "1 field(s)"),
        ("mark x 1", "not a number"),
        ("fill 1 2 3", "not a record"),
        ("close 1 filled 5", "cannot close as `filled`"),
        ("kill", "not a record"),
        ("start x", "not a number"),
        ("start 1 broken", "not name=value"),
        (&good.replace(" => ok:0", ""), "needs `=> outcome`"),
        (&good.replace("=> ok:0", "=> ok:0 ok:1"), "one outcome"),
        (&good.replace("buy", "purchase"), "unknown side"),
        (&good.replace(" open ", " opening "), "unknown purpose"),
        (
            &good.replace("limit:5000000000", "market:1"),
            "unknown pricing",
        ),
        (
            &good.replace("stop:4500000000:-:-", "stop:1"),
            "unknown protection",
        ),
        (&good.replace(" day ", " gtc "), "unknown time in force"),
        (&good.replace("ok:0", "ok:x"), "not a number"),
        (
            &good.replace("ok:0", "rej:whatever"),
            "unknown reject reason",
        ),
        (
            &good.replace("ok:0", "invalid:whatever"),
            "unknown intent error",
        ),
        (&good.replace("ok:0", "maybe:1"), "unknown outcome kind"),
        (&good.replace("ok:0", "ok"), "not `kind:value`"),
        (&good.replace(" 9 1 3 2 ", " 9 1 3 "), "11 fields"),
    ] {
        let e = Record::decode(bad).unwrap_err().0;
        assert!(e.contains(want), "wanted `{want}` in `{e}` for `{bad}`");
    }
}

// ---------------------------------------------------------------- stores

struct MemHarness {
    data: Vec<String>,
}

impl Harness for MemHarness {
    type Store = MemStore;
    fn fresh(&mut self) -> MemStore {
        self.data.clear();
        MemStore::new()
    }
    fn reopen(&mut self) -> MemStore {
        MemStore::from_records(self.data.clone())
    }
    fn tear_tail(&mut self) -> bool {
        false
    }
}

/// Appends to the store the harness hands out must reach `data`; MemStore owns its records, so the
/// memory harness copies them out after each store is dropped. This wrapper does it on drop.
struct Recording {
    inner: MemStore,
    sink: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl LedgerStore for Recording {
    fn load(&mut self) -> Result<Loaded, StoreError> {
        self.inner.load()
    }
    fn append(&mut self, seq: u64, payload: &str) -> Result<(), StoreError> {
        self.inner.append(seq, payload)?;
        *self.sink.borrow_mut() = self.inner.records().to_vec();
        Ok(())
    }
}

struct SharedMem {
    sink: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl Harness for SharedMem {
    type Store = Recording;
    fn fresh(&mut self) -> Recording {
        self.sink.borrow_mut().clear();
        Recording {
            inner: MemStore::new(),
            sink: self.sink.clone(),
        }
    }
    fn reopen(&mut self) -> Recording {
        Recording {
            inner: MemStore::from_records(self.sink.borrow().clone()),
            sink: self.sink.clone(),
        }
    }
    fn tear_tail(&mut self) -> bool {
        false
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-ledger-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

struct FileHarness {
    dir: PathBuf,
}

impl Harness for FileHarness {
    type Store = FileStore;
    fn fresh(&mut self) -> FileStore {
        let _ = std::fs::remove_dir_all(&self.dir);
        FileStore::open(&self.dir).unwrap()
    }
    fn reopen(&mut self) -> FileStore {
        FileStore::open(&self.dir).unwrap()
    }
    fn tear_tail(&mut self) -> bool {
        let log = self.dir.join("ledger.log");
        let bytes = std::fs::read(&log).unwrap();
        std::fs::write(&log, &bytes[..bytes.len() - 5]).unwrap(); // cut into the last record
        true
    }
}

#[test]
fn the_memory_store_conforms() {
    conformance(&mut MemHarness { data: vec![] }.into_shared());
}

impl MemHarness {
    fn into_shared(self) -> SharedMem {
        SharedMem {
            sink: Default::default(),
        }
    }
}

#[test]
fn the_file_store_conforms() {
    let mut h = FileHarness {
        dir: scratch("conform"),
    };
    conformance(&mut h);
    let _ = std::fs::remove_dir_all(&h.dir);
}

fn file_with(dir: &PathBuf, n: u64) -> FileStore {
    let _ = std::fs::remove_dir_all(dir);
    let mut s = FileStore::open(dir).unwrap();
    s.load().unwrap();
    for k in 1..=n {
        s.append(k, &format!("record number {k}")).unwrap();
    }
    s
}

#[test]
fn the_log_file_is_one_framed_line_per_record() {
    let dir = scratch("format");
    let s = file_with(&dir, 2);
    let text = std::fs::read_to_string(s.log_path()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for (k, l) in lines.iter().enumerate() {
        let p: Vec<&str> = l.splitn(3, ' ').collect();
        assert_eq!(p[0], (k + 1).to_string());
        assert_eq!(p[1].len(), 16);
        assert!(p[1].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(p[2], format!("record number {}", k + 1));
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_crash_at_any_byte_of_the_last_record_loses_only_that_record() {
    let dir = scratch("tear");
    let full = {
        let s = file_with(&dir, 3);
        let b = std::fs::read(s.log_path()).unwrap();
        drop(s);
        b
    };
    let third_starts = full
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(1)
        .map(|(i, _)| i + 1)
        .unwrap();
    for cut in third_starts..full.len() {
        let _ = std::fs::remove_file(dir.join("ledger.lock"));
        std::fs::write(dir.join("ledger.log"), &full[..cut]).unwrap();
        let mut s = FileStore::open(&dir).unwrap();
        let l = s.load().unwrap();
        assert_eq!(
            l.records,
            ["record number 1", "record number 2"],
            "cut at {cut}"
        );
        assert_eq!(
            l.repaired.is_some(),
            cut > third_starts,
            "cut at {cut}: an empty tail needs no repair"
        );
        // And the file is really repaired: appending carries on from record 3.
        s.append(3, "again").unwrap();
        drop(s);
        let mut s = FileStore::open(&dir).unwrap();
        assert_eq!(
            s.load().unwrap().records,
            ["record number 1", "record number 2", "again"]
        );
    }
    // The complete file loads whole.
    let _ = std::fs::remove_file(dir.join("ledger.lock"));
    std::fs::write(dir.join("ledger.log"), &full).unwrap();
    let mut s = FileStore::open(&dir).unwrap();
    assert_eq!(s.load().unwrap().records.len(), 3);
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damage_is_repaired_only_at_the_very_end_and_never_skipped() {
    let dir = scratch("damage");
    let good = {
        let s = file_with(&dir, 4);
        let b = std::fs::read_to_string(s.log_path()).unwrap();
        drop(s);
        b
    };
    let lines: Vec<&str> = good.lines().collect();
    let write = |text: &str| {
        let _ = std::fs::remove_file(dir.join("ledger.lock"));
        std::fs::write(dir.join("ledger.log"), text).unwrap();
        FileStore::open(&dir).unwrap()
    };
    // A complete but damaged last line (bad checksum) is a torn write.
    let flipped = lines[3].replace("record number 4", "record number 5");
    let mut s = write(&format!(
        "{}\n{}\n{}\n{flipped}\n",
        lines[0], lines[1], lines[2]
    ));
    let l = s.load().unwrap();
    assert_eq!(l.records.len(), 3);
    assert!(l.repaired.unwrap().contains("checksum"));
    drop(s);
    // The same damage in the middle is corruption: refuse, naming the record.
    let mut s = write(&format!(
        "{}\n{}\n{}\n{}\n",
        lines[0],
        lines[1].replace("number 2", "number 9"),
        lines[2],
        lines[3]
    ));
    assert_eq!(
        s.load().unwrap_err(),
        StoreError::Corrupt {
            line: 2,
            why: "checksum does not match".into()
        }
    );
    drop(s);
    // A record dropped from the middle (a gap) is corruption too.
    let mut s = write(&format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3]));
    assert!(matches!(
        s.load().unwrap_err(),
        StoreError::Corrupt { line: 2, .. }
    ));
    drop(s);
    // A duplicated record is out of order.
    let mut s = write(&format!("{}\n{}\n{}\n", lines[0], lines[1], lines[1]));
    let l = s.load().unwrap();
    assert_eq!(l.records.len(), 2, "a duplicate at the end is a torn write");
    drop(s);
    let mut s = write(&format!(
        "{}\n{}\n{}\n{}\n",
        lines[0], lines[1], lines[1], lines[3]
    ));
    assert!(matches!(
        s.load().unwrap_err(),
        StoreError::Corrupt { line: 3, .. }
    ));
    drop(s);
    // A bad line followed by a torn fragment is not "the last record": corruption.
    let mut s = write(&format!("{}\n{}\n{}\npartial", lines[0], flipped, lines[2]));
    assert!(matches!(
        s.load().unwrap_err(),
        StoreError::Corrupt { line: 2, .. }
    ));
    drop(s);
    // A finished but damaged line with a torn fragment after it is damage in the middle.
    let mut s = write(&format!("{}\n{flipped}\npartial", lines[0]));
    assert!(matches!(
        s.load().unwrap_err(),
        StoreError::Corrupt { line: 2, .. }
    ));
    drop(s);
    // Bytes that are not text at the end are a torn tail, in the middle corruption.
    let _ = std::fs::remove_file(dir.join("ledger.lock"));
    let mut bytes = format!("{}\n{}\n", lines[0], lines[1]).into_bytes();
    bytes.extend([0xff, 0xfe, b'\n']);
    std::fs::write(dir.join("ledger.log"), &bytes).unwrap();
    let mut s = FileStore::open(&dir).unwrap();
    assert_eq!(s.load().unwrap().records.len(), 2);
    drop(s);
    let _ = std::fs::remove_file(dir.join("ledger.lock"));
    let mut bytes = format!("{}\n", lines[0]).into_bytes();
    bytes.extend([0xff, 0xfe, b'\n']);
    bytes.extend(format!("{}\n", lines[2]).into_bytes());
    std::fs::write(dir.join("ledger.log"), &bytes).unwrap();
    let mut s = FileStore::open(&dir).unwrap();
    assert!(matches!(
        s.load().unwrap_err(),
        StoreError::Corrupt { line: 2, .. }
    ));
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_one_writer_holds_the_ledger_and_the_lock_goes_with_it() {
    let dir = scratch("lock");
    let a = FileStore::open(&dir).unwrap();
    let e = FileStore::open(&dir).unwrap_err();
    assert!(matches!(e, StoreError::LockHeld(_)));
    assert!(e.to_string().contains("remove the lock file"));
    drop(a);
    drop(FileStore::open(&dir).unwrap());
    assert!(!dir.join("ledger.lock").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- the journal

fn open_mem(
    records: Vec<String>,
    n: usize,
) -> Result<(Journal<MemStore>, crate::Recovery), JournalError> {
    Journal::open(MemStore::from_records(records), limits(), n)
}

fn fresh(n: usize) -> Journal<MemStore> {
    open_mem(vec![], n).unwrap().0
}

fn recover(j: &Journal<MemStore>, n: usize) -> Journal<MemStore> {
    open_mem(j.store().records().to_vec(), n).unwrap().0
}

fn same(a: &Journal<MemStore>, b: &Journal<MemStore>) {
    assert_eq!(a.snapshot(), b.snapshot());
    assert_eq!(
        a.orders().collect::<Vec<_>>(),
        b.orders().collect::<Vec<_>>()
    );
    assert_eq!(a.records(), b.records());
    assert_eq!(a.open_orders(), b.open_orders());
}

#[test]
fn an_order_goes_through_its_life_and_a_restart_finds_it_exactly_where_it_was() {
    let mut j = fresh(2);
    assert_eq!(j.records(), 1, "the start record");
    let Decision::Accepted(o) = j.decide(&buy(1, 0, 100, 5 * P), 10 * SEC).unwrap() else {
        panic!()
    };
    assert_eq!(j.order(o).unwrap().state(), OrderState::Pending);
    same(&j, &recover(&j, 2));
    j.ack(o, 11 * SEC).unwrap();
    j.fill(o, 40, Px::from_raw(5 * P), 12 * SEC).unwrap();
    let r = recover(&j, 2);
    same(&j, &r);
    assert_eq!(r.order(o).unwrap().state(), OrderState::PartiallyFilled);
    assert_eq!(r.order(o).unwrap().filled_qty(), 40);
    assert_eq!(r.gateway().position(0), 40);
    assert_eq!(r.open_orders().len(), 1);
    j.close(o, OrderState::Expired, 13 * SEC).unwrap();
    let r = recover(&j, 2);
    same(&j, &r);
    assert!(r.open_orders().is_empty());
    assert_eq!(r.order(o).unwrap().state(), OrderState::Expired);
    assert_eq!(r.gateway().working_orders(), 0);
    // Close it out and the position and realised loss come back too.
    let Decision::Accepted(c) = j.decide(&sell(2, 0, 40, 4 * P), 20 * SEC).unwrap() else {
        panic!()
    };
    j.ack(c, 21 * SEC).unwrap();
    j.fill(c, 40, Px::from_raw(4 * P), 22 * SEC).unwrap();
    let r = recover(&j, 2);
    same(&j, &r);
    assert_eq!(r.gateway().position(0), 0);
    assert_eq!(r.snapshot().realized, -40 * P as i128);
}

#[test]
fn refusals_are_recorded_with_their_reason_and_counted_again_after_a_restart() {
    let mut j = fresh(1);
    let d = j.decide(&buy(1, 0, 100_000, 5 * P), SEC).unwrap();
    assert_eq!(d, Decision::Rejected(RejectReason::MaxNotional));
    j.engage_kill_switch(2 * SEC).unwrap();
    assert_eq!(
        j.decide(&buy(2, 0, 10, 5 * P), 3 * SEC).unwrap(),
        Decision::Rejected(RejectReason::KillSwitch)
    );
    let mut bad = buy(3, 0, 0, 5 * P);
    bad.qty = 0;
    assert!(matches!(
        j.decide(&bad, 4 * SEC).unwrap(),
        Decision::Rejected(RejectReason::Invalid(_))
    ));
    let r = recover(&j, 1);
    same(&j, &r);
    assert!(r.gateway().kill_switch_engaged());
    assert_eq!(r.gateway().rejected_count("max_notional"), 1);
    assert_eq!(r.gateway().rejected_count("kill_switch"), 1);
    assert_eq!(r.gateway().accepted_count(), 0);
}

#[test]
fn inputs_the_order_book_refuses_change_nothing_and_write_nothing() {
    let mut j = fresh(1);
    let Decision::Accepted(o) = j.decide(&buy(1, 0, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    let (snap, n) = (j.snapshot(), j.records());
    // A fill before the acknowledgement; an unknown order; closing as a state that is not an ending.
    assert!(matches!(
        j.fill(o, 10, Px::from_raw(5 * P), SEC),
        Err(JournalError::Lifecycle(_))
    ));
    assert!(matches!(
        j.ack(OrderId(99), SEC),
        Err(JournalError::UnknownOrder(_))
    ));
    assert!(matches!(
        j.close(o, OrderState::Filled, SEC),
        Err(JournalError::BadClose(_))
    ));
    assert!(
        matches!(
            j.close(o, OrderState::Expired, SEC),
            Err(JournalError::Lifecycle(_))
        ),
        "pending cannot expire"
    );
    j.ack(o, SEC).unwrap();
    let n2 = j.records();
    assert!(
        matches!(j.ack(o, SEC), Err(JournalError::Lifecycle(_))),
        "twice"
    );
    assert!(
        matches!(
            j.fill(o, 101, Px::from_raw(5 * P), SEC),
            Err(JournalError::Lifecycle(_))
        ),
        "overfill"
    );
    assert!(matches!(
        j.fill(o, 0, Px::from_raw(5 * P), SEC),
        Err(JournalError::Lifecycle(_))
    ));
    assert!(matches!(
        j.fill(o, 5, Px::from_raw(0), SEC),
        Err(JournalError::Lifecycle(_))
    ));
    assert_eq!(j.records(), n2, "nothing written");
    assert_eq!(j.snapshot(), snap);
    assert_eq!(n2, n + 1);
    assert_eq!(j.order(o).unwrap().state(), OrderState::Accepted);
    j.fill(o, 100, Px::from_raw(5 * P), SEC).unwrap();
    assert!(
        matches!(
            j.close(o, OrderState::Cancelled, SEC),
            Err(JournalError::Lifecycle(_))
        ),
        "filled is final"
    );
    same(&j, &recover(&j, 1));
}

#[test]
fn marks_are_written_only_when_a_decision_needs_them_and_decisions_see_them_after_a_restart() {
    let mut j = fresh(2);
    // A long that has gone far against us: the daily loss limit (400) is crossed by the mark alone.
    let Decision::Accepted(o) = j.decide(&buy(1, 0, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    j.ack(o, SEC).unwrap();
    j.fill(o, 100, Px::from_raw(5 * P), SEC).unwrap();
    let n = j.records();
    j.mark(0, Px::from_raw(P)); // 100 shares x -$4 = -$400
    j.mark(1, Px::from_raw(9 * P)); // an instrument with no position: never written
    assert_eq!(j.records(), n, "marking alone writes nothing");
    let d = j.decide(&buy(2, 1, 10, 5 * P), 2 * SEC).unwrap();
    assert_eq!(d, Decision::Rejected(RejectReason::DailyLossLimit));
    let lines = j.store().records();
    assert!(
        lines[n as usize].starts_with("mark 0 1000000000"),
        "{lines:?}"
    );
    assert_eq!(
        j.records(),
        n + 2,
        "one mark, one decision; nothing for the flat instrument"
    );
    // The restarted journal reaches the same answer from the same ledger (verified on open) and
    // the loss latch it set.
    let r = recover(&j, 2);
    same(&j, &r);
    assert_eq!(r.snapshot().positions, vec![(1, 0, 100, 5 * P, P)]);
    assert!(r.snapshot().loss_latched);
    // A new day starts with the marks written first, and clears the latch.
    j.new_day(10 * SEC).unwrap();
    let r = recover(&j, 2);
    same(&j, &r);
    assert!(!r.snapshot().loss_latched);
    // sync_marks twice writes once.
    j.mark(0, Px::from_raw(2 * P));
    j.sync_marks().unwrap();
    let n = j.records();
    j.sync_marks().unwrap();
    assert_eq!(j.records(), n);
}

#[test]
fn a_ledger_for_other_limits_or_another_universe_is_not_opened() {
    let mut j = fresh(2);
    j.decide(&buy(1, 0, 10, 5 * P), SEC).unwrap();
    let recs = j.store().records().to_vec();
    let other = Limits::new(
        4_000 * 1_000_000_000u128,
        1_000,
        20_000 * 1_000_000_000u128,
        400 * 1_000_000_000u128,
        6,
        10 * SEC,
    )
    .unwrap();
    let e = Journal::open(MemStore::from_records(recs.clone()), other, 2)
        .err()
        .unwrap();
    assert!(matches!(e, JournalError::Mismatch(_)), "{e}");
    assert!(e.to_string().contains("different universe or limits"));
    let e = open_mem(recs.clone(), 3).err().unwrap();
    assert!(matches!(e, JournalError::Mismatch(_)));
    // Without the gap rule: different again.
    let no_gap = Limits::new(
        5_000 * 1_000_000_000u128,
        1_000,
        20_000 * 1_000_000_000u128,
        400 * 1_000_000_000u128,
        6,
        10 * SEC,
    )
    .unwrap();
    assert!(matches!(
        Journal::open(MemStore::from_records(recs), no_gap, 2)
            .err()
            .unwrap(),
        JournalError::Mismatch(_)
    ));
}

#[test]
fn a_ledger_that_replays_differently_than_it_was_written_fails_loudly_and_says_where() {
    let mut j = fresh(1);
    let Decision::Accepted(o) = j.decide(&buy(1, 0, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    j.ack(o, SEC).unwrap();
    j.decide(&buy(2, 0, 100_000, 5 * P), 2 * SEC).unwrap();
    let recs = j.store().records().to_vec();
    assert_eq!(recs.len(), 4);
    // The answer to the third record edited: accepted where the gateway says no.
    let mut edited = recs.clone();
    edited[3] = edited[3].replace("rej:max_notional", "ok:1");
    let e = open_mem(edited, 1).err().unwrap();
    match &e {
        JournalError::Diverged {
            record,
            written,
            replayed,
        } => {
            assert_eq!(*record, 4);
            assert!(
                written.contains("ok:1") && replayed.contains("rej:max_notional"),
                "{e}"
            );
        }
        other => panic!("{other:?}"),
    }
    // An intent edited so it no longer passes: the first decision now differs.
    let mut edited = recs.clone();
    edited[1] = edited[1].replace(" buy 100 ", " buy 100000 ");
    assert!(matches!(
        open_mem(edited, 1).err().unwrap(),
        JournalError::Diverged { record: 2, .. }
    ));
    // A record that cannot be read names its number.
    let mut edited = recs.clone();
    edited[2] = "ack banana 1".to_owned();
    assert!(matches!(
        open_mem(edited, 1).err().unwrap(),
        JournalError::Codec { record: 3, .. }
    ));
    // An ack for an order that never existed, and a second start record.
    let mut edited = recs.clone();
    edited.insert(2, "ack 77 1".to_owned());
    assert!(matches!(
        open_mem(edited, 1).err().unwrap(),
        JournalError::UnknownOrder(OrderId(77))
    ));
    let mut edited = recs.clone();
    edited.push(recs[0].clone());
    assert!(matches!(
        open_mem(edited, 1).err().unwrap(),
        JournalError::Structure(_)
    ));
    // A ledger that does not begin with a start record.
    let e = open_mem(recs[1..].to_vec(), 1).err().unwrap();
    assert!(
        matches!(e, JournalError::Mismatch(_) | JournalError::Codec { .. }),
        "{e}"
    );
}

/// A store that stops accepting appends after `ok` of them.
struct Flaky {
    inner: MemStore,
    ok: usize,
}

impl LedgerStore for Flaky {
    fn load(&mut self) -> Result<Loaded, StoreError> {
        self.inner.load()
    }
    fn append(&mut self, seq: u64, payload: &str) -> Result<(), StoreError> {
        if self.ok == 0 {
            return Err(StoreError::Io("disk full".into()));
        }
        self.ok -= 1;
        self.inner.append(seq, payload)
    }
}

#[test]
fn when_the_ledger_cannot_be_written_the_journal_stops_and_a_restart_recovers_what_was_written() {
    let (mut j, _) = Journal::open(
        Flaky {
            inner: MemStore::new(),
            ok: 3,
        },
        limits(),
        1,
    )
    .unwrap();
    let Decision::Accepted(o) = j.decide(&buy(1, 0, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    j.ack(o, SEC).unwrap();
    assert_eq!(j.records(), 3);
    let e = j.fill(o, 10, Px::from_raw(5 * P), SEC).unwrap_err();
    assert_eq!(e, JournalError::Store(StoreError::Io("disk full".into())));
    assert!(j.is_poisoned());
    for r in [
        j.ack(o, SEC).err(),
        j.decide(&buy(2, 0, 1, P), SEC).err(),
        j.engage_kill_switch(SEC).err(),
        j.new_day(SEC).err(),
        j.sync_marks().err(),
    ] {
        assert!(
            r.is_none()
                || r == Some(JournalError::Poisoned)
                || matches!(r, Some(JournalError::Lifecycle(_))),
            "{r:?}"
        );
    }
    assert_eq!(
        j.decide(&buy(2, 0, 1, P), SEC).unwrap_err(),
        JournalError::Poisoned
    );
    assert_eq!(
        j.engage_kill_switch(SEC).unwrap_err(),
        JournalError::Poisoned
    );
    // What was written is exactly what a restart sees: the fill that could not be written is not
    // there (memory had it; the ledger did not, which is why the journal stopped).
    let recs = j.into_store().inner.records().to_vec();
    assert_eq!(recs.len(), 3);
    let (r, rec) = open_mem(recs, 1).unwrap();
    assert_eq!(rec.records, 3);
    assert_eq!(r.order(o).unwrap().filled_qty(), 0);
    assert_eq!(r.gateway().position(0), 0);
}

#[test]
fn opening_reports_what_it_found() {
    let mut j = fresh(2);
    let Decision::Accepted(a) = j.decide(&buy(1, 0, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    j.ack(a, SEC).unwrap();
    j.fill(a, 100, Px::from_raw(5 * P), SEC).unwrap();
    let Decision::Accepted(_) = j.decide(&buy(2, 1, 50, 4 * P), 2 * SEC).unwrap() else {
        panic!()
    };
    let (_, rec) = open_mem(j.store().records().to_vec(), 2).unwrap();
    assert_eq!(rec.records, j.records());
    assert_eq!((rec.open_orders, rec.positions), (1, 1));
    assert_eq!(rec.repaired, None);
    let (_, fresh_rec) = open_mem(vec![], 2).unwrap();
    assert_eq!(
        (
            fresh_rec.records,
            fresh_rec.open_orders,
            fresh_rec.positions
        ),
        (1, 0, 0)
    );
}

// ---------------------------------------------------------------- the property

/// A random but valid-looking day of orders, fills, cancels, marks, a kill switch and new days.
/// After every step the live journal and one rebuilt from its ledger must be the same.
fn random_day(
    seed: u64,
    steps: usize,
    with_budgets: bool,
    check: &mut dyn FnMut(&mut Journal<MemStore>),
) -> Journal<MemStore> {
    const N: usize = 3;
    let mut rng = SplitMix64::new(seed);
    let mut j = fresh(N);
    if with_budgets {
        j.set_budgets(Some(day_budgets(3_000 * P as u128)), SEC)
            .unwrap();
    }
    let (mut seq, mut now) = (0u64, SEC);
    for _ in 0..steps {
        now += (1 + rng.below(4)) * SEC;
        let inst = rng.below(N as u64) as u32;
        let px = (3 + rng.below(8)) as i64 * P;
        match rng.below(100) {
            0..=29 => {
                seq += 1;
                let qty = (10 + rng.below(300)) as u32;
                let strat = 1 + rng.below(3) as u16;
                let held = j.gateway().strategy_position(strat, inst);
                let mut i = match rng.below(4) {
                    0 => intent(seq, inst, Side::SellShort, Purpose::Open, qty, px),
                    1 if held > 0 => sell(seq, inst, (held as u32).min(qty), px),
                    2 if held < 0 => intent(
                        seq,
                        inst,
                        Side::Buy,
                        Purpose::Close,
                        ((-held) as u32).min(qty),
                        px,
                    ),
                    _ => buy(seq, inst, qty, px),
                };
                i.id.strategy = StrategyId(strat);
                let _ = j.decide(&i, now);
            }
            30..=44 => {
                if let Some(o) = j
                    .open_orders()
                    .iter()
                    .find(|o| o.state() == OrderState::Pending)
                {
                    let _ = j.ack(o.id, now);
                }
            }
            45..=69 => {
                let working: Vec<_> = j
                    .open_orders()
                    .into_iter()
                    .filter(|o| o.state() != OrderState::Pending)
                    .collect();
                if !working.is_empty() {
                    let o = working[rng.below(working.len() as u64) as usize];
                    let qty = 1 + rng.below(u64::from(o.remaining())) as u32;
                    let _ = j.fill(o.id, qty, Px::from_raw(px), now);
                }
            }
            70..=79 => {
                let open = j.open_orders();
                if !open.is_empty() {
                    let o = open[rng.below(open.len() as u64) as usize];
                    let st = if o.state() == OrderState::Pending {
                        OrderState::Rejected
                    } else {
                        OrderState::Cancelled
                    };
                    let _ = j.close(o.id, st, now);
                }
            }
            80..=94 => j.mark(inst, Px::from_raw(px)),
            95..=96 => {
                let _ = j.engage_kill_switch(now);
            }
            97 => {
                let _ = j.new_day(now);
            }
            _ => {
                // Inputs that must be refused and change nothing.
                let (s, n) = (j.snapshot(), j.records());
                let _ = j.ack(OrderId(10_000), now);
                let _ = j.fill(OrderId(10_000), 1, Px::from_raw(P), now);
                assert_eq!((j.snapshot(), j.records()), (s, n));
            }
        }
        check(&mut j);
    }
    j
}

#[test]
fn after_every_step_of_a_random_day_a_journal_rebuilt_from_the_ledger_is_the_same_journal() {
    let mut accepted = 0;
    let mut steps_checked = 0;
    let mut kinds = std::collections::BTreeSet::new();
    for seed in 0..12 {
        let mut check = |j: &mut Journal<MemStore>| {
            j.sync_marks().unwrap();
            let r = recover(j, 3);
            same(j, &r);
            steps_checked += 1;
        };
        let j = random_day(seed, 250, false, &mut check);
        accepted += j.gateway().accepted_count();
        kinds.extend(
            j.store()
                .records()
                .iter()
                .map(|r| r.split(' ').next().unwrap().to_owned()),
        );
    }
    // Over the days the interesting things happened.
    for k in ["decide", "ack", "fill", "close", "mark", "kill", "newday"] {
        assert!(kinds.contains(k), "no {k} records in any day: {kinds:?}");
    }
    assert!(
        accepted > 100,
        "enough orders to mean something: {accepted}"
    );
    assert_eq!(steps_checked, 12 * 250);
}

#[test]
fn a_crash_while_writing_any_record_of_a_random_day_recovers_the_state_before_it() {
    for seed in [3] {
        let dir = scratch(&format!("crash{seed}"));
        let _ = std::fs::remove_dir_all(&dir);
        // Run the day against a real log file, keeping each record's end offset.
        let log = dir.join("ledger.log");
        let (mut j, _) = Journal::open(FileStore::open(&dir).unwrap(), limits(), 3).unwrap();
        let mut rng = SplitMix64::new(seed);
        let mut seq = 0;
        for k in 0..48u64 {
            seq += 1;
            let inst = rng.below(3) as u32;
            let px = (3 + rng.below(8)) as i64 * P;
            match k % 4 {
                0 | 1 => {
                    j.mark(inst, Px::from_raw(px));
                    let _ = j.decide(
                        &buy(seq, inst, (10 + rng.below(100)) as u32, px),
                        (k + 2) * SEC,
                    );
                }
                2 => {
                    if let Some(o) = j.open_orders().first().copied() {
                        let _ = if o.state() == OrderState::Pending {
                            j.ack(o.id, (k + 2) * SEC)
                        } else {
                            j.fill(o.id, 1, Px::from_raw(px), (k + 2) * SEC)
                        };
                    }
                }
                _ => {
                    if let Some(o) = j.open_orders().last().copied() {
                        let st = if o.state() == OrderState::Pending {
                            OrderState::Rejected
                        } else {
                            OrderState::Cancelled
                        };
                        let _ = j.close(o.id, st, (k + 2) * SEC);
                    }
                }
            }
        }
        let full = std::fs::read(&log).unwrap();
        // Where each record ends in the file (0 for the start of the first).
        let mut offsets = vec![0usize];
        offsets.extend(
            full.iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        assert_eq!(offsets.len() as u64, j.records() + 1);
        let live_records: Vec<String> = {
            let mut s = MemStore::new();
            let _ = s.load();
            drop(j);
            String::from_utf8(full.clone())
                .unwrap()
                .lines()
                .map(|l| l.splitn(3, ' ').nth(2).unwrap().to_owned())
                .collect()
        };
        assert_eq!(live_records.len() as u64, offsets.len() as u64 - 1);
        // Crash inside every record from the second on: at each byte of it.
        let mut cuts = 0;
        for n in 2..offsets.len() {
            let (start, end) = (offsets[n - 1], offsets[n]);
            let (expected, _) = open_mem(live_records[..n - 1].to_vec(), 3).unwrap();
            // Every byte of the last dozen records; the edges and the middle of the earlier ones.
            let every = n + 12 >= offsets.len();
            for cut in (start..end).filter(|c| {
                every || [start, start + 1, (start + end) / 2, end - 2, end - 1].contains(c)
            }) {
                std::fs::write(&log, &full[..cut]).unwrap();
                let (r, rec) = Journal::open(FileStore::open(&dir).unwrap(), limits(), 3).unwrap();
                assert_eq!(
                    rec.records,
                    (n - 1) as u64,
                    "seed {seed} record {n} cut {cut}"
                );
                assert_eq!(rec.repaired.is_some(), cut > start);
                assert_eq!(r.snapshot(), expected.snapshot());
                assert_eq!(
                    r.orders().collect::<Vec<_>>(),
                    expected.orders().collect::<Vec<_>>()
                );
                cuts += 1;
            }
        }
        assert!(cuts > 500, "{cuts} crash points");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_ledger_can_be_opened_with_nothing_but_itself() {
    let mut j = fresh(3);
    let Decision::Accepted(o) = j.decide(&buy(1, 2, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    j.ack(o, SEC).unwrap();
    j.fill(o, 30, Px::from_raw(5 * P), SEC).unwrap();
    let recs = j.store().records().to_vec();
    let (r, rec) = Journal::open_recorded(MemStore::from_records(recs.clone())).unwrap();
    same(&j, &r);
    assert_eq!((rec.open_orders, rec.positions), (1, 1));
    // An empty ledger, a ledger starting with something else, and one with unreadable limits.
    assert!(matches!(
        Journal::open_recorded(MemStore::new()).err().unwrap(),
        JournalError::Structure(_)
    ));
    assert!(matches!(
        Journal::open_recorded(MemStore::from_records(recs[1..].to_vec()))
            .err()
            .unwrap(),
        JournalError::Structure(_)
    ));
    let bad = recs[0].replace("max_position_shares=1000", "max_position_shares=lots");
    let e = Journal::open_recorded(MemStore::from_records(vec![bad]))
        .err()
        .unwrap();
    assert!(e.to_string().contains("bad limits"), "{e}");
    assert!(matches!(
        Journal::open_recorded(MemStore::from_records(vec!["nonsense".into()]))
            .err()
            .unwrap(),
        JournalError::Codec { record: 1, .. }
    ));
}

#[test]
fn a_new_day_is_based_on_the_marks_it_saw_even_when_no_decision_came_first() {
    let mut j = fresh(2);
    let Decision::Accepted(o) = j.decide(&buy(1, 0, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    j.ack(o, SEC).unwrap();
    j.fill(o, 100, Px::from_raw(5 * P), SEC).unwrap();
    j.mark(0, Px::from_raw(6 * P)); // +$100 unrealised, never written by a decision
    j.new_day(5 * SEC).unwrap();
    assert_eq!(
        j.snapshot().day_base,
        100 * P as i128,
        "the day begins from where the position stood"
    );
    let r = recover(&j, 2);
    same(&j, &r);
    assert_eq!(r.snapshot().day_base, 100 * P as i128);
}

#[test]
fn two_strategies_in_one_symbol_come_back_from_the_ledger_as_two_sub_accounts() {
    let mut j = fresh(1);
    let mk = |strategy: u16, seq: u64, qty: u32, px: i64| {
        let mut i = buy(seq, 0, qty, px);
        i.id.strategy = StrategyId(strategy);
        i
    };
    let Decision::Accepted(a) = j.decide(&mk(1, 1, 100, 5 * P), SEC).unwrap() else {
        panic!()
    };
    let Decision::Accepted(b) = j.decide(&mk(2, 2, 50, 6 * P), 2 * SEC).unwrap() else {
        panic!()
    };
    for (o, q, px) in [(a, 100, 5 * P), (b, 50, 6 * P)] {
        j.ack(o, 3 * SEC).unwrap();
        j.fill(o, q, Px::from_raw(px), 3 * SEC).unwrap();
    }
    let mut close = sell(3, 0, 100, 7 * P);
    close.id.strategy = StrategyId(1);
    let Decision::Accepted(c) = j.decide(&close, 4 * SEC).unwrap() else {
        panic!()
    };
    j.ack(c, 4 * SEC).unwrap();
    j.fill(c, 100, Px::from_raw(7 * P), 4 * SEC).unwrap();
    let r = recover(&j, 1);
    same(&j, &r);
    let snap = r.snapshot();
    assert_eq!(
        snap.positions,
        vec![(2, 0, 50, 6 * P, 0)],
        "strategy 1 is flat, strategy 2 kept its own cost"
    );
    assert_eq!(snap.strategy_realized, vec![(1, 200 * P as i128), (2, 0)]);
    assert_eq!(r.gateway().position(0), 50);
}

fn day_budgets(balance: u128) -> tf_risk::Budgets {
    use tf_budget::{Group, LossLimits, Strategy as S, Tree};
    let tree = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            S {
                id: "s1".into(),
                share: 3_333,
            },
            S {
                id: "s2".into(),
                share: 3_333,
            },
            S {
                id: "s3".into(),
                share: 3_334,
            },
        ],
    }])
    .unwrap();
    tf_risk::Budgets::new(
        tree,
        balance,
        [
            (1, "s1".to_owned()),
            (2, "s2".to_owned()),
            (3, "s3".to_owned()),
        ],
    )
    .unwrap()
}

#[test]
fn budgets_are_a_recorded_input_that_replay_enforces_exactly_as_the_live_run_did() {
    let mut j = fresh(1);
    let before = j.records();
    j.set_budgets(Some(day_budgets(15_000 * P as u128)), 2 * SEC)
        .unwrap();
    assert_eq!(j.records(), before + 1);
    let line = j.store().records().last().unwrap().clone();
    assert!(line.starts_with("budgets 2000000000 15000000000000 1=s1,2=s2,3=s3 g:10000:300:600/s1:3333/s2:3333/s3:3334"), "{line}");
    // Strategy 1's budget is $4,999.50: a $5,000 order is refused and the refusal is in the ledger.
    let big = |seq: u64, strategy: u16, qty: u32| {
        let mut i = buy(seq, 0, qty, 5 * P);
        i.id.strategy = StrategyId(strategy);
        i
    };
    assert_eq!(
        j.decide(&big(1, 1, 1_000), 3 * SEC).unwrap(),
        Decision::Rejected(RejectReason::StrategyBudget)
    );
    assert!(matches!(
        j.decide(&big(2, 1, 999), 4 * SEC).unwrap(),
        Decision::Accepted(_)
    ));
    assert_eq!(
        j.decide(&big(3, 9, 1), 5 * SEC).unwrap(),
        Decision::Rejected(RejectReason::NoBudget)
    );
    assert!(
        j.store()
            .records()
            .iter()
            .any(|r| r.ends_with("=> rej:strategy_budget"))
    );
    let r = recover(&j, 1);
    same(&j, &r);
    assert_eq!(r.gateway().budgets(), j.gateway().budgets());
    assert_eq!(r.gateway().rejected_count("strategy_budget"), 1);
    // Taking budgets away is recorded too, and the same order then passes.
    j.set_budgets(None, 6 * SEC).unwrap();
    assert!(matches!(
        j.decide(&big(4, 1, 1), 7 * SEC).unwrap(),
        Decision::Accepted(_)
    ));
    let r = recover(&j, 1);
    same(&j, &r);
    assert!(r.gateway().budgets().is_none());
    // A ledger whose budgets record was altered replays differently and says where.
    let mut recs = j.store().records().to_vec();
    let at = recs
        .iter()
        .position(|l| l.starts_with("budgets 2000000000"))
        .unwrap();
    recs[at] = recs[at].replace("15000000000000", "1500000000000");
    assert!(matches!(
        open_mem(recs, 1).err().unwrap(),
        JournalError::Diverged { .. }
    ));
}

#[test]
fn budget_records_that_cannot_be_read_are_refused() {
    let good = "budgets 1 15000000000000 1=s1 g:10000:300:600/s1:10000";
    assert!(Record::decode(good).is_ok());
    assert_eq!(
        Record::decode("budgets 1 off").unwrap(),
        Record::Event {
            input: Input::Budgets {
                budgets: None,
                ts: 1
            },
            outcome: None
        }
    );
    for (bad, want) in [
        (
            good.replace("g:10000:300:600", "g:10000:300"),
            "a budget group is",
        ),
        (good.replace("/s1:10000", "/s1"), "a budget strategy is"),
        (good.replace("1=s1", "1"), "a strategy mapping is"),
        (good.replace("1=s1", "1=nope"), "UnknownStrategy"),
        (good.replace("1=s1", "1=s1,2=s1"), "Duplicate"),
        (
            good.replace("10000:300:600/s1:10000", "10001:300:600/s1:10000"),
            "more than the whole",
        ),
        (
            good.replace("g:10000:300:600", "g:10000:600:300"),
            "loss limits need",
        ),
        (good.replace("15000000000000", "lots"), "not a number"),
        (good.replace("1=s1", "x=s1"), "not a number"),
        ("budgets 1 1 2".to_owned(), "field(s) is not a record"),
    ] {
        let e = Record::decode(&bad).unwrap_err().0;
        assert!(e.contains(want), "wanted `{want}` in `{e}` for `{bad}`");
    }
    // An empty tree and no strategies round-trip through the `-` placeholders.
    let empty = tf_risk::Budgets::new(tf_budget::Tree::default(), 5, Vec::new()).unwrap();
    let r = Record::Event {
        input: Input::Budgets {
            budgets: Some(empty),
            ts: 3,
        },
        outcome: None,
    };
    assert_eq!(r.encode().unwrap(), "budgets 3 5 - -");
    assert_eq!(Record::decode(&r.encode().unwrap()).unwrap(), r);
}

#[test]
fn with_budgets_in_force_a_rebuilt_journal_is_the_same_journal_after_every_step_of_random_days() {
    let mut refused = 0;
    let mut steps = 0;
    for seed in 0..6 {
        let mut check = |j: &mut Journal<MemStore>| {
            j.sync_marks().unwrap();
            let r = recover(j, 3);
            same(j, &r);
            assert_eq!(r.gateway().budgets(), j.gateway().budgets());
            steps += 1;
        };
        let j = random_day(seed, 250, true, &mut check);
        refused += j.gateway().rejected_count("strategy_budget")
            + j.gateway().rejected_count("group_budget");
    }
    assert_eq!(steps, 6 * 250);
    assert!(refused > 20, "the budgets bound: {refused} refusals");
}
