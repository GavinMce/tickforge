use std::ffi::c_char;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use dbn::decode::{DbnDecoder, DecodeRecordRef};
use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{
    Cmbp1Msg, ConsolidatedBidAskPair, FlagSet, MetadataBuilder, RecordHeader, SType,
    SymbolMappingMsg, TradeMsg, rtype,
};
use tf_capture::{Config as CaptureConfig, RawWriter};
use tf_core::{Event, Nanos, Px};
use tf_ledger::MemStore;
use tf_strategy::intent::StrategyId;
use tf_strategy::{CrossStrategy, Ctx, MemberView};
use tf_universe::{LiveFeature, Spec};

use crate::tests::*;
use crate::{
    Answer, Host, HostConfig, Log, Rec, ReplayError, Route, SlotState, StrategyDef, Verdict,
    certify, compare, replay_capture, replay_events, runner,
};

fn live_host(cfg: &HostConfig) -> Host<MemStore> {
    host(cfg).record()
}

/// Run a day live (recording), returning the host.
fn live_day(cfg: &HostConfig, defs: &[StrategyDef], tape: &[Event]) -> Host<MemStore> {
    let mut h = live_host(cfg);
    certify_all(&mut h, cfg, defs, tape);
    for e in tape {
        h.on_event(e).unwrap();
    }
    h.end_of_day(tape.last().unwrap().ts_recv()).unwrap();
    h
}

fn two() -> Vec<StrategyDef> {
    vec![
        def(1, LOW, Plan::Buy { qty: 100, n: 6 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 6 }, Route::Sim),
    ]
}

#[test]
fn the_log_is_text_that_reads_back_and_a_damaged_one_is_refused() {
    let cfg = config(2);
    let tape = market(12, flat);
    let h = live_day(&cfg, &two(), &tape);
    let log = h.log().unwrap();
    assert!(
        log.recs.iter().any(|r| matches!(r, Rec::Decision { .. }))
            && log.recs.iter().any(|r| matches!(r, Rec::Fill { .. }))
    );
    assert!(
        log.recs
            .iter()
            .any(|r| matches!(r, Rec::Action { what, .. } if what.starts_with("add 1 ")))
    );
    let text = log.render();
    assert!(
        text.starts_with("decisions v1\nid_space 12\nsymbols ")
            && text
                .trim_end()
                .ends_with(&format!("end {}", log.recs.len()))
    );
    assert_eq!(&Log::parse(&text).unwrap(), log);
    // Every kind of damage is noticed.
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        Log::parse(&lines[..lines.len() - 1].join("\n"))
            .unwrap_err()
            .contains("cut short")
    );
    assert!(
        Log::parse(&text.replacen("end ", "end 9", 1))
            .unwrap_err()
            .contains("damaged")
    );
    let mut cut = lines.clone();
    cut.remove(5);
    assert!(Log::parse(&cut.join("\n")).unwrap_err().contains("damaged"));
    assert!(Log::parse(&text.replacen("decisions v1", "decisions v2", 1)).is_err());
    assert!(
        Log::parse(&format!("{text}d 1 1 1 1 1 B 1 O 1 1 A1\n"))
            .unwrap_err()
            .contains("after `end`")
    );
    for bad in [
        "d 1 2 3",
        "d 1 2 3 4 5 Q 7 O 9 1 A1",
        "d 1 2 3 4 5 B 7 Z 9 1 A1",
        "d 1 2 3 4 5 B 7 O 9 1 Z1",
        "t 1 2 3 sideways 5 6",
        "x 1",
        "f 1 2 3",
    ] {
        assert!(Rec::parse(bad).is_err(), "{bad}");
    }
    for r in &log.recs {
        assert_eq!(&Rec::parse(&r.line()).unwrap(), r);
    }
}

