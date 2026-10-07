//! The closing reversal (T04) through the host: a scripted afternoon, the simulated broker's fills, the exits, and the
//! replay check; the same afternoon as a DBN day through the research runner.

use dbn::encode::{DbnEncoder, EncodeRecord};
use dbn::{MetadataBuilder, SType, SymbolMappingMsg};
use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::{Event, Header, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};
use tf_strategy::ClosingReversalParams;
use tf_universe::Spec;

use crate::host::FillNote;
use crate::library::closing_reversal;
use crate::replay_tests::{dbn_bytes, quote_rec, trade_rec};
use crate::tests::{LOW, SEC, SYMBOLS, config, host, reference};
use crate::{HostConfig, StrategyDef, Verdict, compare, replay_events};
use tf_strategy::exits::REASON_TIME;
use tf_strategy::intent::{Purpose, Side};

#[derive(Clone, Copy, Debug)]
enum K {
    /// A trade at this price, in cents.
    Trade(i64),
    /// A quote: bid and ask, in cents.
    Quote(i64, i64),
}

#[derive(Clone, Copy, Debug)]
struct Ev {
    ts: Nanos,
    sym: u32,
    k: K,
}

fn times(y: i32, m: u8, d: u8) -> SessionTimes {
    Calendar::us_equities()
        .times(Date::new(y, m, d).unwrap())
        .unwrap()
        .unwrap()
}

/// 15:00 prices, in cents, of the members S00..S05 (prior close 20.00 in the test snapshot): -10%, -5%, -2%, +2%, -4%, 0.
const AT_THREE: [i64; 6] = [1800, 1900, 1960, 2040, 1920, 2000];

/// An afternoon around a regular close, as events: a trade of each member at 20.00 before the first snapshot, the prices
/// at 14:58, quotes at 15:29 a cent either side, a trade of S00, S01 and S04 near the end at the given prices, the quotes
/// at 15:59:20, with a quote of a symbol that is nobody's member every so often to carry the time.
fn afternoon(close: Nanos, late: [i64; 3]) -> Vec<Ev> {
    let t = |s: u64| close - s * SEC;
    let mut v = Vec::new();
    let clock = |v: &mut Vec<Ev>, s: u64| {
        v.push(Ev {
            ts: t(s),
            sym: SYMBOLS - 1,
            k: K::Quote(1, 2),
        })
    };
    clock(&mut v, 4 * 3600);
    clock(&mut v, 4 * 3600 - 61);
    for i in 0..6u32 {
        v.push(Ev {
            ts: t(3950) + u64::from(i),
            sym: i,
            k: K::Trade(2000),
        });
    }
    clock(&mut v, 3850);
    for (i, &p) in AT_THREE.iter().enumerate() {
        v.push(Ev {
            ts: t(3700) + i as u64,
            sym: i as u32,
            k: K::Trade(p),
        });
    }
    clock(&mut v, 3590);
    for (i, &p) in AT_THREE.iter().enumerate() {
        v.push(Ev {
            ts: t(1900) + i as u64,
            sym: i as u32,
            k: K::Quote(p - 1, p + 1),
        });
    }
    clock(&mut v, 1790);
    // Between the entry and the exit the names that were bought move: S00 up, S01 down, S04 up.
    for (sym, p) in [(0u32, late[0]), (1, late[1]), (4, late[2])] {
        v.push(Ev {
            ts: t(100) + u64::from(sym),
            sym,
            k: K::Trade(p),
        });
        v.push(Ev {
            ts: t(90) + u64::from(sym),
            sym,
            k: K::Quote(p - 1, p + 1),
        });
    }
    clock(&mut v, 31);
    clock(&mut v, 30);
    for (sym, p) in [(0u32, late[0]), (1, late[1]), (4, late[2])] {
        v.push(Ev {
            ts: t(20) + u64::from(sym),
            sym,
            k: K::Quote(p - 1, p + 1),
        });
    }
    clock(&mut v, 5);
    v.sort_by_key(|e| e.ts);
    v
}

