use tf_core::{Event, Header, ProviderId, Px, Trade, TradeFlags};
use tf_engine::Tier0;

use super::*;

const SPEC: &str = "universe v1
param min_adv = 5000000
param min_price = 2.00
static price >= @min_price; price <= 50; adv_dollar >= @min_adv; etf = no; shortable = yes; exchange in NASDAQ NYSE ARCA
dynamic top 2 by gap_permille desc keep 3 every 5 where trades >= 2
";

const SNAP: &str = "# as_of 2026-10-02
symbol,price,adv_dollar,adv_shares,etf,shortable,exchange
AAA,10.00,9000000,900000,no,yes,NASDAQ
BBB,1.50,9000000,900000,no,yes,NASDAQ
CCC,10.00,4999999,900000,no,yes,NYSE
DDD,10.00,9000000,900000,yes,yes,NYSE
EEE,10.00,9000000,900000,no,no,NYSE
FFF,10.00,9000000,900000,no,yes,OTC
GGG,50.00,5000000,100000,no,yes,ARCA
HHH,50.000000001,9000000,900000,no,yes,ARCA
III,,9000000,900000,no,yes,ARCA
";

fn spec(s: &str) -> Spec {
    Spec::parse(s).unwrap()
}

fn snap() -> Snapshot {
    Snapshot::parse(SNAP).unwrap()
}

#[test]
fn the_spec_reads_and_renders_canonically() {
    let s = spec(SPEC);
    let text = s.render();
    assert_eq!(Spec::parse(&text).unwrap(), s);
    assert_eq!(Spec::parse(&text).unwrap().render(), text);
    assert!(text.contains("param min_price = 2.00\n"));
    // Order of conditions and spelling of numbers do not change the fingerprint.
    let b = spec(
        "universe v1\n# note\nparam min_price = 2\nparam min_adv = 5000000\nstatic exchange in ARCA NYSE NASDAQ; shortable = yes; etf = no; adv_dollar >= @min_adv; price <= 50.0; price >= @min_price\ndynamic top 2 by gap_permille desc keep 3 every 5 where trades >= 2\n",
    );
    assert_eq!(b.fingerprint(), s.fingerprint());
    // A different threshold does.
    assert_ne!(
        spec(&SPEC.replace("= 5000000", "= 5000001")).fingerprint(),
        s.fingerprint()
    );
}

#[test]
fn bad_specs_say_what_is_wrong() {
    let bad = |s: &str, want: &str| {
        let e = Spec::parse(&format!("universe v1\n{s}\n"))
            .unwrap_err()
            .to_string();
        assert!(e.contains(want), "{s}: {e}");
    };
    bad("param a = 1", "never used");
    bad("static price >= @a", "never declared");
    bad("param a = 1\nstatic price >= @a; adv_dollar >= @a", "both");
    bad("param a = 1.5\nstatic adv_dollar >= @a", "whole number");
    bad("static price >= 1.0000000001", "price");
    bad("static colour = red", "unknown feature");
    bad("static etf >= yes", "use = or !=");
    bad("static exchange = NYSE", "use `in`");
    bad("static exchange in nyse", "not a name");
    bad("static exchange in", "at least one");
    bad("static price >= 1\nstatic etf = no", "only one `static`");
    bad("dynamic top 5 by trades desc keep 4 every 5", "below top");
    bad("dynamic top 0 by trades desc keep 4 every 5", "above zero");
    bad("dynamic top 5 by trades up keep 5 every 5", "desc or asc");
    bad(
        "dynamic top 5 by nonsense desc keep 5 every 5",
        "not a live feature",
    );
    bad(
        "dynamic top 5 by trades desc keep 5 every 5 where trades >> 3",
        "comparison",
    );
    bad(
        "dynamic top 5 by trades desc keep 5 every 5\ndynamic top 5 by trades desc keep 5 every 5",
        "only one `dynamic`",
    );
    bad("param a = 1\nparam a = 2\nstatic adv_dollar >= @a", "twice");
    assert!(Spec::parse("universe v2\n").is_err());
    assert!(Spec::parse("").is_err());
}

