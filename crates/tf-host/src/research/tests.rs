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
}

impl CrossStrategy for RoundTrip {
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

fn read_all(dir: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
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
    assert_eq!(read_all(&a).len(), 4);
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
