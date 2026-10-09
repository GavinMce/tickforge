use tf_universe::RefRow;

use super::*;

const D: i64 = 1_000_000_000;
const DAY: u64 = 86_400_000_000_000;

fn day0() -> i64 {
    date_days("2026-09-14").unwrap()
}

/// CSV for the given (session index, id, open, high, low, close, volume); sessions are consecutive days.
fn csv(rows: &[(i64, u32, i64, i64, i64, i64, u64)]) -> String {
    let mut s =
        String::from("ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume\n");
    for (d, id, o, h, l, c, v) in rows {
        let ts = (day0() + d) as u64 * DAY;
        s.push_str(&format!(
            "{ts},35,90,{id},{},{},{},{},{v}\n",
            o * D,
            h * D,
            l * D,
            c * D
        ));
    }
    s
}

fn syms(pairs: &[(&str, u32)]) -> Symbology {
    let body: Vec<String> = pairs
        .iter()
        .map(|(s, id)| {
            format!("\"{s}\":[{{\"d0\":\"2026-01-01\",\"d1\":\"2027-01-01\",\"s\":\"{id}\"}}]")
        })
        .collect();
    Symbology::parse(&format!("{{\"result\":{{{}}}}}", body.join(","))).unwrap()
}

#[test]
fn dates_convert_both_ways() {
    for (s, days) in [
        ("1970-01-01", 0),
        ("2000-03-01", 11_017),
        ("2024-02-29", 19_782),
        ("2026-10-02", 20_728),
        ("2100-03-01", 47_541),
    ] {
        assert_eq!(date_days(s), Some(days), "{s}");
        assert_eq!(date_text(days), s);
    }
    for bad in [
        "2026-02-30",
        "2026-13-01",
        "2026-1-01",
        "2026/10/02",
        "20261002",
        "2100-02-29",
        "2026-00-10",
        "",
    ] {
        assert_eq!(date_days(bad), None, "{bad}");
    }
}

#[test]
fn bars_are_read_strictly() {
    let good = csv(&[(0, 7, 10, 11, 9, 10, 100)]);
    let b = read_bars(&good).unwrap();
    assert_eq!(
        b,
        [Bar {
            day: day0(),
            id: 7,
            open: 10 * D,
            high: 11 * D,
            low: 9 * D,
            close: 10 * D,
            volume: 100
        }]
    );
    let header = good.lines().next().unwrap();
    let line = good.lines().nth(1).unwrap();
    for (bad, want) in [
        ("ts_event,rtype\n".to_owned(), "header"),
        (format!("{header}\n1,2,3\n"), "9"),
        (
            format!("{header}\n{}\n", line.replace(",35,", ",34,")),
            "daily bar",
        ),
        (
            format!(
                "{header}\n{}\n",
                line.replacen(
                    &format!("{}", day0() as u64 * DAY),
                    &format!("{}", day0() as u64 * DAY + 1),
                    1
                )
            ),
            "midnight",
        ),
        (
            format!("{header}\n{}\n", line.replace(",100", ",x")),
            "not a number",
        ),
        (
            format!(
                "{header}\n{}\n",
                line.replace(&format!(",{}", 11 * D), &format!(",{}", 9 * D))
            ),
            "contain",
        ),
    ] {
        let e = read_bars(&bad).unwrap_err().to_string();
        assert!(e.contains(want), "{want}: {e}");
    }
    assert_eq!(
        read_bars(&format!("{header}\n{line}\nbroken\n"))
            .unwrap_err()
            .line,
        3
    );
}

