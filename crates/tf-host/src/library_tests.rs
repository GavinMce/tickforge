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

// ---- the null strategy (T14) ----

use std::path::PathBuf;

use crate::research::null::{null_defs, null_distribution};
use crate::research::{CostModel, DayInput, DaySource, Results, Setup, run, run_day};
use tf_strategy::RandomEntriesParams;

/// A dense afternoon: from 61 minutes before the close to five seconds before it, every ten seconds each member S00 to S05
/// quotes a cent either side of its mid and trades at it. `mid(sym, secs_before_close)` is in cents.
fn steady(close: Nanos, mid: impl Fn(u32, u64) -> i64) -> Vec<Ev> {
    let mut v = Vec::new();
    let mut s = 3660u64;
    loop {
        for sym in 0..6u32 {
            let ts = close - s * SEC + u64::from(sym);
            let m = mid(sym, s);
            v.push(Ev {
                ts,
                sym,
                k: K::Quote(m - 1, m + 1),
            });
            v.push(Ev {
                ts: ts + 1_000,
                sym,
                k: K::Trade(m),
            });
        }
        if s <= 5 {
            break;
        }
        s = if s > 10 { s - 10 } else { 5 };
    }
    v.sort_by_key(|e| e.ts);
    v
}

/// Three trading days of the same afternoon, as DBN files.
struct Afternoons {
    days: Vec<(String, Vec<PathBuf>)>,
}