#[test]
fn static_selection_applies_every_condition_and_unknown_fails() {
    let sel = select(&spec(SPEC), &snap()).unwrap();
    // BBB price, CCC adv one short, DDD etf, EEE not shortable, FFF exchange, HHH one billionth over,
    // III price unknown.
    assert_eq!(sel.symbols, ["AAA", "GGG"]);
    assert_eq!(
        sel.params,
        [
            ("min_adv".to_owned(), "5000000".to_owned()),
            ("min_price".to_owned(), "2.00".to_owned())
        ]
    );
    assert_eq!(sel.spec_fp, spec(SPEC).fingerprint());
    assert_eq!(sel.snapshot_fp, snap().fingerprint());
    // Boundaries are inclusive for >= and <=.
    let t = |s: &str| {
        select(&spec(&format!("universe v1\nstatic {s}\n")), &snap())
            .unwrap()
            .symbols
    };
    assert_eq!(t("price = 50"), ["GGG"]);
    assert_eq!(
        t("price > 10; price < 51; etf = no; shortable = yes; exchange in ARCA"),
        ["GGG", "HHH"]
    );
    assert_eq!(
        t("adv_dollar != 9000000; price >= 1; exchange not in NYSE"),
        ["GGG"]
    );
    assert_eq!(t("etf = yes"), ["DDD"]);
    // No conditions: everyone.
    assert_eq!(
        select(&spec("universe v1\n"), &snap())
            .unwrap()
            .symbols
            .len(),
        9
    );
}

#[test]
fn a_spec_that_needs_a_column_the_snapshot_lacks_refuses() {
    let s = spec("universe v1\nstatic float >= 1000000; price >= 1\n");
    assert_eq!(
        select(&s, &snap()).unwrap_err(),
        SelectError::MissingColumn(StaticFeature::Float)
    );
    // And a dynamic gap needs the reference price.
    let no_price = Snapshot::parse("# as_of 2026-10-02\nsymbol,adv_dollar\nAAA,1\n").unwrap();
    let g = spec("universe v1\ndynamic top 1 by gap_permille desc keep 1 every 1\n");
    assert_eq!(
        select(&g, &no_price).unwrap_err(),
        SelectError::MissingColumn(StaticFeature::Price)
    );
    let v = spec(
        "universe v1\ndynamic top 1 by trades desc keep 1 every 1 where volume_ratio_permille >= 1\n",
    );
    assert_eq!(
        select(&v, &no_price).unwrap_err(),
        SelectError::MissingColumn(StaticFeature::AdvShares)
    );
    assert!(select(&spec("universe v1\nstatic adv_dollar >= 1\n"), &no_price).is_ok());
}

