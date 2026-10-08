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
