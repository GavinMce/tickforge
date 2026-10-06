use std::path::PathBuf;

use tf_core::{InstrumentId, Nanos, Px};
use tf_risk::{GapRule, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{Decision, OrderId, OrderState, RejectReason};
use tf_synth::SplitMix64;

use tf_budget::Bounds;

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
        ev(Input::LossCheck { ts: 9 }),
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
        RejectReason::StrategyLossLimit,
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
    assert_eq!(a.targets(), b.targets());
    assert_eq!(a.scheduled(), b.scheduled());
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
            80..=94 => {
                j.mark(inst, Px::from_raw(px));
                if with_budgets {
                    j.check_loss_limits(now).unwrap();
                }
            }
            95..=96 => {
                let _ = j.engage_kill_switch(now);
            }
            97 => {
                let _ = j.new_day(now);
                if with_budgets && rng.below(2) == 0 {
                    // A person schedules a different split for the next rebalance.
                    let (a, b) = (
                        1_000 + rng.below(4_000) as u32,
                        1_000 + rng.below(4_000) as u32,
                    );
                    let _ = j.schedule_budgets(Some(split(a, b, 10_000 - a - b)), now);
                }
            }
            98 if with_budgets => {
                let _ = j.rebalance(now, Bounds::default(), None);
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
    let (mut refused, mut loss_stops, mut loss_checks) = (0, 0, 0);
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
        loss_stops += j.gateway().rejected_count("strategy_loss_limit");
        loss_checks += j
            .store()
            .records()
            .iter()
            .filter(|r| r.starts_with("losscheck"))
            .count();
    }
    assert_eq!(steps, 6 * 250);
    assert!(refused > 10, "the budgets bound: {refused} refusals");
    assert!(
        loss_stops > 5 && loss_checks > 3,
        "the loss limits tripped: {loss_stops} refusals, {loss_checks} checks"
    );
}

#[test]
fn a_strategy_crossing_its_loss_limits_is_recorded_once_and_replays_the_same() {
    // Strategy 1 has a $1,000 budget (soft limit $30, hard limit $60) and holds 100 shares at $5.
    let mut j = fresh(1);
    j.set_budgets(Some(day_budgets(3_000 * P as u128)), SEC)
        .unwrap();
    let mut open = buy(1, 0, 100, 5 * P);
    open.id.strategy = StrategyId(1);
    let Decision::Accepted(o) = j.decide(&open, 2 * SEC).unwrap() else {
        panic!()
    };
    j.ack(o, 2 * SEC).unwrap();
    j.fill(o, 100, Px::from_raw(5 * P), 2 * SEC).unwrap();
    // Nothing crossed: nothing is written.
    let n = j.records();
    j.mark(0, Px::from_raw(P * 4_800 / 1_000)); // down $0.20: $20
    assert!(j.check_loss_limits(3 * SEC).unwrap().is_empty());
    assert_eq!(j.records(), n + 1, "only the mark that the check needed");
    // $35 down: past the soft limit. One event, one record.
    j.mark(0, Px::from_raw(P * 4_650 / 1_000));
    let ev = j.check_loss_limits(4 * SEC).unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].strategy, ev[0].tier), (1, tf_risk::LossTier::Soft));
    let n = j.records();
    assert!(
        j.store()
            .records()
            .last()
            .unwrap()
            .starts_with("losscheck 4000000000")
    );
    assert!(j.check_loss_limits(5 * SEC).unwrap().is_empty());
    assert_eq!(
        j.records(),
        n,
        "a check that crosses nothing new writes nothing"
    );
    let mut again = buy(2, 0, 10, 5 * P);
    again.id.strategy = StrategyId(1);
    assert_eq!(
        j.decide(&again, 6 * SEC).unwrap(),
        Decision::Rejected(RejectReason::StrategyLossLimit)
    );
    // $65 down: the hard limit. The plan to flatten is the gateway's.
    j.mark(0, Px::from_raw(P * 4_350 / 1_000));
    let ev = j.check_loss_limits(7 * SEC).unwrap();
    assert_eq!((ev.len(), ev[0].tier), (1, tf_risk::LossTier::Hard));
    assert_eq!(
        j.gateway().flatten_plan(1).closes,
        vec![(0, tf_strategy::intent::Side::Sell, 100)]
    );
    // A restart finds the same latches and the same loss baseline.
    let r = recover(&j, 1);
    same(&j, &r);
    let snap = r.snapshot();
    assert_eq!(
        (snap.soft_latched.clone(), snap.hard_latched.clone()),
        (vec![1], vec![1])
    );
    // A new day clears them, and that is recorded and replayed too.
    j.new_day(10 * SEC).unwrap();
    let r = recover(&j, 1);
    same(&j, &r);
    assert!(r.snapshot().soft_latched.is_empty());
    assert_eq!(r.gateway().strategy_loss(1), 0);
}

