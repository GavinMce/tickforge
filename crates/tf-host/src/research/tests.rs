use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tf_core::{Nanos, Px};
use tf_strategy::intent::{Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::{CrossStrategy, Ctx, MemberView, Request};
use tf_universe::LiveFeature;

use super::*;
use crate::host::FillNote;
use crate::replay_tests::{dbn_day_at, scratch, write_capture};
use crate::tests::{HIGH, LOW, SEC, config, snapshot};
use crate::{HostConfig, Route, StrategyDef, runner};

const D: i64 = 1_000_000_000;

// ---- the cost model ----

#[test]
fn the_published_rates_are_those_of_the_day_and_a_day_outside_the_table_is_refused() {
    let m = CostModel::published();
    let rate = |d: &str| m.sec_rate(d).map(|p| p.to_decimal());
    // Section 31: $0.00 from 14 May 2025, $20.60 from 4 April 2026, known through the end of that fiscal year.
    assert!(matches!(rate("2025-05-13"), Err(CostError::NoRate { .. })));
    assert_eq!(rate("2025-05-14").unwrap(), "0.00");
    assert_eq!(rate("2026-04-03").unwrap(), "0.00");
    assert_eq!(rate("2026-04-04").unwrap(), "20.60");
    assert_eq!(rate("2026-09-30").unwrap(), "20.60");
    assert!(matches!(rate("2026-10-01"), Err(CostError::NoRate { .. })));
    // The Trading Activity Fee changes on 1 January.
    let taf = |d: &str| m.taf_rate(d).map(|(r, c)| (r.to_decimal(), c.to_decimal()));
    assert!(taf("2023-12-29").is_err());
    assert_eq!(
        taf("2024-06-03").unwrap(),
        ("0.000166".into(), "8.30".into())
    );
    assert_eq!(
        taf("2025-12-31").unwrap(),
        ("0.000166".into(), "8.30".into())
    );
    assert_eq!(
        taf("2026-01-01").unwrap(),
        ("0.000195".into(), "9.79".into())
    );
    assert_eq!(
        taf("2027-01-04").unwrap(),
        ("0.000232".into(), "11.61".into())
    );
    // Known through the last day of 2027, and not a day after.
    assert!(taf("2027-12-31").is_ok());
    assert!(taf("2028-01-01").is_err());
    assert!(taf("2028-01-03").is_err());
    let e = m.sale_fees("2026-10-05", 1, D).unwrap_err().to_string();
    assert!(e.contains("Section 31") && e.contains("2026-10-05"), "{e}");
}

#[test]
fn fees_are_exact_on_sales_and_the_per_trade_cap_binds() {
    let m = CostModel::published();
    // 100 shares at 19.99 on a day at $20.60 per million and $0.000195 a share: 1999 x 20.6e-6 + 100 x 0.000195.
    assert_eq!(
        m.sale_fees("2026-05-01", 100, 19_990_000_000).unwrap(),
        41_179_400 + 19_500_000
    );
    // Before 4 April 2026 the Section 31 fee is nothing and only the per-share fee is paid.
    assert_eq!(
        m.sale_fees("2026-03-02", 100, 19_990_000_000).unwrap(),
        19_500_000
    );
    // 100,000 shares at one dollar: the per-share fee would be $19.50, and one execution pays at most $9.79.
    assert_eq!(
        m.sale_fees("2026-03-02", 100_000, D).unwrap(),
        9_790_000_000
    );
    // The year before, the rate and the cap were lower.
    assert_eq!(
        m.sale_fees("2025-06-02", 100_000, D).unwrap(),
        8_300_000_000
    );
    assert_eq!(m.sale_fees("2025-06-02", 1_000, D).unwrap(), 166_000_000);
    // Borrow: 1,000 bp a year on $2,000 shorted for an hour.
    let b = CostModel {
        borrow_bps_per_year: 1000,
        ..m
    };
    assert_eq!(b.borrow_fee(2000 * D as u128, 3600 * SEC), 22_831_050);
    assert_eq!(b.borrow_fee(2000 * D as u128, 0), 0);
    assert_eq!(
        CostModel::published().borrow_fee(2000 * D as u128, 3600 * SEC),
        0
    );
    // A nanosecond is far below a raw unit on a small position and a few thousand on a very large one: nothing is
    // charged for no time, and an hour is an hour (1e18 raw units at 10,000 bp a year is a year's value for a year).
    let big = CostModel {
        borrow_bps_per_year: 10_000,
        ..CostModel::published()
    };
    assert_eq!(big.borrow_fee(10u128.pow(18), 0), 0);
    assert_eq!(
        big.borrow_fee(10u128.pow(18), 3600 * SEC),
        10u128.pow(18) / 8760
    );
}

#[test]
fn the_edges_of_a_cost_model_that_reads_are_where_they_should_be() {
    let text = CostModel::published().render();
    // The same date twice is not in date order.
    let e = CostModel::parse(&text.replace("sec 2026-04-04 20.60", "sec 2025-05-14 20.60"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("date order"), "{e}");
    // A table that starts on the day it is known through has that one day; a month 12 and a day 31 are dates, 13 and 32 are not.
    let one_day = text
        .replace("sec_through 2026-09-30", "sec_through 2025-05-14")
        .replace("sec 2026-04-04 20.60\n", "");
    let m = CostModel::parse(&one_day).unwrap();
    assert!(m.sec_rate("2025-05-14").is_ok() && m.sec_rate("2025-05-15").is_err());
    let taf_one_day = text
        .replace("taf_through 2027-12-31", "taf_through 2024-01-01")
        .replace(
            "taf 2025-01-01 0.000166 8.30\ntaf 2026-01-01 0.000195 9.79\ntaf 2027-01-01 0.000232 11.61\n",
            "",
        );
    let m = CostModel::parse(&taf_one_day).unwrap();
    assert!(m.taf_rate("2024-01-01").is_ok() && m.taf_rate("2024-01-02").is_err());
    for (d, ok) in [
        ("2026-12-311", false),
        ("2026-12-3", false),
        ("2026-12x31", false),
        ("2026x12-31", false),
        ("2026-12-31", true),
        ("2026-01-01", true),
        ("2026-13-01", false),
        ("2026-00-10", false),
        ("2026-12-32", false),
        ("2026-12-00", false),
        ("2026-12-3x", false),
        ("26-12-31", false),
    ] {
        assert_eq!(crate::research::cost::is_date(d), ok, "{d}");
    }
    // Missing pieces, one at a time.
    for gone in [
        "borrow_bps_per_year 0\n",
        "sec 2025-05-14 0.00\nsec 2026-04-04 20.60\n",
    ] {
        assert!(CostModel::parse(&text.replace(gone, "")).is_err(), "{gone}");
    }
    assert!(
        CostModel::parse(&text.replace(
            "taf 2024-01-01 0.000166 8.30\ntaf 2025-01-01 0.000166 8.30\ntaf 2026-01-01 0.000195 9.79\ntaf 2027-01-01 0.000232 11.61\n",
            ""
        ))
        .is_err()
    );
    assert!(CostModel::parse(&text.replace("taf_through 2027-12-31\n", "")).is_err());
}

#[test]
fn a_cost_model_reads_back_and_every_change_is_another_fingerprint() {
    let m = CostModel::published();
    let text = m.render();
    assert_eq!(CostModel::parse(&text).unwrap(), m);
    assert!(text.starts_with("cost model v1\nlatency_ns 50000000\n") && text.ends_with("end\n"));
    // What the simulated broker is given is the model's latency and borrow rate.
    let s = CostModel {
        latency_ns: 9,
        borrow_bps_per_year: 7,
        ..m.clone()
    }
    .sim();
    assert_eq!((s.latency_ns, s.borrow_bps_per_year), (9, 7));
    let mut seen = vec![m.fingerprint()];
    let variants: Vec<CostModel> = vec![
        CostModel {
            latency_ns: 1,
            ..m.clone()
        },
        CostModel {
            borrow_bps_per_year: 1,
            ..m.clone()
        },
        CostModel {
            sec_through: "2026-08-31".into(),
            ..m.clone()
        },
        CostModel {
            taf_through: "2027-06-30".into(),
            ..m.clone()
        },
        CostModel {
            sec: vec![("2025-05-14".into(), Px::parse("0.01").unwrap())],
            ..m.clone()
        },
        CostModel {
            taf: vec![(
                "2024-01-01".into(),
                Px::parse("0.000166").unwrap(),
                Px::parse("8.31").unwrap(),
            )],
            ..m.clone()
        },
    ];
    for v in variants {
        assert_eq!(CostModel::parse(&v.render()).unwrap(), v);
        assert!(!seen.contains(&v.fingerprint()));
        seen.push(v.fingerprint());
    }
    for (bad, what) in [
        (
            text.replace("cost model v1", "cost model v2"),
            "not `cost model v1`",
        ),
        (text.replace("end\n", ""), "cut short"),
        (format!("{text}sec 2027-01-01 1.00\n"), "after `end`"),
        (text.replace("latency_ns 50000000\n", ""), "lacks"),
        // Each table must say its date, whichever is the one missing (not just the later check that a table starts
        // before its date).
        (
            text.replace("sec_through 2026-09-30\n", ""),
            "must say the date",
        ),
        (
            text.replace("taf_through 2027-12-31\n", ""),
            "must say the date",
        ),
        (
            text.replace("taf 2025-01-01 0.000166", "taf 2024-01-01 0.000166"),
            "date order",
        ),
        (
            text.replace("sec 2026-04-04 20.60", "sec 2025-01-01 20.60"),
            "date order",
        ),
        (
            text.replace("sec 2026-04-04 20.60", "sec 2026-04-04 -1"),
            "not a rate",
        ),
        (text.replace("sec 2026-04-04", "sec 2026-4-4"), "not a date"),
        (
            text.replace("sec_through 2026-09-30", "sec_through 2025-01-01"),
            "starts after",
        ),
        (text.replace("end\n", "surprise 1\nend\n"), "does not know"),
    ] {
        let e = CostModel::parse(&bad).unwrap_err().to_string();
        assert!(e.contains(what), "{what}: {e}");
    }
}

// ---- trips from executions ----

const DAY: &str = "2026-05-01";

#[allow(clippy::too_many_arguments)]
fn note(
    strategy: u16,
    side: Side,
    purpose: Purpose,
    qty: u32,
    px: i64,
    ts: Nanos,
    reference: i64,
    reason: u16,
    stop: Option<i64>,
) -> FillNote {
    FillNote {
        strategy,
        instrument: 3,
        order: ts,
        seq: ts,
        side,
        purpose,
        reason,
        qty,
        px,
        ts,
        reference,
        stop,
    }
}

fn cents(c: i64) -> i64 {
    c * D / 100
}

fn assemble(
    cost: &CostModel,
    day: &str,
    notes: &[FillNote],
    easy: bool,
    mark: i64,
) -> Result<Vec<Trip>, CostError> {
    let who: BTreeMap<u16, Who> = [(
        7,
        Who {
            name: "gap".into(),
            variant: 0xabc,
        },
    )]
    .into();
    let symbol = |i: u32| format!("S{i}");
    let is_easy = move |_: u32| easy;
    let mut a = Assembler::new(day, cost, &who, &symbol, &is_easy);
    for n in notes {
        a.fill(n);
    }
    a.end(1_000 * SEC, |_| mark)
}

#[test]
fn a_long_round_trip_in_pieces_is_one_record_with_averaged_prices_and_exact_money() {
    let notes = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            60,
            cents(1000),
            SEC,
            cents(1002),
            1,
            Some(cents(900)),
        ),
        note(
            7,
            Side::Buy,
            Purpose::Open,
            40,
            cents(1010),
            2 * SEC,
            cents(1010),
            1,
            Some(cents(900)),
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            70,
            cents(1040),
            3 * SEC,
            cents(1045),
            0xE502,
            None,
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            30,
            cents(1070),
            4 * SEC,
            cents(1060),
            0xE503,
            None,
        ),
    ];
    let t = assemble(&CostModel::published(), DAY, &notes, true, 0).unwrap();
    assert_eq!(t.len(), 1);
    let t = &t[0];
    assert_eq!(
        (t.day.as_str(), t.strategy, t.name.as_str(), t.variant),
        (DAY, 7, "gap", 0xabc)
    );
    assert_eq!((t.symbol.as_str(), t.long, t.qty), ("S3", true, 100));
    // 600 + 404 dollars in, 728 + 321 out.
    assert_eq!((t.entry_ts, t.entry_px), (SEC, cents(1004)));
    assert_eq!((t.exit_ts, t.exit_px), (4 * SEC, cents(1049)));
    assert_eq!(t.gross, 45 * D);
    // Fees on the two sales only: 14,996,800 + 13,650,000 and 6,612,600 + 5,850,000.
    assert_eq!(t.fees, 41_109_400);
    assert_eq!(t.borrow, 0);
    assert_eq!(t.net, 45 * D - 41_109_400);
    assert_eq!(t.net_bps_x100, 44_779);
    // What was paid above and received below the references: -1.20 + 0 + 3.50 - 3.00.
    assert_eq!(t.slippage, -700_000_000);
    assert_eq!(t.slip_bps_x100, -340);
    // At risk: 100 shares from 10.04 to the stop at 9.00.
    assert_eq!(t.r_milli, Some(432));
    assert_eq!(
        (t.entry_reason, t.exit_reason, t.open_at_end),
        (1, 0xE503, false)
    );
}

#[test]
fn a_short_pays_its_fees_on_entry_and_borrow_only_on_names_that_are_not_easy() {
    let notes = [
        note(
            7,
            Side::SellShort,
            Purpose::Open,
            100,
            cents(2000),
            SEC,
            cents(2000),
            5,
            None,
        ),
        note(
            7,
            Side::Buy,
            Purpose::Close,
            100,
            cents(1950),
            3601 * SEC,
            cents(1950),
            0xE503,
            None,
        ),
    ];
    let hard = CostModel {
        borrow_bps_per_year: 1000,
        ..CostModel::published()
    };
    let t = &assemble(&hard, DAY, &notes, false, 0).unwrap()[0];
    assert!(!t.long);
    assert_eq!(t.gross, 50 * D);
    // The sale is the entry: 41,200,000 + 19,500,000.
    assert_eq!(t.fees, 60_700_000);
    assert_eq!(t.borrow, 22_831_050);
    assert_eq!(t.net, 49_916_468_950);
    assert_eq!(t.net_bps_x100, 24_958);
    assert_eq!((t.slippage, t.r_milli), (0, None));
    let easy = &assemble(&hard, DAY, &notes, true, 0).unwrap()[0];
    assert_eq!((easy.borrow, easy.net), (0, 50 * D - 60_700_000));
}

#[test]
fn a_trip_still_open_at_the_end_is_closed_at_the_last_trade_and_says_so() {
    let notes = [note(
        7,
        Side::Buy,
        Purpose::Open,
        100,
        cents(1000),
        SEC,
        cents(1000),
        1,
        None,
    )];
    let t = &assemble(&CostModel::published(), DAY, &notes, true, cents(1100)).unwrap()[0];
    assert!(t.open_at_end && t.exit_reason == OPEN_AT_END);
    assert_eq!((t.exit_px, t.exit_ts), (cents(1100), 1_000 * SEC));
    // Priced as a sale: 22,660,000 + 19,500,000, and no slippage on a mark.
    assert_eq!((t.gross, t.fees, t.slippage), (100 * D, 42_160_000, 0));
    assert_eq!(t.net, 99_957_840_000);
    // No trade at all: the entry price, so the trip makes nothing before costs.
    let t = &assemble(&CostModel::published(), DAY, &notes, true, 0).unwrap()[0];
    assert_eq!((t.exit_px, t.gross), (cents(1000), 0));
}

#[test]
fn a_fill_through_flat_ends_one_trip_and_begins_another_and_strategies_do_not_mix() {
    let notes = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            100,
            cents(1000),
            SEC,
            cents(1000),
            1,
            None,
        ),
        // A second strategy in the same instrument is its own position.
        note(
            8,
            Side::SellShort,
            Purpose::Open,
            10,
            cents(1000),
            SEC + 1,
            cents(1000),
            1,
            None,
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            150,
            cents(1100),
            2 * SEC,
            cents(1100),
            9,
            None,
        ),
        note(
            8,
            Side::Buy,
            Purpose::Close,
            10,
            cents(990),
            3 * SEC,
            cents(990),
            9,
            None,
        ),
    ];
    let t = assemble(&CostModel::published(), DAY, &notes, true, cents(1050)).unwrap();
    let key: Vec<(u16, bool, u32, bool)> = t
        .iter()
        .map(|t| (t.strategy, t.long, t.qty, t.open_at_end))
        .collect();
    // In the order they ended: the short of 8 closed after the long of 7, and the 50 left over is open at the end.
    assert_eq!(
        key,
        [
            (7, true, 100, false),
            (8, false, 10, false),
            (7, false, 50, true)
        ]
    );
    assert_eq!(t[0].exit_px, cents(1100));
    // The rest was sold at 11.00 and is marked at 10.50: a short that gained 25 dollars before costs.
    assert_eq!(
        (t[2].entry_px, t[2].exit_px, t[2].gross),
        (cents(1100), cents(1050), 25 * D)
    );
}