fn events(script: &[Ev]) -> Vec<Event> {
    script
        .iter()
        .enumerate()
        .map(|(n, e)| {
            let hdr = Header {
                ts_event: e.ts,
                ts_recv: e.ts,
                seq: n as u64,
                instrument: e.sym,
                provider: ProviderId::Synthetic,
            };
            match e.k {
                K::Trade(p) => Event::Trade(Trade {
                    hdr,
                    px: Px::from_cents(p),
                    size: 100,
                    flags: TradeFlags::NONE,
                }),
                K::Quote(b, a) => Event::Quote(Quote {
                    hdr,
                    bid_px: Px::from_cents(b),
                    ask_px: Px::from_cents(a),
                    bid_sz: 100_000,
                    ask_sz: 100_000,
                }),
            }
        })
        .collect()
}

fn params(names: u32) -> ClosingReversalParams {
    ClosingReversalParams {
        names,
        ..ClosingReversalParams::default()
    }
}

fn def(names: u32) -> StrategyDef {
    closing_reversal(1, "t04", Spec::parse(LOW).unwrap(), params(names)).unwrap()
}

fn cfg_for(times: SessionTimes) -> HostConfig {
    HostConfig {
        day: Some(times),
        ..config(1)
    }
}

/// Run an afternoon through a host with the strategy: the host, what it noted, and the events.
fn play(
    day: SessionTimes,
    names: u32,
    late: [i64; 3],
) -> (crate::Host<tf_ledger::MemStore>, Vec<FillNote>, Vec<Event>) {
    let cfg = cfg_for(day);
    let evs = events(&afternoon(day.close, late));
    let mut h = host(&cfg).record().with_fill_log();
    h.install_for_test(&def(names));
    for e in &evs {
        h.on_event(e).unwrap();
    }
    h.end_of_day(evs.last().unwrap().ts_recv()).unwrap();
    let notes = h.take_fill_notes();
    (h, notes, evs)
}

#[test]
fn the_three_most_negative_are_bought_at_the_ask_at_half_past_three_and_sold_at_the_bid_at_half_a_minute_to_four()
 {
    let day = times(2026, 5, 1);
    let (h, notes, _) = play(day, 3, [1830, 1890, 1930]);
    assert_eq!(notes.len(), 6, "{notes:#?}");
    let (buys, sells) = notes.split_at(3);
    // Names 0, 1 and 4 (S00, S01, S04): -10%, -5%, -4%. A cent over the quote's mid, $2,000 a name, whole shares.
    for (n, (inst, ask, qty)) in buys
        .iter()
        .zip([(0, 1801, 111), (1, 1901, 105), (4, 1921, 104)])
    {
        assert_eq!(
            (n.strategy, n.instrument, n.side, n.purpose),
            (1, inst, Side::Buy, Purpose::Open)
        );
        assert_eq!((n.px, n.qty), (ask * 10_000_000, qty));
        assert_eq!(n.ts, day.close - 1800 * SEC);
        assert_eq!(n.reason, tf_strategy::closing_reversal::REASON_ENTRY);
        // Carries the framework's disaster stop, ten percent under the ask.
        assert_eq!(n.stop, Some(ask * 10_000_000 * 900 / 1000));
    }
    // Sold at the bid after 15:59:30: a cent under the late trade each of them had, the same shares.
    for (n, (inst, bid, qty)) in sells
        .iter()
        .zip([(0, 1829, 111), (1, 1889, 105), (4, 1929, 104)])
    {
        assert_eq!(
            (n.instrument, n.side, n.purpose),
            (inst, Side::Sell, Purpose::Close)
        );
        assert_eq!((n.px, n.qty), (bid * 10_000_000, qty));
        assert_eq!(n.ts, day.close - 30 * SEC);
        assert_eq!((n.reason, n.stop), (REASON_TIME, None));
    }
    // Nothing refused, nothing odd, and nothing is held overnight.
    let s = h.stats_of(1).unwrap();
    assert_eq!(
        (s.accepted, s.rejected_by_gateway, s.refused_by_broker),
        (6, 0, 0)
    );
    assert!(h.anomalies().is_empty(), "{:?}", h.anomalies());
}