#[test]
fn a_loss_check_that_replays_differently_is_caught() {
    let mut j = fresh(1);
    j.set_budgets(Some(day_budgets(3_000 * P as u128)), SEC)
        .unwrap();
    let mut open = buy(1, 0, 100, 5 * P);
    open.id.strategy = StrategyId(1);
    let Decision::Accepted(o) = j.decide(&open, 2 * SEC).unwrap() else {
        panic!()
    };
    j.ack(o, 2 * SEC).unwrap();
    j.fill(o, 100, Px::from_raw(5 * P), 2 * SEC).unwrap();
    j.mark(0, Px::from_raw(P * 4_650 / 1_000));
    j.check_loss_limits(4 * SEC).unwrap();
    let mut again = buy(2, 0, 10, 5 * P);
    again.id.strategy = StrategyId(1);
    assert_eq!(
        j.decide(&again, 6 * SEC).unwrap(),
        Decision::Rejected(RejectReason::StrategyLossLimit)
    );
    // Remove the loss check from the ledger: the later refusal can no longer be reproduced... except
    // that the decision itself latches the soft limit, so remove the mark that made the loss as well.
    let mut recs = j.store().records().to_vec();
    recs.retain(|r| !r.starts_with("losscheck") && !r.starts_with("mark"));
    let e = open_mem(recs, 1).err().unwrap();
    assert!(matches!(e, JournalError::Diverged { .. }), "{e}");
}

/// The one-group tree of `day_budgets` with the given shares for strategies 1, 2 and 3.
fn split(a: u32, b: u32, c: u32) -> tf_budget::Tree {
    use tf_budget::{Group, LossLimits, Strategy as S, Tree};
    Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            S {
                id: "s1".into(),
                share: a,
            },
            S {
                id: "s2".into(),
                share: b,
            },
            S {
                id: "s3".into(),
                share: c,
            },
        ],
    }])
    .unwrap()
}

/// Strategy `n` buys 100 shares at $5 and sells them at `exit` dollars.
fn round_trip(j: &mut Journal<MemStore>, strategy: u16, seq: u64, exit: i64) {
    let mut i = buy(seq, 0, 100, 5 * P);
    i.id.strategy = StrategyId(strategy);
    let Decision::Accepted(o) = j.decide(&i, seq * SEC).unwrap() else {
        panic!("{i:?}")
    };
    j.ack(o, seq * SEC).unwrap();
    j.fill(o, 100, Px::from_raw(5 * P), seq * SEC).unwrap();
    let mut c = sell(seq + 1, 0, 100, exit * P);
    c.id.strategy = StrategyId(strategy);
    let Decision::Accepted(o) = j.decide(&c, (seq + 1) * SEC).unwrap() else {
        panic!("{c:?}")
    };
    j.ack(o, (seq + 1) * SEC).unwrap();
    j.fill(o, 100, Px::from_raw(exit * P), (seq + 1) * SEC)
        .unwrap();
}