#[test]
fn averages_round_to_the_nearest_raw_unit_and_the_stop_and_reference_edge_cases_hold() {
    let cost = CostModel::published();
    // 1 share at 10.00 and 2 at 10.01: 30.02 over 3 is 10.006666667 (rounded up from ...6666...).
    let notes = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            1,
            cents(1000),
            SEC,
            cents(1000),
            1,
            None,
        ),
        note(
            7,
            Side::Buy,
            Purpose::Open,
            2,
            cents(1001),
            2 * SEC,
            cents(1001),
            1,
            None,
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            3,
            cents(1000),
            3 * SEC,
            cents(1000),
            1,
            None,
        ),
    ];
    let t = &assemble(&cost, DAY, &notes, true, 0).unwrap()[0];
    assert_eq!(t.entry_px, 10_006_666_667);
    assert_eq!(t.exit_px, cents(1000));
    // A stop at the entry price puts nothing at risk: no R, not a division by zero.
    let at = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            10,
            cents(1000),
            SEC,
            cents(1000),
            1,
            Some(cents(1000)),
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            10,
            cents(1100),
            2 * SEC,
            cents(1100),
            1,
            None,
        ),
    ];
    assert_eq!(assemble(&cost, DAY, &at, true, 0).unwrap()[0].r_milli, None);
    // A short's stop is above its entry: the risk is the distance, and a win is a positive R.
    let short = [
        note(
            7,
            Side::SellShort,
            Purpose::Open,
            10,
            cents(1000),
            SEC,
            cents(1000),
            1,
            Some(cents(1100)),
        ),
        note(
            7,
            Side::Buy,
            Purpose::Close,
            10,
            cents(900),
            2 * SEC,
            cents(900),
            1,
            None,
        ),
    ];
    let t = &assemble(&cost, DAY, &short, true, 0).unwrap()[0];
    // 10 dollars of gross over 10 shares x 1.00 at risk, less the fees of the sale (about 3 cents).
    assert!(
        t.net > 9 * D && t.r_milli.is_some_and(|r| (900..1000).contains(&r)),
        "{:?}",
        t.r_milli
    );
    // A reference of nothing (no price asked for) is no slippage, not a price against zero.
    let none = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            10,
            cents(1000),
            SEC,
            0,
            1,
            None,
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            10,
            cents(1100),
            2 * SEC,
            0,
            1,
            None,
        ),
    ];
    let t = &assemble(&cost, DAY, &none, true, 0).unwrap()[0];
    assert_eq!((t.slippage, t.slip_bps_x100), (0, 0));
    // The day's end before the last entry never puts the exit before it.
    let open = [note(
        7,
        Side::Buy,
        Purpose::Open,
        10,
        cents(1000),
        50 * SEC,
        cents(1000),
        1,
        None,
    )];
    let who: BTreeMap<u16, Who> = BTreeMap::new();
    let symbol = |i: u32| format!("S{i}");
    let easy = |_: u32| true;
    let mut a = Assembler::new(DAY, &cost, &who, &symbol, &easy);
    a.fill(&open[0]);
    let t = &a.end(10 * SEC, |_| cents(1010)).unwrap()[0];
    assert_eq!((t.entry_ts, t.exit_ts), (50 * SEC, 50 * SEC));
    // A strategy the assembler was not told about is named by its number.
    assert_eq!((t.name.as_str(), t.variant), ("s7", 0));
}

#[test]
fn a_day_the_cost_model_has_no_rate_for_is_an_error_not_a_guess() {
    let notes = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            100,
            cents(1000),
            SEC,
            cents(1000),
            1,
            None,
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            100,
            cents(1010),
            2 * SEC,
            cents(1010),
            1,
            None,
        ),
    ];
    assert!(assemble(&CostModel::published(), "2026-10-02", &notes, true, 0).is_err());
    assert!(assemble(&CostModel::published(), "2025-01-02", &notes, true, 0).is_err());
}

#[test]
fn a_trip_is_one_line_that_reads_back_exactly_and_a_bad_line_is_refused() {
    let notes = [
        note(
            7,
            Side::Buy,
            Purpose::Open,
            60,
            cents(1000),
            SEC,
            cents(1002),
            1,
            Some(cents(900)),
        ),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            60,
            cents(1040),
            3 * SEC,
            cents(1045),
            0xE502,
            None,
        ),
    ];
    let mut t = assemble(&CostModel::published(), DAY, &notes, true, 0).unwrap();
    let mut b = t[0].clone();
    b.r_milli = None;
    b.open_at_end = true;
    b.long = false;
    t.push(b);
    for trip in &t {
        let line = trip.line();
        assert_eq!(line.split('\t').count(), 22);
        assert_eq!(&Trip::parse(&line).unwrap(), trip);
    }
    assert_eq!(COLUMNS.split('\t').count(), 22);
    let line = t[0].line();
    for bad in [
        line.replacen("\tL\t", "\tX\t", 1),
        line.replacen("\t60\t", "\tsixty\t", 1),
        format!("{line}\textra"),
        line.rsplit_once('\t').unwrap().0.to_owned(),
        line[..line.len() - 1].to_owned() + "2",
    ] {
        assert!(Trip::parse(&bad).is_err(), "{bad}");
    }
}

// ---- a run over days ----

/// Buys 100 shares of the busiest member at review `buy_at` with a stop at half the price, and sells them at review
/// `sell_at`.
struct RoundTrip {
    id: u16,
    qty: u32,
    buy_at: u32,
    sell_at: u32,
    reviews: u32,
    held: Option<u32>,
    tracing: bool,
    traces: Vec<tf_strategy::Trace>,
}

impl CrossStrategy for RoundTrip {
    fn set_tracing(&mut self, on: bool) {
        self.tracing = on;
    }

    fn take_traces(&mut self) -> Vec<tf_strategy::Trace> {
        std::mem::take(&mut self.traces)
    }

    fn id(&self) -> StrategyId {
        StrategyId(self.id)
    }

    fn period(&self) -> Nanos {
        SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.reviews += 1;
        if self.reviews == self.buy_at {
            let Some((_, id)) = view.top_by(LiveFeature::Trades, 1, true).first().copied() else {
                return;
            };
            let Some(last) = view.state(id).and_then(|s| s.last_px) else {
                return;
            };
            self.held = Some(id);
            if self.tracing {
                let mut t = tf_strategy::Trace::new(ctx.now(), "buy")
                    .with("review", self.reviews)
                    .with_columns(&["instrument", "last"]);
                t.push_row(vec![id.to_string(), last.raw().to_string()]);
                self.traces.push(t);
            }
            let _ = ctx.submit(
                id,
                Request {
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
                },
            );
        } else if self.reviews == self.sell_at {
            let Some(id) = self.held else { return };
            let Some(last) = view.state(id).and_then(|s| s.last_px) else {
                return;
            };
            let _ = ctx.submit(
                id,
                Request {
                    side: Side::Sell,
                    qty: self.qty,
                    purpose: Purpose::Close,
                    pricing: Pricing::Limit(Px::from_raw(last.raw() - 50_000_000)),
                    protect: None,
                    tif: Tif::Day,
                    reason: 0xE503,
                },
            );
        }
    }
}

fn round_trip(id: u16, universe: &str, buy_at: u32, sell_at: u32) -> StrategyDef {
    round_trip_of(id, 100, universe, buy_at, sell_at)
}

fn round_trip_of(id: u16, qty: u32, universe: &str, buy_at: u32, sell_at: u32) -> StrategyDef {
    StrategyDef {
        id,
        name: format!("round{id}"),
        params: format!("buy_at {buy_at} sell_at {sell_at}"),
        universe: tf_universe::Spec::parse(universe).unwrap(),
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(RoundTrip {
                id,
                qty,
                buy_at,
                sell_at,
                reviews: 0,
                held: None,
                tracing: false,
                traces: Vec::new(),
            })
        }),
    }
}

fn defs() -> Vec<StrategyDef> {
    vec![round_trip(1, LOW, 2, 5), round_trip(2, HIGH, 3, 6)]
}

/// 13:30 UTC (09:30 New York, daylight time) on the dates used.
const OPENS: [(&str, u64); 3] = [
    ("2026-05-01", 1_777_642_200),
    ("2026-05-04", 1_777_901_400),
    ("2026-05-05", 1_777_987_800),
];

struct Days {
    dirs: Vec<(String, PathBuf)>,
    /// A day that cannot be loaded.
    broken: Option<String>,
    /// How many days were loaded.
    loads: u32,
}

impl Days {
    fn new(name: &str, n: usize) -> Days {
        let root = scratch(name);
        let mut dirs = Vec::new();
        for (date, open) in &OPENS[..n] {
            let dir = root.join(date);
            write_capture(&dir, &dbn_day_at(open * SEC, 10, 0));
            dirs.push(((*date).to_owned(), dir));
        }
        Days {
            dirs,
            broken: None,
            loads: 0,
        }
    }

    fn files(&self, date: &str) -> Vec<PathBuf> {
        let dir = &self.dirs.iter().find(|(d, _)| d == date).unwrap().1;
        tf_capture::list(dir)
            .unwrap()
            .iter()
            .map(|e| dir.join(&e.file))
            .collect()
    }
}

impl DaySource for Days {
    fn dates(&self) -> Vec<String> {
        self.dirs.iter().map(|(d, _)| d.clone()).collect()
    }

    fn data_id(&self, date: &str) -> Result<String, String> {
        let bytes: u64 = self
            .files(date)
            .iter()
            .map(|f| fs::metadata(f).map_or(0, |m| m.len()))
            .sum();
        Ok(format!("{}-{bytes}", self.files(date).len()))
    }

    fn load(&mut self, date: &str) -> Result<DayInput, String> {
        self.loads += 1;
        if self.broken.as_deref() == Some(date) {
            return Err("the file is unreadable".into());
        }
        Ok(DayInput {
            files: self.files(date),
            snapshot: snapshot(),
        })
    }
}

fn host_cfg() -> HostConfig {
    config(2)
}

fn out_dir(name: &str) -> PathBuf {
    scratch(name)
}

fn setup<'a>(host: &'a HostConfig, cost: &'a CostModel, defs: &'a [StrategyDef]) -> Setup<'a> {
    Setup { host, cost, defs }
}

/// Every file of a results directory by its path under it, a day's ledger directory included.
fn read_all(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in fs::read_dir(at).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push((
                    p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                    fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

#[test]
fn a_run_gives_one_record_per_round_trip_for_every_strategy_in_one_pass() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("rt-days", 3);
    let dir = out_dir("rt-out");
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!(rep.ran, ["2026-05-01", "2026-05-04", "2026-05-05"]);
    assert!(rep.skipped.is_empty() && rep.trips == 6 && rep.events > 1_000);
    // Each day was read once, for every strategy.
    assert_eq!(days.loads, 3);
    let results = Results::open(&dir).unwrap();
    assert_eq!(results.dates().unwrap(), rep.ran);
    let trips = results.trips().unwrap();
    assert_eq!(trips.len(), 6);
    // Per day: the first strategy's trip in S02, the second's in S08, each 100 shares, closed by its time exit.
    for (k, day) in rep.ran.iter().enumerate() {
        let (a, b) = (&trips[2 * k], &trips[2 * k + 1]);
        assert_eq!((&a.day, &b.day), (day, day));
        assert_eq!(
            (a.strategy, a.name.as_str(), a.symbol.as_str()),
            (1, "round1", "S02")
        );
        assert_eq!(
            (b.strategy, b.name.as_str(), b.symbol.as_str()),
            (2, "round2", "S08")
        );
        assert_ne!(a.variant, b.variant);
        for t in [a, b] {
            assert!(t.long && t.qty == 100 && !t.open_at_end);
            assert!(t.entry_ts < t.exit_ts);
            assert_eq!((t.entry_reason, t.exit_reason), (1, 0xE503));
            // Bought at the ask 20.01 against a limit of 20.05, sold at the bid 19.99 against a limit of 19.95.
            assert_eq!((t.entry_px, t.exit_px), (cents(2001), cents(1999)));
            assert_eq!(t.gross, -2 * D);
            assert_eq!(t.slippage, -8 * D);
            assert_eq!(t.slip_bps_x100, -2000);
            // From 4 April 2026 and before the fiscal year ends: $20.60 per million and $0.000195 a share.
            assert_eq!(t.fees, 41_179_400 + 19_500_000);
            assert_eq!(t.net, -2 * D - 60_679_400);
            assert_eq!(t.net_bps_x100, -1029);
            // Stop at 10.00 for an entry at 20.01: 1,001 dollars at risk.
            assert_eq!(t.r_milli, Some(-2));
        }
    }
    // The configuration is stored with the results, whole.
    let cfg = fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
    assert!(cfg.contains(&cost.render()) && cfg.contains("def\t1\t") && cfg.contains("round2"));
    assert!(cfg.contains("buy_at 3 sell_at 6"));
    assert_eq!(results.cost(), &cost);
}

#[test]
fn two_runs_of_the_same_days_and_definitions_leave_identical_files() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("det-days", 3);
    let (a, b) = (out_dir("det-a"), out_dir("det-b"));
    run(&setup(&host, &cost, &defs), &mut days, &a).unwrap();
    run(&setup(&host, &cost, &defs), &mut days, &b).unwrap();
    assert_eq!(read_all(&a), read_all(&b));
    // The configuration, and for each of the three days its trips, its decision log, its traces, its report and its ledger.
    assert_eq!(read_all(&a).len(), 16);
}

#[test]
fn a_run_that_stopped_goes_on_from_the_first_day_it_did_not_finish() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("res-days", 3);
    let whole = out_dir("res-whole");
    run(&setup(&host, &cost, &defs), &mut days, &whole).unwrap();

    let dir = out_dir("res-part");
    days.broken = Some("2026-05-05".into());
    let e = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap_err();
    assert_eq!(e.to_string(), "2026-05-05: the file is unreadable");
    // The two days before it are there, whole; the third is not, and nothing half-written is left.
    let have = Results::open(&dir).unwrap().dates().unwrap();
    assert_eq!(have, ["2026-05-01", "2026-05-04"]);
    assert!(
        !fs::read_dir(&dir).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".part"))
    );

    days.broken = None;
    days.loads = 0;
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!(rep.skipped, ["2026-05-01", "2026-05-04"]);
    assert_eq!(rep.ran, ["2026-05-05"]);
    // The finished days were not even loaded, and the result is what an uninterrupted run left.
    assert_eq!(days.loads, 1);
    assert_eq!(read_all(&dir), read_all(&whole));
}

#[test]
fn a_day_whose_data_changed_or_whose_file_is_damaged_is_made_again() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("again-days", 2);
    let dir = out_dir("again-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let whole = read_all(&dir);
    // Damaged on disk: made again, as it was.
    let path = dir.join("2026-05-01.trips");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("round1", "roundX", 1)).unwrap();
    assert!(Results::open(&dir).unwrap().trips().is_err());
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!(
        (rep.ran.as_slice(), rep.skipped.as_slice()),
        (
            &["2026-05-01".to_owned()][..],
            &["2026-05-04".to_owned()][..]
        )
    );
    assert_eq!(read_all(&dir), whole);
    // Other data for a day (a different file set): made again.
    let extra = days.dirs[0].1.join("extra.dbn.zst");
    let first = days.files("2026-05-01")[0].clone();
    fs::copy(&first, &extra).unwrap();
    struct Doubled(Days);
    impl DaySource for Doubled {
        fn dates(&self) -> Vec<String> {
            self.0.dates()
        }
        fn data_id(&self, date: &str) -> Result<String, String> {
            Ok(format!("{}+", self.0.data_id(date)?))
        }
        fn load(&mut self, date: &str) -> Result<DayInput, String> {
            self.0.load(date)
        }
    }
    let rep = run(&setup(&host, &cost, &defs), &mut Doubled(days), &dir).unwrap();
    assert_eq!(rep.ran.len(), 2);
}

#[test]
fn results_without_their_configuration_or_under_another_are_refused() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("cfg-days", 2);
    let dir = out_dir("cfg-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();

    // A different cost, strategy parameter or limit is another configuration, and the directory is not reused.
    let slow = CostModel {
        latency_ns: 1,
        ..cost.clone()
    };
    for (label, e) in [
        ("cost", run(&setup(&host, &slow, &defs), &mut days, &dir)),
        (
            "params",
            run(
                &setup(
                    &host,
                    &cost,
                    &[round_trip(1, LOW, 2, 5), round_trip(2, HIGH, 3, 7)],
                ),
                &mut days,
                &dir,
            ),
        ),
        (
            "limits",
            run(
                &setup(
                    &HostConfig {
                        id_space: host.id_space + 1,
                        ..host.clone()
                    },
                    &cost,
                    &defs,
                ),
                &mut days,
                &dir,
            ),
        ),
    ] {
        let e = e.unwrap_err().to_string();
        assert!(e.contains("another configuration"), "{label}: {e}");
    }
    // Reading: a day made under another configuration is not read as these results.
    let other = out_dir("cfg-other");
    run(&setup(&host, &slow, &defs), &mut days, &other).unwrap();
    fs::copy(other.join("2026-05-01.trips"), dir.join("2026-05-01.trips")).unwrap();
    let e = Results::open(&dir)
        .unwrap()
        .trips()
        .unwrap_err()
        .to_string();
    assert!(e.contains("made under configuration"), "{e}");

    // No configuration: refused to read and refused to add to.
    fs::remove_file(dir.join(CONFIG_FILE)).unwrap();
    let e = Results::open(&dir).err().unwrap().to_string();
    assert!(e.contains("without its configuration"), "{e}");
    let e = run(&setup(&host, &cost, &defs), &mut days, &dir)
        .unwrap_err()
        .to_string();
    assert!(e.contains("without its configuration"), "{e}");
    // One that does not read.
    fs::write(dir.join(CONFIG_FILE), "research config v1\ndef\t1\n").unwrap();
    assert!(Results::open(&dir).is_err());
    fs::write(dir.join(CONFIG_FILE), "hello").unwrap();
    assert!(Results::open(&dir).is_err());
    // A directory that is not there is not results.
    assert!(Results::open(&dir.join("nothing")).is_err());
}

#[test]
fn a_run_with_nothing_to_run_or_names_that_cannot_be_stored_is_refused() {
    let (host, cost) = (host_cfg(), CostModel::published());
    let mut days = Days::new("bad-days", 1);
    let e = run(&setup(&host, &cost, &[]), &mut days, &out_dir("bad-a")).unwrap_err();
    assert!(e.to_string().contains("no strategy"));
    let twice = [round_trip(1, LOW, 2, 5), round_trip(1, HIGH, 3, 6)];
    let e = run(&setup(&host, &cost, &twice), &mut days, &out_dir("bad-b")).unwrap_err();
    assert!(e.to_string().contains("given twice"));
    for name in ["", "a\tb", "a\nb"] {
        let mut d = round_trip(1, LOW, 2, 5);
        d.name = name.into();
        let e = run(&setup(&host, &cost, &[d]), &mut days, &out_dir("bad-c")).unwrap_err();
        assert!(e.to_string().contains("a name that is empty"), "{name:?}");
    }
}

#[test]
fn a_day_the_cost_model_does_not_cover_or_the_market_was_closed_is_refused_before_it_is_run() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let days = Days::new("cov-days", 1);
    let input = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let s = setup(&host, &cost, &defs);
    // The same data under a date after the table's last (a fiscal year whose rate is not in it).
    let e = run_day("2026-10-02", &input, &s).err().unwrap().to_string();
    assert!(e.contains("Section 31"), "{e}");
    // Saturday and Good Friday.
    let e = run_day("2026-05-02", &input, &s).err().unwrap().to_string();
    assert!(e.contains("closed"), "{e}");
    let e = run_day("2026-04-03", &input, &s).err().unwrap().to_string();
    assert!(e.contains("closed"), "{e}");
    let e = run_day("2026-5-1", &input, &s).err().unwrap().to_string();
    assert!(e.contains("not a date"), "{e}");
    let e = run_day("2026-02-31", &input, &s).err().unwrap().to_string();
    assert!(e.contains("calendar date"), "{e}");
    let empty = DayInput {
        files: vec![],
        snapshot: snapshot(),
    };
    assert!(run_day("2026-05-01", &empty, &s).is_err());
}

