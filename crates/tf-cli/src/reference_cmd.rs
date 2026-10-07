//! `tf reference build`: a reference snapshot for the universe filter from fetched files.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;

use tf_reference::{
    ASSET_COLUMNS, BAR_COLUMNS, DEFAULT_WICK_CLIP_PERMILLE, HISTORY_COLUMNS, HistoryParams,
    MinuteHistory, Params, Symbology, build, date_days, merge_assets, merge_etf_list,
    merge_history, parse_assets, read_bars, read_minute_bars,
};
use tf_universe::{Snapshot, StaticFeature};

const USAGE: &str = "usage:
    tf reference build --bars FILE --symbology FILE --out FILE [--assets FILE] [--etf-list FILE]
                       [--minutes FILE [--history-sessions N] [--wick-clip PERMILLE]] [--up-to YYYY-MM-DD] [--window N] [--min-days N]";

pub(crate) fn reference(args: &[String]) -> Result<(), String> {
    print!("{}", run(args)?);
    Ok(())
}

fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    if args.first().map(String::as_str) != Some("build") {
        return Err(USAGE.to_owned());
    }
    let (mut bars, mut sym, mut out, mut assets, mut etf, mut up_to) =
        (None, None, None, None, None, None);
    let mut minutes: Option<String> = None;
    let mut hist_sessions = HistoryParams::default().sessions;
    let mut wick_clip = DEFAULT_WICK_CLIP_PERMILLE;
    let mut p = Params::default();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        let mut v = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
        let num = |what: &str, s: String| {
            s.parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or(format!("{what} {s}: not a number above zero"))
        };
        match a.as_str() {
            "--bars" => bars = Some(v("--bars")?),
            "--symbology" => sym = Some(v("--symbology")?),
            "--out" => out = Some(v("--out")?),
            "--assets" => assets = Some(v("--assets")?),
            "--etf-list" => etf = Some(v("--etf-list")?),
            "--minutes" => minutes = Some(v("--minutes")?),
            "--wick-clip" => {
                let c = v("--wick-clip")?;
                wick_clip = c
                    .parse::<u32>()
                    .map_err(|_| format!("--wick-clip {c}: not a whole number of permille"))?;
            }
            "--history-sessions" => {
                hist_sessions = num("--history-sessions", v("--history-sessions")?)?;
            }
            "--up-to" => {
                let d = v("--up-to")?;
                up_to = Some(date_days(&d).ok_or(format!("--up-to {d}: expected YYYY-MM-DD"))?);
            }
            "--window" => p.window = num("--window", v("--window")?)?,
            "--min-days" => p.min_days = num("--min-days", v("--min-days")?)?,
            o => return Err(format!("unknown argument {o}\n{USAGE}")),
        }
    }
    let (Some(bars), Some(sym), Some(out)) = (bars, sym, out) else {
        return Err(USAGE.to_owned());
    };
    let bar_rows = read_bars(&read(&bars)?).map_err(|e| format!("{bars}: {e}"))?;
    let symbology = Symbology::parse(&read(&sym)?).map_err(|e| format!("{sym}: {e}"))?;
    let (mut rows, rep) =
        build(&bar_rows, &symbology, up_to.unwrap_or(i64::MAX), p).map_err(|e| e.to_string())?;
    let mut columns: BTreeSet<StaticFeature> = BAR_COLUMNS.into_iter().collect();
    let mut text = String::new();
    if let Some(a) = &assets {
        let list = parse_assets(&read(a)?).map_err(|e| format!("{a}: {e}"))?;
        let found = merge_assets(&mut rows, &list);
        columns.extend(ASSET_COLUMNS);
        let _ = writeln!(
            text,
            "assets: {found} of {} symbols are on Alpaca's list",
            rows.len()
        );
    }
    if let Some(e) = &etf {
        let n = merge_etf_list(&mut rows, &read(e)?).map_err(|m| format!("{e}: {m}"))?;
        columns.insert(StaticFeature::Etf);
        let _ = writeln!(text, "etf list: {n} symbols marked");
    }
    if let Some(m) = &minutes {
        // Streamed: a whole market's minutes do not fit in memory as text.
        let file = fs::File::open(m).map_err(|e| format!("{m}: {e}"))?;
        let mut h = MinuteHistory::new(rep.as_of_day, wick_clip);
        read_minute_bars(std::io::BufReader::new(file), &mut h).map_err(|e| format!("{m}: {e}"))?;
        let hp = HistoryParams {
            sessions: hist_sessions,
            average_over: p.window,
            min_days: p.min_days,
        };
        let (hist, hrep) = h.build(hp).map_err(|e| format!("{m}: {e}"))?;
        if hrep.as_of != rep.as_of {
            return Err(format!(
                "{m}: the minute bars end on {} but the daily bars on {}: pull both to the same day",
                hrep.as_of, rep.as_of
            ));
        }
        let (matched, only) = merge_history(&mut rows, &hist);
        columns.extend(HISTORY_COLUMNS);
        let _ = writeln!(
            text,
            "minute history: {} bars read, {} sessions ({} to {}, {} early closes), {matched} of {} symbols matched ({only} in the history only); {} bars after the date, {} on closed days, {} outside the sessions, {} with names not listable; {} regular-session bars had a high or low capped at {wick_clip} permille beyond their open and close",
            hrep.bars_read,
            hrep.sessions,
            hrep.first_session,
            hrep.as_of,
            hrep.early_closes,
            rows.len(),
            hrep.bars_after,
            hrep.bars_closed_day,
            hrep.bars_outside_sessions,
            hrep.bars_bad_symbol,
            hrep.bars_clipped,
        );
    }
    let snap = Snapshot {
        as_of: rep.as_of.clone(),
        columns,
        rows,
    };
    fs::write(&out, snap.render()).map_err(|e| format!("{out}: {e}"))?;
    let mut s = format!(
        "snapshot as of {} written to {out} ({} symbols, fingerprint {:016x})\nsessions {} ({} to {}); {} bars used, {} unmapped, {} after the date (ignored), {} symbols not listable\n{} symbols have no price (no bar on {}), {} have no averages (fewer than {} sessions)\n",
        rep.as_of,
        rep.symbols,
        snap.fingerprint(),
        rep.sessions.len(),
        rep.sessions.first().map_or("-", String::as_str),
        rep.sessions.last().map_or("-", String::as_str),
        rep.bars_used,
        rep.bars_unmapped,
        rep.bars_after,
        rep.symbols_skipped,
        rep.no_price,
        rep.as_of,
        rep.no_average,
        p.min_days,
    );
    s.push_str(&text);
    if assets.is_none() {
        s.push_str("no asset list: tradable, shortable, easy_to_borrow and exchange are absent, so a universe using them will refuse\n");
    }
    if etf.is_none() {
        s.push_str("no etf list: etf is absent\n");
    }
    if minutes.is_none() {
        s.push_str("no minute history: the previous session's high, low and close, the ATR, the volume baselines and the hourly EMA are absent, so a strategy that needs them will refuse\n");
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn builds_a_snapshot_a_universe_can_use() {
        let d = std::env::temp_dir().join(format!("tf-reference-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let p = |n: &str| d.join(n).to_string_lossy().into_owned();
        let mut bars =
            String::from("ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume\n");
        for day in 0..3u64 {
            let ts = (20_700 + day) * 86_400_000_000_000;
            bars.push_str(&format!(
                "{ts},35,90,1,10000000000,11000000000,9000000000,10000000000,1000\n"
            ));
        }
        fs::write(p("bars.csv"), bars).unwrap();
        fs::write(
            p("sym.json"),
            r#"{"result":{"AAA":[{"d0":"2026-01-01","d1":"2027-01-01","s":"1"}]}}"#,
        )
        .unwrap();
        fs::write(p("assets.json"), r#"[{"symbol":"AAA","class":"us_equity","exchange":"NYSE","status":"active","tradable":true,"shortable":true,"easy_to_borrow":true}]"#).unwrap();
        let (pb, ps, po, pa) = (
            p("bars.csv"),
            p("sym.json"),
            p("snap.csv"),
            p("assets.json"),
        );
        let mut a = vec![
            "build",
            "--bars",
            &pb,
            "--symbology",
            &ps,
            "--out",
            &po,
            "--window",
            "3",
            "--min-days",
            "2",
        ];
        let out = run(&args(&a)).unwrap();
        assert!(
            out.contains("1 symbols")
                && out.contains("no asset list")
                && out.contains("no etf list"),
            "{out}"
        );
        let snap = Snapshot::parse(&fs::read_to_string(p("snap.csv")).unwrap()).unwrap();
        assert_eq!(snap.as_of, "2026-09-06");
        assert_eq!(snap.row("AAA").unwrap().adv_dollar, Some(10_000 * 3 / 3));
        // Without the asset list a universe that uses its flags refuses; with it, it selects.
        let spec = tf_universe::Spec::parse("universe v1\nstatic shortable = yes\n").unwrap();
        assert!(tf_universe::select(&spec, &snap).is_err());
        a.extend(["--assets", &pa]);
        run(&args(&a)).unwrap();
        let snap = Snapshot::parse(&fs::read_to_string(p("snap.csv")).unwrap()).unwrap();
        assert_eq!(tf_universe::select(&spec, &snap).unwrap().symbols, ["AAA"]);
        // --up-to before any bar is an error; bad arguments are refused.
        let mut early = args(&a);
        early.extend(args(&["--up-to", "2020-01-01"]));
        assert!(run(&early).unwrap_err().contains("no bars"));
        assert!(run(&args(&["build"])).is_err());
        assert!(run(&args(&["build", "--bogus"])).is_err());
        assert!(
            run(&args(&[
                "build",
                "--bars",
                "x",
                "--symbology",
                "y",
                "--out",
                "z",
                "--window",
                "0"
            ]))
            .is_err()
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn minute_history_adds_the_columns_and_must_end_on_the_same_day() {
        let d = std::env::temp_dir().join(format!("tf-reference-min-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let p = |n: &str| d.join(n).to_string_lossy().into_owned();
        // Wednesday 30 September to Friday 2 October 2026, one symbol.
        let mut bars =
            String::from("ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume\n");
        for day in 20_726..=20_728u64 {
            bars.push_str(&format!(
                "{},35,90,1,10000000000,11000000000,9000000000,10000000000,1000\n",
                day * 86_400_000_000_000
            ));
        }
        fs::write(p("bars.csv"), bars).unwrap();
        fs::write(
            p("sym.json"),
            r#"{"result":{"AAA":[{"d0":"2026-01-01","d1":"2027-01-01","s":"1"}]}}"#,
        )
        .unwrap();
        // The open is 13:30 UTC in October: a bar at the open and one an hour later, on each day.
        let min = String::from(
            "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume,symbol\n",
        );
        let minutes = |last: u64| {
            let mut s = min.clone();
            for day in 20_726..=last {
                for (m, v) in [(0u64, 100u64), (60, 40)] {
                    let ts = (day * 86_400 + 13 * 3600 + 1800 + m * 60) * 1_000_000_000;
                    s.push_str(&format!(
                        "{ts},33,90,1,10000000000,11000000000,9000000000,10500000000,{v},AAA\n"
                    ));
                }
            }
            s
        };
        fs::write(p("min.csv"), minutes(20_728)).unwrap();
        let (pb, ps, po, pm) = (p("bars.csv"), p("sym.json"), p("snap.csv"), p("min.csv"));
        let a = [
            "build",
            "--bars",
            &pb,
            "--symbology",
            &ps,
            "--out",
            &po,
            "--window",
            "3",
            "--min-days",
            "2",
            "--minutes",
            &pm,
        ];
        let out = run(&args(&a)).unwrap();
        assert!(
            out.contains("minute history: 6 bars read, 3 sessions")
                && !out.contains("no minute history"),
            "{out}"
        );
        let snap = Snapshot::parse(&fs::read_to_string(p("snap.csv")).unwrap()).unwrap();
        let r = snap.row("AAA").unwrap();
        // The bars' highs (11) and lows (9) stand 5% outside their bodies (10 to 10.5): capped at 0.5% by default.
        assert_eq!(r.prev_high, Some(10_552_500_000));
        assert_eq!(r.prev_low, Some(9_950_000_000));
        assert_eq!(r.prev_close, Some(10_500_000_000));
        assert_eq!(r.vol_first1, Some(100));
        assert_eq!(r.cumvol[6], Some(140), "100 at the open and 40 an hour in");
        assert_eq!(r.atr14, None, "three sessions are too few");
        assert!(
            out.contains("6 regular-session bars had a high or low capped at 5 permille"),
            "{out}"
        );
        // No cap leaves the raw highs and lows.
        let mut raw = args(&a);
        raw.extend(args(&["--wick-clip", "4294967295"]));
        run(&raw).unwrap();
        let snap = Snapshot::parse(&fs::read_to_string(p("snap.csv")).unwrap()).unwrap();
        assert_eq!(snap.row("AAA").unwrap().prev_high, Some(11_000_000_000));
        run(&args(&a)).unwrap();
        let snap = Snapshot::parse(&fs::read_to_string(p("snap.csv")).unwrap()).unwrap();
        // A strategy that requires them can run on this snapshot; without --minutes it cannot.
        let need =
            tf_universe::Spec::parse("universe v1\nrequires prev_high vol_first1\n").unwrap();
        assert!(tf_universe::select(&need, &snap).is_ok());
        let without = run(&args(&[
            "build",
            "--bars",
            &pb,
            "--symbology",
            &ps,
            "--out",
            &po,
        ]))
        .unwrap();
        assert!(without.contains("no minute history"), "{without}");
        let snap = Snapshot::parse(&fs::read_to_string(p("snap.csv")).unwrap()).unwrap();
        assert!(tf_universe::select(&need, &snap).is_err());
        // Minute bars that end a day earlier than the daily bars are refused, not merged.
        fs::write(p("min.csv"), minutes(20_727)).unwrap();
        let e = run(&args(&a)).unwrap_err();
        assert!(
            e.contains("end on 2026-10-01 but the daily bars on 2026-10-02"),
            "{e}"
        );
        let _ = fs::remove_dir_all(&d);
    }
}