#[test]
fn schedule_and_rebalance_records_are_readable_and_refuse_nonsense() {
    let t = split(3_000, 3_000, 4_000);
    let ev = |input| Record::Event {
        input,
        outcome: None,
    };
    for r in [
        ev(Input::Schedule {
            tree: Some(t.clone()),
            ts: 5,
        }),
        ev(Input::Schedule { tree: None, ts: 5 }),
        ev(Input::Schedule {
            tree: Some(tf_budget::Tree::default()),
            ts: 5,
        }),
        ev(Input::Rebalance {
            ts: 7,
            bounds: Bounds::new(5_000, 20_000).unwrap(),
            balance: None,
        }),
        ev(Input::Rebalance {
            ts: 7,
            bounds: Bounds::new(10_000, 10_000).unwrap(),
            balance: Some(123),
        }),
    ] {
        assert_eq!(rt(&r), r);
    }
    assert_eq!(
        ev(Input::Schedule {
            tree: Some(t),
            ts: 5
        })
        .encode()
        .unwrap(),
        "schedule 5 g:10000:300:600/s1:3000/s2:3000/s3:4000"
    );
    assert_eq!(
        ev(Input::Rebalance {
            ts: 7,
            bounds: Bounds::default(),
            balance: None
        })
        .encode()
        .unwrap(),
        "rebalance 7 5000 20000 -"
    );
    for (bad, want) in [
        ("rebalance 7 0 20000 -", "rebalance: rebalance bounds need"),
        ("rebalance 7 5000 9999 -", "rebalance bounds need"),
        ("rebalance 7 5000 20000", "not a record"),
        ("rebalance 7 5000 20000 lots", "not a number"),
        ("schedule 5 g:10000:300/s1:10000", "a budget group is"),
        ("schedule 5 g:10001:300:600", "more than the whole"),
        ("schedule x off", "not a number"),
    ] {
        let e = Record::decode(bad).unwrap_err().0;
        assert!(e.contains(want), "wanted `{want}` in `{e}` for `{bad}`");
    }
}

#[test]
fn a_scheduled_change_waits_for_the_rebalance_and_profit_moves_into_the_strategy_that_made_it() {
    let mut j = fresh(1);
    let balance = 30_000 * P as u128;
    let b = tf_risk::Budgets::new(
        split(3_333, 3_333, 3_334),
        balance,
        [
            (1, "s1".to_owned()),
            (2, "s2".to_owned()),
            (3, "s3".to_owned()),
        ],
    )
    .unwrap();
    j.set_budgets(Some(b), SEC).unwrap();
    assert_eq!(j.targets().unwrap(), &split(3_333, 3_333, 3_334));
    assert!(j.scheduled().is_none());
    round_trip(&mut j, 1, 10, 7); // strategy 1: +$200
    round_trip(&mut j, 2, 20, 4); // strategy 2: -$100
    same(&j, &recover(&j, 1));
    // A scheduled change is held, not applied.
    let want = split(5_000, 2_000, 3_000);
    j.schedule_budgets(Some(want.clone()), 30 * SEC).unwrap();
    assert_eq!(j.scheduled(), Some(&want));
    assert_eq!(
        j.gateway().budgets().unwrap().tree(),
        &split(3_333, 3_333, 3_334),
        "still the old split"
    );
    same(&j, &recover(&j, 1));
    // Withdrawn, then the rebalance is the profit one.
    j.schedule_budgets(None, 31 * SEC).unwrap();
    assert!(j.scheduled().is_none());
    j.rebalance(40 * SEC, Bounds::default(), None).unwrap();
    let after = j.gateway().budgets().unwrap();
    assert_eq!(
        after.balance(),
        30_100 * P as u128,
        "the old balance plus the net profit"
    );
    let (d1, d2, d3) = (
        after.strategy_budget(1).unwrap(),
        after.strategy_budget(2).unwrap(),
        after.strategy_budget(3).unwrap(),
    );
    let tol = 3 * after.balance() / 10_000;
    assert!(d1.abs_diff(10_199 * P as u128) <= tol, "9,999 + 200: {d1}");
    assert!(
        d1 > 9_999 * P as u128 && d2 < 9_999 * P as u128,
        "the winner grew and the loser shrank: {d1} {d2}"
    );
    assert!(
        d3.abs_diff(10_002 * P as u128) <= tol,
        "an idle strategy keeps its dollars: {d3}"
    );
    assert_eq!(
        j.targets().unwrap(),
        &split(3_333, 3_333, 3_334),
        "the targets are what a person set"
    );
    same(&j, &recover(&j, 1));
    // Nothing new has been made since: another rebalance changes nothing.
    let before = j.gateway().budgets().unwrap().clone();
    j.rebalance(50 * SEC, Bounds::default(), None).unwrap();
    assert_eq!(j.gateway().budgets().unwrap(), &before);
    // Now the scheduled split goes in at the next rebalance, and becomes the targets.
    j.schedule_budgets(Some(want.clone()), 60 * SEC).unwrap();
    round_trip(&mut j, 3, 70, 8); // strategy 3: +$300
    j.rebalance(80 * SEC, Bounds::default(), None).unwrap();
    assert_eq!(j.gateway().budgets().unwrap().tree(), &want);
    assert_eq!(
        j.gateway().budgets().unwrap().balance(),
        30_400 * P as u128,
        "the profit still changes the balance"
    );
    assert_eq!(j.targets().unwrap(), &want);
    assert!(j.scheduled().is_none());
    same(&j, &recover(&j, 1));
    // The broker's real balance wins when given.
    j.rebalance(90 * SEC, Bounds::default(), Some(31_000 * P as u128))
        .unwrap();
    assert_eq!(j.gateway().budgets().unwrap().balance(), 31_000 * P as u128);
    same(&j, &recover(&j, 1));
}