#[test]
fn data_that_cannot_be_read_is_an_error_not_a_shorter_day() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let days = Days::new("unread-days", 1);
    let mut files = days.files("2026-05-01");
    let s = setup(&host, &cost, &defs);
    // A file missing from the middle.
    files.insert(1, files[0].with_file_name("not-there.dbn.zst"));
    let input = DayInput {
        files: files.clone(),
        snapshot: snapshot(),
    };
    let e = run_day("2026-05-01", &input, &s).err().unwrap().to_string();
    assert!(e.contains("cannot be read"), "{e}");
    // A file that is not a DBN stream.
    let junk = files[0].with_file_name("junk.dbn.zst");
    fs::write(&junk, b"this is not zstd").unwrap();
    let input = DayInput {
        files: vec![files[0].clone(), junk],
        snapshot: snapshot(),
    };
    let e = run_day("2026-05-01", &input, &s).err().unwrap().to_string();
    assert!(e.contains("cannot be read"), "{e}");
}

#[test]
fn a_day_run_here_replays_through_the_live_hosts_replay_check_to_the_same_decisions() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let days = Days::new("eq-days", 1);
    let files = days.files("2026-05-01");
    let input = DayInput {
        files: files.clone(),
        snapshot: snapshot(),
    };
    let out = run_day("2026-05-01", &input, &setup(&host, &cost, &defs)).unwrap();
    // Two strategies: each added, two decisions answered, two fills, and the end of the day.
    assert!(out.log.recs.len() >= 10 && out.anomalies.is_empty() && out.ledger_refusals == 0);
    assert_eq!(
        out.log
            .recs
            .iter()
            .filter(|r| matches!(r, crate::Rec::Fill { .. }))
            .count(),
        4
    );
    // The replay is given the configuration the run used: this day's sessions and the cost model's latency.
    let times = tf_calendar::Calendar::us_equities()
        .times(tf_calendar::Date::new(2026, 5, 1).unwrap())
        .unwrap()
        .unwrap();
    let cfg = HostConfig {
        day: Some(times),
        sim: cost.sim(),
        ..host.clone()
    };
    let rep = crate::replay_files(&files, &out.log, &cfg, snapshot(), &defs, 0).unwrap();
    assert!(rep.verdict.is_equal(), "{}", rep.text());
    assert_eq!(rep.events, out.events);
    // And it is a comparison that can fail: another latency decides another fill.
    let slow = HostConfig {
        sim: CostModel {
            latency_ns: 3 * SEC,
            ..cost.clone()
        }
        .sim(),
        ..cfg
    };
    let rep = crate::replay_files(&files, &out.log, &slow, snapshot(), &defs, 0).unwrap();
    assert!(!rep.verdict.is_equal());
}

#[test]
fn the_latency_in_the_cost_model_is_the_latency_of_the_fills() {
    let (host, defs) = (host_cfg(), defs());
    let days = Days::new("lat-days", 1);
    let input = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let entry = |latency_ns: u64| {
        let cost = CostModel {
            latency_ns,
            ..CostModel::published()
        };
        let out = run_day("2026-05-01", &input, &setup(&host, &cost, &defs)).unwrap();
        (out.trips[0].entry_ts, out.trips[0].exit_ts)
    };
    let (fast, fast_exit) = entry(0);
    let (slow, slow_exit) = entry(500_000_000);
    assert!(slow >= fast + 400_000_000 && slow_exit >= fast_exit + 400_000_000);
}

#[test]
fn a_host_given_the_day_has_its_sessions_from_the_start() {
    let times = tf_calendar::Calendar::us_equities()
        .times(tf_calendar::Date::new(2026, 5, 1).unwrap())
        .unwrap()
        .unwrap();
    let cfg = HostConfig {
        day: Some(times),
        ..config(2)
    };
    let h = crate::tests::host(&cfg);
    assert_eq!(h.tier0().day(), Some(&times));
    assert_eq!(crate::tests::host(&config(2)).tier0().day(), None);
}

#[test]
fn a_day_file_says_what_it_is_and_every_kind_of_damage_is_noticed() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("dmg-days", 1);
    let dir = out_dir("dmg-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let text = fs::read_to_string(dir.join("2026-05-01.trips")).unwrap();
    let d = DayFile::parse(&text).unwrap();
    assert_eq!(d.render(), text);
    assert!(text.starts_with("research trips v1\nday 2026-05-01\nconfig "));
    assert!(d.events > 100 && d.trips.len() == 2 && d.anomalies == 0);
    assert_eq!((d.rejected, d.refused), (0, 0));
    let seal = |body: &str| format!("{body}end {:016x}\n", crate::def::fnv(&[body.as_bytes()]));
    let body = &text[..text.rfind("end ").unwrap()];
    for (bad, what) in [
        (text.replacen("round1", "roundX", 1), "checksum"),
        (text[..text.len() - 4].to_owned(), "cut short"),
        (format!("{text}more\n"), "text after"),
        (text.replace("end ", "end zz"), "bad checksum"),
        (
            seal(&body.replacen("research trips v1", "research trips v2", 1)),
            "not `research trips v1`",
        ),
        (seal(&body.replacen("trips 2", "trips 3", 1)), "3 said"),
        (
            seal(&body.replacen("events ", "evens ", 1)),
            "expected `events`",
        ),
        (
            seal(&body.replacen("day\tstrategy", "day\tstrat", 1)),
            "columns",
        ),
        (seal(&body.replacen("\tL\t", "\tX\t", 1)), "does not read"),
    ] {
        let Err(e) = DayFile::parse(&bad) else {
            panic!("accepted a file with this damage: {what}");
        };
        assert!(e.contains(what), "{what}: {e}");
    }
}

#[test]
fn orders_that_are_refused_are_counted_so_that_no_trips_is_not_mistaken_for_no_edge() {
    let (host, cost) = (host_cfg(), CostModel::published());
    let days = Days::new("ref-days", 1);
    let input = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    // 100,000 shares at $20 is far past the order limit: the gateway refuses it, and the close finds nothing to close.
    let defs = [round_trip_of(1, 100_000, LOW, 2, 5)];
    let out = run_day("2026-05-01", &input, &setup(&host, &cost, &defs)).unwrap();
    assert!(out.trips.is_empty());
    assert_eq!(out.rejected, 2);
    assert_eq!(out.refused, 0);
    // And the same in the file the run writes.
    let mut days = days;
    let dir = out_dir("ref-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let d = Results::open(&dir).unwrap().day("2026-05-01").unwrap();
    assert_eq!((d.rejected, d.refused, d.trips.len()), (2, 0, 0));
}

/// Every record of a DBN stream twice in a row: what a gateway that sent a message again would leave.
fn doubled(bytes: &[u8]) -> Vec<u8> {
    use dbn::decode::{DbnDecoder, DbnMetadata, DecodeRecordRef};
    use dbn::encode::DbnEncoder;
    let mut dec = DbnDecoder::new(bytes).unwrap();
    let md = dec.metadata().clone();
    let mut out = Vec::new();
    // Constructing the encoder writes the stream's header into `out`.
    let _ = DbnEncoder::new(&mut out, &md).unwrap();
    while let Some(rec) = dec.decode_record_ref().unwrap() {
        out.extend_from_slice(rec.as_ref());
        out.extend_from_slice(rec.as_ref());
    }
    out
}

#[test]
fn what_the_gateway_sent_twice_is_taken_once() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let plain = Days::new("dup-plain", 1);
    let bytes = dbn_day_at(OPENS[0].1 * SEC, 10, 0);
    let dir = scratch("dup-doubled");
    write_capture(&dir, &doubled(&bytes));
    let files: Vec<PathBuf> = tf_capture::list(&dir)
        .unwrap()
        .iter()
        .map(|e| dir.join(&e.file))
        .collect();
    let twice = DayInput {
        files,
        snapshot: snapshot(),
    };
    let once = DayInput {
        files: plain.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let s = setup(&host, &cost, &defs);
    let (a, b) = (
        run_day("2026-05-01", &once, &s).unwrap(),
        run_day("2026-05-01", &twice, &s).unwrap(),
    );
    assert_eq!(a.events, b.events);
    assert_eq!(a.trips, b.trips);
}

// ---- found by mutation: each of these fails if the line it names is changed ----

#[test]
fn a_trip_entered_at_a_price_of_nothing_has_no_basis_points_and_no_division_by_zero() {
    let notes = [
        note(7, Side::Buy, Purpose::Open, 100, 0, SEC, 0, 1, None),
        note(
            7,
            Side::Sell,
            Purpose::Close,
            100,
            cents(1),
            2 * SEC,
            0,
            0,
            None,
        ),
    ];
    let t = assemble(&CostModel::published(), DAY, &notes, true, 0).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(
        (t[0].entry_px, t[0].net_bps_x100, t[0].slip_bps_x100),
        (0, 0, 0)
    );
    // The rounding division is for a positive divisor only: nothing, not a panic, for none.
    assert_eq!(super::trips::round_div(5, 0), 0);
    assert_eq!(super::trips::round_div(7, 2), 4);
}

#[test]
fn a_days_host_has_its_sessions_and_the_cost_models_latency_and_borrow_rate() {
    let times = tf_calendar::Calendar::us_equities()
        .times(tf_calendar::Date::new(2026, 5, 1).unwrap())
        .unwrap()
        .unwrap();
    let cost = CostModel {
        latency_ns: 7 * SEC,
        borrow_bps_per_year: 123,
        ..CostModel::published()
    };
    let host = host_cfg();
    let c = super::run::day_config(&host, &cost, times);
    assert_eq!(c.day, Some(times));
    assert_eq!(
        (c.sim.latency_ns, c.sim.borrow_bps_per_year),
        (7 * SEC, 123)
    );
    assert_eq!(c.id_space, host.id_space);
}

#[test]
fn the_easy_to_borrow_set_is_the_instruments_the_snapshot_says_are() {
    let text = "# as_of 2026-10-02\nsymbol,price,adv_shares,easy_to_borrow\nS00,20.00,100,yes\nS01,20.00,200,no\nS02,20.00,300,yes\n";
    let reference = crate::host::Reference {
        symbols: crate::tests::names(),
        snapshot: tf_universe::Snapshot::parse(text).unwrap(),
    };
    let id = |n: &str| reference.symbols.get(n).unwrap();
    assert_eq!(
        super::run::easy_to_borrow(&reference),
        std::collections::BTreeSet::from([id("S00"), id("S02")])
    );
}

#[test]
fn a_days_file_carries_every_count_of_the_day_it_is_made_from() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let days = Days::new("file-days", 1);
    let input = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let mut out = run_day("2026-05-01", &input, &setup(&host, &cost, &defs)).unwrap();
    out.events = 11;
    out.outcome_hash = 0xabcdef;
    out.anomalies = vec!["a".into(), "b".into()];
    out.ledger_refusals = 3;
    out.rejected = 4;
    out.refused = 6;
    let trips = out.trips.clone();
    let f = DayFile::of("2026-05-01", 0x1234, "data".into(), out);
    assert_eq!(
        (f.day.as_str(), f.config, f.data.as_str()),
        ("2026-05-01", 0x1234, "data")
    );
    assert_eq!((f.events, f.outcome_hash), (11, 0xabcdef));
    // Anomalies are the host's notes and the fills the ledger would not take.
    assert_eq!((f.anomalies, f.rejected, f.refused), (5, 4, 6));
    assert_eq!(f.trips, trips);
}

#[test]
fn an_open_order_with_protective_orders_before_the_open_is_refused_by_the_broker_and_counted() {
    let (host, cost) = (host_cfg(), CostModel::published());
    let defs = [round_trip(1, LOW, 2, 5)];
    // The same day an hour early, in the premarket: the broker takes no protective orders with an open there.
    let dir = scratch("pre-days");
    write_capture(&dir, &dbn_day_at((OPENS[0].1 - 3600) * SEC, 10, 0));
    let early = DayInput {
        files: tf_capture::list(&dir)
            .unwrap()
            .iter()
            .map(|e| dir.join(&e.file))
            .collect(),
        snapshot: snapshot(),
    };
    let out = run_day("2026-05-01", &early, &setup(&host, &cost, &defs)).unwrap();
    assert!(out.trips.is_empty(), "{:?}", out.trips);
    assert_eq!(out.refused, 1);
    // In the regular session the same strategy trades and nothing is refused.
    let days = Days::new("pre-regular", 1);
    let regular = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let out = run_day("2026-05-01", &regular, &setup(&host, &cost, &defs)).unwrap();
    assert_eq!((out.refused, out.trips.len()), (0, 1));
}

/// A DBN stream with its header and its first `n` records, as a whole zstd file in `dir`.
fn first_records(bytes: &[u8], n: usize, dir: &Path) -> PathBuf {
    use dbn::decode::{DbnDecoder, DbnMetadata, DecodeRecordRef};
    use dbn::encode::DbnEncoder;
    let mut dec = DbnDecoder::new(bytes).unwrap();
    let md = dec.metadata().clone();
    let mut out = Vec::new();
    // Constructing the encoder writes the stream's header into `out`.
    let _ = DbnEncoder::new(&mut out, &md).unwrap();
    for _ in 0..n {
        let rec = dec.decode_record_ref().unwrap().expect("a record");
        out.extend_from_slice(rec.as_ref());
    }
    // The capture writer keeps no file for no records, so the file is made here.
    fs::create_dir_all(dir).unwrap();
    let file = dir.join(format!("first-{n}.dbn.zst"));
    fs::write(&file, zstd::encode_all(&out[..], 0).unwrap()).unwrap();
    file
}

#[test]
fn a_day_with_no_market_event_is_an_error_and_a_day_with_one_is_a_day() {
    let (host, cost) = (host_cfg(), CostModel::published());
    let bytes = dbn_day_at(OPENS[0].1 * SEC, 10, 0);
    let dir = scratch("tiny-days");
    let input = |n: usize| DayInput {
        files: vec![first_records(&bytes, n, &dir)],
        snapshot: snapshot(),
    };
    // A strategy that never gets as far as buying, on the one symbol the first record is for.
    let defs = [round_trip(
        1,
        "universe v1\nstatic adv_shares <= 100\n",
        1_000,
        1_001,
    )];
    let run = |n: usize| run_day("2026-05-01", &input(n), &setup(&host, &cost, &defs));
    // The first record is not a market event: a file of only that is a day with nothing in it.
    let e = run(1).err().unwrap().to_string();
    assert!(e.contains("no events"), "{e}");
    // The first prefix with a market event in it has exactly one, and is a day.
    let (n, out) = (2..80)
        .find_map(|n| run(n).ok().map(|o| (n, o)))
        .expect("some prefix has an event");
    assert_eq!((out.events, out.trips.len()), (1, 0), "{n} records");
}

#[test]
fn a_date_the_cost_model_does_not_cover_is_refused_even_when_nothing_would_have_been_sold() {
    let host = host_cfg();
    // A strategy that never gets as far as buying: no sale, so no fee is ever looked up while the day runs.
    let defs = [round_trip(1, LOW, 1_000, 1_001)];
    let days = Days::new("nosale-days", 1);
    let input = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let e = run_day(
        "2026-10-02",
        &input,
        &setup(&host, &CostModel::published(), &defs),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(e.contains("Section 31"), "{e}");
    // And the same for the other fee, with the first table made to reach the date.
    let cost = CostModel {
        sec_through: "2028-12-31".into(),
        ..CostModel::published()
    };
    let e = run_day("2028-01-03", &input, &setup(&host, &cost, &defs))
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("Trading Activity Fee"), "{e}");
}

#[test]
fn every_execution_is_noted_with_the_intent_it_belongs_to() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let days = Days::new("notes-days", 1);
    let input = DayInput {
        files: days.files("2026-05-01"),
        snapshot: snapshot(),
    };
    let out = run_day("2026-05-01", &input, &setup(&host, &cost, &defs)).unwrap();
    assert_eq!(out.notes.len(), 4);
    // Two intents a strategy, a buy and a sell, each with its own number within the strategy.
    let ids: std::collections::BTreeSet<(u16, u64)> =
        out.notes.iter().map(|n| (n.strategy, n.seq)).collect();
    assert_eq!(ids.len(), 4);
    // The decision log numbers its decisions the same way, so a note and the decision it came from can be matched.
    let decided: std::collections::BTreeSet<(u16, u64)> = out
        .log
        .recs
        .iter()
        .filter_map(|r| match r {
            crate::Rec::Decision { strategy, seq, .. } => Some((*strategy, *seq)),
            _ => None,
        })
        .collect();
    assert_eq!(decided, ids);
    for n in &out.notes {
        assert_eq!(n.side == Side::Buy, n.purpose == Purpose::Open);
        assert!(n.qty == 100 && n.px > 0 && n.ts > 0 && n.reference > 0);
    }
}

// ---- statistics over a results directory (E19-S14) ----

fn boot() -> tf_stats::Bootstrap {
    tf_stats::Bootstrap {
        replicates: 200,
        block: Some(1),
        seed: 9,
    }
}

#[test]
fn a_run_is_reported_variant_by_variant_once_its_strategies_are_in_the_registry() {
    use super::stats::{register_defs, report_results};
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("st-days", 3);
    let dir = out_dir("st-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let results = Results::open(&dir).unwrap();
    // The stored configuration lists the strategies by fingerprint and name.
    let listed: Vec<(u64, String)> = defs
        .iter()
        .map(|d| (d.fingerprint(), d.name.clone()))
        .collect();
    assert_eq!(results.definitions().unwrap(), listed);
    // Not in the registry: no report, and the error says which.
    let mut reg = tf_stats::Registry::new();
    let e = report_results(&results, &reg, boot())
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("round1") && e.contains("trial registry"), "{e}");
    // Entered before the run: both are new; entered again, none is, and the first date stays.
    assert_eq!(register_defs(&mut reg, &defs, "2026-10-07").unwrap(), 2);
    assert_eq!(register_defs(&mut reg, &defs, "2026-10-08").unwrap(), 0);
    assert_eq!(reg.get(listed[0].0).unwrap().first_run, "2026-10-07");
    assert!(register_defs(&mut reg, &defs, "yesterday").is_err());
    let rep = report_results(&results, &reg, boot()).unwrap();
    assert_eq!(rep.len(), 2);
    for (s, (fp, name)) in rep.iter().zip(&listed) {
        assert_eq!((s.fingerprint, &s.name), (*fp, name));
        assert_eq!((s.trades, s.days, s.trials), (3, 3, 2));
        // Each day's trade is the same: -2 dollars and fees on 2,001 a share, in hundredths of a basis point -1029.
        assert!((s.bp.mean.unwrap() + 10.29).abs() < 1e-12);
        assert!((s.r.mean.unwrap() + 0.002).abs() < 1e-12);
        assert_eq!((s.hit_rate, s.payoff), (Some(0.0), None));
        assert!((s.max_drawdown_bp - 30.87).abs() < 1e-9);
        // A result that cannot vary from day to day has an error of nothing, and so no t-statistic.
        let b = s.bp.boot.unwrap();
        assert_eq!((b.se, b.t), (0.0, None));
    }
}