#[test]
fn the_snapshot_reads_and_renders_canonically() {
    let s = snap();
    assert_eq!(s.as_of, "2026-10-02");
    assert_eq!(s.row("HHH").unwrap().price, Some(50_000_000_001));
    assert_eq!(s.row("III").unwrap().price, None);
    let text = s.render();
    assert_eq!(Snapshot::parse(&text).unwrap(), s);
    // Column order and row order in the file do not matter.
    let shuffled = "# as_of 2026-10-02\nsymbol,exchange,shortable,etf,adv_shares,adv_dollar,price\nZZZ,NYSE,yes,no,1,2,3.5\nAAA,NASDAQ,yes,no,900000,9000000,10\n";
    let a = Snapshot::parse(shuffled).unwrap();
    assert!(a.render().starts_with("# as_of 2026-10-02\nsymbol,price,adv_dollar,adv_shares,exchange,etf,shortable\nAAA,10.00,9000000,900000,NASDAQ,no,yes\nZZZ,3.50,2,1,NYSE,no,yes\n"));
    assert_eq!(
        a.fingerprint(),
        Snapshot::parse(&a.render()).unwrap().fingerprint()
    );
    assert_ne!(a.fingerprint(), s.fingerprint());
    for (bad, want) in [
        ("", "empty"),
        ("symbol,price\n", "as_of"),
        ("# as_of 2026-13-01\nsymbol\n", "as_of"),
        ("# as_of 2026/10/02\nsymbol\n", "as_of"),
        ("# as_of 2026-10/02\nsymbol\n", "as_of"),
        ("# as_of 2026/10-02\nsymbol\n", "as_of"),
        ("# as_of 2026-10-32\nsymbol\n", "as_of"),
        ("# as_of 2026-10-02\n", "header"),
        ("# as_of 2026-10-02\nprice\n", "start with `symbol`"),
        ("# as_of 2026-10-02\nsymbol,colour\n", "unknown column"),
        ("# as_of 2026-10-02\nsymbol,price,price\n", "twice"),
        ("# as_of 2026-10-02\nsymbol,price\nAAA\n", "cells"),
        ("# as_of 2026-10-02\nsymbol,price\naaa,1\n", "not a symbol"),
        ("# as_of 2026-10-02\nsymbol,price\nAAA,1e3\n", "price"),
        ("# as_of 2026-10-02\nsymbol,etf\nAAA,maybe\n", "yes or no"),
        (
            "# as_of 2026-10-02\nsymbol,exchange\nAAA,nyse\n",
            "exchange",
        ),
        ("# as_of 2026-10-02\nsymbol,price\nAAA,1\nAAA,2\n", "twice"),
    ] {
        let e = Snapshot::parse(bad).unwrap_err().to_string();
        assert!(e.contains(want), "{bad:?}: {e}");
    }
}

#[test]
fn a_selection_round_trips_and_rejects_damage() {
    let sel = select(&spec(SPEC), &snap()).unwrap();
    let text = sel.render();
    assert_eq!(Selection::parse(&text).unwrap(), sel);
    assert_eq!(
        Selection::parse(&text).unwrap().fingerprint(),
        sel.fingerprint()
    );
    for (from, to) in [
        ("count 2", "count 3"),
        ("GGG", "AAA"),
        ("members v1", "members v2"),
        ("spec ", "spec x"),
        ("AAA\nGGG", "GGG\nAAA"),
    ] {
        assert!(
            Selection::parse(&text.replacen(from, to, 1)).is_err(),
            "{from}"
        );
    }
}

#[test]
fn diff_names_the_changes_and_their_effect() {
    let old = spec(SPEC);
    let new = spec(
        &SPEC
            .replace("min_adv = 5000000", "min_adv = 4000000")
            .replace("etf = no; ", "")
            .replace("every 5", "every 10"),
    );
    let d = diff(&old, &new, Some(&snap())).unwrap();
    assert!(
        d.changes
            .contains(&"~ param min_adv: 5000000 -> 4000000".to_owned()),
        "{:?}",
        d.changes
    );
    assert!(d.changes.contains(&"- static etf = no".to_owned()));
    assert!(
        d.changes
            .iter()
            .any(|c| c.starts_with("+ dynamic top 2") && c.contains("every 10"))
    );
    assert!(
        d.changes
            .iter()
            .any(|c| c.starts_with("- dynamic top 2") && c.contains("every 5"))
    );
    // The etf and the 4,999,999 one are not let in by 4,000,000 alone: CCC (4999999) and DDD (etf).
    assert_eq!(d.added, ["CCC", "DDD"]);
    assert!(d.removed.is_empty());
    assert!(d.widens);
    // The reverse narrows.
    let r = diff(&new, &old, Some(&snap())).unwrap();
    assert_eq!(r.removed, ["CCC", "DDD"]);
    assert!(!r.widens);
    // Without a snapshot only the text differences; identical specs differ in nothing.
    assert!(diff(&old, &new, None).unwrap().added.is_empty());
    let same = diff(&old, &old, Some(&snap())).unwrap();
    assert!(same.changes.is_empty() && same.added.is_empty() && !same.widens);
    // Parameters added and dropped.
    let p = diff(
        &spec("universe v1\nstatic price >= 1\n"),
        &spec("universe v1\nparam p = 2\nstatic price >= @p\n"),
        None,
    )
    .unwrap();
    assert!(
        p.changes.contains(&"+ param p = 2.00".to_owned()),
        "{:?}",
        p.changes
    );
    let q = diff(
        &spec("universe v1\nparam p = 2\nstatic price >= @p\n"),
        &spec("universe v1\nstatic price >= 1\n"),
        None,
    )
    .unwrap();
    assert!(
        q.changes.contains(&"- param p".to_owned()),
        "{:?}",
        q.changes
    );
}