#[test]
fn budget_changes_that_cannot_be_applied_are_refused_and_write_nothing() {
    let mut j = fresh(1);
    let n = j.records();
    // No budgets in force.
    assert!(matches!(
        j.schedule_budgets(Some(split(3_333, 3_333, 3_334)), SEC),
        Err(JournalError::Budgets(_))
    ));
    assert!(matches!(
        j.rebalance(SEC, Bounds::default(), None),
        Err(JournalError::Budgets(_))
    ));
    assert_eq!(j.records(), n);
    j.schedule_budgets(None, SEC).unwrap(); // withdrawing nothing is fine
    j.set_budgets(Some(day_budgets(30_000 * P as u128)), 2 * SEC)
        .unwrap();
    let n = j.records();
    // A schedule that drops a strategy that is in force.
    use tf_budget::{Group, LossLimits, Strategy as S, Tree};
    let drops = Tree::new(vec![Group {
        id: "g".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            S {
                id: "s1".into(),
                share: 5_000,
            },
            S {
                id: "s2".into(),
                share: 5_000,
            },
        ],
    }])
    .unwrap();
    let e = j.schedule_budgets(Some(drops), 3 * SEC).unwrap_err();
    assert!(e.to_string().contains("leave out strategy `s3`"), "{e}");
    assert_eq!(j.records(), n);
    assert!(j.scheduled().is_none());
}

#[test]
fn setting_budgets_again_clears_what_was_scheduled_and_counts_profit_from_there() {
    let mut j = fresh(1);
    j.set_budgets(Some(day_budgets(30_000 * P as u128)), SEC)
        .unwrap();
    round_trip(&mut j, 1, 10, 7);
    j.schedule_budgets(Some(split(2_000, 4_000, 4_000)), 20 * SEC)
        .unwrap();
    j.set_budgets(Some(day_budgets(31_000 * P as u128)), 30 * SEC)
        .unwrap();
    assert!(j.scheduled().is_none());
    let before = j.gateway().budgets().unwrap().clone();
    j.rebalance(40 * SEC, Bounds::default(), None).unwrap();
    assert_eq!(
        j.gateway().budgets().unwrap(),
        &before,
        "the earlier profit was already counted when budgets were set"
    );
    same(&j, &recover(&j, 1));
}