#[test]
fn a_strategy_that_made_no_trade_is_still_a_variant_of_the_run() {
    use super::stats::{register_defs, report_results};
    let (host, cost) = (config(3), CostModel::published());
    let mut defs = defs();
    defs.push(round_trip(3, LOW, 1_000, 1_001));
    let mut days = Days::new("idle-days", 2);
    let dir = out_dir("idle-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let results = Results::open(&dir).unwrap();
    let mut reg = tf_stats::Registry::new();
    register_defs(&mut reg, &defs, "2026-10-07").unwrap();
    assert_eq!(reg.len(), 3);
    let rep = report_results(&results, &reg, boot()).unwrap();
    assert_eq!(rep.len(), 3);
    assert_eq!((rep[2].trades, rep[2].bp.mean), (0, None));
    assert_eq!((rep[0].trades, rep[1].trades), (2, 2));
}

#[test]
fn a_refinement_of_a_run_is_compared_with_its_plain_version_by_fingerprint() {
    use super::stats::{paired_results, register_defs};
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("pair-days", 3);
    let dir = out_dir("pair-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let results = Results::open(&dir).unwrap();
    let mut reg = tf_stats::Registry::new();
    register_defs(&mut reg, &defs, "2026-10-07").unwrap();
    let (a, b) = (defs[1].fingerprint(), defs[0].fingerprint());
    let p = paired_results(&results, a, b, &reg, boot()).unwrap();
    // Three days on which both traded, with the same result on each, in different symbols (S08 and S02).
    assert_eq!(p.common_days, 3);
    assert!(p.bp.unwrap().estimate.abs() < 1e-12);
    assert_eq!((p.shared_signals, p.other_signals), (0, 3));
    // A fingerprint the run does not have, and one the registry does not have.
    assert!(paired_results(&results, 1, b, &reg, boot()).is_err());
    let mut small = tf_stats::Registry::new();
    register_defs(&mut small, &defs[..1], "2026-10-07").unwrap();
    assert!(paired_results(&results, a, b, &small, boot()).is_err());
}

#[test]
fn a_trip_is_an_outcome_with_the_same_day_time_and_result() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("out-days", 1);
    let dir = out_dir("out-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let trips = Results::open(&dir).unwrap().trips().unwrap();
    let (t, o) = (&trips[0], super::stats::outcome(&trips[0]));
    assert_eq!(
        (o.day.as_str(), o.symbol.as_str(), o.entry_ts, o.exit_ts),
        (t.day.as_str(), t.symbol.as_str(), t.entry_ts, t.exit_ts)
    );
    assert_eq!((o.net_bps_x100, o.r_milli), (t.net_bps_x100, t.r_milli));
    assert_eq!((o.net_bps_x100, o.r_milli), (-1029, Some(-2)));
}

#[test]
fn a_trip_of_a_strategy_the_configuration_does_not_list_is_an_error_not_a_dropped_trade() {
    use super::stats::group;
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("alien-days", 1);
    let dir = out_dir("alien-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let results = Results::open(&dir).unwrap();
    let trips = results.trips().unwrap();
    let listed = results.definitions().unwrap();
    // All listed: each variant has its own trade.
    let all = group(listed.clone(), &trips).unwrap();
    assert_eq!(
        all.iter().map(|v| v.outcomes.len()).collect::<Vec<_>>(),
        [1, 1]
    );
    assert_eq!(all[0].name, "round1");
    // The second left out of the list: its trade is of a variant nobody listed, and it is said so, not left out.
    let e = group(listed[..1].to_vec(), &trips).unwrap_err().to_string();
    assert!(e.contains("round2") && e.contains("does not list"), "{e}");
    // One listed that has no trade is still a variant.
    let mut more = listed.clone();
    more.push((7, "idle".into()));
    let g = group(more, &trips).unwrap();
    assert_eq!((g.len(), g[2].outcomes.len(), g[2].fingerprint), (3, 0, 7));
}

// ---- what a day keeps beside its trips (E19-S33) ----

/// A trip with only what the evidence pass reads: a symbol and the times.
fn trip_at(symbol: &str, entry: tf_core::Nanos, exit: tf_core::Nanos) -> Trip {
    Trip {
        day: "2026-05-01".into(),
        strategy: 1,
        name: "round1".into(),
        variant: 1,
        symbol: symbol.into(),
        long: true,
        qty: 100,
        entry_ts: entry,
        entry_px: 20 * D,
        exit_ts: exit,
        exit_px: 20 * D,
        gross: 0,
        fees: 0,
        borrow: 0,
        slippage: 0,
        net: 0,
        net_bps_x100: 0,
        slip_bps_x100: 0,
        r_milli: None,
        entry_reason: 1,
        exit_reason: 0,
        open_at_end: false,
    }
}

/// What the day's files hold of `symbol` between two times, found without the evidence pass: every event the host would have
/// been given (what the gateway sent twice taken once), in the integers the evidence keeps.
fn independent(
    files: &[PathBuf],
    symbol: &str,
    from: tf_core::Nanos,
    to: tf_core::Nanos,
) -> Vec<EvEvent> {
    use tf_capture::CaptureReplay;
    use tf_core::{Dedupe, Event};
    use tf_provider::{Poll, Provider};
    let id = crate::replay::learn_symbols(files)
        .get(symbol)
        .expect("a known symbol");
    let mut src = CaptureReplay::from_files(files.to_vec());
    let (mut dedupe, mut raw, mut out) = (Dedupe::new(), Vec::new(), Vec::new());
    loop {
        raw.clear();
        match src.poll(&mut raw, 4096) {
            Poll::Events(_) => {}
            _ => break,
        }
        for ev in raw.iter().filter(|e| dedupe.admit(e)) {
            if ev.instrument() != id || ev.ts_recv() < from || ev.ts_recv() > to {
                continue;
            }
            match ev {
                Event::Trade(t) => out.push(EvEvent::Trade {
                    ts: t.hdr.ts_recv,
                    px: t.px.raw(),
                    size: t.size,
                }),
                Event::Quote(q) => out.push(EvEvent::Quote {
                    ts: q.hdr.ts_recv,
                    bid: q.bid_px.raw(),
                    ask: q.ask_px.raw(),
                    bid_sz: q.bid_sz,
                    ask_sz: q.ask_sz,
                }),
                _ => {}
            }
        }
    }
    out
}

#[test]
fn a_run_keeps_each_days_log_and_traces_beside_its_trips_and_reads_them_back() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("keep-days", 3);
    let dir = out_dir("keep-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let results = Results::open(&dir).unwrap();
    for date in results.dates().unwrap() {
        // The log is the one the day's run made; reading it back gives that log.
        let input = days.load(&date).unwrap();
        let out = run_day(&date, &input, &setup(&host, &cost, &defs)).unwrap();
        assert_eq!(results.log(&date).unwrap().render(), out.log.render());
        assert!(!results.log(&date).unwrap().recs.is_empty());
        // The test strategies trace nothing but are asked: one trace each, the buy; then the host's counts and instruments.
        let traces = results.traces(&date).unwrap();
        assert_eq!(traces, out.traces);
        assert_eq!(
            traces
                .iter()
                .map(|t| (t.0, t.1.kind.as_str()))
                .collect::<Vec<_>>(),
            [
                (1, "buy"),
                (2, "buy"),
                (1, "stats"),
                (2, "stats"),
                (0, "instruments"),
                (0, "fills")
            ]
        );
        // What each strategy was allowed to do and refused, from the host's counts: a buy and a sell each, accepted.
        for (k, id) in [(2usize, 1u16), (3, 2)] {
            let st = &traces[k];
            assert_eq!(st.0, id);
            assert_eq!(
                (
                    st.1.value("accepted"),
                    st.1.value("rejected"),
                    st.1.value("refused")
                ),
                (Some("2"), Some("0"), Some("0"))
            );
        }
        assert_eq!(traces[0].1.columns[1], "last");
        // No evidence was asked for.
        assert!(results.evidence(&date).is_err());
    }
    assert!(results.log("2026-05-09").is_err());
}

#[test]
fn a_companion_of_another_day_or_run_or_a_damaged_one_is_refused_and_a_day_without_them_is_made_again()
 {
    use super::keep::wrap;
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("link-days", 3);
    let dir = out_dir("link-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let whole = read_all(&dir);
    let results = Results::open(&dir).unwrap();
    let (d1, d2) = ("2026-05-01", "2026-05-04");
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap();
    // The log of one day put where another's is.
    fs::write(dir.join(format!("{d2}.log")), read(&format!("{d1}.log"))).unwrap();
    assert!(
        results
            .log(d2)
            .unwrap_err()
            .to_string()
            .contains("another day")
    );
    // One altered letter.
    let t = read(&format!("{d1}.trace"));
    fs::write(dir.join(format!("{d1}.trace")), t.replacen("buy", "bux", 1)).unwrap();
    assert!(
        results
            .traces(d1)
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    // Another run's configuration, and another outcome, under a checksum that is right.
    let outcome = results.day(d1).unwrap().outcome_hash;
    let log_body = results.log(d1).unwrap().render();
    fs::write(
        dir.join(format!("{d1}.log")),
        wrap("research log", d1, 1, outcome, &log_body),
    )
    .unwrap();
    assert!(
        results
            .log(d1)
            .unwrap_err()
            .to_string()
            .contains("another configuration")
    );
    fs::write(
        dir.join(format!("{d1}.log")),
        wrap(
            "research log",
            d1,
            results.fingerprint(),
            outcome ^ 1,
            &log_body,
        ),
    )
    .unwrap();
    assert!(
        results
            .log(d1)
            .unwrap_err()
            .to_string()
            .contains("another outcome")
    );
    // A file of another kind, a cut one, one with text after its end, and none.
    fs::write(
        dir.join(format!("{d1}.log")),
        wrap(
            "research trace",
            d1,
            results.fingerprint(),
            outcome,
            &log_body,
        ),
    )
    .unwrap();
    assert!(
        results
            .log(d1)
            .unwrap_err()
            .to_string()
            .contains("not `research log v1`")
    );
    let good = wrap(
        "research log",
        d1,
        results.fingerprint(),
        outcome,
        &log_body,
    );
    fs::write(dir.join(format!("{d1}.log")), &good[..good.len() - 20]).unwrap();
    assert!(results.log(d1).is_err());
    fs::write(dir.join(format!("{d1}.log")), format!("{good}extra\n")).unwrap();
    assert!(
        results
            .log(d1)
            .unwrap_err()
            .to_string()
            .contains("after `end`")
    );
    fs::remove_file(dir.join(format!("{d1}.log"))).unwrap();
    assert!(results.log(d1).is_err());
    // A run goes on from the days whose companions are not whole, and leaves what an uninterrupted run left.
    days.loads = 0;
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!(
        (rep.ran.as_slice(), days.loads),
        (&[d1.to_owned(), d2.to_owned()][..], 2)
    );
    assert_eq!(rep.skipped, ["2026-05-05"]);
    assert_eq!(read_all(&dir), whole);
    // A day with no trace file is made again too.
    fs::remove_file(dir.join("2026-05-05.trace")).unwrap();
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!(rep.ran, ["2026-05-05"]);
    assert_eq!(read_all(&dir), whole);
}

#[test]
fn the_evidence_pass_keeps_the_market_around_each_trade_as_the_host_saw_it() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("ev-days", 3);
    let (a, b) = (out_dir("ev-a"), out_dir("ev-b"));
    let opts = RunOptions {
        evidence: Some(EvidenceWindow::default()),
    };
    let rep = run_with(&setup(&host, &cost, &defs), &mut days, &a, &opts).unwrap();
    assert_eq!((rep.ran.len(), rep.no_evidence.len()), (3, 0));
    run_with(&setup(&host, &cost, &defs), &mut days, &b, &opts).unwrap();
    // Two runs leave the same files, the compressed evidence included.
    assert_eq!(read_all(&a), read_all(&b));
    let results = Results::open(&a).unwrap();
    let date = "2026-05-01";
    let e = results.evidence(date).unwrap();
    assert_eq!(e.window, Some(EvidenceWindow::default()));
    // The two names the strategies traded, and nothing else.
    assert_eq!(e.symbols.keys().collect::<Vec<_>>(), ["S02", "S08"]);
    // Every event of those names in the windows, found independently from the day's files: the whole ten seconds of data, as
    // the windows reach far beyond them.
    let w = EvidenceWindow::default();
    let files = days.files(date);
    let trips = results.day(date).unwrap().trips;
    for sym in ["S02", "S08"] {
        let (from, to) = (
            trips
                .iter()
                .filter(|t| t.symbol == sym)
                .map(|t| t.entry_ts)
                .min()
                .unwrap()
                - w.before,
            trips
                .iter()
                .filter(|t| t.symbol == sym)
                .map(|t| t.exit_ts)
                .max()
                .unwrap()
                + w.after,
        );
        let want = independent(&files, sym, from, to);
        assert!(want.len() > 20, "{sym}: {}", want.len());
        assert_eq!(e.symbols[sym], want, "{sym}");
    }
    // The results are readable with the store gone.
    let root = days.files(date)[0]
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    fs::remove_dir_all(&root).unwrap();
    assert!(!root.exists());
    assert!(
        results.log(date).is_ok() && results.traces(date).is_ok() && results.evidence(date).is_ok()
    );
}

#[test]
fn evidence_is_made_when_asked_for_and_not_asked_for_again_and_a_day_made_without_it_is_made_again()
{
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("ask-days", 2);
    let dir = out_dir("ask-out");
    let with = RunOptions {
        evidence: Some(EvidenceWindow::default()),
    };
    // Without: no evidence file.
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert!(Results::open(&dir).unwrap().evidence("2026-05-01").is_err());
    // Asked for afterwards: the days are made again, and have it.
    let rep = run_with(&setup(&host, &cost, &defs), &mut days, &dir, &with).unwrap();
    assert_eq!((rep.ran.len(), rep.skipped.len()), (2, 0));
    assert!(Results::open(&dir).unwrap().evidence("2026-05-01").is_ok());
    // Asked for again: nothing to do. Not asked for: nothing to do either, and the evidence stays.
    let rep = run_with(&setup(&host, &cost, &defs), &mut days, &dir, &with).unwrap();
    assert_eq!((rep.ran.len(), rep.skipped.len()), (0, 2));
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!((rep.ran.len(), rep.skipped.len()), (0, 2));
    assert!(Results::open(&dir).unwrap().evidence("2026-05-04").is_ok());
    // A day damaged and made again without evidence leaves none from before.
    fs::write(dir.join("2026-05-04.log"), "damaged").unwrap();
    let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    assert_eq!(rep.ran, ["2026-05-04"]);
    assert!(Results::open(&dir).unwrap().evidence("2026-05-04").is_err());
    assert!(Results::open(&dir).unwrap().evidence("2026-05-01").is_ok());
}

#[test]
fn overlapping_windows_of_one_name_are_one_and_the_gaps_between_windows_are_not_kept() {
    let days = Days::new("merge-days", 1);
    let files = days.files("2026-05-01");
    let base = OPENS[0].1 * SEC;
    let w = EvidenceWindow {
        before: SEC / 2,
        after: SEC / 2,
    };
    // Trips of S02 at seconds 1 to 2 and 2 to 3 (their windows overlap: one), and at 8 to 9; and of S05 at 4 to 5.
    let trips = vec![
        trip_at("S02", base + SEC, base + 2 * SEC),
        trip_at("S02", base + 2 * SEC + SEC / 4, base + 3 * SEC),
        trip_at("S02", base + 8 * SEC, base + 9 * SEC),
        trip_at("S05", base + 4 * SEC, base + 5 * SEC),
    ];
    let e = super::keep::gather_evidence(&files, &trips, w).unwrap();
    let lo = |s: u64, f: u64| base + s * SEC - f;
    let mut want = independent(&files, "S02", lo(1, SEC / 2), lo(3, 0) + SEC / 2);
    want.extend(independent(
        &files,
        "S02",
        lo(8, SEC / 2),
        lo(9, 0) + SEC / 2,
    ));
    assert_eq!(e.symbols["S02"], want);
    assert!(e.symbols["S02"].iter().all(|ev| {
        let t = ev.ts();
        (t >= lo(1, SEC / 2) && t <= lo(3, 0) + SEC / 2)
            || (t >= lo(8, SEC / 2) && t <= lo(9, 0) + SEC / 2)
    }));
    // Nothing from between the two stretches, and no event twice.
    assert!(
        !e.symbols["S02"]
            .iter()
            .any(|ev| ev.ts() > lo(3, 0) + SEC / 2 && ev.ts() < lo(8, SEC / 2))
    );
    let mut times: Vec<_> = e.symbols["S02"]
        .iter()
        .map(|ev| format!("{ev:?}"))
        .collect();
    let n = times.len();
    times.sort();
    times.dedup();
    assert_eq!(times.len(), n);
    assert_eq!(
        e.symbols["S05"],
        independent(&files, "S05", lo(4, SEC / 2), lo(5, 0) + SEC / 2)
    );
    // A symbol the day does not have is not kept, and unreadable data is an error, not a shorter day.
    let none =
        super::keep::gather_evidence(&files, &[trip_at("NOPE", base, base + SEC)], w).unwrap();
    assert!(none.symbols.is_empty());
    let mut broken = files.clone();
    broken.insert(1, files[0].with_file_name("not-there.dbn.zst"));
    assert!(
        super::keep::gather_evidence(&broken, &trips, w)
            .unwrap_err()
            .contains("cannot be read")
    );
}

#[test]
fn a_trip_with_nothing_kept_around_it_is_named_and_a_slice_is_inclusive_at_both_ends() {
    let w = EvidenceWindow {
        before: 10,
        after: 10,
    };
    let q = |ts| EvEvent::Quote {
        ts,
        bid: 1,
        ask: 2,
        bid_sz: 3,
        ask_sz: 4,
    };
    let e = Evidence {
        window: Some(w),
        symbols: [("A".to_owned(), vec![q(100), q(105), q(110)])].into(),
    };
    assert_eq!(e.slice("A", 105, 110).len(), 2);
    assert_eq!(e.slice("A", 100, 100).len(), 1);
    assert_eq!(e.slice("A", 101, 104).len(), 0);
    assert_eq!(e.slice("B", 0, 1_000).len(), 0);
    // A trip from 100 to 110 has its window 90 to 120: it has events. One at 200 to 210 has none; so has one in a symbol absent.
    let trips = [
        trip_at("A", 100, 110),
        trip_at("A", 200, 210),
        trip_at("B", 100, 110),
    ];
    let missing = e.missing("2026-05-01", &trips, w);
    assert_eq!(
        missing,
        ["2026-05-01 A round1 200", "2026-05-01 B round1 100"]
    );
    // The edge of a window counts: a trip entering at 120 has its window from 110.
    assert!(e.missing("d", &[trip_at("A", 120, 130)], w).is_empty());
    assert_eq!(e.missing("d", &[trip_at("A", 121, 130)], w).len(), 1);
}

#[test]
fn the_text_of_the_evidence_and_of_a_companion_file_refuses_every_kind_of_damage() {
    use super::keep::{evidence_file, read_evidence, unwrap, wrap};
    let e = Evidence {
        window: Some(EvidenceWindow {
            before: 7,
            after: 9,
        }),
        symbols: [
            (
                "A".to_owned(),
                vec![
                    EvEvent::Trade {
                        ts: 1,
                        px: 5,
                        size: 6,
                    },
                    EvEvent::Quote {
                        ts: 2,
                        bid: 3,
                        ask: 4,
                        bid_sz: 5,
                        ask_sz: 6,
                    },
                    EvEvent::Status {
                        ts: 3,
                        kind: 2,
                        lo: -7,
                        hi: 8,
                    },
                ],
            ),
            ("B".to_owned(), vec![]),
        ]
        .into(),
    };
    let bytes = evidence_file("2026-05-01", 0xabc, 0xdef, &e);
    assert_eq!(
        read_evidence(&bytes, "2026-05-01", 0xabc, 0xdef).unwrap(),
        e
    );
    // The same evidence always gives the same bytes.
    assert_eq!(bytes, evidence_file("2026-05-01", 0xabc, 0xdef, &e));
    assert!(
        read_evidence(&bytes, "2026-05-02", 0xabc, 0xdef)
            .unwrap_err()
            .contains("another day")
    );
    assert!(
        read_evidence(&bytes, "2026-05-01", 1, 0xdef)
            .unwrap_err()
            .contains("another configuration")
    );
    assert!(
        read_evidence(&bytes, "2026-05-01", 0xabc, 1)
            .unwrap_err()
            .contains("another outcome")
    );
    assert!(
        read_evidence(b"plain text", "2026-05-01", 0xabc, 0xdef)
            .unwrap_err()
            .contains("not compressed")
    );
    assert!(
        read_evidence(&zstd::encode_all(&[0xff, 0xfe][..], 3).unwrap(), "d", 1, 1)
            .unwrap_err()
            .contains("not text")
    );
    // A body that is not evidence, under a checksum that is right.
    let ev = |body: &str| {
        let text = wrap("research evidence", "d", 1, 2, body);
        read_evidence(&zstd::encode_all(text.as_bytes(), 3).unwrap(), "d", 1, 2)
    };
    for (body, what) in [
        ("", "no `window` line"),
        ("window\tx\t1\n", "not a number"),
        ("window\t1\t1\nt\t1\t2\t3\n", "before any `symbol`"),
        ("window\t1\t1\nsymbol\tA\t1\n", "has 0 events, not 1"),
        (
            "window\t1\t1\nsymbol\tA\t0\nt\t1\t2\t3\n",
            "has 1 events, not 0",
        ),
        ("window\t1\t1\nsymbol\tA\t0\nsymbol\tA\t0\n", "twice"),
        ("window\t1\t1\nsymbol\tA\t1\nt\t1\n", "does not know"),
        ("window\t1\t1\nsymbol\tA\t1\nz\t1\t2\t3\n", "does not know"),
        ("window\t1\t1\nsymbol\tA\t1\nt\t1\tx\t3\n", "not a number"),
        (
            "window\t1\t1\nsymbol\tA\t1\nq\t1\t2\t3\t4\n",
            "does not know",
        ),
        ("window\t1\t1\nsymbol\tA\t1\ns\t1\t2\t3\n", "does not know"),
    ] {
        let err = ev(body).unwrap_err();
        assert!(err.contains(what), "{body:?}: {err}");
    }
    assert!(ev("window\t1\t1\n").unwrap().symbols.is_empty());
    // The wrapper: whole or refused.
    let good = wrap("k", "d", 1, 2, "body\n");
    assert_eq!(unwrap("k", &good, "d", 1, 2).unwrap(), "body\n");
    for (bad, what) in [
        (good.replacen("body", "bady", 1), "checksum"),
        (good.replace("end ", "ent "), "cut short"),
        (format!("{good}x\n"), "after `end`"),
        (good.replace("end ", "end zz"), "hexadecimal"),
    ] {
        assert!(
            unwrap("k", &bad, "d", 1, 2).unwrap_err().contains(what),
            "{what}"
        );
    }
    assert!(
        unwrap("other", &good, "d", 1, 2)
            .unwrap_err()
            .contains("not `other v1`")
    );
    // A value that says `end ` does not end the file.
    let tricky = wrap("k", "d", 1, 2, "the end of it\n");
    assert_eq!(unwrap("k", &tricky, "d", 1, 2).unwrap(), "the end of it\n");
}

#[test]
fn the_default_window_is_ten_minutes_before_and_two_after() {
    let w = EvidenceWindow::default();
    assert_eq!((w.before, w.after), (600 * SEC, 120 * SEC));
}

#[test]
fn an_event_exactly_at_the_edge_of_a_window_is_kept_at_both_ends() {
    let days = Days::new("edge-days", 1);
    let files = days.files("2026-05-01");
    let base = OPENS[0].1 * SEC;
    // S02's events in the fixture: a quote at the start of each second plus 2 ms, and trades just after it. Windows that begin and
    // end exactly on events: from the quote at second 2 to the quote at second 4.
    let (start, end) = (
        base + 2 * SEC + 2 * 1_000_000,
        base + 4 * SEC + 2 * 1_000_000,
    );
    let w = EvidenceWindow {
        before: 1_000,
        after: 1_000,
    };
    let trips = vec![trip_at("S02", start + w.before, end - w.after)];
    let e = super::keep::gather_evidence(&files, &trips, w).unwrap();
    let want = independent(&files, "S02", start, end);
    assert_eq!(e.symbols["S02"], want);
    assert_eq!(e.symbols["S02"].first().map(EvEvent::ts), Some(start));
    assert_eq!(e.symbols["S02"].last().map(EvEvent::ts), Some(end));
    // One nanosecond either side loses the edge events.
    let tight = vec![trip_at("S02", start + w.before + 1, end - w.after - 1)];
    let t = super::keep::gather_evidence(&files, &tight, w).unwrap();
    assert!(
        t.symbols["S02"]
            .iter()
            .all(|ev| ev.ts() > start && ev.ts() < end)
    );
    assert_eq!(t.symbols["S02"].len(), want.len() - 2);
}

#[test]
fn a_companion_whose_trailer_is_gone_is_cut_short_not_misread_from_a_body_that_says_end() {
    use super::keep::{unwrap, wrap};
    let tricky = wrap("k", "d", 1, 2, "the end of it\n");
    let cut = &tricky[..tricky.rfind("end ").unwrap()];
    assert!(cut.contains("the end of it"));
    assert!(
        unwrap("k", cut, "d", 1, 2)
            .unwrap_err()
            .contains("cut short")
    );
}

// ---- the data of the backtest view (E19-S34) ----

/// A strict little JSON reader for the tests: that what the view says is JSON, and what is in it.
#[derive(Debug, Clone, PartialEq)]
enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn get(&self, k: &str) -> &J {
        match self {
            J::Obj(v) => v
                .iter()
                .find(|(x, _)| x == k)
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("no `{k}` in {self:?}")),
            _ => panic!("not an object: {self:?}"),
        }
    }
    fn s(&self) -> &str {
        match self {
            J::Str(s) | J::Num(s) => s,
            other => panic!("not text: {other:?}"),
        }
    }
    fn arr(&self) -> &[J] {
        match self {
            J::Arr(v) => v,
            other => panic!("not an array: {other:?}"),
        }
    }
}

fn json(text: &str) -> J {
    fn ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && matches!(b[*i], b' ' | b'\n' | b'\t' | b'\r') {
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
                    b'/' => out.push(b'/'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let h = std::str::from_utf8(&b[*i + 1..*i + 5]).unwrap();
                        let c = char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap();
                        out.extend_from_slice(c.to_string().as_bytes());
                        *i += 4;
                    }
                    other => panic!("bad escape {}", other as char),
                }
            } else {
                assert!(b[*i] >= 0x20, "a raw control character in a string");
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
                        c => panic!("expected , or }} but {}", c as char),
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
                        c => panic!("expected , or ] but {}", c as char),
                    }
                }
            }
            b'"' => J::Str(string(b, i)),
            b't' | b'f' | b'n' => {
                let rest = &b[*i..];
                for (w, j) in [
                    ("true", J::Bool(true)),
                    ("false", J::Bool(false)),
                    ("null", J::Null),
                ] {
                    if rest.starts_with(w.as_bytes()) {
                        *i += w.len();
                        return j;
                    }
                }
                panic!("a bad word");
            }
            _ => {
                let start = *i;
                while *i < b.len()
                    && (b[*i].is_ascii_digit() || matches!(b[*i], b'-' | b'.' | b'e' | b'E' | b'+'))
                {
                    *i += 1;
                }
                assert!(
                    *i > start,
                    "not a value at {start}: {}",
                    String::from_utf8_lossy(&b[start..(start + 20).min(b.len())])
                );
                J::Num(String::from_utf8_lossy(&b[start..*i]).into_owned())
            }
        }
    }
    let b = text.as_bytes();
    let mut i = 0;
    let v = value(b, &mut i);
    ws(b, &mut i);
    assert_eq!(i, b.len(), "text after the value");
    v
}