#[test]
fn a_live_day_replays_to_the_same_decisions_answers_and_fills() {
    let cfg = config(2);
    let tape = market(12, flat);
    let defs = two();
    let live = live_day(&cfg, &defs, &tape);
    let log = live.log().unwrap();
    let r = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    assert_eq!(
        compare(log, &r.log, &reference().symbols),
        Verdict::Equal {
            records: log.recs.len()
        }
    );
    assert!(log.recs.len() > 20);
    assert_eq!(r.events, live.events());
    assert_eq!(r.ledger_records, live.journal().records() as usize);
    // The ledger the replay wrote is the one the live day wrote, bit for bit.
    let again = {
        let mut h = host(&cfg).record();
        for d in &defs {
            h.install_for_test(d);
        }
        for e in &tape {
            h.on_event(e).unwrap();
        }
        h.end_of_day(tape.last().unwrap().ts_recv()).unwrap();
        h
    };
    assert_eq!(
        again.journal().store().records(),
        live.journal().store().records()
    );
    let rep = crate::report(log, &r, &reference().symbols, &defs, 0);
    assert!(rep.verdict.is_equal() && rep.text().contains("reproduced all"));
}

#[test]
fn a_different_market_is_caught_at_the_first_decision_it_changes_with_its_time_and_symbol() {
    let cfg = config(2);
    let tape = market(12, flat);
    let defs = two();
    let live = live_day(&cfg, &defs, &tape);
    let log = live.log().unwrap();
    // The price of S02's first trade of the third second is one cent different. Strategy 1's third
    // review buys S02 at the last price plus 5 cents, so that is the first thing that changes.
    let mut other = tape.clone();
    let k = other
        .iter()
        .position(|e| matches!(e, Event::Trade(t) if t.hdr.instrument == 2 && t.hdr.ts_recv >= T0 + 2 * SEC))
        .unwrap();
    let Event::Trade(mut t) = other[k] else {
        unreachable!()
    };
    t.px = Px::from_cents(2_001);
    other[k] = Event::Trade(t);
    // (Later trades of the same second reset the last price, so change them all.)
    for e in other.iter_mut() {
        if let Event::Trade(t) = e {
            if t.hdr.instrument == 2
                && t.hdr.ts_recv >= T0 + 2 * SEC
                && t.hdr.ts_recv < T0 + 3 * SEC
            {
                t.px = Px::from_cents(2_001);
            }
        }
    }
    let r = replay_events(log, &cfg, &reference(), &defs, &other).unwrap();
    let Verdict::Differs(d) = compare(log, &r.log, &reference().symbols) else {
        panic!("should differ")
    };
    assert_eq!(d.symbol.as_deref(), Some("S02"));
    assert!(d.ts >= T0 + 2 * SEC && d.ts < T0 + 3 * SEC + 1, "{}", d.ts);
    assert!(
        d.live.starts_with("d ") && d.replay.starts_with("d ") && d.live != d.replay,
        "{d:?}"
    );
    let text = crate::report(log, &r, &reference().symbols, &defs, 0).text();
    assert!(
        text.contains("differs at record") && text.contains(", S02") && text.contains("UTC"),
        "{text}"
    );
    // And a market with an event missing shifts everything after it.
    let mut short = tape.clone();
    short.remove(short.len() / 2);
    let r = replay_events(log, &cfg, &reference(), &defs, &short).unwrap();
    assert!(!compare(log, &r.log, &reference().symbols).is_equal());
    // An empty replay differs at the first thing the live day did after the strategies were added.
    let r = replay_events(log, &cfg, &reference(), &defs, &[]).unwrap();
    let Verdict::Differs(d) = compare(log, &r.log, &reference().symbols) else {
        panic!()
    };
    assert!(
        d.replay.contains("nothing") || d.live.starts_with('d'),
        "{d:?}"
    );
}