#[test]
fn rebalancing_keeps_shares_near_the_targets_a_person_set_not_near_where_it_last_left_them() {
    let mut j = fresh(1);
    let ids = [
        (1, "s1".to_owned()),
        (2, "s2".to_owned()),
        (3, "s3".to_owned()),
    ];
    let b = tf_risk::Budgets::new(split(2_000, 2_000, 2_000), 30_000 * P as u128, ids).unwrap();
    j.set_budgets(Some(b), SEC).unwrap();
    // Strategy 1 makes a fortune, twice (each trip: 100 shares bought at $5, sold at $300).
    round_trip(&mut j, 1, 10, 300);
    j.rebalance(20 * SEC, Bounds::default(), None).unwrap();
    assert_eq!(
        j.gateway()
            .budgets()
            .unwrap()
            .tree()
            .strategy("s1")
            .unwrap()
            .share,
        4_000,
        "twice its 20% target"
    );
    round_trip(&mut j, 1, 30, 300);
    j.rebalance(40 * SEC, Bounds::default(), None).unwrap();
    assert_eq!(
        j.gateway()
            .budgets()
            .unwrap()
            .tree()
            .strategy("s1")
            .unwrap()
            .share,
        4_000,
        "still twice the target, not twice where it stood"
    );
    assert_eq!(j.targets().unwrap(), &split(2_000, 2_000, 2_000));
    same(&j, &recover(&j, 1));
}

#[test]
fn an_observer_sees_the_journal_after_each_replayed_record_in_order() {
    let mut j = fresh(1);
    round_trip(&mut j, 1, 10, 7);
    j.new_day(40 * SEC).unwrap();
    let total = j.store().records().len();
    let mut seen: Vec<(u64, bool, i128)> = Vec::new();
    let (end, _) = Journal::open_recorded_observed(
        MemStore::from_records(j.store().records().to_vec()),
        &mut |at, rec| {
            seen.push((
                at.records(),
                matches!(rec, Record::Start { .. }),
                at.gateway().strategy_realized(1),
            ));
        },
    )
    .unwrap();
    assert_eq!(seen.len(), total, "once per record, the start included");
    assert_eq!(seen[0], (1, true, 0));
    assert!(seen.windows(2).all(|w| w[1].0 == w[0].0 + 1 && !w[1].1));
    assert_eq!(seen.last().unwrap().0, end.records());
    // The profit appears at the closing fill, not before and not only at the end.
    let first_profit = seen.iter().position(|s| s.2 != 0).unwrap();
    assert!(first_profit > 1 && first_profit < total - 1, "{seen:?}");
    assert_eq!(seen.last().unwrap().2, 200 * P as i128);
}