/// A root holding one scenario (a run of the three-day fixture), and what it was made of.
fn view_root(name: &str, scenario: &str) -> PathBuf {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new(&format!("{name}-days"), 3);
    let root = scratch(&format!("{name}-root"));
    run(&setup(&host, &cost, &defs), &mut days, &root.join(scenario)).unwrap();
    root
}

#[test]
fn what_the_view_writes_is_json_with_text_escaped() {
    use super::view::{bp, dollars, et, js, milli, price, reason_text, valid_name};
    // The strings: quotes, backslashes, controls, and a `<` kept out of a page.
    assert_eq!(
        js("a\"b\\c\nd\te\u{1}<f/>"),
        r#""a\"b\\c\nd\te\u0001\u003cf/>""#
    );
    assert_eq!(
        json(&js("a\"b\\c\nd\te\u{1}<f/>")),
        J::Str("a\"b\\c\nd\te\u{1}<f/>".into())
    );
    // A space stays a space, and the last control character is escaped.
    assert_eq!(js(" \u{1f}\u{7f}é"), "\" \\u001f\u{7f}é\"");
    // Money and prices, rounded half away from zero, and signed only when something is left.
    assert_eq!(dollars(-6_182_038_200), "-6.18");
    assert_eq!(dollars(5_000_000), "0.01");
    assert_eq!(dollars(4_999_999), "0.00");
    assert_eq!(dollars(-4_999_999), "0.00");
    assert_eq!(dollars(123_456_789_000), "123.46");
    assert_eq!(price(20_010_000_000), "20.0100");
    assert_eq!(price(-50_000), "-0.0001");
    assert_eq!(price(49_999), "0.0000");
    assert_eq!(price(-49_999), "0.0000");
    assert_eq!(price(-50_000), "-0.0001");
    // Hundredths of a basis point: -1029 is -10.29.
    assert_eq!(
        (
            bp(-1029).as_str(),
            bp(5).as_str(),
            bp(0).as_str(),
            bp(155).as_str()
        ),
        ("-10.29", "0.05", "0.00", "1.55")
    );
    assert_eq!(bp(i64::MIN), "-92233720368547758.08");
    // Thousandths of R.
    assert_eq!(
        (
            milli(-2).as_str(),
            milli(0).as_str(),
            milli(1500).as_str(),
            milli(-1000).as_str()
        ),
        ("-0.002", "0.000", "1.500", "-1.000")
    );
    // The time is New York's, with milliseconds: 20:00 UTC in May is 16:00.
    let close = tf_calendar::Calendar::us_equities()
        .times(tf_calendar::Date::new(2026, 5, 1).unwrap())
        .unwrap()
        .unwrap()
        .close;
    assert_eq!(et(close - 1800 * SEC + 50_000_000), "15:30:00.050");
    assert_eq!(et(close - 30 * SEC), "15:59:30.000");
    // What the exit reasons mean.
    assert_eq!(
        [0xE501u16, 0xE502, 0xE503, 0xFFFF, 7].map(reason_text),
        [
            "stop",
            "target",
            "time exit",
            "still open at the end of the day",
            "strategy code 7"
        ]
    );
    // A scenario is one plain directory name.
    for ok in ["a", "run-1_x.y", "2026"] {
        assert!(valid_name(ok), "{ok}");
    }
    for bad in [
        "", ".", "..", ".hidden", "a/b", "a\\b", "a b", "../x", "a\0", "é",
    ] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    assert!(!valid_name(&"x".repeat(101)) && valid_name(&"x".repeat(100)));
}

#[test]
fn a_scenario_is_listed_with_its_days_strategies_costs_and_budgets() {
    use super::view::scenarios_json;
    let root = view_root("list", "first");
    // A second scenario that is not one (no configuration) and a file are not listed; a damaged one is, with its error.
    fs::create_dir_all(root.join("not-a-scenario")).unwrap();
    fs::write(root.join("loose.txt"), "x").unwrap();
    fs::create_dir_all(root.join("broken")).unwrap();
    fs::write(root.join("broken").join(CONFIG_FILE), "garbage").unwrap();
    let j = json(&scenarios_json(&root).unwrap());
    let all = j.get("scenarios").arr();
    assert_eq!(
        all.iter().map(|s| s.get("name").s()).collect::<Vec<_>>(),
        ["broken", "first"]
    );
    assert!(!all[0].get("error").s().is_empty());
    let s = &all[1];
    assert_eq!(
        s.get("days").arr().iter().map(J::s).collect::<Vec<_>>(),
        ["2026-05-01", "2026-05-04", "2026-05-05"]
    );
    // Three days of two strategies, a trade each: six trades of -2.0606794 dollars.
    assert_eq!((s.get("trades").s(), s.get("net").s()), ("6", "-12.36"));
    let cost = s.get("cost");
    assert_eq!(
        (
            cost.get("latency_ms").s(),
            cost.get("borrow_bps_per_year").s(),
            cost.get("sec_through").s(),
            cost.get("taf_through").s()
        ),
        ("50", "0", "2026-09-30", "2027-12-31")
    );
    let strategies = s.get("strategies").arr();
    assert_eq!(strategies.len(), 2);
    for (st, (id, name, params, universe)) in strategies.iter().zip([
        ("1", "round1", "buy_at 2 sell_at 5", "adv_shares <= 600"),
        ("2", "round2", "buy_at 3 sell_at 6", "adv_shares >= 700"),
    ]) {
        assert_eq!(
            (st.get("id").s(), st.get("name").s(), st.get("params").s()),
            (id, name, params)
        );
        assert!(
            st.get("universe").s().contains(universe),
            "{}",
            st.get("universe").s()
        );
        assert_eq!(st.get("variant").s().len(), 16);
        // Three trades of -10.29 basis points, and a budget of half the $100,000 the tree divides.
        assert_eq!(
            (
                st.get("trades").s(),
                st.get("net").s(),
                st.get("mean_bp").s(),
                st.get("budget").s()
            ),
            ("3", "-6.18", "-10.29", "50000.00")
        );
    }
    // The tree: one group with everything, the loss limits of the test configuration, the two strategies at half each.
    let b = s.get("budgets");
    assert_eq!(
        (b.get("balance").s(), b.get("unassigned_bp").s()),
        ("100000.00", "0")
    );
    let g = &b.get("groups").arr()[0];
    assert_eq!(
        (
            g.get("id").s(),
            g.get("share_bp").s(),
            g.get("budget").s(),
            g.get("soft_bp").s(),
            g.get("hard_bp").s()
        ),
        ("g", "10000", "100000.00", "300", "600")
    );
    let members = g.get("strategies").arr();
    assert_eq!(
        members
            .iter()
            .map(|m| (m.get("id").s(), m.get("number").s(), m.get("share_bp").s()))
            .collect::<Vec<_>>(),
        [("s1", "1", "5000"), ("s2", "2", "5000")]
    );
    // A root that is not there is an error, and one with nothing in it lists nothing.
    assert!(scenarios_json(&root.join("nope")).is_err());
    let empty = scratch("list-empty");
    fs::create_dir_all(&empty).unwrap();
    assert_eq!(
        json(&scenarios_json(&empty).unwrap())
            .get("scenarios")
            .arr()
            .len(),
        0
    );
}