#[test]
fn on_an_early_close_day_the_entry_and_the_exit_move_with_the_close() {
    // 13:00 close: buys at 12:30 and sells at 12:59:30.
    let day = times(2026, 11, 27);
    assert_eq!(day.close % (86_400 * SEC), 18 * 3600 * SEC);
    let (_, notes, _) = play(day, 3, [1830, 1890, 1930]);
    assert_eq!(notes.len(), 6);
    assert!(notes[..3].iter().all(|n| n.ts == day.close - 1800 * SEC));
    assert!(notes[3..].iter().all(|n| n.ts == day.close - 30 * SEC));
    assert_eq!(notes[0].ts % (86_400 * SEC), 17 * 3600 * SEC + 1800 * SEC);
}

#[test]
fn a_host_that_was_not_told_the_day_trades_nothing() {
    let day = times(2026, 5, 1);
    let cfg = HostConfig {
        day: None,
        ..config(1)
    };
    let evs = events(&afternoon(day.close, [1830, 1890, 1930]));
    let mut h = host(&cfg).record().with_fill_log();
    h.install_for_test(&def(3));
    for e in &evs {
        h.on_event(e).unwrap();
    }
    assert!(h.take_fill_notes().is_empty());
    assert_eq!(h.stats_of(1).unwrap().accepted, 0);
}

#[test]
fn the_day_replays_to_the_same_decisions_and_another_afternoon_does_not() {
    let day = times(2026, 5, 1);
    let (h, _, evs) = play(day, 3, [1830, 1890, 1930]);
    let log = h.log().unwrap();
    let cfg = cfg_for(day);
    let r = replay_events(log, &cfg, &reference(), &[def(3)], &evs).unwrap();
    assert_eq!(
        compare(log, &r.log, &reference().symbols),
        Verdict::Equal {
            records: log.recs.len()
        }
    );
    assert!(log.recs.len() > 6);
    // The same afternoon with S00 not falling as far: another ranking, another decision, and the check says so.
    let mut other = afternoon(day.close, [1830, 1890, 1930]);
    for e in &mut other {
        if e.sym == 0 && e.ts < day.close - 3000 * SEC {
            if let K::Trade(p) = &mut e.k {
                *p = 1990;
            }
            if let K::Quote(b, a) = &mut e.k {
                *b += 190;
                *a += 190;
            }
        }
    }
    let r = replay_events(log, &cfg, &reference(), &[def(3)], &events(&other)).unwrap();
    assert!(matches!(
        compare(log, &r.log, &reference().symbols),
        Verdict::Differs(_)
    ));
}