fn trade(t: &mut Tier0, id: u32, ts: u64, px: i64, size: u32) {
    t.on_event(&Event::Trade(Trade {
        hdr: Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument: id,
            provider: ProviderId::Synthetic,
        },
        px: Px::from_raw(px),
        size,
        flags: TradeFlags::NONE,
    }));
}

const D: i64 = 1_000_000_000;

fn world() -> (Tier0, Vec<RefInfo>) {
    let refs = vec![
        RefInfo {
            price: Some(10 * D),
            adv_shares: Some(1000),
            ..RefInfo::default()
        };
        6
    ];
    (Tier0::new(6), refs)
}

#[test]
fn live_measurements_come_from_tier0() {
    let (mut t, mut refs) = world();
    refs[3] = RefInfo::default();
    trade(&mut t, 0, 1, 10 * D, 100);
    trade(&mut t, 0, 2, 11 * D, 100); // +10%
    trade(&mut t, 0, 3, 9 * D, 50); // down 10%, range 2/9
    trade(&mut t, 3, 1, 10 * D, 10);
    let v = Tier0View {
        tier0: &t,
        refs: &refs,
    };
    assert_eq!(v.value(0, LiveFeature::Trades), Some(3));
    assert_eq!(v.value(0, LiveFeature::GapPermille), Some(-100));
    assert_eq!(
        v.value(0, LiveFeature::VolumeRatioPermille),
        Some(250_000 / 1000)
    );
    assert_eq!(
        v.value(0, LiveFeature::DollarVolume),
        Some((10 * 100 + 11 * 100 + 9 * 50) as i64)
    );
    assert_eq!(v.value(0, LiveFeature::RangePermille), Some(2 * 1000 / 9));
    // No reference: no gap or ratio, but the plain counts work. Never traded: nothing but zero trades.
    assert_eq!(v.value(3, LiveFeature::GapPermille), None);
    assert_eq!(v.value(3, LiveFeature::VolumeRatioPermille), None);
    assert_eq!(v.value(3, LiveFeature::Trades), Some(1));
    assert_eq!(v.value(1, LiveFeature::GapPermille), None);
    assert_eq!(v.value(1, LiveFeature::DollarVolume), None);
    assert_eq!(v.value(1, LiveFeature::VolumeRatioPermille), None);
    assert_eq!(v.value(99, LiveFeature::Trades), None);
}

fn gaps(t: &mut Tier0, ts: u64, pct: [i64; 6]) {
    // Two trades each so the `trades >= 2` filter passes; the last one sets the gap.
    for (id, p) in pct.iter().enumerate() {
        trade(t, id as u32, ts, 10 * D, 1);
        trade(t, id as u32, ts + 1, 10 * D + p * D / 10, 1);
    }
}