#[test]
fn a_strategys_day_is_its_trades_and_what_it_was_refused() {
    use super::view::{ViewError, trades_json};
    let root = view_root("trades", "s");
    let j = json(&trades_json(&root, "s", "2026-05-04", 1).unwrap());
    assert_eq!(
        (j.get("scenario").s(), j.get("day").s()),
        ("s", "2026-05-04")
    );
    let st = j.get("strategy");
    assert_eq!((st.get("id").s(), st.get("name").s()), ("1", "round1"));
    // A buy and a sell, accepted; nothing refused, nothing the gateway turned away.
    assert_eq!(
        (
            j.get("accepted").s(),
            j.get("rejected").s(),
            j.get("refused").s()
        ),
        ("2", "0", "0")
    );
    assert!(j.get("rejections").arr().is_empty());
    let t = &j.get("trades").arr()[0];
    assert_eq!(j.get("trades").arr().len(), 1);
    // Bought at the ask 20.01 and sold at the bid 19.99, 100 shares of S02, in New York time (13:30 UTC on 4 May is 09:30).
    assert_eq!(
        (
            t.get("n").s(),
            t.get("symbol").s(),
            t.get("side").s(),
            t.get("qty").s()
        ),
        ("0", "S02", "long", "100")
    );
    assert_eq!(
        (t.get("entry_px").s(), t.get("exit_px").s()),
        ("20.0100", "19.9900")
    );
    assert!(
        t.get("entry").s().starts_with("09:30:0") && t.get("exit").s().starts_with("09:30:0"),
        "{}",
        t.get("entry").s()
    );
    assert_eq!(
        (
            t.get("net").s(),
            t.get("bp").s(),
            t.get("exit_reason").s(),
            t.get("open_at_end"),
            t.get("r").s()
        ),
        ("-2.06", "-10.29", "time exit", &J::Bool(false), "-0.002")
    );
    // The other strategy is in its own symbol.
    let k = json(&trades_json(&root, "s", "2026-05-04", 2).unwrap());
    assert_eq!(k.get("trades").arr()[0].get("symbol").s(), "S08");
    // Not found: another day, another strategy, another scenario, a name that is not one, a day that is not a date.
    let nf = |r: Result<String, ViewError>| matches!(r, Err(ViewError::NotFound(_)));
    assert!(nf(trades_json(&root, "s", "2026-05-02", 1)));
    assert!(nf(trades_json(&root, "s", "2026-05-04", 9)));
    assert!(nf(trades_json(&root, "other", "2026-05-04", 1)));
    for bad in ["..", "../s", "s/..", "", ".s"] {
        assert!(nf(trades_json(&root, bad, "2026-05-04", 1)), "{bad}");
    }
    assert!(nf(trades_json(&root, "s", "../../x", 1)));
    assert!(nf(trades_json(&root, "s", "2026-5-4", 1)));
    // A day whose companions are gone cannot be shown, and says so.
    fs::remove_file(root.join("s").join("2026-05-04.trace")).unwrap();
    assert!(matches!(
        trades_json(&root, "s", "2026-05-04", 1),
        Err(ViewError::Refused(_))
    ));
}

#[test]
fn what_the_gateway_turned_away_is_counted_with_its_reasons() {
    use super::view::trades_json;
    // 100,000 shares at $20 is far past the order limit: the gateway refuses the buy and the sell finds nothing to close.
    let (host, cost) = (host_cfg(), CostModel::published());
    let defs = [round_trip_of(1, 100_000, LOW, 2, 5)];
    let mut days = Days::new("turn-days", 1);
    let root = scratch("turn-root");
    run(&setup(&host, &cost, &defs), &mut days, &root.join("s")).unwrap();
    let j = json(&trades_json(&root, "s", "2026-05-01", 1).unwrap());
    assert_eq!(
        (
            j.get("accepted").s(),
            j.get("rejected").s(),
            j.get("refused").s()
        ),
        ("0", "2", "0")
    );
    assert!(j.get("trades").arr().is_empty());
    let reasons: Vec<(String, String)> = j
        .get("rejections")
        .arr()
        .iter()
        .map(|r| {
            (
                r.get("reason").s().to_owned(),
                r.get("count").s().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        reasons
            .iter()
            .map(|r| r.1.parse::<u64>().unwrap())
            .sum::<u64>(),
        2
    );
    assert!(reasons.iter().all(|r| !r.0.is_empty()), "{reasons:?}");
}

#[test]
fn each_strategys_day_of_a_scenario_is_a_backtest_run_read_from_the_days_ledger() {
    use super::view::{ViewError, catalog_runs, replay_ledger, scenarios_json};
    use tf_catalog::{Kind, Source};
    let root = view_root("cat", "alpha");
    let results = Results::open(&root.join("alpha")).unwrap();
    let runs = catalog_runs(&root).unwrap();
    // Three days of two strategies, one trade each, named as a live ledger names them: by the budget tree.
    assert_eq!(runs.len(), 6);
    for r in &runs {
        assert_eq!((r.kind, r.trades), (Kind::Backtest, Some(1)));
        assert!(
            ["s1", "s2"].contains(&r.strategy.as_str()),
            "{}",
            r.strategy
        );
        assert_eq!(r.budget, Some(50_000 * 1_000_000_000));
        assert!(r.replayed() && !r.explorable());
        let Source::Replay { scenario, day } = &r.source else {
            panic!("not a replayed day")
        };
        assert_eq!(scenario, "alpha");
        // The ledger's profit is the trip's gross.
        let n: u16 = r.strategy[1..].parse().unwrap();
        let trips = results.day(day).unwrap().trips;
        let mine: Vec<&Trip> = trips.iter().filter(|t| t.strategy == n).collect();
        assert_eq!(
            r.net_pnl,
            Some(mine.iter().map(|t| i128::from(t.gross)).sum())
        );
    }
    let mut days: Vec<String> = runs
        .iter()
        .map(|r| match &r.source {
            Source::Replay { day, .. } => day.clone(),
            _ => unreachable!(),
        })
        .collect();
    days.sort();
    days.dedup();
    assert_eq!(days, ["2026-05-01", "2026-05-04", "2026-05-05"]);
    // Both strategies of a day share its source: a day is one source, whichever strategy.
    assert!(runs.chunks(2).all(|pair| pair.len() == 2
        && pair[0].source == pair[1].source
        && pair[0].strategy != pair[1].strategy));
    // A scenario that does not read is left out of the catalog, not a failure of it.
    fs::create_dir_all(root.join("broken")).unwrap();
    fs::write(root.join("broken").join(CONFIG_FILE), "garbage").unwrap();
    assert_eq!(catalog_runs(&root).unwrap().len(), 6);
    // A day whose ledger is damaged is left out and says why; a day made before ledgers were kept says that instead.
    let log = root
        .join("alpha")
        .join("2026-05-04.ledger")
        .join("ledger.log");
    let bytes = fs::read(&log).unwrap();
    fs::write(&log, &bytes[..bytes.len() - 9]).unwrap();
    fs::remove_dir_all(root.join("alpha").join("2026-05-05.ledger")).unwrap();
    assert_eq!(catalog_runs(&root).unwrap().len(), 2);
    let j = json(&scenarios_json(&root).unwrap());
    let alpha = &j.get("scenarios").arr()[0];
    let ledgers = alpha.get("ledgers").arr();
    assert_eq!(
        ledgers
            .iter()
            .map(|l| (l.get("day").s(), l.get("ok")))
            .collect::<Vec<_>>(),
        [
            ("2026-05-01", &J::Bool(true)),
            ("2026-05-04", &J::Bool(false)),
            ("2026-05-05", &J::Bool(false))
        ]
    );
    assert!(
        ledgers[1]
            .get("error")
            .s()
            .contains("not the one the day's report was made with"),
        "{:?}",
        ledgers[1]
    );
    assert!(
        ledgers[2]
            .get("error")
            .s()
            .contains("made before ledgers were kept"),
        "{:?}",
        ledgers[2]
    );
    // Asked for, a good day's ledger is its directory; the others are refused with those reasons.
    assert_eq!(
        replay_ledger(&root, "alpha", "2026-05-01").unwrap(),
        root.join("alpha").join("2026-05-01.ledger")
    );
    assert!(
        matches!(replay_ledger(&root, "alpha", "2026-05-04"), Err(ViewError::Refused(m)) if m.contains("report was made with"))
    );
    assert!(
        matches!(replay_ledger(&root, "alpha", "2026-05-05"), Err(ViewError::NotFound(m)) if m.contains("before ledgers were kept"))
    );
    assert!(matches!(
        replay_ledger(&root, "alpha", "2026-05-02"),
        Err(ViewError::NotFound(_))
    ));
    assert!(matches!(
        replay_ledger(&root, "..", "2026-05-01"),
        Err(ViewError::NotFound(_))
    ));
}

#[test]
fn the_budget_tree_is_kept_in_the_configuration_and_read_back_and_a_run_without_one_has_none() {
    let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
    let mut days = Days::new("bud-days", 1);
    let dir = out_dir("bud-out");
    run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
    let r = Results::open(&dir).unwrap();
    let b = r.budgets().unwrap().unwrap();
    assert_eq!(b.balance, 100_000 * 1_000_000_000);
    assert_eq!(
        b.ids
            .iter()
            .map(|(n, s)| (*n, s.as_str()))
            .collect::<Vec<_>>(),
        [(1, "s1"), (2, "s2")]
    );
    assert_eq!(b.tree, host.budgets.as_ref().unwrap().tree().clone());
    // The definitions with their numbers, parameters and universes.
    let lines = r.definition_lines().unwrap();
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.id, l.name.as_str(), l.params.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, "round1", "buy_at 2 sell_at 5"),
            (2, "round2", "buy_at 3 sell_at 6")
        ]
    );
    assert!(lines[0].universe.contains("adv_shares <= 600") && lines[0].universe.contains('\n'));
    assert_eq!(lines[0].fingerprint, defs[0].fingerprint());
    // Without budgets: none, and the configuration is another configuration.
    let bare = HostConfig {
        budgets: None,
        ..host_cfg()
    };
    let dir2 = out_dir("bud-out2");
    run(&setup(&bare, &cost, &defs), &mut days, &dir2).unwrap();
    assert_eq!(Results::open(&dir2).unwrap().budgets().unwrap(), None);
    assert_ne!(
        Results::open(&dir).unwrap().fingerprint(),
        Results::open(&dir2).unwrap().fingerprint()
    );
}

#[test]
fn strategies_of_different_sizes_keep_their_own_trades_and_scenario_directories_are_found_only_by_name()
 {
    use super::view::{catalog_runs, scenario_dir, scenarios_json, trades_json};
    let (host, cost) = (host_cfg(), CostModel::published());
    let defs = [
        round_trip_of(1, 100, LOW, 2, 5),
        round_trip_of(2, 300, HIGH, 3, 6),
    ];
    let mut days = Days::new("size-days", 3);
    let root = scratch("size-root");
    run(&setup(&host, &cost, &defs), &mut days, &root.join("sz")).unwrap();
    let j = json(&scenarios_json(&root).unwrap());
    let st = j.get("scenarios").arr()[0].get("strategies").arr().to_vec();
    assert_eq!(st[0].get("net").s(), "-6.18");
    assert_ne!(st[1].get("net").s(), "-6.18");
    assert_eq!(
        (st[0].get("trades").s(), st[1].get("trades").s()),
        ("3", "3")
    );
    // The days' ledgers: each strategy's session has its own size's profit (a hundred shares lose $2 a day, three hundred $6).
    let runs = catalog_runs(&root).unwrap();
    assert_eq!(runs.len(), 6);
    for r in &runs {
        let want = if r.strategy == "s1" {
            -2_000_000_000
        } else {
            -6_000_000_000
        };
        assert_eq!(r.net_pnl, Some(want), "{}", r.strategy);
    }
    // Each strategy's day is its own size.
    let (a, b) = (
        json(&trades_json(&root, "sz", "2026-05-01", 1).unwrap()),
        json(&trades_json(&root, "sz", "2026-05-01", 2).unwrap()),
    );
    assert_eq!(
        (
            a.get("trades").arr()[0].get("qty").s(),
            b.get("trades").arr()[0].get("qty").s()
        ),
        ("100", "300")
    );
    // A directory is a scenario only if it is named plainly and holds a configuration.
    assert_eq!(scenario_dir(&root, "sz"), Some(root.join("sz")));
    fs::create_dir_all(root.join("plain")).unwrap();
    assert_eq!(scenario_dir(&root, "plain"), None, "no configuration in it");
    assert_eq!(scenario_dir(&root, "nope"), None);
    // A configuration reached by a name that is not plain: `..` from inside a scenario, and a hidden directory.
    fs::copy(
        root.join("sz").join(CONFIG_FILE),
        root.join("sz").join("again.cfg"),
    )
    .unwrap();
    fs::create_dir_all(root.join(".hid")).unwrap();
    fs::copy(
        root.join("sz").join(CONFIG_FILE),
        root.join(".hid").join(CONFIG_FILE),
    )
    .unwrap();
    for bad in ["..", ".", ".hid", "sz/../sz", "sz/"] {
        assert_eq!(scenario_dir(&root, bad), None, "{bad}");
    }
    assert_eq!(scenario_dir(&root.join("sz"), ".."), None);
}

#[test]
fn what_the_broker_and_the_rate_limit_turned_away_is_one_count() {
    use crate::host::StrategyStats;
    let st = StrategyStats {
        refused_by_broker: 2,
        rate_limited: 3,
        rejected_by_gateway: 7,
        ..StrategyStats::default()
    };
    assert_eq!(super::run::refusals(&st), 5);
    assert_eq!(super::run::refusals(&StrategyStats::default()), 0);
}

#[test]
fn the_chart_keeps_every_event_near_the_trade_and_thins_the_rest_evenly() {
    use super::trade::{px4, thin};
    // 1e-4 dollars, rounded half up.
    assert_eq!(
        (
            px4(20_010_000_000),
            px4(49_999),
            px4(50_000),
            px4(-50_000),
            px4(0)
        ),
        (200_100, 0, 1, 0, 0)
    );
    // At or under the cap nothing is dropped.
    let few: Vec<u64> = (0..100).collect();
    assert_eq!(thin(&few, |e| *e, 40, 60, 100), few);
    // Over it: all of those inside [40_000, 41_000] stay, the others are thinned, order and the newest event are kept.
    let many: Vec<u64> = (0..100_000).collect();
    let kept = thin(&many, |e| *e, 40_000, 41_000, 5_000);
    assert!(kept.len() <= 5_000 && kept.len() > 1_000, "{}", kept.len());
    assert!(kept.windows(2).all(|w| w[0] < w[1]));
    assert!(
        (40_000..=41_000).all(|e| kept.contains(&e)),
        "every event near the trade is kept"
    );
    assert!(
        kept.iter().any(|&e| e > 99_000),
        "the end of the day is kept"
    );
    let outside = kept
        .iter()
        .filter(|&&e| !(40_000..=41_000).contains(&e))
        .count();
    assert!(outside < 4_000 && outside > 500, "{outside}");
    // Too many even near the trade: thinned to the cap.
    let crowded = thin(&many, |e| *e, 0, u64::MAX, 1_000);
    assert!(
        crowded.len() <= 1_000 && crowded.len() > 400,
        "{}",
        crowded.len()
    );
}

#[test]
fn a_long_trace_shows_its_first_rows_and_every_row_about_the_symbol() {
    use super::trade::trace_json;
    use tf_strategy::trace::Trace;
    let mut t = Trace::new(0, "rank").with_columns(&["rank", "symbol", "bid"]);
    for i in 0..100 {
        t.push_row(vec![
            i.to_string(),
            format!("S{i:02}"),
            "20010000000".into(),
        ]);
    }
    let j = json(&trace_json(&t, "S77"));
    assert_eq!(j.get("total").s(), "100");
    let rows = j.get("rows").arr();
    // The first twenty, and the one about S77; each says whether it is about the symbol; the price column is dollars.
    assert_eq!(rows.len(), 21);
    assert_eq!(rows[19].arr()[0].s(), "19");
    assert_eq!(rows[20].arr()[0].s(), "77");
    assert_eq!(
        rows.iter()
            .map(|r| r.arr()[1].s())
            .filter(|m| *m == "1")
            .count(),
        1
    );
    assert_eq!(rows[20].arr()[1].s(), "1");
    assert_eq!(rows[20].arr()[2].arr()[2].s(), "20.0100");
    // A short one is whole.
    let mut short = Trace::new(0, "draw").with_columns(&["symbol"]);
    for i in 0..40 {
        short.push_row(vec![format!("S{i}")]);
    }
    assert_eq!(
        json(&trace_json(&short, "none")).get("rows").arr().len(),
        40
    );
    short.push_row(vec!["S40".into()]);
    assert_eq!(json(&trace_json(&short, "S40")).get("rows").arr().len(), 21);
    // No symbol column: no row is marked; a head value that is a time says the time.
    let mut plain = Trace::new(0, "x")
        .with("close", 1_777_924_800_000_000_000u64)
        .with("n", 5);
    plain.columns = vec!["a".into()];
    plain.push_row(vec!["1".into()]);
    let p = json(&trace_json(&plain, "S1"));
    assert_eq!(p.get("rows").arr()[0].arr()[1].s(), "0");
    let head: Vec<(String, String)> = p
        .get("head")
        .arr()
        .iter()
        .map(|h| (h.arr()[0].s().to_owned(), h.arr()[1].s().to_owned()))
        .collect();
    assert_eq!(head[0].0, "close");
    assert!(
        head[0].1.contains(':') && head[0].1.ends_with(".000"),
        "{head:?}"
    );
    assert_eq!(head[1], ("n".to_owned(), "5".to_owned()));
}

#[test]
fn the_two_fees_of_a_sale_are_its_total() {
    let c = CostModel::published();
    for (q, px) in [
        (1u32, 1_000_000_000i64),
        (99, 20_010_000_000),
        (100_000, 5_000_000_000),
    ] {
        let (a, b) = c.sale_fee_parts("2026-05-04", q, px).unwrap();
        assert_eq!(a + b, c.sale_fees("2026-05-04", q, px).unwrap());
        assert!(a > 0 && b > 0);
    }
    assert!(c.sale_fee_parts("2099-01-01", 1, 1).is_err());
}

// ---- the pieces of a trade's page (E19-S35) ----

mod trade_parts {
    use super::super::trade::{
        Dec, Leg, fee_parts, instrument_of, legs_json, legs_of, listed_in, marks_of, previous_exit,
        tenths_pct, thin, trade_recs,
    };
    use super::*;
    use crate::equiv::{Answer, Rec};
    use tf_strategy::trace::Trace;

    fn dec(
        ts: Nanos,
        strategy: u16,
        instrument: u32,
        side: Side,
        purpose: Purpose,
        answer: Answer,
    ) -> Rec {
        Rec::Decision {
            idx: 0,
            ts,
            strategy,
            seq: 0,
            instrument,
            side,
            qty: 100,
            purpose,
            limit: 20_100_000_000,
            reason: 0xE503,
            answer,
        }
    }

    fn fill(ts: Nanos, order: u64, instrument: u32, qty: u32, px: i64) -> Rec {
        Rec::Fill {
            idx: 0,
            ts,
            order,
            instrument,
            qty,
            px,
        }
    }

    fn acc(o: u64) -> Answer {
        Answer::Accepted(o)
    }