#[test]
fn symbology_follows_the_date() {
    let s = Symbology::parse(r#"{"result":{"OLD":[{"d0":"2026-09-01","d1":"2026-09-20","s":"5"}],"NEW":[{"d0":"2026-09-20","d1":"2026-10-10","s":"5"}],"X":[{"d0":"2026-09-01","d1":"2026-10-10","s":"6"}]}}"#).unwrap();
    let d = |t: &str| date_days(t).unwrap();
    assert_eq!(s.symbol(5, d("2026-09-19")), Some("OLD"));
    assert_eq!(s.symbol(5, d("2026-09-20")), Some("NEW"));
    assert_eq!(s.symbol(5, d("2026-10-10")), None);
    assert_eq!(s.symbol(5, d("2026-08-31")), None);
    assert_eq!(s.symbol(6, d("2026-09-30")), Some("X"));
    assert_eq!(s.symbol(7, d("2026-09-30")), None);
    assert_eq!(s.len(), 3);
    assert!(Symbology::parse("{}").is_err());
    assert!(
        Symbology::parse(r#"{"result":{"A":[{"d0":"2026-02-30","d1":"2026-10-10","s":"1"}]}}"#)
            .is_err()
    );
    assert!(
        Symbology::parse(r#"{"result":{"A":[{"d0":"2026-09-01","d1":"2026-10-10","s":"x"}]}}"#)
            .is_err()
    );
}

fn market() -> (Vec<Bar>, Symbology) {
    let mut rows = Vec::new();
    for d in 0..12 {
        // A: every session, close 10, range 9..11 against a close of 10, 1,000 shares.
        rows.push((d, 1, 10, 11, 9, 10, 1000));
        // B: only the last five sessions.
        if d >= 7 {
            rows.push((d, 2, 20, 21, 19, 20, 500));
        }
        // C: every session but the last.
        if d < 11 {
            rows.push((d, 3, 5, 5, 5, 5, 2000));
        }
        // E: every session but the fifth.
        if d != 4 {
            rows.push((d, 5, 8, 9, 7, 8, 100));
        }
        // An id with no symbol.
        rows.push((d, 99, 1, 1, 1, 1, 1));
        // A symbol that cannot be listed.
        rows.push((d, 6, 1, 1, 1, 1, 1));
    }
    (
        read_bars(&csv(&rows)).unwrap(),
        syms(&[
            ("AAA", 1),
            ("BBB", 2),
            ("CCC", 3),
            ("EEE", 5),
            ("bad name", 6),
        ]),
    )
}

fn row<'a>(rows: &'a [RefRow], s: &str) -> &'a RefRow {
    rows.iter()
        .find(|r| r.symbol == s)
        .unwrap_or_else(|| panic!("no row {s}"))
}

#[test]
fn rows_are_built_from_the_window() {
    let (bars, sy) = market();
    let up_to = day0() + 11;
    let (rows, rep) = build(&bars, &sy, up_to, Params::default()).unwrap();
    assert_eq!(rep.as_of, date_text(up_to));
    assert_eq!(rep.sessions.len(), 12);
    assert_eq!(
        (
            rep.bars_after,
            rep.bars_unmapped,
            rep.symbols_skipped,
            rep.symbols
        ),
        (0, 12, 1, 4)
    );
    let a = row(&rows, "AAA");
    assert_eq!(
        (a.price, a.adv_dollar, a.adv_shares, a.atr_permille),
        (Some(10 * D), Some(10_000), Some(1000), Some(200))
    );
    // Too few sessions: the price is known, no average is.
    let b = row(&rows, "BBB");
    assert_eq!(
        (b.price, b.adv_dollar, b.adv_shares, b.atr_permille),
        (Some(20 * D), None, None, None)
    );
    // No bar on the last day: the price is unknown (a stale close is not today's), the averages
    // run over all 12 sessions with the missing one counting as zero volume.
    let c = row(&rows, "CCC");
    assert_eq!(c.price, None);
    assert_eq!(c.adv_shares, Some(2000 * 11 / 12));
    assert_eq!(c.adv_dollar, Some(5 * 2000 * 11 / 12));
    assert_eq!(c.atr_permille, None);
    assert_eq!((rep.no_price, rep.no_average), (1, 1));
    // A gap in a symbol's sessions loses the range of the day after the gap (no previous close) and
    // of the first bar: 11 bars give 9 usable ranges, so with min_days 10 there is no ATR but there
    // are averages, and with min_days 9 there is: TR is 2 (9..7 against close 8 is 2), 2*1000/8.
    let e = row(&rows, "EEE");
    assert_eq!((e.adv_shares, e.atr_permille), (Some(100 * 11 / 12), None));
    let (rows9, _) = build(
        &bars,
        &sy,
        up_to,
        Params {
            window: 20,
            min_days: 9,
        },
    )
    .unwrap();
    assert_eq!(row(&rows9, "EEE").atr_permille, Some(2 * 1000 / 8));
    // The window length matters: 5 sessions.
    let (r5, rep5) = build(
        &bars,
        &sy,
        up_to,
        Params {
            window: 5,
            min_days: 5,
        },
    )
    .unwrap();
    assert_eq!(rep5.sessions.len(), 5);
    assert_eq!(row(&r5, "BBB").adv_shares, Some(500));
    // B's first bar in the window has no previous close (it began that day), so 4 usable ranges:
    // not enough for min_days 5, enough for 4 (TR 2 on a 20 close).
    assert_eq!(row(&r5, "BBB").atr_permille, None);
    let (r4, _) = build(
        &bars,
        &sy,
        up_to,
        Params {
            window: 5,
            min_days: 4,
        },
    )
    .unwrap();
    assert_eq!(row(&r4, "BBB").atr_permille, Some(100));
}

#[test]
fn nothing_after_the_date_is_used() {
    let (bars, sy) = market();
    let cut = day0() + 8;
    let (full, _) = build(&bars, &sy, cut, Params::default()).unwrap();
    let kept: Vec<Bar> = bars.iter().filter(|b| b.day <= cut).copied().collect();
    let (only, rep) = build(&kept, &sy, cut, Params::default()).unwrap();
    assert_eq!(full, only);
    let (_, rep_full) = build(&bars, &sy, cut, Params::default()).unwrap();
    assert!(rep_full.bars_after > 0 && rep.bars_after == 0);
    assert_eq!(rep_full.as_of, date_text(cut));
    // A date that is not a session uses the last one before it.
    let (_, between) = build(&bars, &sy, day0() + 40, Params::default()).unwrap();
    assert_eq!(between.as_of, date_text(day0() + 11));
    assert_eq!(
        build(&bars, &sy, day0() - 1, Params::default()).unwrap_err(),
        BuildError::NoBars
    );
}

#[test]
fn a_gap_counts_in_the_true_range() {
    // Two sessions; the second opens away from the first close.
    let bars = read_bars(&csv(&[
        (0, 1, 10, 10, 10, 10, 1),
        (1, 1, 15, 16, 15, 15, 1),
        (0, 2, 10, 10, 10, 10, 1),
        (1, 2, 5, 5, 4, 4, 1),
        (0, 3, 10, 10, 10, 10, 1),
        (1, 3, 10, 12, 8, 10, 1),
    ]))
    .unwrap();
    let sy = syms(&[("UP", 1), ("DOWN", 2), ("INSIDE", 3)]);
    let (rows, _) = build(
        &bars,
        &sy,
        day0() + 1,
        Params {
            window: 2,
            min_days: 1,
        },
    )
    .unwrap();
    // UP: range 1, high - prev close 6. DOWN: range 1, prev close - low 6. INSIDE: range 4.
    assert_eq!(row(&rows, "UP").atr_permille, Some(6 * 1000 / 15));
    assert_eq!(row(&rows, "DOWN").atr_permille, Some(6 * 1000 / 4));
    assert_eq!(row(&rows, "INSIDE").atr_permille, Some(4 * 1000 / 10));
}

const ASSETS: &str = r#"[
 {"symbol":"AAA","class":"us_equity","exchange":"NASDAQ","status":"active","tradable":true,"shortable":true,"easy_to_borrow":false},
 {"symbol":"CCC","class":"us_equity","exchange":"NYSE","status":"active","tradable":false,"shortable":false,"easy_to_borrow":false},
 {"symbol":"BBB","class":"us_equity","exchange":"NYSE","status":"active","tradable":true,"shortable":false,"easy_to_borrow":false},
 {"symbol":"OLD","class":"us_equity","exchange":"NYSE","status":"inactive","tradable":false,"shortable":false,"easy_to_borrow":false},
 {"symbol":"BTC/USD","class":"crypto","exchange":"CRYPTO","status":"active","tradable":true,"shortable":false,"easy_to_borrow":false},
 {"symbol":"ODD","class":"us_equity","exchange":"weird","status":"active","tradable":true,"shortable":true,"easy_to_borrow":true}
]"#;