#[test]
fn the_selector_keeps_the_top_with_hysteresis_and_on_schedule() {
    let s =
        spec("universe v1\ndynamic top 2 by gap_permille desc keep 3 every 5 where trades >= 2\n");
    let mut sel = Selector::new(&s).unwrap();
    let (mut t, refs) = world();
    let cand = [0, 1, 2, 3, 4, 5];
    // Nothing has traded: first call evaluates (empty).
    {
        let v = Tier0View {
            tier0: &t,
            refs: &refs,
        };
        assert_eq!(sel.update(0, &v, &cand), Some(Change::default()));
    }
    // Ranks by gap: id 5 (+50), 4 (+40), 3 (+30), 2 (+20), 1, 0. Too soon to re-rank.
    gaps(&mut t, 10, [0, 10, 20, 30, 40, 50]);
    {
        let v = Tier0View {
            tier0: &t,
            refs: &refs,
        };
        assert_eq!(sel.update(4_999_999_999, &v, &cand), None);
        assert_eq!(
            sel.update(5_000_000_000, &v, &cand),
            Some(Change {
                entered: vec![4, 5],
                left: vec![]
            })
        );
        assert_eq!(sel.members(), [4, 5]);
    }
    // Id 3 overtakes 4 (+45 vs +40): 3 is rank 2 (enters), 4 is rank 3 (stays by hysteresis).
    trade(&mut t, 3, 20, 10 * D + 45 * D / 10, 1);
    {
        let v = Tier0View {
            tier0: &t,
            refs: &refs,
        };
        assert_eq!(
            sel.update(10_000_000_000, &v, &cand),
            Some(Change {
                entered: vec![3],
                left: vec![]
            })
        );
        assert_eq!(sel.members(), [3, 4, 5]);
    }
    // Id 2 climbs to rank 4 at best: not above keep, never joins. Id 4 falls to rank 4: leaves.
    trade(&mut t, 4, 30, 10 * D + 25 * D / 10, 1);
    trade(&mut t, 2, 30, 10 * D + 35 * D / 10, 1);
    {
        let v = Tier0View {
            tier0: &t,
            refs: &refs,
        };
        // Ranks: 5 (+50), 3 (+45), 2 (+35), 4 (+25). Top 2 are 5 and 3; 2 is rank 3 but not a member.
        let c = sel.update(15_000_000_000, &v, &cand).unwrap();
        assert_eq!(
            c,
            Change {
                entered: vec![],
                left: vec![4]
            }
        );
        assert_eq!(sel.members(), [3, 5]);
    }
    // A symbol that fails the filter drops out whatever its rank.
    let mut one = Selector::new(&spec(
        "universe v1\ndynamic top 2 by gap_permille desc keep 2 every 1 where trades >= 3\n",
    ))
    .unwrap();
    let v = Tier0View {
        tier0: &t,
        refs: &refs,
    };
    // Only ids 2, 3 and 4 have three trades; id 5 has the best gap but two.
    assert_eq!(one.update(0, &v, &cand).unwrap().entered, [2, 3]);
    let mut none = Selector::new(&spec(
        "universe v1\ndynamic top 2 by gap_permille desc keep 2 every 1 where trades >= 4\n",
    ))
    .unwrap();
    assert_eq!(
        none.update(0, &v, &cand).unwrap().entered,
        Vec::<u32>::new()
    );
    // Ties break by instrument id, and ascending order ranks the lowest first.
    let (mut t2, refs2) = world();
    gaps(&mut t2, 1, [10, 10, 10, 10, 10, 10]);
    let v2 = Tier0View {
        tier0: &t2,
        refs: &refs2,
    };
    let mut tie = Selector::new(&spec(
        "universe v1\ndynamic top 2 by gap_permille desc keep 2 every 1\n",
    ))
    .unwrap();
    tie.update(0, &v2, &cand);
    assert_eq!(tie.members(), [0, 1]);
    gaps(&mut t2, 3, [60, 50, 10, 10, 10, 40]);
    let v2 = Tier0View {
        tier0: &t2,
        refs: &refs2,
    };
    let mut asc = Selector::new(&spec(
        "universe v1\ndynamic top 2 by gap_permille asc keep 2 every 1\n",
    ))
    .unwrap();
    asc.update(0, &v2, &cand);
    assert_eq!(asc.members(), [2, 3]);
}

#[test]
fn only_candidates_are_ranked_and_a_static_only_spec_has_no_selector() {
    let s = spec("universe v1\ndynamic top 1 by trades desc keep 1 every 1\n");
    let (mut t, refs) = world();
    gaps(&mut t, 1, [0; 6]);
    trade(&mut t, 5, 9, 10 * D, 1);
    let v = Tier0View {
        tier0: &t,
        refs: &refs,
    };
    let mut sel = Selector::new(&s).unwrap();
    sel.update(0, &v, &[0, 1, 2]);
    assert_eq!(sel.members(), [0]);
    assert!(Selector::new(&spec("universe v1\nstatic price >= 1\n")).is_none());
}