    fn trip(symbol: &str, long: bool, qty: u32, entry_ts: Nanos, exit_ts: Nanos) -> Trip {
        Trip {
            day: "2026-05-04".into(),
            strategy: 1,
            name: "t".into(),
            variant: 1,
            symbol: symbol.into(),
            long,
            qty,
            entry_ts,
            entry_px: 20_010_000_000,
            exit_ts,
            exit_px: 19_400_000_000,
            gross: 0,
            fees: 0,
            borrow: 0,
            slippage: 0,
            net: 0,
            net_bps_x100: 0,
            slip_bps_x100: 0,
            r_milli: None,
            entry_reason: 0,
            exit_reason: 0xE503,
            open_at_end: false,
        }
    }

    const MS: Nanos = 1_000_000;

    fn log() -> Vec<Rec> {
        use Purpose::{Close, Open};
        vec![
            dec(10, 1, 5, Side::Buy, Open, acc(1)),
            fill(15, 1, 5, 100, 20_010_000_000),
            dec(20, 1, 5, Side::Sell, Close, acc(2)),
            fill(25, 2, 5, 100, 19_990_000_000),
            // Not this strategy's, not this instrument's, not accepted.
            dec(30, 2, 5, Side::Buy, Open, acc(8)),
            dec(31, 1, 6, Side::Buy, Open, acc(9)),
            fill(32, 8, 5, 5, 1),
            fill(33, 9, 6, 5, 1),
            dec(
                100,
                1,
                5,
                Side::Buy,
                Open,
                Answer::Rejected("no_budget".into()),
            ),
            dec(110, 1, 5, Side::Buy, Open, acc(3)),
            fill(120, 3, 5, 100, 20_000_000_000),
            dec(130, 1, 5, Side::Sell, Close, acc(4)),
            fill(140, 4, 5, 100, 19_900_000_000),
        ]
    }

    #[test]
    fn the_orders_of_a_trade_are_the_strategys_decisions_in_the_instrument_between_its_trades() {
        let recs = log();
        let ts = |d: &[Dec]| d.iter().map(|d| d.ts).collect::<Vec<_>>();
        let orders = |f: &[(Nanos, u64, u32, i64)]| f.iter().map(|f| f.1).collect::<Vec<_>>();
        // The first trade: everything of the strategy's in the instrument to its exit, and the fills of those orders only.
        let (d, f) = trade_recs(&recs, 1, 5, None, 25);
        assert_eq!((ts(&d), orders(&f)), (vec![10, 20], vec![1, 2]));
        // The second: after the first ended, with the refusal, to its exit decision (inclusive).
        let (d, f) = trade_recs(&recs, 1, 5, Some(25), 130);
        assert_eq!(ts(&d), [100, 110, 130]);
        assert_eq!(orders(&f), [3, 4]);
        assert_eq!((d[0].order, d[0].why.as_deref()), (None, Some("no_budget")));
        assert_eq!((d[1].order, d[1].why.clone()), (Some(3), None));
        // The edges: a decision at the previous exit's time is the previous trade's; one at the exit's time is this one's.
        assert_eq!(
            ts(&trade_recs(&recs, 1, 5, Some(20), 25).0),
            Vec::<Nanos>::new()
        );
        assert_eq!(ts(&trade_recs(&recs, 1, 5, Some(19), 25).0), [20]);
        assert_eq!(ts(&trade_recs(&recs, 1, 5, None, 20).0), [10, 20]);
        assert_eq!(ts(&trade_recs(&recs, 1, 5, None, 19).0), [10]);
        // Another strategy's or instrument's orders are not here.
        assert_eq!(ts(&trade_recs(&recs, 2, 5, None, 1000).0), [30]);
        assert_eq!(ts(&trade_recs(&recs, 1, 6, None, 1000).0), [31]);
        // The decision keeps what it said.
        let (d, _) = trade_recs(&recs, 1, 5, None, 25);
        assert_eq!(
            (d[0].side, d[0].purpose, d[0].qty, d[0].limit, d[0].reason),
            (Side::Buy, Purpose::Open, 100, 20_100_000_000, 0xE503)
        );
    }

    #[test]
    fn a_trade_follows_the_last_earlier_trade_in_the_same_symbol() {
        let (a, b, c, d) = (
            trip("A", true, 1, 1, 50),
            trip("B", true, 1, 1, 90),
            trip("A", true, 1, 60, 70),
            trip("A", true, 1, 80, 200),
        );
        let mine = [&a, &b, &c, &d];
        assert_eq!(previous_exit(&mine, 3, "A"), Some(70));
        assert_eq!(previous_exit(&mine, 3, "B"), Some(90));
        assert_eq!(previous_exit(&mine, 1, "A"), Some(50));
        assert_eq!(previous_exit(&mine, 3, "C"), None);
        assert_eq!(previous_exit(&mine, 0, "A"), None);
    }

    #[test]
    fn the_symbols_instrument_is_read_from_the_hosts_own_trace_and_no_other() {
        let book = |pairs: &[(&str, &str)]| {
            let mut t = Trace::new(0, "instruments").with_columns(&["instrument", "symbol"]);
            for (i, s) in pairs {
                t.push_row(vec![(*i).into(), (*s).into()]);
            }
            t
        };
        let traces = vec![
            // A strategy's trace of the same kind is not the host's.
            (1, book(&[("9", "A")])),
            (0, Trace::new(0, "stats")),
            (0, book(&[("5", "A"), ("6", "B"), ("x", "C")])),
        ];
        assert_eq!(instrument_of(&traces, "A"), Some(5));
        assert_eq!(instrument_of(&traces, "B"), Some(6));
        assert_eq!(instrument_of(&traces, "C"), None, "not a number");
        assert_eq!(instrument_of(&traces, "D"), None);
        assert_eq!(instrument_of(&[], "A"), None);
        assert_eq!(
            instrument_of(&[(0, Trace::new(0, "instruments"))], "A"),
            None,
            "no columns"
        );
    }

    #[test]
    fn the_markers_are_the_first_accepted_opening_and_the_last_accepted_closing_decision_with_their_fills()
     {
        use Purpose::{Close, Open};
        let us = |ts: Nanos| (ts / 1000) as i64;
        let decs = vec![
            Dec {
                ts: 5 * MS,
                side: Side::Buy,
                qty: 100,
                purpose: Open,
                limit: 20_100_000_000,
                reason: 1,
                order: None,
                why: Some("max_notional".into()),
            },
            Dec {
                ts: 10 * MS,
                side: Side::Buy,
                qty: 100,
                purpose: Open,
                limit: 20_100_000_000,
                reason: 1,
                order: Some(1),
                why: None,
            },
            Dec {
                ts: 900 * MS,
                side: Side::Sell,
                qty: 100,
                purpose: Close,
                limit: 19_000_000_000,
                reason: 0xE503,
                order: Some(2),
                why: None,
            },
            Dec {
                ts: 950 * MS,
                side: Side::Sell,
                qty: 100,
                purpose: Close,
                limit: 19_000_000_000,
                reason: 0xE503,
                order: None,
                why: Some("nothing_to_close".into()),
            },
        ];
        let fills = vec![
            (60 * MS, 1, 100, 20_010_000_000),
            (960 * MS, 2, 100, 19_400_000_000),
        ];
        let t = trip("S00", true, 100, 60 * MS, 960 * MS);
        let m = marks_of(&decs, &fills, &t, &us);
        assert_eq!(
            m,
            vec![
                (
                    10_000,
                    "decision",
                    "Decision: buy 100 at most 20.1000; accepted and sent as order 1".to_owned()
                ),
                (
                    60_000,
                    "fill",
                    "Filled 100 at 20.0100, 50 ms after the decision".to_owned()
                ),
                (
                    900_000,
                    "exit_decision",
                    "Exit decision: sell 100, time exit; sent as order 2".to_owned()
                ),
                (
                    960_000,
                    "exit_fill",
                    "Exit filled 100 at 19.4000, 60 ms after the decision".to_owned()
                ),
            ]
        );
        // Still held at the end of the day: no exit, the end instead.
        let mut open = trip("S00", true, 100, 60 * MS, 1_000 * MS);
        open.open_at_end = true;
        let m = marks_of(&decs[..2], &fills[..1], &open, &us);
        assert_eq!(
            m.iter().map(|x| x.1).collect::<Vec<_>>(),
            ["decision", "fill", "end"]
        );
        assert_eq!(m[2].0, 1_000_000);
        assert!(
            m[2].2.contains("still held") && m[2].2.contains("19.4000"),
            "{}",
            m[2].2
        );
        // Nothing accepted: nothing to mark.
        assert!(marks_of(&decs[..1], &[], &t, &us).is_empty());
        assert!(marks_of(&[], &[], &open, &us).iter().all(|x| x.1 == "end"));
        // An accepted order that was never filled has its decision and no fill.
        let m = marks_of(&decs[1..2], &[], &open, &us);
        assert_eq!(
            m.iter().map(|x| x.1).collect::<Vec<_>>(),
            ["decision", "end"]
        );
    }

    fn sells_and_buys() -> Vec<Dec> {
        let d = |side, purpose, order| Dec {
            ts: 0,
            side,
            qty: 100,
            purpose,
            limit: 0,
            reason: 0,
            order: Some(order),
            why: None,
        };
        vec![
            d(Side::Buy, Purpose::Open, 1),
            d(Side::Sell, Purpose::Close, 2),
            d(Side::Sell, Purpose::Close, 3),
            d(Side::SellShort, Purpose::Open, 4),
            d(Side::Buy, Purpose::Close, 5),
        ]
    }

    #[test]
    fn the_fees_are_split_from_the_sale_fills_and_only_if_they_add_up() {
        let cost = CostModel::published();
        let day = "2026-05-04";
        let decs = sells_and_buys();
        let (px_a, px_b) = (19_400_000_000i64, 19_450_000_000i64);
        let (a1, t1) = cost.sale_fee_parts(day, 60, px_a).unwrap();
        let (a2, t2) = cost.sale_fee_parts(day, 40, px_b).unwrap();
        // A long: bought, then sold in two fills. Only the sales pay; the parts add over the fills.
        let fills = vec![
            (0, 1, 100, 20_010_000_000),
            (1, 2, 60, px_a),
            (2, 3, 40, px_b),
        ];
        let mut t = trip("S00", true, 100, 0, 2);
        t.fees = (a1 + t1 + a2 + t2) as i64;
        assert_eq!(
            fee_parts(&cost, day, &decs, &fills, &t),
            Some((a1 + a2, t1 + t2))
        );
        // What the trade paid differs by anything: only the total can be shown.
        t.fees += 1;
        assert_eq!(fee_parts(&cost, day, &decs, &fills, &t), None);
        t.fees -= 2;
        assert_eq!(fee_parts(&cost, day, &decs, &fills, &t), None);
        // A day the cost model has no rate for.
        t.fees = (a1 + t1 + a2 + t2) as i64;
        assert_eq!(fee_parts(&cost, "2099-01-01", &decs, &fills, &t), None);
        // A short pays on its entries (the short sale) and not on the buy that covers.
        let (a3, t3) = cost.sale_fee_parts(day, 100, 20_000_000_000).unwrap();
        let fills = vec![(0, 4, 100, 20_000_000_000), (1, 5, 100, 19_000_000_000)];
        let mut s = trip("S01", false, 100, 0, 1);
        s.fees = (a3 + t3) as i64;
        assert_eq!(fee_parts(&cost, day, &decs, &fills, &s), Some((a3, t3)));
        // A long still held at the end of the day is counted as sold at the mark: no sale fill, the fee of one.
        let (a4, t4) = cost.sale_fee_parts(day, 100, 19_400_000_000).unwrap();
        let mut o = trip("S02", true, 100, 0, 3);
        o.open_at_end = true;
        o.fees = (a4 + t4) as i64;
        assert_eq!(
            fee_parts(&cost, day, &decs, &[(0, 1, 100, 20_010_000_000)], &o),
            Some((a4, t4))
        );
        // Not for a short held at the end: its sales are its entries.
        let mut so = trip("S03", false, 100, 0, 3);
        so.open_at_end = true;
        so.fees = 0;
        assert_eq!(
            fee_parts(&cost, day, &decs, &[(0, 1, 100, 20_010_000_000)], &so),
            Some((0, 0))
        );
        // A trade that paid nothing and sold nothing.
        assert_eq!(
            fee_parts(&cost, day, &[], &[], &trip("S04", true, 1, 0, 1)),
            Some((0, 0))
        );
    }

    #[test]
    fn a_chart_keeps_the_edges_of_the_trade_and_what_comes_just_before_it() {
        let all: Vec<u64> = (0..1000).collect();
        // Exactly the cap: nothing is dropped, even with nothing near.
        let ten: Vec<u64> = (0..10).collect();
        assert_eq!(thin(&ten, |e| *e, 100, 200, 10), ten);
        // 11 events near (401 to 411, inclusive), 989 far, room for 40 of them: every 25th far event, and the one just before the
        // near ones; 51 in all, which is the cap and is not thinned again.
        let kept = thin(&all, |e| *e, 401, 411, 51);
        assert_eq!(kept.len(), 51);
        assert!((401..=411).all(|e| kept.contains(&e)));
        assert!(
            kept.contains(&400),
            "the event before the trade is the level it starts from"
        );
        assert_eq!(
            kept.iter().filter(|&&e| !(401..=411).contains(&e)).count(),
            40
        );
        assert!(kept.windows(2).all(|w| w[0] < w[1]));
        // Exact examples, where counting an edge event as near or not, or the event after the last near one, changes what is kept:
        // a range of eight events and one of a single event, in 200.
        let two: Vec<u64> = (0..200).collect();
        assert_eq!(
            thin(&two, |e| *e, 50, 55, 12),
            [32, 49, 50, 51, 52, 53, 54, 55, 71, 104, 137, 170]
        );
        assert_eq!(
            thin(&two, |e| *e, 50, 50, 12),
            [18, 37, 49, 50, 57, 76, 95, 114, 133, 152, 171, 190]
        );
    }

    #[test]
    fn a_trace_cell_beyond_the_named_columns_is_left_as_it_is() {
        use super::super::trade::trace_json;
        let mut t = Trace::new(0, "x").with_columns(&["bid"]);
        t.rows
            .push(vec!["20010000000".into(), "20010000000".into()]);
        let j = json(&trace_json(&t, "S"));
        let cells = j.get("rows").arr()[0].arr()[2].arr().to_vec();
        assert_eq!((cells[0].s(), cells[1].s()), ("20.0100", "20010000000"));
    }

    fn fills_trace(rows: &[[&str; 10]]) -> Trace {
        let mut t = Trace::new(0, "fills").with_columns(&[
            "strategy",
            "symbol",
            "ts",
            "side",
            "purpose",
            "reason",
            "qty",
            "px",
            "reference",
            "stop",
        ]);
        for r in rows {
            t.push_row(r.iter().map(|c| (*c).to_owned()).collect());
        }
        t
    }

    #[test]
    fn the_legs_of_a_trade_are_the_strategys_fills_in_the_symbol_within_it_with_what_they_were_for()
    {
        let t = fills_trace(&[
            [
                "1",
                "A",
                "100",
                "Buy",
                "Open",
                "1",
                "10",
                "20010000000",
                "20000000000",
                "18000000000",
            ],
            [
                "1",
                "A",
                "200",
                "Sell",
                "Close",
                "58627",
                "10",
                "19900000000",
                "20000000000",
                "-",
            ],
            ["2", "A", "150", "Buy", "Open", "1", "5", "1", "1", "-"],
            ["1", "B", "150", "Buy", "Open", "1", "5", "1", "1", "-"],
            ["1", "A", "300", "Buy", "Open", "1", "5", "1", "1", "-"],
            ["1", "A", "bad", "Buy", "Open", "1", "5", "1", "1", "-"],
        ]);
        let traces = vec![(0, t)];
        let legs = legs_of(&traces, 1, "A", 100, 200).unwrap();
        assert_eq!(
            legs.len(),
            2,
            "another strategy's, another symbol's, after the trade and unreadable rows are not here"
        );
        assert_eq!(
            legs[0],
            Leg {
                ts: 100,
                side: "Buy".into(),
                purpose: "Open".into(),
                reason: 1,
                qty: 10,
                px: 20_010_000_000,
                reference: 20_000_000_000,
                stop: Some(18_000_000_000)
            }
        );
        assert_eq!(
            (legs[1].ts, legs[1].stop, legs[1].reason),
            (200, None, 58627)
        );
        // Both edges are inclusive.
        assert_eq!(legs_of(&traces, 1, "A", 101, 200).unwrap().len(), 1);
        assert_eq!(legs_of(&traces, 1, "A", 100, 199).unwrap().len(), 1);
        // A day without the trace, or with another trace's columns, has none.
        assert_eq!(legs_of(&[], 1, "A", 0, 1000), None);
        assert_eq!(
            legs_of(&[(1, fills_trace(&[]))], 1, "A", 0, 1000),
            None,
            "a strategy's trace is not the host's"
        );
        assert_eq!(
            legs_of(&[(0, Trace::new(0, "fills"))], 1, "A", 0, 1000),
            None
        );
        assert_eq!(
            legs_of(&[(0, fills_trace(&[]))], 1, "A", 0, 1000),
            Some(vec![])
        );
    }

    #[test]
    fn what_a_fill_cost_against_its_reference_is_positive_when_worse_for_a_buy_and_a_sale() {
        let leg = |side: &str, px: i64, reference: i64| Leg {
            ts: 0,
            side: side.into(),
            purpose: "Open".into(),
            reason: 0,
            qty: 1,
            px,
            reference,
            stop: None,
        };
        assert_eq!(leg("Buy", 101, 100).slippage(), 1);
        assert_eq!(leg("Buy", 99, 100).slippage(), -1);
        assert_eq!(leg("Sell", 99, 100).slippage(), 1);
        assert_eq!(leg("Sell", 101, 100).slippage(), -1);
        assert_eq!(leg("SellShort", 99, 100).slippage(), 1);
        assert_eq!(leg("Buy", 100, 100).slippage(), 0);
    }

    #[test]
    fn the_distance_to_a_stop_is_in_tenths_of_a_percent_rounded() {
        assert_eq!(tenths_pct(20_010_000_000, 18_009_000_000), "10.0");
        assert_eq!(
            tenths_pct(20_000_000_000, 22_000_000_000),
            "10.0",
            "a stop above a short's entry"
        );
        assert_eq!(tenths_pct(20_000_000_000, 19_990_000_000), "0.1");
        assert_eq!(tenths_pct(20_000_000_000, 19_999_000_000), "0.0");
        assert_eq!(tenths_pct(20_000_000_000, 19_999_900_000), "0.0");
        assert_eq!(tenths_pct(1_000, 995), "0.5");
        assert_eq!(tenths_pct(1_000, 994), "0.6");
        assert_eq!(tenths_pct(0, 5), "0.0");
        assert_eq!(tenths_pct(-5, 5), "0.0");
        assert_eq!(tenths_pct(100, 0), "100.0");
    }