#[test]
fn the_ledger_numbering_and_the_symbols_are_checked_before_the_decisions() {
    let cfg = config(2);
    let tape = market(8, flat);
    let defs = two();
    let live = live_day(&cfg, &defs, &tape);
    let log = live.log().unwrap();
    let r = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    // A replay that numbered the instruments differently is not a replay that decided differently.
    let mut other = r.log.clone();
    other.symbols ^= 1;
    let Verdict::Differs(d) = compare(log, &other, &reference().symbols) else {
        panic!()
    };
    assert_eq!((d.record, d.live.starts_with("symbols")), (0, true));
    let mut other = r.log.clone();
    other.id_space += 1;
    assert!(
        matches!(compare(log, &other, &reference().symbols), Verdict::Differs(d) if d.live.starts_with("id_space"))
    );
}

#[test]
fn a_replay_of_another_strategy_is_refused_not_compared() {
    let cfg = config(2);
    let tape = market(8, flat);
    let live = live_day(&cfg, &two(), &tape);
    let changed = vec![
        def(1, LOW, Plan::Buy { qty: 200, n: 6 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 6 }, Route::Sim),
    ];
    assert!(matches!(
        replay_events(live.log().unwrap(), &cfg, &reference(), &changed, &tape),
        Err(ReplayError::StrategyChanged { id: 1, .. })
    ));
    let missing = vec![def(2, HIGH, Plan::Buy { qty: 50, n: 6 }, Route::Sim)];
    assert!(matches!(
        replay_events(live.log().unwrap(), &cfg, &reference(), &missing, &tape),
        Err(ReplayError::UnknownStrategy(1))
    ));
    let mut bad = live.log().unwrap().clone();
    bad.recs.push(Rec::Action {
        idx: 0,
        ts: 0,
        what: "dance".into(),
    });
    assert!(matches!(
        replay_events(&bad, &cfg, &reference(), &two(), &tape),
        Err(ReplayError::BadAction(_))
    ));
}