#[test]
fn a_dynamic_threshold_can_be_a_param() {
    let s = spec(
        "universe v1\nparam busy = 3\ndynamic top 5 by trades desc keep 5 every 1 where trades >= @busy\n",
    );
    assert_eq!(s.params["busy"].raw, 3);
    let (mut t, refs) = world();
    gaps(&mut t, 1, [0; 6]);
    trade(&mut t, 2, 5, 10 * D, 1);
    let v = Tier0View {
        tier0: &t,
        refs: &refs,
    };
    let mut sel = Selector::new(&s).unwrap();
    sel.update(0, &v, &[0, 1, 2, 3]);
    assert_eq!(sel.members(), [2]);
}

// ---- history columns (E19-S04) ----

#[test]
fn a_snapshot_and_a_spec_from_before_the_history_columns_keep_their_fingerprints_and_selection() {
    // Values taken from the code as it was before the columns were added: nothing about an old snapshot, an old
    // spec or a selection stored from them may move.
    let s = spec(SPEC);
    let n = snap();
    assert_eq!(s.fingerprint(), 0x83f5_4caf_ea37_8012);
    assert_eq!(n.fingerprint(), 0xb699_fb4a_a458_d822);
    assert_eq!(Snapshot::parse(&n.render()).unwrap(), n);
    let sel = select(&s, &n).unwrap();
    assert_eq!(
        sel.render(),
        "members v1\nas_of 2026-10-02\nspec 83f54cafea378012\nsnapshot b699fb4aa458d822\nparam min_adv = 5000000\nparam min_price = 2.00\ncount 2\nAAA\nGGG\n"
    );
    assert_eq!(sel.symbols, ["AAA", "GGG"]);
    // None of the new columns is present, so none of them is asked for or shown.
    for f in HISTORY_FEATURES {
        assert!(!n.columns.contains(&f), "{}", f.name());
        assert!(!n.render().contains(f.name()));
    }
}

const HIST: &str = "# as_of 2026-10-02
symbol,price,prev_high,prev_low,prev_close,atr14,ema100h_state,ema100h_count,vol_first1,vol_first5,vol_pre,cumvol_0935,cumvol_1000,cumvol_1030,cumvol_1100,cumvol_1200,cumvol_1400,cumvol_1530
AAA,10.00,10.50,9.25,10.10,0.35,655360000000,140,1200,5400,30000,9000,40000,60000,80000,100000,150000,190000
BBB,5.00,,,,,,,,,,,,,,,,
";