#[test]
fn a_ledger_can_be_read_while_a_writer_holds_it_without_changing_a_byte() {
    let dir = scratch("readonly");
    let mut w = file_with(&dir, 3);
    let log = dir.join("ledger.log");
    // The writer is live (lock held): a reader still sees every finished record.
    let mut r = ReadOnlyStore::open(&dir);
    let l = r.load().unwrap();
    assert_eq!(
        l.records,
        ["record number 1", "record number 2", "record number 3"]
    );
    assert_eq!(l.repaired, None);
    assert!(
        dir.join("ledger.lock").exists(),
        "the reader took and removed nothing"
    );
    // A record still being written is left out and reported, and left on disk for its writer.
    let mut bytes = std::fs::read(&log).unwrap();
    bytes.extend_from_slice(b"4 par");
    std::fs::write(&log, &bytes).unwrap();
    let l = r.load().unwrap();
    assert_eq!(l.records.len(), 3);
    assert!(l.repaired.unwrap().contains("5 byte(s) after record 3"));
    assert_eq!(std::fs::read(&log).unwrap(), bytes, "not truncated");
    // Damage before the end is still refused, and nothing can be appended.
    let text = String::from_utf8(bytes)
        .unwrap()
        .replace("number 2", "number 9");
    std::fs::write(&log, text).unwrap();
    assert!(matches!(r.load(), Err(StoreError::Corrupt { line: 2, .. })));
    assert_eq!(r.append(4, "x"), Err(StoreError::ReadOnly));
    assert!(
        StoreError::ReadOnly
            .to_string()
            .contains("cannot be written")
    );
    drop(w.load());
    drop(w);
    // A ledger that does not exist reads as empty, like an unwritten one.
    assert!(
        ReadOnlyStore::open(dir.join("nope"))
            .load()
            .unwrap()
            .records
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- the inbox

fn tree_of(a: u32, b: u32, c: u32) -> tf_budget::Tree {
    split(a, b, c)
}

#[test]
fn requests_wait_in_order_and_come_back_exactly_as_made() {
    use crate::inbox::{pending, settle, submit};
    let dir = scratch("inbox");
    assert_eq!(
        pending(&dir).unwrap(),
        (vec![], vec![]),
        "no inbox is an empty one"
    );
    let a = submit(&dir, "alice", Some(&tree_of(5_000, 3_000, 2_000))).unwrap();
    let b = submit(&dir, "bob\nwith a line break", None).unwrap();
    let c = submit(&dir, &"x".repeat(200), Some(&tree_of(1, 1, 1))).unwrap();
    assert_eq!(
        (a.as_str(), b.as_str()),
        ("0000000001.req", "0000000002.req")
    );
    let (got, bad) = pending(&dir).unwrap();
    assert!(bad.is_empty());
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].by, "alice");
    assert_eq!(got[0].tree, Some(tree_of(5_000, 3_000, 2_000)));
    assert_eq!(
        got[1].by, "bobwith a line break",
        "a request cannot smuggle in a line"
    );
    assert_eq!(got[1].tree, None, "a withdrawal has no tree");
    assert_eq!(got[2].by.len(), 160, "who asked is kept to a length");
    // Settled requests leave the queue; a refused one is kept apart with its reason; the numbers
    // are never reused.
    settle(&dir, &a, Ok(())).unwrap();
    settle(&dir, &b, Err("too\nbig".into())).unwrap();
    let (left, _) = pending(&dir).unwrap();
    assert_eq!(
        left.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        [c.as_str()]
    );
    let rej = std::fs::read_to_string(crate::inbox::dir(&dir).join("0000000002.rej")).unwrap();
    assert!(
        rej.starts_with("tfreq 1\nby bobwith a line break") && rej.ends_with("rejected: too big\n"),
        "{rej}"
    );
    assert_eq!(submit(&dir, "d", None).unwrap(), "0000000004.req");
    settle(&dir, &c, Ok(())).unwrap();
    assert_eq!(submit(&dir, "e", None).unwrap(), "0000000005.req");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_that_is_not_a_request_is_reported_and_the_rest_still_come() {
    use crate::inbox::{dir as inbox_dir, pending, submit};
    let dir = scratch("inbox-bad");
    submit(&dir, "alice", None).unwrap();
    let d = inbox_dir(&dir);
    std::fs::write(d.join("0000000002.req"), "hello").unwrap();
    std::fs::write(
        d.join("0000000003.req"),
        "tfreq 1\nby x\nschedule\nbudgets v1\ngroup g 20000 300 600\n",
    )
    .unwrap();
    std::fs::write(d.join("0000000004.req"), "tfreq 1\nby x\nmaybe\n").unwrap();
    std::fs::write(d.join("0000000005.req"), "tfreq 1\nno by line\nwithdraw\n").unwrap();
    std::fs::write(d.join("notes.txt"), "ignored").unwrap();
    std::fs::write(d.join("123.tmp"), "half written").unwrap();
    let (ok, bad) = pending(&dir).unwrap();
    assert_eq!(ok.len(), 1);
    let names: Vec<&str> = bad.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "0000000002.req",
            "0000000003.req",
            "0000000004.req",
            "0000000005.req"
        ]
    );
    assert!(bad[0].1.contains("tfreq 1"), "{bad:?}");
    assert!(
        bad[1].1.contains("more than the whole")
            || bad[1].1.contains("too big")
            || bad[1].1.contains("share"),
        "{bad:?}"
    );
    assert!(bad[2].1.contains("schedule"), "{bad:?}");
    assert!(bad[3].1.contains("by"), "{bad:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn applying_the_inbox_records_valid_changes_and_refuses_the_rest_with_the_reason() {
    use crate::inbox::{apply, pending, submit};
    let dir = scratch("inbox-apply");
    let mut j = fresh(1);
    j.set_budgets(Some(day_budgets(30_000 * P as u128)), SEC)
        .unwrap();
    // s1 holds 100 shares at $5 = $500 against a budget of about $9,999.
    let mut i = buy(10, 0, 100, 5 * P);
    i.id.strategy = StrategyId(1);
    let Decision::Accepted(o) = j.decide(&i, 10 * SEC).unwrap() else {
        panic!()
    };
    j.ack(o, 10 * SEC).unwrap();
    j.fill(o, 100, Px::from_raw(5 * P), 10 * SEC).unwrap();
    // Renamed strategies: the shape of the budgets in force is split(3333,3333,3334) over s1..s3.
    let good = submit(&dir, "alice", Some(&tree_of(5_000, 2_000, 3_000))).unwrap();
    let too_small = submit(&dir, "bob", Some(&tree_of(100, 4_000, 5_900))).unwrap(); // s1 = 1% of $30,000 = $300 < $500
    let other_shape = submit(&dir, "carol", Some(&tf_budget::Tree::default())).unwrap();
    let withdraw = submit(&dir, "dave", None).unwrap();
    let before = j.records();
    let r = apply(&mut j, &dir, 20 * SEC).unwrap();
    assert_eq!(r.scheduled, [good.clone(), withdraw.clone()]);
    assert_eq!(r.refused.len(), 2);
    assert_eq!(r.refused[0].0, too_small);
    assert!(
        r.refused[0].1.contains("s1") && r.refused[0].1.contains("in use"),
        "{:?}",
        r.refused
    );
    assert_eq!(r.refused[1].0, other_shape);
    assert!(r.refused[1].1.contains("same groups"), "{:?}", r.refused);
    let rejected: Vec<String> = std::fs::read_dir(crate::inbox::dir(&dir))
        .unwrap()
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".rej"))
        .collect();
    assert_eq!(
        rejected.len(),
        2,
        "only the two refused requests are kept: {rejected:?}"
    );
    // Two records went into the ledger (the schedule and the withdrawal), and the inbox is empty.
    assert_eq!(j.records(), before + 2);
    assert_eq!(pending(&dir).unwrap().0.len(), 0);
    assert_eq!(
        j.scheduled(),
        None,
        "the later withdrawal cleared the earlier schedule"
    );
    same(&j, &recover(&j, 1));
    // Alone, the good one is scheduled and survives a restart.
    submit(&dir, "alice", Some(&tree_of(5_000, 2_000, 3_000))).unwrap();
    apply(&mut j, &dir, 30 * SEC).unwrap();
    assert_eq!(j.scheduled(), Some(&tree_of(5_000, 2_000, 3_000)));
    same(&j, &recover(&j, 1));
    // Without budgets in force there is nothing to change.
    let mut bare = fresh(1);
    submit(&dir, "eve", Some(&tree_of(5_000, 2_000, 3_000))).unwrap();
    let r = apply(&mut bare, &dir, SEC).unwrap();
    assert!(
        r.scheduled.is_empty() && r.refused[0].1.contains("no budgets"),
        "{r:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_number_is_never_reused_even_when_the_latest_request_was_refused() {
    use crate::inbox::{settle, submit};
    let dir = scratch("inbox-numbers");
    let a = submit(&dir, "a", None).unwrap();
    settle(&dir, &a, Err("no".into())).unwrap();
    assert_eq!(submit(&dir, "b", None).unwrap(), "0000000002.req");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn requests_made_at_the_same_moment_all_arrive_under_different_names() {
    use crate::inbox::{pending, submit};
    let dir = scratch("inbox-race");
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let d = dir.clone();
            std::thread::spawn(move || submit(&d, &format!("p{i}"), None).unwrap())
        })
        .collect();
    let mut names: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 8, "{names:?}");
    assert_eq!(pending(&dir).unwrap().0.len(), 8);
    let _ = std::fs::remove_dir_all(&dir);
}