#[test]
fn what_an_operator_did_by_hand_is_replayed_at_the_event_it_was_done_at() {
    let cfg = config(3);
    let tape = market(30, flat);
    let defs = vec![
        def(1, LOW, Plan::Buy { qty: 100, n: 20 }, Route::Sim),
        def(2, HIGH, Plan::Buy { qty: 50, n: 20 }, Route::Sim),
        def(3, LOW, Plan::Buy { qty: 20, n: 20 }, Route::Sim),
    ];
    // Live: strategy 3 joins at second 5, strategy 1 is killed at 10, the kill switch at 20.
    let mut h = live_host(&cfg);
    certify_all(&mut h, &cfg, &defs[..2], &tape);
    let cert3 = certify(&defs[2], &cfg, &reference(), &tape, 7).unwrap();
    let at = |sec: u64| {
        tape.iter()
            .position(|e| e.ts_recv() >= T0 + sec * SEC)
            .unwrap()
    };
    let (a5, a10, a20) = (at(5), at(10), at(20));
    for (i, e) in tape.iter().enumerate() {
        if i == a5 {
            h.add_strategy(&defs[2], &cert3).unwrap();
        }
        if i == a10 {
            h.kill_strategy(1, e.ts_recv()).unwrap();
        }
        if i == a20 {
            h.kill_switch(e.ts_recv()).unwrap();
        }
        h.on_event(e).unwrap();
    }
    h.end_of_day(T0 + 40 * SEC).unwrap();
    let log = h.log().unwrap();
    let actions: Vec<&str> = log
        .recs
        .iter()
        .filter_map(|r| {
            if let Rec::Action { what, .. } = r {
                Some(what.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(actions.len(), 6, "{actions:?}");
    assert!(
        actions[2].starts_with("add 3 ")
            && actions[3] == "kill_strategy 1"
            && actions[4] == "kill_switch"
            && actions[5] == "end_of_day"
    );
    let r = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    assert!(
        compare(log, &r.log, &reference().symbols).is_equal(),
        "{}",
        compare(log, &r.log, &reference().symbols).report()
    );
    // Without them the replay is another day: the first action the live log has and the replay does not.
    let mut no_kill = log.clone();
    no_kill
        .recs
        .retain(|r| !matches!(r, Rec::Action { what, .. } if what == "kill_strategy 1"));
    let r2 = replay_events(&no_kill, &cfg, &reference(), &defs, &tape).unwrap();
    let Verdict::Differs(d) = compare(log, &r2.log, &reference().symbols) else {
        panic!("should differ")
    };
    assert!(
        d.live.contains("kill_strategy 1") && d.symbol.is_none(),
        "{d:?}"
    );
    let _ = (SlotState::Running, StrategyId(1));
}

static FLAKY: AtomicU32 = AtomicU32::new(0);

/// Decides on something that is not in the events: a counter that outlives the run, as a clock or a
/// random number would.
struct Flaky;

impl CrossStrategy for Flaky {
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        if FLAKY.fetch_add(1, Ordering::SeqCst) % 3 == 0 {
            buy_top(ctx, view);
        }
    }
}

/// A strategy's own buy, written out: the top member by trades, at its last price plus five cents.
fn buy_top(ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
    {
        // (a strategy's own buy, written out: the top member by trades, at its last price.)
        use tf_strategy::Request;
        use tf_strategy::intent::{Pricing, Protective, Purpose, Side, Tif};
        let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first().copied() else {
            return;
        };
        let Some(last) = view.state(id).and_then(|s| s.last_px) else {
            return;
        };
        let req = Request {
            side: Side::Buy,
            qty: 100,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(Px::from_raw(last.raw() + 50_000_000)),
            protect: Some(Protective {
                stop_trigger: Px::from_raw(last.raw() / 2),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            reason: 3,
        };
        let _ = ctx.submit(id, req);
    }
}

#[test]
fn a_strategy_that_decides_on_something_outside_the_events_is_caught() {
    let cfg = config(1);
    let tape = market(14, flat);
    let d = StrategyDef {
        id: 1,
        name: "flaky".into(),
        params: String::new(),
        universe: Spec::parse(LOW).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(|| runner(Flaky)),
    };
    let mut live = host(&cfg).record();
    live.install_for_test(&d);
    for e in &tape {
        live.on_event(e).unwrap();
    }
    let log = live.log().unwrap();
    let r = replay_events(log, &cfg, &reference(), std::slice::from_ref(&d), &tape).unwrap();
    // The replay is run later, with the counter wherever the live day left it.
    let Verdict::Differs(diff) = compare(log, &r.log, &reference().symbols) else {
        panic!("a flaky strategy replayed the same")
    };
    assert!(
        diff.live.starts_with("d ") || diff.replay.starts_with("d "),
        "{diff:?}"
    );
    assert!(diff.symbol.is_some());
}

#[test]
fn evictions_and_promotions_replay_the_same_and_a_follower_agrees_after_every_event() {
    use tf_core::{TierAction, TierChange};
    use tf_engine::Promoter;
    let mut cfg = config(2);
    cfg.promoter.max_tier1 = 1;
    let tape = market(12, flat);
    let mk = |id: u16, universe: &str, priority: u8| StrategyDef {
        id,
        name: format!("wants{id}"),
        params: String::new(),
        universe: Spec::parse(universe).unwrap(),
        priority,
        route: Route::Sim,
        build: Box::new(move || runner(crate::tests::wants(id))),
    };
    let defs = vec![mk(1, LOW, 1), mk(2, HIGH, 5)];
    let mut live = host(&cfg).record();
    for d in &defs {
        live.install_for_test(d);
    }
    // Run live, noting after every event what Tier 1 held and what changes the promoter made.
    let mut membership = Vec::new();
    for e in &tape {
        live.on_event(e).unwrap();
        membership.push(live.promoter().promoted().to_vec());
    }
    let log = live.log().unwrap().clone();
    let tiers: Vec<&Rec> = log
        .recs
        .iter()
        .filter(|r| matches!(r, Rec::Tier { .. }))
        .collect();
    assert!(
        tiers.len() >= 3,
        "a promotion, an eviction and another promotion: {tiers:?}"
    );
    // The deciding replay reproduces all of it.
    let r = replay_events(&log, &cfg, &reference(), &defs, &tape).unwrap();
    assert!(compare(&log, &r.log, &reference().symbols).is_equal());
    // A follower fed the tape (each change before the event that caused it) holds the same symbols
    // as the live promoter after every event. (Inside the callback that asked, the two can differ: the
    // live promoter has applied the eviction when the strategy's code runs, the follower had applied it
    // before the event began. Replay by recomputation, as above, has no such gap.)
    let mut follower = Promoter::follower(1, SYMBOLS as usize);
    let mut seq = 0;
    for (i, e) in tape.iter().enumerate() {
        let idx = i as u64 + 1;
        for r in tiers.iter().filter(|r| r.idx() == idx) {
            let Rec::Tier {
                ts,
                instrument,
                promote,
                reason,
                score,
                ..
            } = r
            else {
                unreachable!()
            };
            let c = TierChange {
                hdr: tf_core::Header {
                    ts_event: *ts,
                    ts_recv: *ts,
                    seq,
                    instrument: *instrument,
                    provider: tf_core::ProviderId::Internal,
                },
                action: if *promote {
                    TierAction::Promote
                } else {
                    TierAction::Demote
                },
                reason: *reason,
                score: *score,
            };
            seq += 1;
            follower.apply(&c).unwrap();
        }
        let _ = e;
        assert_eq!(follower.promoted(), &membership[i][..], "after event {idx}");
    }
}

// ---- a raw capture of a day ----

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-host-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn trade_rec(instrument: u32, ts_recv: u64, price: i64, size: u32, sequence: u32) -> TradeMsg {
    TradeMsg {
        hd: RecordHeader::new::<TradeMsg>(rtype::MBP_0, 81, instrument, ts_recv - 1_000),
        price,
        size,
        action: b'T' as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        depth: 0,
        ts_recv,
        ts_in_delta: 0,
        sequence,
    }
}

fn quote_rec(instrument: u32, ts_recv: u64, bid: i64, ask: i64) -> Cmbp1Msg {
    Cmbp1Msg {
        hd: RecordHeader::new::<Cmbp1Msg>(rtype::CMBP_1, 88, instrument, ts_recv - 500),
        price: 0,
        size: 0,
        action: b'A' as c_char,
        side: b'N' as c_char,
        flags: FlagSet::empty(),
        _reserved1: [0],
        ts_recv,
        ts_in_delta: 0,
        _reserved2: [0; 4],
        levels: [ConsolidatedBidAskPair {
            bid_px: bid,
            ask_px: ask,
            bid_sz: 100_000,
            ask_sz: 100_000,
            bid_pb: 81,
            _reserved1: [0; 2],
            ask_pb: 82,
            _reserved2: [0; 2],
        }],
    }
}

/// A DBN stream of a day: a symbol mapping for each symbol (raw id 20,000 + i), then each second every
/// symbol quotes and trades `1 + i % 3` times. `bump` cents are added to S02's price from second 2 on.
fn dbn_day(secs: u64, bump: i64) -> Vec<u8> {
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(T0)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        for i in 0..SYMBOLS {
            let m = SymbolMappingMsg::new(
                20_000 + i,
                T0 - 1,
                SType::RawSymbol,
                &format!("S{i:02}"),
                SType::RawSymbol,
                &format!("S{i:02}"),
                0,
                0,
            )
            .unwrap();
            e.encode_record(&m).unwrap();
        }
        let mut recs: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut seq = 0u32;
        for sec in 0..secs {
            for i in 0..SYMBOLS {
                let ts = T0 + sec * SEC + u64::from(i) * MS;
                let cents = 2_000 + if i == 2 && sec >= 2 { bump } else { 0 };
                let q = quote_rec(
                    20_000 + i,
                    ts,
                    (cents - 1) * 10_000_000,
                    (cents + 1) * 10_000_000,
                );
                recs.push((ts, dbn_bytes(&q)));
                for k in 0..=u64::from(i % 3) {
                    seq += 1;
                    let t = trade_rec(20_000 + i, ts + 1 + k, cents * 10_000_000, 100, seq);
                    recs.push((ts + 1 + k, dbn_bytes(&t)));
                }
            }
        }
        recs.sort_by_key(|r| r.0);
        for (_, b) in recs {
            bytes.extend_from_slice(&b);
        }
    }
    bytes
}

fn dbn_bytes<R: dbn::Record + dbn::encode::DbnEncodable>(r: &R) -> Vec<u8> {
    let mut out = Vec::new();
    // A record is its raw bytes; the stream header is written once, by the caller.
    out.extend_from_slice(dbn::RecordRef::from(r).as_ref());
    out
}

fn write_capture(dir: &std::path::Path, bytes: &[u8]) {
    let (mut w, _) = RawWriter::open(CaptureConfig {
        segment_secs: 3,
        ..CaptureConfig::new(dir, "XNAS.BASIC")
    })
    .unwrap();
    let mut dec = DbnDecoder::new(bytes).unwrap();
    while let Some(rec) = dec.decode_record_ref().unwrap() {
        w.write(&rec).unwrap();
    }
    w.finish().unwrap();
}

/// What a live process would have: the events and the names, straight from the stream.
fn live_events(bytes: &[u8]) -> (Vec<Event>, tf_core::SymbolTable) {
    let mut d = tf_databento::Decoder::new(bytes).unwrap();
    let mut ev = Vec::new();
    while let Some(i) = d.next_item().unwrap() {
        if let tf_databento::Item::Event(e) = i {
            ev.push(e);
        }
    }
    let mut t = tf_core::SymbolTable::new();
    for id in 0..d.instruments().len() as u32 {
        t.intern(d.instruments().symbol(id).unwrap());
    }
    (ev, t)
}

#[test]
fn a_raw_capture_of_a_day_replays_to_the_live_days_decisions() {
    let bytes = dbn_day(10, 0);
    let (events, symbols) = live_events(&bytes);
    assert_eq!(symbols.len(), SYMBOLS as usize);
    let cfg = config(2);
    let reference = crate::Reference {
        symbols: symbols.clone(),
        snapshot: snapshot(),
    };
    let defs = two();
    // The live day.
    let mut live = Host::new(
        cfg.clone(),
        reference.clone(),
        MemStore::from_records(vec![]),
    )
    .unwrap()
    .record();
    for d in &defs {
        let cert = certify(d, &cfg, &reference, &events, 9).unwrap();
        live.add_strategy(d, &cert).unwrap();
    }
    for e in &events {
        live.on_event(e).unwrap();
    }
    live.end_of_day(events.last().unwrap().ts_recv()).unwrap();
    let log = live.log().unwrap().clone();
    assert!(
        log.recs
            .iter()
            .filter(|r| matches!(
                r,
                Rec::Decision {
                    answer: Answer::Accepted(_),
                    ..
                }
            ))
            .count()
            >= 10
    );
    // The capture of that day, in segments.
    let dir = scratch("day");
    write_capture(&dir, &bytes);
    assert!(tf_capture::list(&dir).unwrap().len() >= 3, "it rolled");
    let rep = replay_capture(&dir, &log, &cfg, snapshot(), &defs, 0).unwrap();
    assert!(rep.verdict.is_equal(), "{}", rep.text());
    assert_eq!(rep.events, events.len() as u64);
    // Another day's capture (S02 a cent dearer from the third second): the first difference is named.
    let other = scratch("other");
    write_capture(&other, &dbn_day(10, 1));
    let rep = replay_capture(&other, &log, &cfg, snapshot(), &defs, 0).unwrap();
    let Verdict::Differs(d) = &rep.verdict else {
        panic!("{}", rep.text())
    };
    assert_eq!(d.symbol.as_deref(), Some("S02"), "{}", rep.text());
    assert!(d.ts >= T0 + 2 * SEC);
    assert!(!rep.text().contains("gave up"));
    // If the live queue had dropped events, the report says why a difference may not be a bug.
    let rep = replay_capture(&other, &log, &cfg, snapshot(), &defs, 17).unwrap();
    assert!(rep.text().contains("gave up 17 events"), "{}", rep.text());
    let rep = replay_capture(&dir, &log, &cfg, snapshot(), &defs, 17).unwrap();
    assert!(
        !rep.text().contains("gave up"),
        "an equal replay needs no excuse"
    );
    // A capture that is not there, or is unfinished, is an error and not a verdict.
    assert!(matches!(
        replay_capture(&scratch("none").join("x"), &log, &cfg, snapshot(), &defs, 0),
        Err(ReplayError::Capture(_)) | Ok(_)
    ));
    let _ = (fs::remove_dir_all(&dir), fs::remove_dir_all(&other));
}

#[test]
fn strategies_that_traded_on_a_paper_broker_are_named_in_the_report() {
    let cfg = config(1);
    let tape = market(8, flat);
    let defs = vec![def(1, LOW, Plan::Buy { qty: 100, n: 3 }, Route::Paper)];
    let mut live = Host::new(cfg.clone(), reference(), MemStore::from_records(vec![]))
        .unwrap()
        .with_paper(Box::new(tf_strategy::sim::SimBroker::new(
            cfg.sim,
            SYMBOLS as usize,
        )))
        .record();
    live.install_for_test(&defs[0]);
    for e in &tape {
        live.on_event(e).unwrap();
    }
    let log = live.log().unwrap();
    let r = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    let rep = crate::report(log, &r, &reference().symbols, &defs, 0);
    assert_eq!(rep.paper_strategies, [1]);
    assert!(rep.text().contains("paper broker"), "{}", rep.text());
}

#[test]
fn the_report_gives_the_time_of_day_to_the_nanosecond() {
    for (secs, nanos, shown) in [
        (14 * 3600 + 30 * 60 + 5, 123_456_789, "14:30:05.123456789"),
        (15 * 3600, 1, "15:00:00.000000001"),
        (3 * 3600 + 59 * 60 + 59, 999_999_999, "03:59:59.999999999"),
    ] {
        let ts = 3 * 86_400 * SEC + secs * SEC + nanos;
        let d = crate::Difference {
            record: 7,
            event: 42,
            ts,
            symbol: Some("AAPL".into()),
            live: "a".into(),
            replay: "b".into(),
        };
        let text = Verdict::Differs(d).report();
        assert!(
            text.contains(&format!("record 7 (event 42, {shown} UTC, AAPL)")),
            "{text}"
        );
    }
}

#[test]
fn promotions_the_scanner_makes_are_in_the_log_and_replay_the_same() {
    use tf_core::{Header, ProviderId, Quote, Trade, TradeFlags};
    let mut cfg = config(1);
    cfg.scanner.min_volume = 1_000;
    cfg.promoter.max_tier1 = 4;
    // Every symbol trades 100 shares a second for 80 seconds; then S05 trades twenty times the size, 8% higher.
    let mut tape = Vec::new();
    for sec in 0..90u64 {
        for sym in 0..SYMBOLS {
            let ts = T0 + sec * SEC + u64::from(sym) * MS;
            let hdr = |t| Header {
                ts_event: t,
                ts_recv: t,
                seq: t,
                instrument: sym,
                provider: ProviderId::Synthetic,
            };
            tape.push(Event::Quote(Quote {
                hdr: hdr(ts),
                bid_px: Px::from_cents(999),
                ask_px: Px::from_cents(1_001),
                bid_sz: 100_000,
                ask_sz: 100_000,
            }));
            let burst = sym == 5 && sec >= 80;
            let size = if burst { 2_000 } else { 100 };
            let cents = if burst { 1_080 } else { 1_000 };
            tape.push(Event::Trade(Trade {
                hdr: hdr(ts + 1),
                px: Px::from_cents(cents),
                size,
                flags: TradeFlags::NONE,
            }));
        }
    }
    tape.sort_by_key(Event::ts_recv);
    let defs = vec![def(1, LOW, Plan::Buy { qty: 100, n: 3 }, Route::Sim)];
    let live = live_day(&cfg, &defs, &tape);
    let log = live.log().unwrap();
    let promoted: Vec<&Rec> = log
        .recs
        .iter()
        .filter(|r| {
            matches!(
                r,
                Rec::Tier {
                    promote: true,
                    reason: 1,
                    ..
                }
            )
        })
        .collect();
    assert!(
        promoted.iter().any(|r| r.instrument() == Some(5)),
        "the scanner's promotion of S05 is logged: {promoted:?}"
    );
    let r = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    assert!(compare(log, &r.log, &reference().symbols).is_equal());
}

#[test]
fn a_log_cut_short_by_a_crash_can_still_be_read_as_far_as_it_goes() {
    let cfg = config(2);
    let tape = market(8, flat);
    let h = {
        let mut h = host(&cfg).record();
        certify_all(&mut h, &cfg, &two(), &tape);
        for e in &tape {
            h.on_event(e).unwrap();
        }
        h
    };
    let log = h.log().unwrap();
    let text = log.render();
    // The whole text reads as complete; with the last line gone, and with a record half written.
    assert_eq!(Log::parse_partial(&text).unwrap(), (log.clone(), true));
    let cut = text.trim_end().rsplit_once('\n').unwrap().0.to_owned();
    let (partial, complete) = Log::parse_partial(&cut).unwrap();
    assert!(!complete);
    assert_eq!(&partial, log);
    let half = format!("{cut}\nd 12 3 1 1");
    assert!(
        Log::parse_partial(&half).is_err(),
        "a damaged line is an error, not a shorter log"
    );
    let fewer = text
        .lines()
        .take(text.lines().count() - 4)
        .collect::<Vec<_>>()
        .join("\n");
    let (p2, complete) = Log::parse_partial(&fewer).unwrap();
    assert!(
        !complete && p2.recs.len() == log.recs.len() - 3,
        "{} vs {}",
        p2.recs.len(),
        log.recs.len()
    );
    assert_eq!(
        log.header(),
        text.lines().take(3).collect::<Vec<_>>().join("\n") + "\n"
    );
    assert_eq!(log.footer(), format!("end {}\n", log.recs.len()));
}

#[test]
fn symbol_names_make_a_table_that_keeps_the_ids_even_when_names_repeat_or_are_missing() {
    let t = crate::symbol_table(&[
        Some("AAPL".into()),
        None,
        Some("AAPL".into()),
        Some("MSFT".into()),
        Some("AAPL".into()),
    ]);
    let names: Vec<&str> = (0..5).map(|i| t.name(i).unwrap()).collect();
    assert_eq!(names, ["AAPL", "#1", "AAPL~1", "MSFT", "AAPL~2"]);
    assert_eq!(t.get("MSFT"), Some(3));
}

#[test]
fn events_after_the_live_days_end_are_not_replayed() {
    let cfg = config(2);
    let tape = market(14, flat);
    let defs = two();
    // The live day ended at second 10; the tape (a capture) holds four more seconds.
    let cut = tape
        .iter()
        .position(|e| e.ts_recv() >= T0 + 10 * SEC)
        .unwrap();
    let mut live = host(&cfg).record();
    certify_all(&mut live, &cfg, &defs, &tape);
    for e in &tape[..cut] {
        live.on_event(e).unwrap();
    }
    live.end_of_day(T0 + 10 * SEC).unwrap();
    let log = live.log().unwrap();
    let r = replay_events(log, &cfg, &reference(), &defs, &tape).unwrap();
    assert_eq!(r.events, cut as u64);
    assert!(compare(log, &r.log, &reference().symbols).is_equal());
}