#[test]
fn the_history_columns_read_render_and_stay_unknown_where_the_cell_is_empty() {
    let s = Snapshot::parse(HIST).unwrap();
    for f in HISTORY_FEATURES {
        assert!(s.columns.contains(&f), "{}", f.name());
    }
    let a = s.row("AAA").unwrap();
    // Every column, by name, against the cell it came from.
    let expected: [i64; 16] = [
        10_500_000_000,
        9_250_000_000,
        10_100_000_000,
        350_000_000,
        655_360_000_000,
        140,
        1200,
        5400,
        30_000,
        9_000,
        40_000,
        60_000,
        80_000,
        100_000,
        150_000,
        190_000,
    ];
    for (f, want) in HISTORY_FEATURES.iter().zip(expected) {
        assert_eq!(a.num(*f), Some(want), "{}", f.name());
    }
    for (i, (f, _)) in CUMVOL_CHECKPOINTS.iter().enumerate() {
        assert_eq!(a.cumvol[i], Some(expected[9 + i]), "{}", f.name());
    }
    let b = s.row("BBB").unwrap();
    assert_eq!(
        b.num(StaticFeature::PrevHigh),
        None,
        "an empty cell is unknown, not zero"
    );
    assert_eq!(b.num(StaticFeature::CumVol1400), None);
    // Canonical: the same text comes back, and the fingerprint follows the content.
    assert_eq!(s.render(), HIST);
    assert_eq!(
        Snapshot::parse(&s.render()).unwrap().fingerprint(),
        s.fingerprint()
    );
    // The reference row a host holds carries them; the EMA state rounds to a price (655,360,000,000 / 2^16 = 10 million
    // raw units, ten milliseconds).
    let info = RefInfo::from_row(a);
    assert_eq!(info.price, Some(10_000_000_000));
    assert_eq!(
        info.hist,
        HistInfo {
            prev_high: Some(10_500_000_000),
            prev_low: Some(9_250_000_000),
            prev_close: Some(10_100_000_000),
            atr14: Some(350_000_000),
            ema100h_state: Some(655_360_000_000),
            ema100h_count: Some(140),
            vol_first1: Some(1200),
            vol_first5: Some(5400),
            vol_pre: Some(30_000),
            cumvol: [
                Some(9_000),
                Some(40_000),
                Some(60_000),
                Some(80_000),
                Some(100_000),
                Some(150_000),
                Some(190_000)
            ],
        }
    );
    assert_eq!(info.hist.ema100h(), Some(10_000_000));
    // The state is rounded to the nearest raw unit, half up: 10,000,000 and a half is 10,000,001.
    let at = |state: i64| HistInfo {
        ema100h_state: Some(state),
        ..HistInfo::default()
    };
    assert_eq!(at((10_000_000 << 16) + 32_767).ema100h(), Some(10_000_000));
    assert_eq!(at((10_000_000 << 16) + 32_768).ema100h(), Some(10_000_001));
    assert_eq!(HistInfo::default().ema100h(), None);
    assert_eq!(RefInfo::from_row(b).hist, HistInfo::default());
    // A damaged cell is refused with its column named.
    let bad = HIST.replacen(",140,", ",x,", 1);
    assert!(
        Snapshot::parse(&bad)
            .unwrap_err()
            .to_string()
            .contains("ema100h_count")
    );
}

#[test]
fn a_spec_can_require_columns_it_does_not_filter_on_and_a_snapshot_without_them_is_refused() {
    let with = spec("universe v1\nrequires prev_high atr14\nstatic price >= 1\n");
    // Canonical: requires is named in a fixed order, and a repeat changes nothing.
    assert_eq!(
        with.render(),
        "universe v1\nrequires prev_high atr14\nstatic price >= 1.00\n"
    );
    assert_eq!(
        spec("universe v1\nrequires atr14 prev_high atr14\nstatic price >= 1\n").fingerprint(),
        with.fingerprint()
    );
    assert_eq!(Spec::parse(&with.render()).unwrap(), with);
    assert_ne!(
        with.fingerprint(),
        spec("universe v1\nstatic price >= 1\n").fingerprint()
    );
    assert!(with.needs().contains(&StaticFeature::Atr14));
    assert!(with.needs().contains(&StaticFeature::PrevHigh));
    // The snapshot of before the columns lacks them: it refuses, naming the first.
    assert_eq!(
        select(&with, &snap()).unwrap_err(),
        SelectError::MissingColumn(StaticFeature::PrevHigh)
    );
    // One with them, or with only one of them, is judged accordingly.
    let full = Snapshot::parse(HIST).unwrap();
    assert_eq!(select(&with, &full).unwrap().symbols, ["AAA", "BBB"]);
    let mut part = full.clone();
    part.columns.remove(&StaticFeature::Atr14);
    assert_eq!(
        select(&with, &part).unwrap_err(),
        SelectError::MissingColumn(StaticFeature::Atr14)
    );
    // Only requiring does not filter on an unknown value: a symbol with nothing in the column still passes.
    assert!(
        select(&with, &full)
            .unwrap()
            .symbols
            .contains(&"BBB".to_owned())
    );
    // Bad lines say what is wrong.
    for (text, want) in [
        ("universe v1\nrequires nonsense\n", "not a column"),
        (
            "universe v1\nrequires atr14\nrequires prev_low\n",
            "only one `requires`",
        ),
    ] {
        let e = Spec::parse(text).unwrap_err().to_string();
        assert!(e.contains(want), "{text}: {e}");
    }
}