#[test]
fn alpaca_assets_set_the_flags_and_only_for_listed_symbols() {
    let assets = parse_assets(ASSETS).unwrap();
    assert_eq!(
        assets.iter().map(|a| a.symbol.as_str()).collect::<Vec<_>>(),
        ["AAA", "CCC", "BBB", "ODD"]
    );
    assert_eq!(assets[3].exchange, None);
    let (bars, sy) = market();
    let (mut rows, _) = build(&bars, &sy, day0() + 11, Params::default()).unwrap();
    assert_eq!(merge_assets(&mut rows, &assets), 3);
    assert_eq!(row(&rows, "CCC").tradable, Some(false));
    let a = row(&rows, "AAA");
    assert_eq!(
        (
            a.tradable,
            a.shortable,
            a.easy_to_borrow,
            a.exchange.as_deref()
        ),
        (Some(true), Some(true), Some(false), Some("NASDAQ"))
    );
    assert_eq!(row(&rows, "BBB").shortable, Some(false));
    let e = row(&rows, "EEE");
    assert_eq!(
        (e.tradable, e.shortable, e.exchange.clone()),
        (None, None, None)
    );
    for (bad, want) in [
        ("{}", "list"),
        (r#"[{"class":"us_equity"}]"#, "no symbol"),
        (
            r#"[{"symbol":"Z","class":"us_equity","status":"active","tradable":"yes","shortable":true,"easy_to_borrow":true}]"#,
            "tradable",
        ),
        (
            r#"[{"symbol":"Z","class":"us_equity","status":"active","tradable":true,"easy_to_borrow":true}]"#,
            "shortable",
        ),
        ("[", "JSON"),
    ] {
        let e = parse_assets(bad).unwrap_err();
        assert!(e.contains(want), "{bad}: {e}");
    }
}

#[test]
fn the_etf_list_marks_every_row() {
    let (bars, sy) = market();
    let (mut rows, _) = build(&bars, &sy, day0() + 11, Params::default()).unwrap();
    assert_eq!(
        merge_etf_list(&mut rows, "# funds\nBBB # one\n\nZZZ\n").unwrap(),
        1
    );
    assert_eq!(row(&rows, "BBB").etf, Some(true));
    assert_eq!(row(&rows, "AAA").etf, Some(false));
    assert!(
        merge_etf_list(&mut rows, "bad name\n")
            .unwrap_err()
            .contains("line 1")
    );
}

#[test]
fn the_names_on_a_day_are_the_symbols_in_force_that_day_one_to_an_id() {
    let s = Symbology::parse(
        r#"{"result":{"OLD":[{"d0":"2026-10-01","d1":"2026-10-08","s":"7"}],
            "NEW":[{"d0":"2026-10-08","d1":"2026-10-20","s":"7"}],
            "ZED":[{"d0":"2026-10-08","d1":"2026-10-09","s":"3"}],
            "ALT":[{"d0":"2026-10-08","d1":"2026-10-09","s":"3"}],
            "GONE":[{"d0":"2026-09-01","d1":"2026-10-01","s":"9"}]}}"#,
    )
    .unwrap();
    let d = |t: &str| date_days(t).unwrap();
    assert_eq!(s.names_on(d("2026-10-08")).len(), 2);
    assert_eq!(s.names_on(d("2026-10-08")), [(3, "ALT"), (7, "NEW")]);
    assert_eq!(s.names_on(d("2026-10-07")), [(7, "OLD")]);
    assert_eq!(s.names_on(d("2026-10-09")), [(7, "NEW")]);
    assert!(s.names_on(d("2026-11-01")).is_empty());
}