fn afternoons(name: &str, mid: impl Fn(u32, u64) -> i64 + Copy) -> Afternoons {
    use crate::replay_tests::scratch;
    let root = scratch(name);
    let mut days = Vec::new();
    for (y, m, d) in [(2026, 5, 1), (2026, 5, 4), (2026, 5, 5)] {
        let close = times(y, m, d).close;
        let dir = root.join(format!("{y}-{m:02}-{d:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        // One file a day: the capture writer's segments of three seconds would be a thousand files for an hour.
        let bytes = dbn_afternoon(&steady(close, mid));
        let file = dir.join("day.dbn.zst");
        std::fs::write(&file, zstd::encode_all(&bytes[..], 0).unwrap()).unwrap();
        days.push((format!("{y}-{m:02}-{d:02}"), vec![file]));
    }
    Afternoons { days }
}

impl DaySource for Afternoons {
    fn dates(&self) -> Vec<String> {
        self.days.iter().map(|d| d.0.clone()).collect()
    }

    fn data_id(&self, date: &str) -> Result<String, String> {
        let files = &self
            .days
            .iter()
            .find(|d| d.0 == date)
            .ok_or("no such day")?
            .1;
        let bytes: u64 = files
            .iter()
            .map(|f| std::fs::metadata(f).map_or(0, |m| m.len()))
            .sum();
        Ok(format!("{}-{bytes}", files.len()))
    }

    fn load(&mut self, date: &str) -> Result<DayInput, String> {
        let files = self
            .days
            .iter()
            .find(|d| d.0 == date)
            .ok_or("no such day")?
            .1
            .clone();
        Ok(DayInput {
            files,
            snapshot: crate::tests::snapshot(),
        })
    }
}

fn null_base() -> RandomEntriesParams {
    RandomEntriesParams {
        names: 3,
        ..RandomEntriesParams::default()
    }
}

/// Run the null strategy with these seeds over three days of `src`: the results and the definitions' fingerprints.
fn run_nulls(
    out: &str,
    src: &mut Afternoons,
    seeds: &[u64],
    base: RandomEntriesParams,
) -> (Results, Vec<u64>) {
    use crate::replay_tests::scratch;
    let defs = null_defs(1, "null", &Spec::parse(LOW).unwrap(), base, seeds).unwrap();
    let fps: Vec<u64> = defs.iter().map(StrategyDef::fingerprint).collect();
    let (host_cfg, cost) = (config(seeds.len() as u32), CostModel::published());
    let dir = scratch(out);
    run(
        &Setup {
            host: &host_cfg,
            cost: &cost,
            defs: &defs,
        },
        src,
        &dir,
    )
    .unwrap();
    (Results::open(&dir).unwrap(), fps)
}

const SEEDS: [u64; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

#[test]
fn in_a_market_with_nothing_in_it_the_null_pays_the_spread_and_the_fees_and_nothing_else() {
    let mut src = afternoons("null-flat", |_, _| 2000);
    let (results, fps) = run_nulls("null-flat-out", &mut src, &SEEDS, null_base());
    let dist = null_distribution(&results, &fps).unwrap().unwrap();
    // Eight seeds, each three names a day for three days.
    assert_eq!((dist.seeds, dist.traded, dist.trades), (8, 8, 72));
    // Every trade buys at the ask 20.01 and sells at the bid 19.99, 99 shares: -1.98 gross, and Section 31 at $20.60 a
    // million on $1,979.01 and 99 shares at $0.000195 in fees, -2.040072606 net, over $1,980.99 put in: -10.29 basis points
    // (the figure by hand: -1,029 hundredths).
    for t in results.trips().unwrap() {
        assert_eq!(
            (t.qty, t.entry_px, t.exit_px),
            (99, 20_010_000_000, 19_990_000_000)
        );
        assert_eq!(
            (t.gross, t.fees, t.net, t.net_bps_x100),
            (-1_980_000_000, 60_072_606, -2_040_072_606, -1029)
        );
    }
    assert!(dist.means_bp.iter().all(|&m| m == -10.29));
    assert_eq!(
        (dist.mean_of_means_bp, dist.pooled_mean_bp),
        (-10.29, -10.29)
    );
    assert_eq!(dist.sd_of_means_bp, Some(0.0));
    // A result is held against it: better than every seed has the least p-value eight seeds allow; worse than all, one.
    let better = dist.against(-5.0).unwrap();
    assert_eq!((better.runs, better.at_or_above), (8, 0));
    assert!((better.p_value - 1.0 / 9.0).abs() < 1e-15);
    assert_eq!(dist.against(-20.0).unwrap().p_value, 1.0);
}

/// A market with a drift after 15:30: S00 falls and S05 rises, by 29.5 cents for each step of the name's number from S02.
fn drifting(sym: u32, secs: u64) -> i64 {
    2000 + (i64::from(sym) - 2) * (1800 - secs.min(1800)) as i64 / 60
}

#[test]
fn the_null_in_a_market_that_moves_varies_with_the_seed_and_the_days_it_drew_and_repeats_with_the_same_seed()
 {
    let mut src = afternoons("null-drift", drifting);
    let (a, fps) = run_nulls("null-drift-a", &mut src, &SEEDS, null_base());
    let dist = null_distribution(&a, &fps).unwrap().unwrap();
    assert_eq!((dist.seeds, dist.traded, dist.trades), (8, 8, 72));
    // The seeds drew different names, so their means differ.
    assert!(dist.sd_of_means_bp.unwrap() > 1.0, "{dist:?}");
    let mut distinct = dist.means_bp.clone();
    distinct.sort_by(f64::total_cmp);
    distinct.dedup();
    assert!(distinct.len() >= 4, "{:?}", dist.means_bp);
    // Every null trade is at 15:30 and 15:59:30 (each after the cost model's 50 ms), on three distinct names a day.
    let trips = a.trips().unwrap();
    for t in &trips {
        let close = times(
            2026,
            t.day[5..7].parse().unwrap(),
            t.day[8..10].parse().unwrap(),
        )
        .close;
        assert_eq!(t.entry_ts, close - 1800 * SEC + 50_000_000);
        assert_eq!(t.exit_ts, close - 30 * SEC + 50_000_000);
    }
    for (fp, name) in fps.iter().zip(SEEDS.iter()) {
        for day in a.dates().unwrap() {
            let mut syms: Vec<&str> = trips
                .iter()
                .filter(|t| t.variant == *fp && t.day == day)
                .map(|t| t.symbol.as_str())
                .collect();
            assert_eq!(syms.len(), 3, "seed {name} on {day}");
            syms.sort();
            syms.dedup();
            assert_eq!(syms.len(), 3, "seed {name} on {day}: a name twice");
        }
    }
    // The pooled mean is every null trade counted once, found here from the trips themselves.
    let all: f64 = trips
        .iter()
        .map(|t| t.net_bps_x100 as f64 / 100.0)
        .sum::<f64>()
        / trips.len() as f64;
    assert!((dist.pooled_mean_bp - all).abs() < 1e-9);
    // Run again with the same seeds: the same trips, the same distribution. Other seeds: another.
    let (b, fps_b) = run_nulls("null-drift-b", &mut src, &SEEDS, null_base());
    assert_eq!(fps, fps_b);
    assert_eq!(a.trips().unwrap(), b.trips().unwrap());
    assert_eq!(null_distribution(&b, &fps_b).unwrap().unwrap(), dist);
    let other = [101u64, 102, 103, 104, 105, 106, 107, 108];
    let (c, fps_c) = run_nulls("null-drift-c", &mut src, &other, null_base());
    assert_ne!(
        null_distribution(&c, &fps_c).unwrap().unwrap().means_bp,
        dist.means_bp
    );
}

#[test]
fn the_null_of_a_run_reads_the_seeds_it_is_asked_for_and_nothing_else() {
    let mut src = afternoons("null-ask", |_, _| 2000);
    let (results, fps) = run_nulls("null-ask-out", &mut src, &[1, 2, 3], null_base());
    // Two of the three: their distribution only.
    let two = null_distribution(&results, &fps[..2]).unwrap().unwrap();
    assert_eq!((two.seeds, two.trades), (2, 18));
    // A fingerprint the run does not have is refused, not skipped.
    assert!(null_distribution(&results, &[fps[0], 12345]).is_err());
    // The seeds are not in the trial registry, and asking for the distribution did not put them there.
    let reg = tf_stats::Registry::new();
    assert!(
        crate::research::stats::report_results(&results, &reg, tf_stats::Bootstrap::default())
            .is_err()
    );
    // Nobody traded (a dollar buys no share): no distribution, not a distribution of nothing.
    let none = RandomEntriesParams {
        dollars: 1,
        ..null_base()
    };
    let (results, fps) = run_nulls("null-none-out", &mut src, &[1, 2], none);
    assert_eq!(null_distribution(&results, &fps).unwrap(), None);
}

#[test]
fn a_definition_for_each_seed_with_its_own_number_name_and_fingerprint() {
    let u = Spec::parse(LOW).unwrap();
    let defs = null_defs(10, "n", &u, null_base(), &[5, 9, 700]).unwrap();
    assert_eq!(defs.iter().map(|d| d.id).collect::<Vec<_>>(), [10, 11, 12]);
    assert_eq!(
        defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        ["n5", "n9", "n700"]
    );
    assert!(defs[1].params.starts_with("seed=9 names=3 "));
    let mut fps: Vec<u64> = defs.iter().map(StrategyDef::fingerprint).collect();
    fps.sort();
    fps.dedup();
    assert_eq!(fps.len(), 3);
    // Only the seed (and the number and name) differ.
    assert_eq!(
        defs[0].params.split_once(' ').unwrap().1,
        defs[1].params.split_once(' ').unwrap().1
    );
    assert!(null_defs(10, "n", &u, null_base(), &[5, 9, 5]).is_err());
    assert!(null_defs(65_535, "n", &u, null_base(), &[1, 2]).is_err());
    assert!(
        null_defs(
            1,
            "n",
            &u,
            RandomEntriesParams {
                names: 0,
                ..null_base()
            },
            &[1]
        )
        .is_err()
    );
    assert!(null_defs(1, "n", &u, null_base(), &[]).unwrap().is_empty());
}

#[test]
fn the_null_replays_to_the_same_decisions_through_the_live_check() {
    let mut src = afternoons("null-replay", drifting);
    let defs = null_defs(1, "null", &Spec::parse(LOW).unwrap(), null_base(), &[3]).unwrap();
    let (host_cfg, cost) = (config(1), CostModel::published());
    let input = src.load("2026-05-01").unwrap();
    let out = run_day(
        "2026-05-01",
        &input,
        &Setup {
            host: &host_cfg,
            cost: &cost,
            defs: &defs,
        },
    )
    .unwrap();
    assert_eq!(out.trips.len(), 3);
    let cfg = HostConfig {
        day: Some(times(2026, 5, 1)),
        sim: cost.sim(),
        ..host_cfg
    };
    let rep = crate::replay_files(
        &input.files,
        &out.log,
        &cfg,
        crate::tests::snapshot(),
        &defs,
        0,
    )
    .unwrap();
    assert!(rep.verdict.is_equal(), "{}", rep.text());
}

#[test]
fn the_spread_of_the_seeds_is_the_sample_standard_deviation_and_one_seed_has_none() {
    let mut src = afternoons("null-sd", drifting);
    let (results, fps) = run_nulls("null-sd-out", &mut src, &SEEDS, null_base());
    let dist = null_distribution(&results, &fps).unwrap().unwrap();
    // Found here from the seeds' own means: the square root of the sum of squared deviations over n - 1.
    let n = dist.means_bp.len() as f64;
    let mean = dist.means_bp.iter().sum::<f64>() / n;
    let sd = (dist
        .means_bp
        .iter()
        .map(|m| (m - mean).powi(2))
        .sum::<f64>()
        / (n - 1.0))
        .sqrt();
    assert!((dist.sd_of_means_bp.unwrap() - sd).abs() < 1e-12);
    assert!((dist.mean_of_means_bp - mean).abs() < 1e-12);
    // One seed has a mean and no spread.
    let one = null_distribution(&results, &fps[..1]).unwrap().unwrap();
    assert_eq!((one.seeds, one.traded, one.sd_of_means_bp), (1, 1, None));
    assert_eq!(one.mean_of_means_bp, one.means_bp[0]);
}

// ---- the traces through the host (E19-S32) ----

#[test]
fn a_day_traced_through_the_host_has_the_same_log_and_the_trace_names_the_symbols() {
    use tf_strategy::trace::{parse_all, render_all};
    let day = times(2026, 5, 1);
    let cfg = cfg_for(day);
    let (plain, plain_notes, evs) = play(day, 3, [1830, 1890, 1930]);
    let mut h = host(&cfg).record().with_fill_log().with_traces();
    h.install_for_test(&def(3));
    for e in &evs {
        h.on_event(e).unwrap();
    }
    h.end_of_day(evs.last().unwrap().ts_recv()).unwrap();
    // The same decisions and fills.
    assert_eq!(h.take_fill_notes(), plain_notes);
    let (a, b) = (plain.log().unwrap(), h.log().unwrap());
    assert_eq!(
        compare(a, b, &reference().symbols),
        Verdict::Equal {
            records: a.recs.len()
        }
    );
    // One trace of strategy 1: the six members, ranked, with symbols and not numbers.
    let traces = h.take_traces();
    assert_eq!(traces.len(), 1);
    let (strategy, t) = &traces[0];
    assert_eq!((*strategy, t.kind.as_str()), (1, "rank"));
    assert_eq!(t.columns[1], "symbol");
    // Returns of -10%, -5%, -4%, -2%, 0 and +2% against a prior close of 20.00: S00, S01 and S04 are the three bought.
    assert_eq!(
        t.column("symbol").unwrap(),
        ["S00", "S01", "S04", "S02", "S05", "S03"]
    );
    assert_eq!(
        t.column("status").unwrap(),
        [
            "entered",
            "entered",
            "entered",
            "not_chosen",
            "not_chosen",
            "not_chosen"
        ]
    );
    assert_eq!(
        t.rows[0][2..6],
        ["20000000000", "18000000000", "tier0", "-100000"]
    );
    assert_eq!(t.ts, day.close - 1800 * SEC);
    // The text of the day's traces reads back, and is taken once.
    assert_eq!(parse_all(&render_all(&traces)).unwrap(), traces);
    assert!(h.take_traces().is_empty());
    // A host not asked has none, and one asked after the strategy was added has them too.
    assert!(plain.log().is_some());
    let mut late = host(&cfg).record();
    late.install_for_test(&def(3));
    let mut late = late.with_traces();
    for e in &evs {
        late.on_event(e).unwrap();
    }
    assert_eq!(late.take_traces().len(), 1);
    let mut quiet = host(&cfg).record();
    quiet.install_for_test(&def(3));
    for e in &evs {
        quiet.on_event(e).unwrap();
    }
    assert!(quiet.take_traces().is_empty());
}

#[test]
fn the_null_strategys_draw_and_entries_are_traced_through_the_host_with_symbols() {
    let day = times(2026, 5, 1);
    let cfg = cfg_for(day);
    let null = null_defs(1, "null", &Spec::parse(LOW).unwrap(), null_base(), &[9]).unwrap();
    let evs = events(&steady(day.close, |_, _| 2000));
    let mut h = host(&cfg).record().with_traces();
    h.install_for_test(&null[0]);
    for e in &evs {
        h.on_event(e).unwrap();
    }
    let traces = h.take_traces();
    let kinds: Vec<&str> = traces.iter().map(|t| t.1.kind.as_str()).collect();
    assert_eq!(kinds, ["draw", "entry", "entry", "entry"]);
    let draw = &traces[0].1;
    assert_eq!(draw.columns[1], "symbol");
    for s in draw.column("symbol").unwrap() {
        assert!(
            ["S00", "S01", "S02", "S03", "S04", "S05"].contains(&s),
            "{s}"
        );
    }
    for (_, t) in &traces[1..] {
        assert_eq!(t.columns[1], "symbol");
        assert_eq!(t.rows[0][2], "entered");
    }
}

// ---- what a T04 day keeps (E19-S33) ----

#[test]
fn a_t04_day_keeps_its_ranked_cross_section_and_the_market_around_its_trades() {
    use crate::replay_tests::scratch;
    use crate::research::{EvidenceWindow, RunOptions, run_with};
    let mut src = afternoons("keep-t04", drifting);
    let def3 = def(3);
    let (host_cfg, cost) = (config(1), CostModel::published());
    let dir = scratch("keep-t04-out");
    let opts = RunOptions {
        evidence: Some(EvidenceWindow::default()),
    };
    let rep = run_with(
        &Setup {
            host: &host_cfg,
            cost: &cost,
            defs: std::slice::from_ref(&def3),
        },
        &mut src,
        &dir,
        &opts,
    )
    .unwrap();
    assert_eq!((rep.ran.len(), rep.trips, rep.no_evidence.len()), (3, 9, 0));
    let results = Results::open(&dir).unwrap();
    for date in results.dates().unwrap() {
        // The day's one decision, with symbols: the three the strategy bought and the others ranked.
        let traces = results.traces(&date).unwrap();
        assert_eq!(traces.len(), 4);
        // The strategy's rank trace, the host's count of what it tried (three buys and three sells, all accepted) and the instruments.
        assert_eq!((traces[2].0, traces[2].1.kind.as_str()), (0, "instruments"));
        let mut names: Vec<&str> = traces[2].1.column("symbol").unwrap();
        names.sort();
        let mut bought: Vec<String> = results
            .day(&date)
            .unwrap()
            .trips
            .iter()
            .map(|x| x.symbol.clone())
            .collect();
        bought.sort();
        assert_eq!(names, bought.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(traces[1].1.kind, "stats");
        assert_eq!(
            (traces[1].1.value("accepted"), traces[1].1.value("rejected")),
            (Some("6"), Some("0"))
        );
        let (strategy, t) = &traces[0];
        assert_eq!((*strategy, t.kind.as_str(), t.rows.len()), (1, "rank", 6));
        assert_eq!(t.columns[1], "symbol");
        assert_eq!(t.value("entered"), Some("3"));
        let entered: Vec<&str> = t
            .column("symbol")
            .unwrap()
            .into_iter()
            .zip(t.column("status").unwrap())
            .filter(|&(_, st)| st == "entered")
            .map(|(s, _)| s)
            .collect();
        // They are the symbols of the day's trips.
        let trips = results.day(&date).unwrap().trips;
        let mut traded: Vec<&str> = trips.iter().map(|x| x.symbol.as_str()).collect();
        traded.sort();
        let mut want = entered.clone();
        want.sort();
        assert_eq!(traded, want);
        // The market around each of them was kept.
        let ev = results.evidence(&date).unwrap();
        assert_eq!(
            ev.symbols.keys().map(String::as_str).collect::<Vec<_>>(),
            want
        );
        for tr in &trips {
            let around = ev.slice(&tr.symbol, tr.entry_ts, tr.exit_ts);
            assert!(
                !around.is_empty(),
                "{} has nothing from entry to exit",
                tr.symbol
            );
        }
    }
}

// ---- one trade replayed (E19-S35) ----

mod trade_view {
    use super::*;
    use crate::research::view::{ViewError, trade_page};
    use crate::research::{EvidenceWindow, RunOptions, run_with};

    /// Minimal strict JSON reader for what the page embeds (text, numbers, arrays, objects).
    #[derive(Debug, Clone, PartialEq)]
    pub enum J {
        Null,
        Bool(bool),
        Num(String),
        Str(String),
        Arr(Vec<J>),
        Obj(Vec<(String, J)>),
    }

    impl J {
        pub fn get(&self, k: &str) -> &J {
            match self {
                J::Obj(v) => v
                    .iter()
                    .find(|(x, _)| x == k)
                    .map(|(_, v)| v)
                    .unwrap_or_else(|| panic!("no `{k}`")),
                _ => panic!("not an object"),
            }
        }
        pub fn s(&self) -> &str {
            match self {
                J::Str(s) | J::Num(s) => s,
                o => panic!("not text: {o:?}"),
            }
        }
        pub fn a(&self) -> &[J] {
            match self {
                J::Arr(v) => v,
                o => panic!("not an array: {o:?}"),
            }
        }
    }

    pub fn parse(text: &str) -> J {
        fn ws(b: &[u8], i: &mut usize) {
            while *i < b.len() && b[*i].is_ascii_whitespace() {
                *i += 1;
            }
        }
        fn string(b: &[u8], i: &mut usize) -> String {
            assert_eq!(b[*i], b'"');
            *i += 1;
            let mut out = Vec::new();
            while b[*i] != b'"' {
                if b[*i] == b'\\' {
                    *i += 1;
                    match b[*i] {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'n' => out.push(b'\n'),
                        b'u' => {
                            let h = std::str::from_utf8(&b[*i + 1..*i + 5]).unwrap();
                            let c = char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap();
                            out.extend_from_slice(c.to_string().as_bytes());
                            *i += 4;
                        }
                        o => panic!("escape {}", o as char),
                    }
                } else {
                    out.push(b[*i]);
                }
                *i += 1;
            }
            *i += 1;
            String::from_utf8(out).unwrap()
        }
        fn value(b: &[u8], i: &mut usize) -> J {
            ws(b, i);
            match b[*i] {
                b'{' => {
                    *i += 1;
                    let mut v = Vec::new();
                    ws(b, i);
                    if b[*i] == b'}' {
                        *i += 1;
                        return J::Obj(v);
                    }
                    loop {
                        ws(b, i);
                        let k = string(b, i);
                        ws(b, i);
                        assert_eq!(b[*i], b':');
                        *i += 1;
                        v.push((k, value(b, i)));
                        ws(b, i);
                        match b[*i] {
                            b',' => *i += 1,
                            b'}' => {
                                *i += 1;
                                return J::Obj(v);
                            }
                            c => panic!("{}", c as char),
                        }
                    }
                }
                b'[' => {
                    *i += 1;
                    let mut v = Vec::new();
                    ws(b, i);
                    if b[*i] == b']' {
                        *i += 1;
                        return J::Arr(v);
                    }
                    loop {
                        v.push(value(b, i));
                        ws(b, i);
                        match b[*i] {
                            b',' => *i += 1,
                            b']' => {
                                *i += 1;
                                return J::Arr(v);
                            }
                            c => panic!("{}", c as char),
                        }
                    }
                }
                b'"' => J::Str(string(b, i)),
                b't' => {
                    *i += 4;
                    J::Bool(true)
                }
                b'f' => {
                    *i += 5;
                    J::Bool(false)
                }
                b'n' => {
                    *i += 4;
                    J::Null
                }
                _ => {
                    let s = *i;
                    while *i < b.len()
                        && (b[*i].is_ascii_digit()
                            || matches!(b[*i], b'-' | b'.' | b'e' | b'E' | b'+'))
                    {
                        *i += 1;
                    }
                    assert!(*i > s, "not a value at {s}");
                    J::Num(String::from_utf8_lossy(&b[s..*i]).into_owned())
                }
            }
        }
        let b = text.as_bytes();
        let mut i = 0;
        let v = value(b, &mut i);
        ws(b, &mut i);
        assert_eq!(i, b.len());
        v
    }

    /// The T04 scenario `s` (three days, evidence kept) under a fresh root.
    pub fn root(name: &str) -> std::path::PathBuf {
        let mut src = afternoons(&format!("{name}-days"), drifting);
        let def3 = def(3);
        let (host_cfg, cost) = (config(1), CostModel::published());
        let root = crate::replay_tests::scratch(&format!("{name}-root"));
        run_with(
            &Setup {
                host: &host_cfg,
                cost: &cost,
                defs: std::slice::from_ref(&def3),
            },
            &mut src,
            &root.join("s"),
            &RunOptions {
                evidence: Some(EvidenceWindow::default()),
            },
        )
        .unwrap();
        root
    }

    /// "20.0100" as 200100.
    fn px(s: &str) -> i64 {
        s.replace('.', "").parse().unwrap()
    }

    pub fn data(root: &std::path::Path, day: &str, n: usize) -> J {
        let page = trade_page(root, "s", day, 1, n).unwrap();
        let open = "<script id=\"data\" type=\"application/json\">";
        let body = &page[page.find(open).unwrap() + open.len()..];
        parse(&body[..body.find("</script>").unwrap()])
    }

    #[test]
    fn the_chart_draws_each_fill_at_its_price_and_says_the_row_of_markers_is_for_the_time() {
        // The markers (decision, fill, exit) are a row at the top of the chart: their height is not a price. The fills are drawn
        // again at the price they were made at, on the quotes' axis, and the price range takes them in; the legend says which is which.
        let root = root("tp-fills");
        let page = trade_page(&root, "s", "2026-05-04", 1, 0).unwrap();
        assert!(
            page.contains("circle\", { cx: X(o.us), cy: Y(o.px4)"),
            "a dot at the fill's price"
        );
        assert!(
            page.contains("lo = Math.min(lo, o.px4)"),
            "the axis reaches every fill"
        );
        assert!(page.contains("each fill at its price (the row of dots at the top marks the time"));
        // What the dot says when pointed at, and that the data it is made from holds the price of each fill.
        assert!(page.contains("entry\" : \"exit\") + \" fill: \""));
        let j = data(&root, "2026-05-04", 0);
        assert!(
            j.get("orders")
                .a()
                .iter()
                .any(|o| o.get("kind").s() == "fill" && o.get("px4").s() != "0")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_trade_is_shown_with_its_orders_its_markers_its_costs_and_the_strategys_account() {
        let root = root("tp1");
        let j = data(&root, "2026-05-04", 0);
        assert_eq!(
            (
                j.get("scenario").s(),
                j.get("day").s(),
                j.get("n").s(),
                j.get("of").s()
            ),
            ("s", "2026-05-04", "0", "3")
        );
        assert_eq!(
            (
                j.get("strategy").get("name").s(),
                j.get("side").s(),
                j.get("qty").s()
            ),
            ("t04", "long", "99")
        );
        assert_eq!(j.get("open_at_end"), &J::Bool(false));
        assert!(j.get("notes").a().is_empty(), "{:?}", j.get("notes"));
        // The orders: the entry decision, its fill 50 ms later, the exit decision and its fill 50 ms later.
        let kinds: Vec<&str> = j
            .get("orders")
            .a()
            .iter()
            .map(|o| o.get("kind").s())
            .collect();
        assert_eq!(kinds, ["decision", "fill", "decision", "fill"]);
        let o = j.get("orders").a();
        assert_eq!(
            (
                o[0].get("side").s(),
                o[0].get("purpose").s(),
                o[0].get("answer").s()
            ),
            ("buy", "open", "accepted")
        );
        assert_eq!(
            (
                o[2].get("side").s(),
                o[2].get("purpose").s(),
                o[2].get("reason").s()
            ),
            ("sell", "close", "time exit")
        );
        assert_eq!(
            (o[1].get("latency_ms").s(), o[3].get("latency_ms").s()),
            ("50", "50")
        );
        assert_eq!(
            (o[1].get("px").s(), o[3].get("px").s()),
            (j.get("entry_px").s(), j.get("exit_px").s())
        );
        assert_eq!(o[1].get("order").s(), o[0].get("order").s());
        // Microseconds from the entry's second: the decision at 15:30:00.000 and the fill 50 ms on.
        assert_eq!((o[0].get("us").s(), o[1].get("us").s()), ("0", "50000"));
        let marks: Vec<(&str, &str)> = j
            .get("marks")
            .a()
            .iter()
            .map(|m| (m.get("kind").s(), m.get("us").s()))
            .collect();
        assert_eq!(marks[0], ("decision", "0"));
        assert_eq!(marks[1], ("fill", "50000"));
        assert_eq!(
            marks.iter().map(|m| m.0).collect::<Vec<_>>(),
            ["decision", "fill", "exit_decision", "exit_fill"]
        );
        let labels: Vec<&str> = j
            .get("marks")
            .a()
            .iter()
            .map(|m| m.get("label").s())
            .collect();
        assert_eq!(labels[1], "Filled 99 at 20.0100, 50 ms after the decision");
        assert_eq!(
            labels[3],
            "Exit filled 99 at 19.4000, 50 ms after the decision"
        );
        // The fees split into Section 31 and the Trading Activity Fee, and add up to what the trade paid; gross less fees is net.
        let m = j.get("money");
        let (gross, sec, taf, fees, net) = (
            px(m.get("gross").s()),
            px(m.get("sec").s()),
            px(m.get("taf").s()),
            px(m.get("fees").s()),
            px(m.get("net").s()),
        );
        assert!(sec > 0 && taf > 0, "{sec} {taf}");
        assert_eq!(sec + taf, fees);
        assert_eq!(gross - fees - px(m.get("borrow").s()), net);
        // The trade of the day's file: the same numbers as the trips.
        let trips = Results::open(&root.join("s"))
            .unwrap()
            .day("2026-05-04")
            .unwrap()
            .trips;
        assert_eq!(
            m.get("net_cents").s(),
            crate::research::view::dollars_for_tests(i128::from(trips[0].net))
        );
        // The market was kept: quotes and trades from before the entry to after the exit.
        let mk = j.get("market");
        assert!(mk.get("quotes").a().len() > 10 && !mk.get("trades").a().is_empty());
        assert!(mk.get("from_us").s().parse::<i64>().unwrap() < 0);
        // The span is the first and the last event kept (nothing was thinned here).
        let firsts = [
            mk.get("quotes").a()[0].a()[0].s(),
            mk.get("trades").a()[0].a()[0].s(),
        ];
        let first = firsts
            .iter()
            .map(|x| x.parse::<i64>().unwrap())
            .min()
            .unwrap();
        assert_eq!(mk.get("from_us").s().parse::<i64>().unwrap(), first);
        let (lq, lt) = (
            mk.get("quotes").a().last().unwrap(),
            mk.get("trades").a().last().unwrap(),
        );
        let last = lq.a()[0]
            .s()
            .parse::<i64>()
            .unwrap()
            .max(lt.a()[0].s().parse::<i64>().unwrap());
        assert_eq!(mk.get("to_us").s().parse::<i64>().unwrap(), last);
        assert!(
            mk.get("to_us").s().parse::<i64>().unwrap()
                > j.get("exit_us").s().parse::<i64>().unwrap()
        );
        // The strategy as configured, and what it asked for at each execution.
        let def = j.get("def");
        assert_eq!(def.get("name").s(), "t04");
        assert!(
            def.get("params").s().contains("stop_permille=100")
                && def.get("params").s().contains("names=3")
        );
        assert!(def.get("universe").s().contains("adv_shares <= 600"));
        let legs = j.get("legs").a();
        assert_eq!(legs.len(), 2);
        let (e, x) = (&legs[0], &legs[1]);
        assert_eq!(
            (
                e.get("purpose").s(),
                e.get("side").s(),
                e.get("qty").s(),
                e.get("time").s()
            ),
            ("open", "buy", "99", "15:30:00.050")
        );
        assert_eq!(
            (e.get("reference").s(), e.get("px").s(), e.get("slip").s()),
            ("20.0100", "20.0100", "0.0000")
        );
        assert_eq!(
            (e.get("stop").s(), e.get("stop_pct").s()),
            ("18.0090", "10.0")
        );
        assert_eq!(
            (x.get("purpose").s(), x.get("side").s(), x.get("reason").s()),
            ("close", "sell", "time exit")
        );
        assert_eq!(
            (
                x.get("reference").s(),
                x.get("px").s(),
                x.get("slip").s(),
                x.get("slip_bp").s()
            ),
            ("19.4200", "19.4000", "0.0200", "10.29")
        );
        assert_eq!(x.get("stop"), &J::Null);
        let listed = j.get("listed");
        assert_eq!(
            (
                listed.get("kind").s(),
                listed.get("row").s(),
                listed.get("of").s(),
                listed.get("status").s()
            ),
            ("rank", "1", "6", "entered")
        );
        // The engine promoted nobody on this day (T04 decides on Tier 0): the page says that, with no changes listed.
        let tiers = j.get("tiers");
        assert_eq!(tiers.get("day_events").s(), "0");
        assert!(
            tiers.get("around").a().is_empty()
                && tiers.get("start").a().is_empty()
                && tiers.get("mine").a().is_empty()
        );
        // Why: the rank, with the symbol's row marked, the prices as dollars and the close as a time.
        let ev = &j.get("evidence").a()[0];
        assert_eq!(ev.get("kind").s(), "rank");
        let head: Vec<(&str, &str)> = ev
            .get("head")
            .a()
            .iter()
            .map(|h| (h.a()[0].s(), h.a()[1].s()))
            .collect();
        assert!(head.contains(&("close", "16:00:00.000")), "{head:?}");
        assert!(head.contains(&("entered", "3")));
        let cols: Vec<&str> = ev.get("columns").a().iter().map(J::s).collect();
        assert_eq!(&cols[..4], ["rank", "symbol", "prior_close", "ref_px"]);
        let rows = ev.get("rows").a();
        assert_eq!(rows.len(), 6);
        let marked: Vec<&str> = rows
            .iter()
            .filter(|r| r.a()[1].s() == "1")
            .map(|r| r.a()[2].a()[1].s())
            .collect();
        assert_eq!(marked, [j.get("symbol").s()]);
        assert_eq!(rows[0].a()[2].a()[2].s(), "20.0000");
    }

    #[test]
    fn each_trade_of_the_day_is_its_own_page_and_one_that_is_not_is_not_found() {
        let root = root("tp2");
        let a = data(&root, "2026-05-04", 0);
        let c = data(&root, "2026-05-04", 2);
        assert_ne!(a.get("symbol").s(), c.get("symbol").s());
        assert_eq!(c.get("n").s(), "2");
        assert!(
            matches!(trade_page(&root, "s", "2026-05-04", 1, 3), Err(ViewError::NotFound(m)) if m.contains("3 trades") && m.contains("no trade 4"))
        );
        assert!(matches!(
            trade_page(&root, "s", "2026-05-04", 9, 0),
            Err(ViewError::NotFound(_))
        ));
        assert!(matches!(
            trade_page(&root, "s", "2026-05-02", 1, 0),
            Err(ViewError::NotFound(_))
        ));
        assert!(matches!(
            trade_page(&root, "..", "2026-05-04", 1, 0),
            Err(ViewError::NotFound(_))
        ));
        // The page is whole: the data is in the place for it, once, and the mark is gone.
        let page = trade_page(&root, "s", "2026-05-04", 1, 0).unwrap();
        assert!(!page.contains("TRADE_DATA") && page.matches("id=\"data\"").count() == 1);
        // It asks nobody for anything.
        // (the SVG namespace is a name, not an address anything is fetched from)
        let page = page.replace("http://www.w3.org/2000/svg", "");
        for outside in [
            "https://",
            "http://",
            "//cdn",
            "@import",
            "src=\"http",
            "fetch(",
            "XMLHttpRequest",
            "innerHTML",
        ] {
            assert!(!page.contains(outside), "{outside}");
        }
        // And what the data holds cannot end the script early.
        let body = &page[page.find("type=\"application/json\">").unwrap()..];
        assert!(!body[..body.find("</script>").unwrap()].contains('<'));
    }

    #[test]
    fn what_is_missing_is_said_and_the_rest_is_shown() {
        let root = root("tp3");
        let day = "2026-05-04";
        // Without the market kept: no chart and a note, the orders and costs still there.
        std::fs::remove_file(root.join("s").join(format!("{day}.evidence.zst"))).unwrap();
        let j = data(&root, day, 0);
        assert_eq!(j.get("market"), &J::Null);
        assert!(
            j.get("notes")
                .a()
                .iter()
                .any(|n| n.s().contains("without evidence"))
        );
        assert_eq!(j.get("orders").a().len(), 4);
        // A day kept before the instruments were recorded: the orders cannot be followed, and it says so.
        let r = Results::open(&root.join("s")).unwrap();
        let (cfg, outcome) = (r.fingerprint(), r.day(day).unwrap().outcome_hash);
        let kept: Vec<_> = r
            .traces(day)
            .unwrap()
            .into_iter()
            .filter(|(s, _)| *s != 0)
            .collect();
        let body = tf_strategy::trace::render_all(&kept);
        let text = crate::research::keep_wrap_for_tests("research trace", day, cfg, outcome, &body);
        std::fs::write(root.join("s").join(format!("{day}.trace")), text).unwrap();
        let j = data(&root, day, 0);
        assert!(j.get("orders").a().is_empty() && j.get("marks").a().is_empty());
        assert!(
            j.get("notes")
                .a()
                .iter()
                .any(|n| n.s().contains("kept before the host recorded"))
        );
        // Neither were the fills recorded: no legs, and the note says so.
        assert_eq!(j.get("legs"), &J::Null);
        assert!(
            j.get("notes")
                .a()
                .iter()
                .any(|n| n.s().contains("what each fill was for"))
        );
        assert_eq!(
            j.get("evidence").a().len(),
            1,
            "the strategy's account is still there"
        );
        // A strategy that recorded nothing says so (the trace is dropped too).
        let none: Vec<_> = kept.into_iter().filter(|(s, _)| *s == 99).collect();
        let text = crate::research::keep_wrap_for_tests(
            "research trace",
            day,
            cfg,
            outcome,
            &tf_strategy::trace::render_all(&none),
        );
        std::fs::write(root.join("s").join(format!("{day}.trace")), text).unwrap();
        let j = data(&root, day, 0);
        assert!(j.get("evidence").a().is_empty());
        assert!(
            j.get("notes")
                .a()
                .iter()
                .any(|n| n.s().contains("recorded no account"))
        );
        // A damaged log refuses the page, with why.
        std::fs::write(root.join("s").join(format!("{day}.log")), "garbage").unwrap();
        assert!(matches!(
            trade_page(&root, "s", day, 1, 0),
            Err(ViewError::Refused(_))
        ));
    }

    #[test]
    fn what_the_engine_did_with_tier_1_comes_from_the_days_log_and_marks_the_chart() {
        use crate::equiv::Rec;
        let root = root("tp5");
        let day = "2026-05-04";
        let r = Results::open(&root.join("s")).unwrap();
        let trip = r.day(day).unwrap().trips[0].clone();
        let mut traces = r.traces(day).unwrap();
        let book = traces
            .iter()
            .position(|(s, t)| *s == 0 && t.kind == "instruments")
            .unwrap();
        let mine: u32 = {
            let t = &traces[book].1;
            let rows = t
                .column("instrument")
                .unwrap()
                .into_iter()
                .zip(t.column("symbol").unwrap());
            rows.into_iter()
                .find(|(_, s)| *s == trip.symbol)
                .unwrap()
                .0
                .parse()
                .unwrap()
        };
        // This name was promoted just outside the window the page covers (ten minutes before the entry to two after the exit) and
        // again exactly at its start, on a scanner hit half a minute before the entry; a strategy asked for another's promotion;
        // and the name was demoted ten seconds after the exit, exactly at the window's end, and just after it.
        let sec = 1_000_000_000;
        let mut log = r.log(day).unwrap();
        let tier = |ts, instrument, promote, reason, score| Rec::Tier {
            idx: 9_000_000 + ts / sec,
            ts,
            instrument,
            promote,
            reason,
            score,
        };
        log.recs
            .push(tier(trip.entry_ts - 601 * sec, mine, true, 1, 5_000));
        log.recs
            .push(tier(trip.entry_ts - 600 * sec, mine, true, 1, 6_000));
        log.recs
            .push(tier(trip.entry_ts - 30 * sec, mine, true, 1, 9_200));
        log.recs
            .push(tier(trip.entry_ts - 20 * sec, 77, true, 3, 0));
        log.recs
            .push(tier(trip.exit_ts + 10 * sec, mine, false, 2, 0));
        log.recs
            .push(tier(trip.exit_ts + 120 * sec, mine, false, 2, 1));
        log.recs
            .push(tier(trip.exit_ts + 121 * sec, mine, false, 2, 2));
        traces[book].1.push_row(vec!["77".into(), "ZZZ".into()]);
        let (cfg, outcome) = (r.fingerprint(), r.day(day).unwrap().outcome_hash);
        let wrap = |kind: &str, body: &str| {
            crate::research::keep_wrap_for_tests(kind, day, cfg, outcome, body)
        };
        std::fs::write(
            root.join("s").join(format!("{day}.log")),
            wrap("research log", &log.render()),
        )
        .unwrap();
        std::fs::write(
            root.join("s").join(format!("{day}.trace")),
            wrap("research trace", &tf_strategy::trace::render_all(&traces)),
        )
        .unwrap();
        let j = data(&root, day, 0);
        let t = j.get("tiers");
        assert_eq!(t.get("day_events").s(), "7");
        // This name held Tier 1 when the window began (the promotion just before it); the window's changes are listed, this name's
        // marked and the other by its name; both edges are in and the changes just outside are not.
        assert_eq!(
            t.get("start").a().iter().map(J::s).collect::<Vec<_>>(),
            [trip.symbol.as_str()]
        );
        assert_eq!(t.get("around_total").s(), "5");
        let around = t.get("around").a();
        assert_eq!(
            around
                .iter()
                .map(|a| (a.get("symbol").s(), a.get("action").s()))
                .collect::<Vec<_>>(),
            [
                (trip.symbol.as_str(), "promoted"),
                (trip.symbol.as_str(), "promoted"),
                ("ZZZ", "promoted"),
                (trip.symbol.as_str(), "demoted"),
                (trip.symbol.as_str(), "demoted")
            ]
        );
        assert_eq!(
            around
                .iter()
                .filter(|a| a.get("mine") == &J::Bool(true))
                .count(),
            4
        );
        assert_eq!(
            around[1].get("reason").s(),
            "scanner hit, volume z-score 9.200"
        );
        assert_eq!(around[2].get("reason").s(), "a strategy asked for it");
        assert_eq!(around[3].get("reason").s(), "cooled off");
        assert_eq!(
            t.get("mine").a().len(),
            6,
            "all of this name's changes of the day"
        );
        // This name's changes in the window are markers on the chart, among the others in order; the ones outside are not.
        let marks: Vec<(&str, &str)> = j
            .get("marks")
            .a()
            .iter()
            .map(|m| (m.get("kind").s(), m.get("us").s()))
            .collect();
        let kinds: Vec<&str> = marks.iter().map(|m| m.0).collect();
        assert_eq!(
            kinds,
            [
                "tier_up",
                "tier_up",
                "decision",
                "fill",
                "exit_decision",
                "exit_fill",
                "tier_down",
                "tier_down"
            ]
        );
        assert!(
            j.get("marks").a()[1]
                .get("label")
                .s()
                .contains("promoted to Tier 1: scanner hit")
        );
        let times: Vec<i64> = marks.iter().map(|m| m.1.parse().unwrap()).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
        // The rest of the page is as it was.
        assert_eq!(j.get("orders").a().len(), 4);
    }

    /// The page's script is not run by any Rust test, so a syntax error in it would show only in a browser: have node read it, if
    /// there is a node (CI has one; a machine without skips this).
    #[test]
    fn the_pages_script_is_valid_javascript() {
        let page = include_str!("../viewer/trade.html");
        let from = page.find("<script>\n").unwrap() + "<script>\n".len();
        let script = &page[from..from + page[from..].find("</script>").unwrap()];
        let dir = crate::replay_tests::scratch("page-js");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("page.js");
        std::fs::write(&file, script).unwrap();
        // No node on this machine: not checked here.
        if let Ok(out) = std::process::Command::new("node")
            .arg("--check")
            .arg(&file)
            .output()
        {
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

// ---- a strategy set run over the days of a store (E19-S31) ----

mod store_run {
    use super::*;
    use crate::replay_tests::scratch;
    use crate::research::StoreSource;
    use crate::set::StrategySet;

    const DATES: [&str; 3] = ["2026-05-01", "2026-05-04", "2026-05-05"];

    /// A store of three days as `TEST`'s `tcbbo`, with each day's snapshot (as of the day before) in `snaps`, and a strategy set
    /// of one closing reversal over the universe file `u.txt`: `(store, snapshots, set file)`.
    fn world(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let days = afternoons(&format!("{name}-days"), drifting);
        let root = scratch(&format!("{name}-world"));
        let (store, snaps) = (root.join("store"), root.join("snaps"));
        std::fs::create_dir_all(store.join("TEST").join("tcbbo")).unwrap();
        std::fs::create_dir_all(&snaps).unwrap();
        let text = crate::tests::snapshot().render();
        for (i, (date, files)) in days.days.iter().enumerate() {
            std::fs::copy(
                &files[0],
                store
                    .join("TEST")
                    .join("tcbbo")
                    .join(format!("{date}.dbn.zst")),
            )
            .unwrap();
            let before = ["2026-04-30", "2026-05-01", "2026-05-04"][i];
            let snap = text.replacen("2026-10-02", before, 1);
            std::fs::write(snaps.join(format!("{date}.snapshot")), snap).unwrap();
        }
        tf_history::index(&store, "TEST", "tcbbo", "ALL_SYMBOLS").unwrap();
        std::fs::write(
            root.join("u.txt"),
            "universe v1\nstatic adv_shares <= 600\n",
        )
        .unwrap();
        let set = root.join("month.set");
        std::fs::write(
            &set,
            "strategy set v1\nbalance 100000\nstrategy 1 rev t04 universe=u.txt names=3\n",
        )
        .unwrap();
        (store, snaps, set)
    }

    #[test]
    fn a_strategy_set_is_run_over_the_days_of_a_store_to_a_results_directory() {
        let (store, snaps, set) = world("sr1");
        let (set, defs) = StrategySet::load(&set).unwrap();
        let mut src = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None).unwrap();
        assert_eq!(src.dates(), DATES);
        let host = set.host_config(16).unwrap();
        let cost = CostModel::published();
        let out = scratch("sr1-out");
        let rep = run(
            &Setup {
                host: &host,
                cost: &cost,
                defs: &defs,
            },
            &mut src,
            &out,
        )
        .unwrap();
        assert_eq!((rep.ran.len(), rep.trips), (3, 9));
        // What the directory says it is: the set's strategy and budget, and a ledger and report for each day.
        let r = Results::open(&out).unwrap();
        let lines = r.definition_lines().unwrap();
        assert_eq!((lines[0].id, lines[0].name.as_str()), (1, "rev"));
        assert!(lines[0].params.contains("names=3"));
        let b = r.budgets().unwrap().unwrap();
        assert_eq!(b.balance, 100_000 * 1_000_000_000);
        assert_eq!(b.ids.get(&1).map(String::as_str), Some("rev"));
        for d in DATES {
            assert!(
                r.ledger(d).is_ok() && r.report(d).unwrap().contains("rev"),
                "{d}"
            );
        }
        // The summary says what the directory is and what each strategy did.
        let text = crate::research::describe(&r).unwrap();
        assert!(
            text.contains("1 definitions over 3 days (2026-05-01 to 2026-05-05); 9 round trips"),
            "{text}"
        );
        assert!(
            text.contains("budgets: $100000.00 divided over 1 strategies"),
            "{text}"
        );
        assert!(
            text.contains("ledgers: 3 of 3 days have theirs\n"),
            "{text}"
        );
        assert!(
            text.contains("1 rev (variant ") && text.contains("names=3"),
            "{text}"
        );
        assert!(text.contains("9 trades, net -$"), "{text}");
        assert!(text.contains("costs: 50 ms to the broker"), "{text}");
        std::fs::remove_dir_all(r.ledger_dir("2026-05-04")).unwrap();
        let cut = crate::research::describe(&Results::open(&out).unwrap()).unwrap();
        assert!(
            cut.contains("ledgers: 2 of 3 days have theirs (run the rest again to have them)"),
            "{cut}"
        );
        run(
            &Setup {
                host: &host,
                cost: &cost,
                defs: &defs,
            },
            &mut src,
            &out,
        )
        .unwrap();
        // A second run finds every day there and does nothing; with a changed snapshot, that day is made again.
        let again = run(
            &Setup {
                host: &host,
                cost: &cost,
                defs: &defs,
            },
            &mut src,
            &out,
        )
        .unwrap();
        assert_eq!((again.ran.len(), again.skipped.len()), (0, 3));
        let p = snaps.join("2026-05-04.snapshot");
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, text.replace("S05,20.00,600", "S05,20.00,500")).unwrap();
        let mut src = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None).unwrap();
        let third = run(
            &Setup {
                host: &host,
                cost: &cost,
                defs: &defs,
            },
            &mut src,
            &out,
        )
        .unwrap();
        assert_eq!(third.ran, ["2026-05-04"]);
    }

    #[test]
    fn a_stores_days_are_identified_by_their_file_and_snapshot_and_subset_and_checked() {
        let (store, snaps, _) = world("sr2");
        let open = |from: Option<&str>, to: Option<&str>, syms: Option<Vec<String>>| {
            StoreSource::open(&store, "TEST", "tcbbo", from, to, &snaps, syms)
        };
        // The range is inclusive, either end open; nothing in it, or another schema, is refused.
        assert_eq!(
            open(Some("2026-05-04"), None, None).unwrap().dates(),
            &DATES[1..]
        );
        assert_eq!(
            open(None, Some("2026-05-04"), None).unwrap().dates(),
            &DATES[..2]
        );
        assert_eq!(
            open(Some("2026-05-04"), Some("2026-05-04"), None)
                .unwrap()
                .dates(),
            [DATES[1]]
        );
        assert!(
            open(Some("2026-06-01"), None, None)
                .err()
                .unwrap()
                .contains("no days of TEST tcbbo")
        );
        assert!(
            StoreSource::open(&store, "TEST", "trades", None, None, &snaps, None)
                .err()
                .unwrap()
                .contains("no days of TEST trades")
        );
        assert!(
            StoreSource::open(
                &store.join("nope"),
                "TEST",
                "tcbbo",
                None,
                None,
                &snaps,
                None
            )
            .is_err()
        );
        // The id of a day is its file, its snapshot and the subset: any of them changed is another id.
        let id = |s: &StoreSource, d: &str| s.data_id(d).unwrap();
        let a = open(None, None, None).unwrap();
        let b = open(None, None, None).unwrap();
        let subset = open(None, None, Some(vec!["S00".into(), "S01".into()])).unwrap();
        assert_eq!(id(&a, DATES[0]), id(&b, DATES[0]));
        assert_ne!(id(&a, DATES[0]), id(&subset, DATES[0]));
        assert_ne!(id(&a, DATES[0]), id(&a, DATES[1]));
        // Each day's id starts with its own file's checksum, as the manifest lists it.
        let manifest = tf_history::Store::read(&store).unwrap();
        for date in DATES {
            let sha = &manifest
                .of("TEST", "tcbbo")
                .find(|d| d.date == date)
                .unwrap()
                .sha256;
            assert!(id(&a, date).starts_with(sha.as_str()), "{date}");
        }
        let p = snaps.join("2026-05-01.snapshot");
        let text = std::fs::read_to_string(&p).unwrap();
        let before = id(&a, DATES[0]);
        std::fs::write(&p, text.replace("S00,20.00,100", "S00,20.00,101")).unwrap();
        assert_ne!(before, id(&open(None, None, None).unwrap(), DATES[0]));
        assert!(a.data_id("2026-05-02").is_err());
        // A subset restricts the snapshot's rows to those names; none of them in it is refused.
        let mut s = open(
            None,
            None,
            Some(vec!["S00".into(), "S01".into(), "ZZZ".into()]),
        )
        .unwrap();
        let d = s.load(DATES[0]).unwrap();
        assert_eq!(
            d.snapshot
                .rows
                .iter()
                .map(|r| r.symbol.as_str())
                .collect::<Vec<_>>(),
            ["S00", "S01"]
        );
        assert_eq!(d.files.len(), 1);
        let mut none = open(None, None, Some(vec!["ZZZ".into()])).unwrap();
        assert!(
            none.load(DATES[0])
                .err()
                .unwrap()
                .contains("none of the symbols")
        );
        assert!(s.load("2026-05-02").is_err());
        // A day's file that is not the size the manifest says is refused when it is loaded.
        let file = store.join("TEST").join("tcbbo").join("2026-05-05.dbn.zst");
        let mut bytes = std::fs::read(&file).unwrap();
        bytes.pop();
        std::fs::write(&file, bytes).unwrap();
        let mut s = open(None, None, None).unwrap();
        assert!(
            s.load("2026-05-05")
                .err()
                .unwrap()
                .contains("not the file that was stored")
        );
    }

    #[test]
    fn every_day_needs_its_snapshot_and_it_must_be_from_before_the_day() {
        let (store, snaps, _) = world("sr3");
        // A snapshot as of the day itself would give the strategy the day's close.
        let p = snaps.join("2026-05-04.snapshot");
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, text.replace("2026-05-01", "2026-05-04")).unwrap();
        let mut s = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None).unwrap();
        assert!(
            s.load("2026-05-04")
                .err()
                .unwrap()
                .contains("must be from before the day")
        );
        std::fs::write(&p, text.replace("2026-05-01", "2026-05-05")).unwrap();
        let mut s = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None).unwrap();
        assert!(
            s.load("2026-05-04")
                .err()
                .unwrap()
                .contains("must be from before the day")
        );
        // The snapshot of the day before is fine, and so is any earlier one.
        std::fs::write(&p, text.replace("2026-05-01", "2026-04-01")).unwrap();
        let mut s = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None).unwrap();
        assert!(s.load("2026-05-04").is_ok());
        // Days without a snapshot are named, all at once.
        std::fs::remove_file(snaps.join("2026-05-01.snapshot")).unwrap();
        std::fs::remove_file(&p).unwrap();
        let e = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None)
            .err()
            .unwrap();
        assert!(
            e.contains("no reference snapshot for 2 of 3 days")
                && e.contains("2026-05-01 2026-05-04"),
            "{e}"
        );
        // One that does not read is refused when it is loaded.
        std::fs::write(snaps.join("2026-05-01.snapshot"), "garbage").unwrap();
        std::fs::write(&p, "garbage").unwrap();
        let mut s = StoreSource::open(&store, "TEST", "tcbbo", None, None, &snaps, None).unwrap();
        assert!(s.load("2026-05-01").is_err());
    }

    #[test]
    fn a_strategy_is_certified_on_a_stored_day_streamed_and_its_certificate_survives_a_file() {
        use crate::{Certificate, Reference, certify, certify_files};
        use tf_capture::CaptureReplay;
        use tf_provider::{Poll, Provider};
        let (store, snaps, set) = world("sr4");
        let (set, defs) = StrategySet::load(&set).unwrap();
        let files = tf_history::files(
            &store,
            "TEST",
            "tcbbo",
            Some("2026-05-04"),
            Some("2026-05-04"),
        )
        .unwrap();
        let mut cfg = set.host_config(16).unwrap();
        cfg.day = Some(times(2026, 5, 4));
        cfg.min_certified_events = 1;
        let snap = tf_universe::Snapshot::parse(
            &std::fs::read_to_string(snaps.join("2026-05-04.snapshot")).unwrap(),
        )
        .unwrap();
        let cert = certify_files(&defs[0], &cfg, snap.clone(), &files, 7).unwrap();
        // What the stream shows is what the same events held in memory show.
        let mut events = Vec::new();
        let mut source = CaptureReplay::from_files(files.clone());
        let mut dedupe = tf_core::Dedupe::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match source.poll(&mut buf, 4096) {
                Poll::Events(_) => events.extend(buf.iter().copied().filter(|e| dedupe.admit(e))),
                Poll::Idle => continue,
                _ => break,
            }
        }
        let reference = Reference {
            symbols: crate::replay::learn_symbols(&files),
            snapshot: snap.clone(),
        };
        assert_eq!(
            cert,
            certify(&defs[0], &cfg, &reference, &events, 7).unwrap()
        );
        // A snapshot of the whole market holds names the day never ticked on. The universe selects NOTICK too, but the tape cannot
        // name it, so certifying over the stored day leaves it out and gives the same certificate; certifying with the wide
        // snapshot as it stands (a live day's gateway names everything it subscribed) refuses it.
        let mut wide = snap.clone();
        let mut extra = wide.rows[0].clone();
        extra.symbol = "NOTICK".into();
        wide.rows.push(extra);
        wide.rows.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        assert_eq!(
            certify_files(&defs[0], &cfg, wide.clone(), &files, 7).unwrap(),
            cert
        );
        let wide_ref = Reference {
            symbols: reference.symbols.clone(),
            snapshot: wide,
        };
        assert!(matches!(
            certify(&defs[0], &cfg, &wide_ref, &events, 7),
            Err(crate::CertifyError::Admit(crate::AdmitError::UnknownSymbols(v))) if v == ["NOTICK"]
        ));
        assert_eq!(
            (
                cert.tape_id,
                cert.events > 1_000,
                cert.intents,
                cert.accepted
            ),
            (7, true, 6, 6)
        );
        assert_eq!(cert.strategy_fp, defs[0].fingerprint());
        // It is one word of text that reads back whole, and one that was touched does not read.
        let text = cert.to_text();
        assert!(!text.contains(char::is_whitespace));
        assert_eq!(Certificate::from_text(&text).unwrap(), cert);
        assert_eq!(
            Certificate::from_text(&format!("  {text}\n")).unwrap(),
            cert
        );
        let edit = |from: &str, to: &str| text.replacen(from, to, 1);
        let events_hex = format!("{:x}", cert.events);
        for bad in [
            edit(&events_hex, "ffffffff"),
            text.replace("cert1", "cert2"),
            text[..text.len() - 1].to_owned(),
            format!("{text}:0"),
            text.replacen(':', ":zz", 1),
            String::new(),
        ] {
            assert!(Certificate::from_text(&bad).is_err(), "{bad}");
        }
        // The host takes the certificate as it takes one made in memory.
        assert!(Certificate::from_text(&text).unwrap().is_intact());
        // A tape that is not there does not certify: its instruments are unknown, so the strategy cannot be set up. (A capture's
        // torn last block reads as far as it goes, by design, so a cut file is not an error here; a store's files are checked
        // against their manifest before they are ever given.)
        let missing = certify_files(
            &defs[0],
            &cfg,
            snap.clone(),
            &[std::path::PathBuf::from("/no/such/day.dbn.zst")],
            7,
        );
        assert!(missing.is_err(), "{missing:?}");
        // Garbage is not a tape either.
        let junk = files[0].with_file_name("junk.dbn.zst");
        std::fs::write(&junk, b"not a dbn file at all").unwrap();
        assert!(certify_files(&defs[0], &cfg, snap, &[junk], 7).is_err());
    }
}