/// A DBN stream of a script: a symbol mapping for each symbol (raw id 20,000 + i), then the records in time order.
fn dbn_afternoon(script: &[Ev]) -> Vec<u8> {
    let base = script[0].ts;
    let md = MetadataBuilder::new()
        .dataset("XNAS.BASIC".to_owned())
        .schema(None)
        .start(base)
        .stype_in(None)
        .stype_out(SType::InstrumentId)
        .build();
    let mut bytes = Vec::new();
    {
        let mut e = DbnEncoder::new(&mut bytes, &md).unwrap();
        for i in 0..SYMBOLS {
            let m = SymbolMappingMsg::new(
                20_000 + i,
                base - 1,
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
    }
    for (n, ev) in script.iter().enumerate() {
        let raw = 20_000 + ev.sym;
        match ev.k {
            K::Trade(p) => bytes.extend(dbn_bytes(&trade_rec(
                raw,
                ev.ts,
                p * 10_000_000,
                100,
                n as u32 + 1,
            ))),
            K::Quote(b, a) => bytes.extend(dbn_bytes(&quote_rec(
                raw,
                ev.ts,
                b * 10_000_000,
                a * 10_000_000,
            ))),
        }
    }
    bytes
}

/// The afternoon as a DBN day, run through the research runner: the files, the day's input and the outcome.
fn run_the_day(
    name: &str,
    late: [i64; 3],
) -> (
    crate::research::DayOutcome,
    Vec<std::path::PathBuf>,
    HostConfig,
    crate::research::CostModel,
) {
    use crate::replay_tests::{scratch, write_capture};
    use crate::research::{CostModel, DayInput, Setup, run_day};
    let day = times(2026, 5, 1);
    let script = afternoon(day.close, late);
    let dir = scratch(name);
    write_capture(&dir, &dbn_afternoon(&script));
    let files: Vec<std::path::PathBuf> = tf_capture::list(&dir)
        .unwrap()
        .iter()
        .map(|e| dir.join(&e.file))
        .collect();
    let (host_cfg, cost) = (config(1), CostModel::published());
    let input = DayInput {
        files: files.clone(),
        snapshot: crate::tests::snapshot(),
    };
    let out = run_day(
        "2026-05-01",
        &input,
        &Setup {
            host: &host_cfg,
            cost: &cost,
            defs: &[def(3)],
        },
    )
    .unwrap();
    (out, files, host_cfg, cost)
}

#[test]
fn run_over_a_day_of_data_it_leaves_a_record_of_each_trade_with_the_costs_by_hand() {
    let (out, _, _, _) = run_the_day("t04-trips", [1830, 1890, 1930]);
    let day = times(2026, 5, 1);
    assert_eq!((out.rejected, out.refused, out.anomalies.len()), (0, 0, 0));
    assert_eq!(out.trips.len(), 3);
    // S00, S01, S04: shares, the ask paid, the bid received, gross by hand: 111 x 0.28, 105 x -0.12, 104 x 0.08.
    for (t, (sym, qty, entry, exit, gross)) in out.trips.iter().zip([
        (
            "S00",
            111,
            18_010_000_000i64,
            18_290_000_000i64,
            31_080_000_000i64,
        ),
        ("S01", 105, 19_010_000_000, 18_890_000_000, -12_600_000_000),
        ("S04", 104, 19_210_000_000, 19_290_000_000, 8_320_000_000),
    ]) {
        assert_eq!((t.symbol.as_str(), t.qty, t.long), (sym, qty, true));
        assert_eq!((t.entry_px, t.exit_px, t.gross), (entry, exit, gross));
        // In at 15:30 and out at 15:59:30, each after the cost model's 50 ms.
        assert_eq!(t.entry_ts, day.close - 1800 * SEC + 50_000_000);
        assert_eq!(t.exit_ts, day.close - 30 * SEC + 50_000_000);
        assert_eq!(
            (t.entry_reason, t.exit_reason, t.open_at_end),
            (1, REASON_TIME, false)
        );
        assert_eq!(t.borrow, 0);
    }
    // The first trade's fees: Section 31 at $20.60 a million on $2,030.19 sold, and 111 shares at $0.000195.
    assert_eq!(out.trips[0].fees, 41_821_914 + 21_645_000);
    assert_eq!(out.trips[0].net, 31_080_000_000 - 41_821_914 - 21_645_000);
    // R is against the disaster stop, ten percent under the ask: 31.0165 over 111 x 1.801.
    assert_eq!(out.trips[0].r_milli, Some(155));
    // The day's sum, in dollars: 26.80 gross.
    assert_eq!(
        out.trips.iter().map(|t| t.gross).sum::<i64>(),
        26_800_000_000
    );
}

#[test]
fn the_day_run_for_research_replays_to_the_same_decisions_through_the_live_check() {
    let (out, files, host_cfg, cost) = run_the_day("t04-replay", [1830, 1890, 1930]);
    let day = times(2026, 5, 1);
    let cfg = HostConfig {
        day: Some(day),
        sim: cost.sim(),
        ..host_cfg
    };
    let rep = crate::replay_files(
        &files,
        &out.log,
        &cfg,
        crate::tests::snapshot(),
        &[def(3)],
        0,
    )
    .unwrap();
    assert!(rep.verdict.is_equal(), "{}", rep.text());
    assert_eq!(rep.events, out.events);
}