    #[test]
    fn a_symbol_is_listed_where_the_strategys_own_trace_names_it_first() {
        let mut a = Trace::new(1_777_924_800_000_000_000, "rank")
            .with_columns(&["rank", "symbol", "status"]);
        for (i, (s, st)) in [
            ("X", "entered"),
            ("Y", "skipped:halted"),
            ("Z", "not_chosen"),
        ]
        .into_iter()
        .enumerate()
        {
            a.push_row(vec![(i + 1).to_string(), s.into(), st.into()]);
        }
        let mut b = Trace::new(0, "draw").with_columns(&["symbol"]);
        b.push_row(vec!["Y".into()]);
        let c = Trace::new(0, "x");
        let own = [&c, &a, &b];
        let j = json(&listed_in(&own, "Y").unwrap());
        assert_eq!(
            (
                j.get("kind").s(),
                j.get("row").s(),
                j.get("of").s(),
                j.get("status").s()
            ),
            ("rank", "2", "3", "skipped:halted")
        );
        assert!(j.get("time").s().ends_with(".000"));
        assert_eq!(
            json(&listed_in(&[&b], "Y").unwrap()).get("status"),
            &J::Null,
            "no status column"
        );
        assert_eq!(listed_in(&own, "Q"), None);
        assert_eq!(listed_in(&[], "Y"), None);
        // A symbol only in a later trace is found there.
        let only_b = json(&listed_in(&[&a, &b], "Y").unwrap());
        assert_eq!(only_b.get("kind").s(), "rank");
        let mut d = Trace::new(0, "entry").with_columns(&["symbol"]);
        d.push_row(vec!["W".into()]);
        assert_eq!(
            json(&listed_in(&[&a, &d], "W").unwrap()).get("kind").s(),
            "entry"
        );
    }

    #[test]
    fn a_leg_is_written_with_its_slippage_in_dollars_and_basis_points_and_a_zero_reference_does_not_divide()
     {
        let leg = |px, reference| Leg {
            ts: 1_500_000,
            side: "Buy".into(),
            purpose: "Open".into(),
            reason: 0xE503,
            qty: 7,
            px,
            reference,
            stop: Some(18_000_000_000),
        };
        let j = json(&legs_json(&[leg(20_010_000_000, 20_000_000_000)], &|ts| {
            (ts / 1000) as i64
        }));
        let l = &j.arr()[0];
        assert_eq!(
            (
                l.get("us").s(),
                l.get("side").s(),
                l.get("purpose").s(),
                l.get("qty").s()
            ),
            ("1500", "buy", "open", "7")
        );
        assert_eq!(
            (
                l.get("slip").s(),
                l.get("slip_bp").s(),
                l.get("reference").s(),
                l.get("px").s()
            ),
            ("0.0100", "5.00", "20.0000", "20.0100")
        );
        assert_eq!(
            (
                l.get("stop").s(),
                l.get("stop_pct").s(),
                l.get("reason").s()
            ),
            ("18.0000", "10.0", "time exit")
        );
        // A price the strategy measured against nothing is not divided by.
        let z = json(&legs_json(&[leg(20_010_000_000, 0)], &|_| 0));
        assert_eq!(z.arr()[0].get("slip_bp").s(), "0.00");
        assert_eq!(z.arr()[0].get("slip").s(), "20.0100");
        assert_eq!(json(&legs_json(&[], &|_| 0)), J::Arr(vec![]));
    }
}

// ---- a replayed day is a live day (E19-S41) ----

mod live_day {
    use super::*;
    use tf_catalog::Kind;
    use tf_ledger::ReadOnlyStore;

    fn made(name: &str) -> (PathBuf, Results) {
        let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
        let mut days = Days::new(&format!("{name}-days"), 3);
        let dir = out_dir(&format!("{name}-out"));
        run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
        let r = Results::open(&dir).unwrap();
        (dir, r)
    }

    #[test]
    fn each_day_has_the_file_ledger_and_the_daily_report_a_live_day_has() {
        let (dir, r) = made("ld1");
        for date in r.dates().unwrap() {
            // The ledger is in the day's own directory, with the file a live ledger is, and no lock left behind.
            let ledger = r.ledger(&date).unwrap();
            assert_eq!(ledger, dir.join(format!("{date}.ledger")));
            assert!(ledger.join("ledger.log").is_file() && !ledger.join("ledger.lock").exists());
            // The report is the host's end-of-day report: what each strategy did, then the system.
            let report = r.report(&date).unwrap();
            assert!(report.contains("replay "), "{report}");
            assert!(
                report.contains("round1") && report.contains("round2"),
                "{report}"
            );
            assert!(
                !report.contains("ledger "),
                "the seal is not part of the report"
            );
        }
    }

    #[test]
    fn the_ledger_and_the_trips_tell_the_same_story() {
        let (_, r) = made("ld2");
        for date in r.dates().unwrap() {
            let trips = r.day(&date).unwrap().trips;
            // Read as a live ledger is read: one session per strategy, with its trades and its profit.
            let sessions = tf_catalog::sessions_detailed(
                ReadOnlyStore::open(r.ledger(&date).unwrap()),
                "replay",
                Kind::Backtest,
            )
            .unwrap();
            assert_eq!(sessions.len(), 2, "{date}");
            for (run, detail) in &sessions {
                let id: u16 = run.strategy.trim_start_matches('s').parse().unwrap();
                let mine: Vec<&Trip> = trips.iter().filter(|t| t.strategy == id).collect();
                assert_eq!(
                    run.trades,
                    Some(mine.len() as u64),
                    "{date} {}",
                    run.strategy
                );
                // The gateway's profit is before the regulatory fees and borrow: the trips' gross.
                let gross: i128 = mine.iter().map(|t| i128::from(t.gross)).sum();
                assert_eq!(run.net_pnl, Some(gross), "{date} {}", run.strategy);
                assert_eq!(
                    detail.fills.len(),
                    mine.len() * 2,
                    "an entry and an exit each"
                );
            }
        }
    }

    #[test]
    fn two_runs_leave_the_same_ledger_and_a_day_made_again_does_not_add_to_its_old_one() {
        let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
        let mut days = Days::new("ld3-days", 3);
        let (a, b) = (out_dir("ld3-a"), out_dir("ld3-b"));
        run(&setup(&host, &cost, &defs), &mut days, &a).unwrap();
        run(&setup(&host, &cost, &defs), &mut days, &b).unwrap();
        let log = |d: &Path| fs::read(d.join("2026-05-04.ledger").join("ledger.log")).unwrap();
        assert_eq!(log(&a), log(&b));
        assert!(!log(&a).is_empty());
        // Take the day's trips away: it is run again, over a ledger that is made afresh, to the same bytes.
        let before = read_all(&a);
        fs::remove_file(a.join("2026-05-04.trips")).unwrap();
        let rep = run(&setup(&host, &cost, &defs), &mut days, &a).unwrap();
        assert_eq!(rep.ran, ["2026-05-04"]);
        assert_eq!(read_all(&a), before);
    }

    #[test]
    fn a_day_without_its_ledger_or_with_another_one_is_refused_and_made_again() {
        let (host, cost, defs) = (host_cfg(), CostModel::published(), defs());
        let mut days = Days::new("ld4-days", 3);
        let dir = out_dir("ld4-out");
        run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
        let before = read_all(&dir);
        let r = Results::open(&dir).unwrap();
        // The ledger of another day in its place is not this day's.
        fs::copy(
            dir.join("2026-05-01.ledger").join("ledger.log"),
            dir.join("2026-05-04.ledger").join("ledger.log"),
        )
        .unwrap();
        let err = r.ledger("2026-05-04").unwrap_err().to_string();
        assert!(
            err.contains("not the one the day's report was made with"),
            "{err}"
        );
        let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
        assert_eq!(rep.ran, ["2026-05-04"]);
        assert_eq!(read_all(&dir), before);
        // Cut, gone, or a day made before there were ledgers: each is made again.
        for damage in 0..3 {
            let log = dir.join("2026-05-05.ledger").join("ledger.log");
            match damage {
                0 => {
                    let b = fs::read(&log).unwrap();
                    fs::write(&log, &b[..b.len() - 7]).unwrap();
                }
                1 => fs::remove_dir_all(dir.join("2026-05-05.ledger")).unwrap(),
                _ => fs::remove_file(dir.join("2026-05-05.report.txt")).unwrap(),
            }
            assert!(
                Results::open(&dir).unwrap().ledger("2026-05-05").is_err(),
                "{damage}"
            );
            let rep = run(&setup(&host, &cost, &defs), &mut days, &dir).unwrap();
            assert_eq!(rep.ran, ["2026-05-05"], "{damage}");
            assert_eq!(read_all(&dir), before, "{damage}");
        }
    }
}

// ---- what the engine did with Tier 1 around a trade (E19-S43) ----

mod tier_parts {
    use super::super::trade::{
        TIER_ROWS, TierEv, held_before, symbol_names, tier_events, tier_reason, tiers_json,
    };
    use super::*;
    use crate::equiv::{Answer, Rec};
    use tf_strategy::trace::Trace;

    fn dec(
        ts: Nanos,
        strategy: u16,
        instrument: u32,
        side: Side,
        purpose: Purpose,
        answer: Answer,
    ) -> Rec {
        Rec::Decision {
            idx: 0,
            ts,
            strategy,
            seq: 0,
            instrument,
            side,
            qty: 100,
            purpose,
            limit: 1,
            reason: 0,
            answer,
        }
    }

    fn fill(ts: Nanos, order: u64, instrument: u32, qty: u32, px: i64) -> Rec {
        Rec::Fill {
            idx: 0,
            ts,
            order,
            instrument,
            qty,
            px,
        }
    }

    fn acc(o: u64) -> Answer {
        Answer::Accepted(o)
    }

    fn tier(ts: Nanos, instrument: u32, promote: bool, reason: u8, score: i64) -> Rec {
        Rec::Tier {
            idx: 0,
            ts,
            instrument,
            promote,
            reason,
            score,
        }
    }

    #[test]
    fn the_tier_changes_of_a_log_are_read_in_order_with_their_reasons_in_words() {
        let recs = vec![
            dec(1, 1, 5, Side::Buy, Purpose::Open, acc(1)),
            tier(10, 5, true, 1, 9_200),
            fill(11, 1, 5, 100, 1),
            tier(20, 6, false, 2, 0),
            Rec::Action {
                idx: 0,
                ts: 21,
                what: "end_of_day".into(),
            },
            tier(30, 7, true, 3, 0),
        ];
        let evs = tier_events(&recs);
        assert_eq!(evs.len(), 3);
        assert_eq!(
            evs[0],
            TierEv {
                ts: 10,
                instrument: 5,
                promote: true,
                reason: 1,
                score: 9_200
            }
        );
        assert_eq!(
            (evs[1].ts, evs[1].promote, evs[2].instrument),
            (20, false, 7)
        );
        assert!(tier_events(&[]).is_empty());
        assert_eq!(tier_reason(1, 9_200), "scanner hit, volume z-score 9.200");
        assert_eq!(tier_reason(1, -500), "scanner hit, volume z-score -0.500");
        assert_eq!(tier_reason(2, 0), "cooled off");
        assert_eq!(tier_reason(3, 0), "a strategy asked for it");
        assert_eq!(
            tier_reason(4, 0),
            "made room for a request of higher priority"
        );
        assert_eq!(tier_reason(9, 0), "reason 9");
    }

    #[test]
    fn who_held_tier_1_before_a_time_is_what_the_changes_before_it_leave() {
        let e = |ts, instrument, promote| TierEv {
            ts,
            instrument,
            promote,
            reason: 1,
            score: 0,
        };
        let evs = [
            e(10, 1, true),
            e(20, 2, true),
            e(30, 1, false),
            e(40, 1, true),
            e(45, 1, true),
            e(50, 9, false),
        ];
        assert_eq!(held_before(&evs, 5), Vec::<u32>::new());
        assert_eq!(
            held_before(&evs, 10),
            Vec::<u32>::new(),
            "a change at the time is not before it"
        );
        assert_eq!(held_before(&evs, 11), [1]);
        assert_eq!(held_before(&evs, 25), [1, 2]);
        assert_eq!(held_before(&evs, 31), [2]);
        assert_eq!(held_before(&evs, 46), [2, 1], "promoted again, once");
        assert_eq!(
            held_before(&evs, 99),
            [2, 1],
            "a demotion of a name that was not held changes nothing"
        );
        assert!(held_before(&[], 99).is_empty());
    }

    #[test]
    fn instrument_names_come_from_the_hosts_trace_and_the_page_lists_the_changes_around_a_trade() {
        let mut book = Trace::new(0, "instruments").with_columns(&["instrument", "symbol"]);
        for (i, n) in [("5", "AAA"), ("6", "BBB"), ("x", "bad")] {
            book.push_row(vec![i.into(), n.into()]);
        }
        let mut other = Trace::new(0, "instruments").with_columns(&["instrument", "symbol"]);
        other.push_row(vec!["9".into(), "NOPE".into()]);
        let traces = vec![(1, other), (0, book)];
        let names = symbol_names(&traces);
        assert_eq!(names.len(), 2);
        assert_eq!((names[&5].as_str(), names[&6].as_str()), ("AAA", "BBB"));
        assert!(symbol_names(&[]).is_empty());
        let e = |ts, instrument, promote, reason, score| TierEv {
            ts,
            instrument,
            promote,
            reason,
            score,
        };
        let evs = [
            e(5_000, 6, true, 1, 7_000),
            e(10_000, 5, true, 1, 9_200),
            e(20_000, 7, true, 3, 0),
            e(30_000, 5, false, 2, 0),
            e(99_000, 5, true, 1, 1),
        ];
        let us = |ts: Nanos| (ts / 1000) as i64;
        // The window is inclusive at both ends; the traded instrument's changes of the whole day are listed apart.
        let j = json(&tiers_json(&evs, &names, Some(5), 10_000, 30_000, &us));
        assert_eq!(j.get("day_events").s(), "5");
        assert_eq!(
            j.get("start").arr().iter().map(J::s).collect::<Vec<_>>(),
            ["BBB"]
        );
        assert_eq!(j.get("around_total").s(), "3");
        let around = j.get("around").arr();
        assert_eq!(
            around
                .iter()
                .map(|a| a.get("symbol").s())
                .collect::<Vec<_>>(),
            ["AAA", "#7", "AAA"]
        );
        assert_eq!(
            around
                .iter()
                .map(|a| a.get("action").s())
                .collect::<Vec<_>>(),
            ["promoted", "promoted", "demoted"]
        );
        assert_eq!(
            around.iter().map(|a| a.get("mine")).collect::<Vec<_>>(),
            [&J::Bool(true), &J::Bool(false), &J::Bool(true)]
        );
        assert_eq!(
            (
                around[0].get("us").s(),
                around[0].get("reason").s(),
                around[0].get("score").s()
            ),
            ("10", "scanner hit, volume z-score 9.200", "9200")
        );
        assert_eq!(around[1].get("reason").s(), "a strategy asked for it");
        let mine = j.get("mine").arr();
        assert_eq!(
            mine.iter().map(|m| m.get("us").s()).collect::<Vec<_>>(),
            ["10", "30", "99"]
        );
        // No traded instrument known: none is marked, and none is listed apart.
        let none = json(&tiers_json(&evs, &names, None, 0, 100_000, &us));
        assert!(none.get("mine").arr().is_empty());
        assert!(
            none.get("around")
                .arr()
                .iter()
                .all(|a| a.get("mine") == &J::Bool(false))
        );
        // A long window is cut at the page's limit and says how many there were.
        let many: Vec<TierEv> = (0..=TIER_ROWS as u64)
            .map(|i| e(i * 1000, 5, i % 2 == 0, 1, 0))
            .collect();
        let long = json(&tiers_json(&many, &names, Some(5), 0, 1_000_000, &us));
        assert_eq!(
            (long.get("around").arr().len(), long.get("around_total").s()),
            (TIER_ROWS, "301")
        );
        // Nothing at all.
        let empty = json(&tiers_json(&[], &names, Some(5), 0, 1, &us));
        assert_eq!(empty.get("day_events").s(), "0");
        assert!(empty.get("around").arr().is_empty() && empty.get("start").arr().is_empty());
    }
}

#[test]
fn what_a_summary_says_of_money_basis_points_and_a_variants_trades() {
    use super::summary::{bp, money, tally};
    assert_eq!(
        (
            money(-6_182_038_200).as_str(),
            money(0).as_str(),
            money(5_000_000).as_str(),
            money(4_999_999).as_str(),
            money(-4_999_999).as_str()
        ),
        ("-$6.18", "$0.00", "$0.01", "$0.00", "$0.00")
    );
    assert_eq!(money(123_456_789_000), "$123.46");
    assert_eq!(
        (
            bp(-1029).as_str(),
            bp(5).as_str(),
            bp(0).as_str(),
            bp(155).as_str(),
            bp(-5).as_str(),
            bp(100).as_str()
        ),
        ("-10.29", "0.05", "0.00", "1.55", "-0.05", "1.00")
    );
    let t = |net: i64, bps: i64| Trip {
        day: "2026-05-04".into(),
        strategy: 1,
        name: "t".into(),
        variant: 1,
        symbol: "A".into(),
        long: true,
        qty: 1,
        entry_ts: 1,
        entry_px: 1,
        exit_ts: 2,
        exit_px: 1,
        gross: 0,
        fees: 0,
        borrow: 0,
        slippage: 0,
        net,
        net_bps_x100: bps,
        slip_bps_x100: 0,
        r_milli: None,
        entry_reason: 0,
        exit_reason: 0,
        open_at_end: false,
    };
    let (a, b, c) = (t(100, 40), t(0, 0), t(-300, -80));
    assert_eq!(
        tally(&[&a, &b, &c]),
        (-200, 1, -13),
        "a break-even trade is not a win; -40 over three truncates to -13"
    );
    assert_eq!(tally(&[&a]), (100, 1, 40));
    assert_eq!(tally(&[&b]), (0, 0, 0));
    assert_eq!(tally(&[]), (0, 0, 0));
}
