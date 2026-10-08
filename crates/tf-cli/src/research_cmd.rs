//! `tf research`: a strategy set over the days of a history store, and what it left (E19-S31).
//!
//! `run` reads a strategy set (the variants, their budgets and limits: `tf_host::set`), the days of a store with each day's reference
//! snapshot as it was before that session, and runs every day as a live day is run (one host, a file ledger, the daily report) into a
//! results directory. `snapshots` makes those snapshots from daily bars. `show` says what a results directory holds.

use std::fs;
use std::path::{Path, PathBuf};

use tf_calendar::{Calendar, Date};
use tf_host::research::{
    CostModel, EvidenceWindow, Results, RunOptions, Setup, StoreSource, describe, run_with,
};
use tf_host::set::StrategySet;
use tf_reference::{
    ASSET_COLUMNS, BAR_COLUMNS, Params, Symbology, build, date_days, merge_assets, merge_etf_list,
    parse_assets, read_bars,
};
use tf_universe::{Snapshot, StaticFeature};

pub(crate) const USAGE: &str = "usage:
    tf research run --set FILE --store DIR --dataset NAME --schema NAME --snapshots DIR --out DIR
                    [--from DATE] [--to DATE] [--symbols A,B,C | --symbols-file FILE] [--evidence]
                    [--id-space N] [--latency-ms N]
    tf research show DIR
    tf research snapshots --bars FILE --symbology FILE --from DATE --to DATE --out DIR
                          [--assets FILE] [--etf-list FILE] [--window N] [--min-days N]";

pub(crate) fn research(args: &[String]) -> Result<(), String> {
    print!("{}", run(args)?);
    Ok(())
}

fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("run") => run_cmd(&args[1..]),
        Some("show") => show(&args[1..]),
        Some("snapshots") => snapshots(&args[1..]),
        _ => Err(USAGE.to_owned()),
    }
}

/// The value of each `--flag value` among `args`, and the flags that take none.
struct Flags {
    values: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Flags {
    fn parse(args: &[String], takes_value: &[&str], switches: &[&str]) -> Result<Flags, String> {
        let mut f = Flags {
            values: Vec::new(),
            switches: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if switches.contains(&a.as_str()) {
                f.switches.push(a.clone());
            } else if takes_value.contains(&a.as_str()) {
                let v = it.next().ok_or(format!("{a} needs a value"))?;
                if f.values.iter().any(|(k, _)| k == a) {
                    return Err(format!("{a} is given twice"));
                }
                f.values.push((a.clone(), v.clone()));
            } else {
                return Err(format!("unexpected argument {a}\n{USAGE}"));
            }
        }
        Ok(f)
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }

    fn need(&self, k: &str) -> Result<&str, String> {
        self.get(k).ok_or(format!("{k} is required\n{USAGE}"))
    }

    fn on(&self, k: &str) -> bool {
        self.switches.iter().any(|s| s == k)
    }
}

fn date_arg(f: &Flags, k: &str) -> Result<Option<String>, String> {
    match f.get(k) {
        None => Ok(None),
        Some(d) if date_days(d).is_some() => Ok(Some(d.to_owned())),
        Some(d) => Err(format!("{k} {d}: expected YYYY-MM-DD")),
    }
}

/// The symbols of a comma-separated list or of a file with one on each line (`#` begins a comment).
fn symbols_of(f: &Flags) -> Result<Option<Vec<String>>, String> {
    match (f.get("--symbols"), f.get("--symbols-file")) {
        (Some(_), Some(_)) => Err("give --symbols or --symbols-file, not both".to_owned()),
        (Some(list), None) => Ok(Some(
            list.split(',')
                .map(|s| s.trim().to_owned())
                .collect::<Vec<String>>(),
        )),
        (None, Some(path)) => Ok(Some(
            read(path)?
                .lines()
                .map(|l| l.split('#').next().unwrap_or("").trim())
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect::<Vec<String>>(),
        )),
        (None, None) => Ok(None),
    }
    .and_then(|v| match v {
        Some(v) if v.iter().any(|s| s.is_empty()) => Err("a symbol is empty".to_owned()),
        Some(v) if v.is_empty() => Err("no symbols were given".to_owned()),
        v => Ok(v),
    })
}

/// Milliseconds as the nanoseconds the cost model keeps.
fn latency_ns(ms: &str) -> Result<u64, String> {
    ms.parse::<u64>()
        .ok()
        .and_then(|m| m.checked_mul(1_000_000))
        .ok_or(format!(
            "--latency-ms {ms}: not a whole number of milliseconds"
        ))
}

fn run_cmd(args: &[String]) -> Result<String, String> {
    let f = Flags::parse(
        args,
        &[
            "--set",
            "--store",
            "--dataset",
            "--schema",
            "--snapshots",
            "--out",
            "--from",
            "--to",
            "--symbols",
            "--symbols-file",
            "--id-space",
            "--latency-ms",
        ],
        &["--evidence"],
    )?;
    let (from, to) = (date_arg(&f, "--from")?, date_arg(&f, "--to")?);
    let id_space = match f.get("--id-space") {
        None => 16_384,
        Some(n) => n
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(format!("--id-space {n}: not a number above zero"))?,
    };
    let mut cost = CostModel::published();
    if let Some(ms) = f.get("--latency-ms") {
        cost.latency_ns = latency_ns(ms)?;
    }
    let (set, defs) = StrategySet::load(Path::new(f.need("--set")?)).map_err(|e| e.to_string())?;
    let host = set.host_config(id_space).map_err(|e| e.to_string())?;
    let mut source = StoreSource::open(
        Path::new(f.need("--store")?),
        f.need("--dataset")?,
        f.need("--schema")?,
        from.as_deref(),
        to.as_deref(),
        Path::new(f.need("--snapshots")?),
        symbols_of(&f)?,
    )?;
    let opts = RunOptions {
        evidence: f.on("--evidence").then(EvidenceWindow::default),
    };
    let out = PathBuf::from(f.need("--out")?);
    let rep = run_with(
        &Setup {
            host: &host,
            cost: &cost,
            defs: &defs,
        },
        &mut source,
        &out,
        &opts,
    )
    .map_err(|e| e.to_string())?;
    let mut s = format!(
        "{} days run, {} already there; {} events, {} round trips, into {}\n",
        rep.ran.len(),
        rep.skipped.len(),
        rep.events,
        rep.trips,
        out.display()
    );
    if !rep.no_evidence.is_empty() {
        s.push_str(&format!(
            "{} trades have no market data kept around them\n",
            rep.no_evidence.len()
        ));
    }
    s.push_str(&format!(
        "see them with: tf research show {0}   or   tf serve --research {1} --token-file FILE\n",
        out.display(),
        out.parent()
            .map_or(".".to_owned(), |p| p.display().to_string())
    ));
    Ok(s)
}

fn show(args: &[String]) -> Result<String, String> {
    let [dir] = args else {
        return Err(USAGE.to_owned());
    };
    let r = Results::open(Path::new(dir)).map_err(|e| e.to_string())?;
    describe(&r).map_err(|e| e.to_string())
}

fn snapshots(args: &[String]) -> Result<String, String> {
    let f = Flags::parse(
        args,
        &[
            "--bars",
            "--symbology",
            "--from",
            "--to",
            "--out",
            "--assets",
            "--etf-list",
            "--window",
            "--min-days",
        ],
        &[],
    )?;
    let num = |k: &str, default: usize| -> Result<usize, String> {
        match f.get(k) {
            None => Ok(default),
            Some(s) => s
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or(format!("{k} {s}: not a number above zero")),
        }
    };
    let defaults = Params::default();
    let p = Params {
        window: num("--window", defaults.window)?,
        min_days: num("--min-days", defaults.min_days)?,
    };
    let (Some(from), Some(to)) = (date_arg(&f, "--from")?, date_arg(&f, "--to")?) else {
        return Err(format!("--from and --to are required\n{USAGE}"));
    };
    let bars_path = f.need("--bars")?;
    let sym_path = f.need("--symbology")?;
    let out = Path::new(f.need("--out")?);
    let bars = read_bars(&read(bars_path)?).map_err(|e| format!("{bars_path}: {e}"))?;
    let symbology = Symbology::parse(&read(sym_path)?).map_err(|e| format!("{sym_path}: {e}"))?;
    let assets = match f.get("--assets") {
        Some(a) => Some(parse_assets(&read(a)?).map_err(|e| format!("{a}: {e}"))?),
        None => None,
    };
    let etf = match f.get("--etf-list") {
        Some(e) => Some(read(e)?),
        None => None,
    };
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    // Each trading day from `from` to `to`, with what was known before it.
    let (a, b) = (parse_date(&from)?, parse_date(&to)?);
    let cal = Calendar::us_equities();
    let mut day = a;
    let mut made = Vec::new();
    while day <= b {
        if cal
            .is_trading_day(day)
            .map_err(|e| format!("{day}: {e:?}"))?
        {
            let date = day.to_string();
            let before = date_days(&date).ok_or("date")? - 1;
            let (mut rows, rep) = build(&bars, &symbology, before, p)
                .map_err(|e| format!("{date}: {e} (the bars start too late for this day)"))?;
            let mut columns: std::collections::BTreeSet<StaticFeature> =
                BAR_COLUMNS.into_iter().collect();
            if let Some(list) = &assets {
                merge_assets(&mut rows, list);
                columns.extend(ASSET_COLUMNS);
            }
            if let Some(text) = &etf {
                merge_etf_list(&mut rows, text).map_err(|m| format!("--etf-list: {m}"))?;
                columns.insert(StaticFeature::Etf);
            }
            let snap = Snapshot {
                as_of: rep.as_of.clone(),
                columns,
                rows,
            };
            let path = out.join(format!("{date}.snapshot"));
            fs::write(&path, snap.render()).map_err(|e| format!("{}: {e}", path.display()))?;
            made.push((date, rep.as_of, rep.symbols));
        }
        day = day.next();
    }
    let mut s = format!(
        "{} snapshots written to {}, each as of the last session before its day\n",
        made.len(),
        out.display()
    );
    if let (Some(first), Some(last)) = (made.first(), made.last()) {
        s.push_str(&format!(
            "{} (as of {}, {} symbols) to {} (as of {}, {} symbols)\n",
            first.0, first.1, first.2, last.0, last.1, last.2
        ));
    }
    if assets.is_none() {
        s.push_str("no asset list: tradable, shortable and easy_to_borrow are absent, so a universe using them will refuse\n");
    }
    if etf.is_none() {
        s.push_str("no etf list: etf is absent\n");
    }
    s.push_str("no minute history: the columns a strategy needs from it (previous high and low, ATR, volume baselines) are absent\n");
    Ok(s)
}

fn parse_date(s: &str) -> Result<Date, String> {
    let (y, m, d) = (
        s.get(..4).and_then(|x| x.parse::<i32>().ok()),
        s.get(5..7).and_then(|x| x.parse::<u8>().ok()),
        s.get(8..10).and_then(|x| x.parse::<u8>().ok()),
    );
    match (y, m, d) {
        (Some(y), Some(m), Some(d)) => Date::new(y, m, d).ok_or(format!("{s} is not a date")),
        _ => Err(format!("{s}: expected YYYY-MM-DD")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-research-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn what_is_asked_wrongly_is_refused_before_anything_is_read() {
        let e = |a: &[&str]| run(&args(a)).unwrap_err();
        assert!(e(&[]).starts_with("usage:") && e(&["wat"]).starts_with("usage:"));
        assert!(e(&["run"]).contains("--set is required"));
        assert!(e(&["run", "--wat", "1"]).contains("unexpected argument --wat"));
        assert!(e(&["run", "--set"]).contains("--set needs a value"));
        assert!(e(&["run", "--set", "a", "--set", "b"]).contains("--set is given twice"));
        let base = [
            "run",
            "--set",
            "s",
            "--store",
            "d",
            "--dataset",
            "D",
            "--schema",
            "S",
            "--snapshots",
            "p",
            "--out",
            "o",
        ];
        let with = |extra: &[&str]| {
            let mut v = base.to_vec();
            v.extend_from_slice(extra);
            run(&args(&v)).unwrap_err()
        };
        assert!(with(&["--from", "2026-1-1"]).contains("--from 2026-1-1: expected YYYY-MM-DD"));
        assert!(with(&["--to", "soon"]).contains("--to soon"));
        assert!(with(&["--id-space", "0"]).contains("--id-space 0: not a number above zero"));
        assert!(with(&["--id-space", "x"]).contains("--id-space x"));
        assert!(with(&["--latency-ms", "fast"]).contains("--latency-ms fast"));
        // A set that is not there is the next thing said.
        assert!(with(&[]).contains("s:"), "{}", with(&[]));
        assert!(run(&args(&["show"])).unwrap_err().starts_with("usage:"));
        assert!(
            run(&args(&["show", "a", "b"]))
                .unwrap_err()
                .starts_with("usage:")
        );
        assert!(run(&args(&["show", "/no/such/results"])).is_err());
        assert!(e(&["snapshots", "--bars", "b"]).contains("--from and --to are required"));
    }

    #[test]
    fn symbols_are_a_list_or_a_file_and_never_both_or_none() {
        let flags =
            |a: &[&str]| Flags::parse(&args(a), &["--symbols", "--symbols-file"], &[]).unwrap();
        assert_eq!(symbols_of(&flags(&[])).unwrap(), None);
        assert_eq!(
            symbols_of(&flags(&["--symbols", "AAPL, MSFT ,F"]))
                .unwrap()
                .unwrap(),
            ["AAPL", "MSFT", "F"]
        );
        assert!(
            symbols_of(&flags(&["--symbols", "AAPL,,F"]))
                .unwrap_err()
                .contains("empty")
        );
        assert!(symbols_of(&flags(&["--symbols", ""])).is_err());
        let d = scratch("symbols");
        let file = d.join("s.txt");
        fs::write(&file, "# the screen\nAAPL\n\nMSFT # big\n  F  \n").unwrap();
        let f = file.to_str().unwrap();
        assert_eq!(
            symbols_of(&flags(&["--symbols-file", f])).unwrap().unwrap(),
            ["AAPL", "MSFT", "F"]
        );
        fs::write(&file, "# nothing\n\n").unwrap();
        assert!(
            symbols_of(&flags(&["--symbols-file", f]))
                .unwrap_err()
                .contains("no symbols")
        );
        assert!(symbols_of(&flags(&["--symbols-file", "/no/such/file"])).is_err());
        assert!(
            symbols_of(&flags(&["--symbols", "A", "--symbols-file", f]))
                .unwrap_err()
                .contains("not both")
        );
    }

    #[test]
    fn a_snapshot_is_made_for_each_trading_day_as_of_the_last_session_before_it() {
        let d = scratch("snapshots");
        let p = |n: &str| d.join(n).to_string_lossy().into_owned();
        let mut bars =
            String::from("ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume\n");
        for day in ["2026-04-29", "2026-04-30", "2026-05-01"] {
            let ts = date_days(day).unwrap() as u64 * 86_400_000_000_000;
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
        let out = p("snaps");
        let common = [
            "snapshots",
            "--bars",
            &p("bars.csv"),
            "--symbology",
            &p("sym.json"),
            "--min-days",
            "1",
            "--window",
            "3",
            "--out",
            &out,
        ];
        let mut a = common.to_vec();
        a.extend_from_slice(&["--from", "2026-04-30", "--to", "2026-05-05"]);
        let text = run(&args(&a)).unwrap();
        // Thursday to Tuesday: four trading days, the weekend left out.
        let mut files: Vec<String> = fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            [
                "2026-04-30.snapshot",
                "2026-05-01.snapshot",
                "2026-05-04.snapshot",
                "2026-05-05.snapshot"
            ]
        );
        assert!(text.starts_with("4 snapshots written"), "{text}");
        let as_of = |date: &str| {
            let t = fs::read_to_string(format!("{out}/{date}.snapshot")).unwrap();
            t.lines().next().unwrap().to_owned()
        };
        assert_eq!(as_of("2026-04-30"), "# as_of 2026-04-29");
        assert_eq!(as_of("2026-05-01"), "# as_of 2026-04-30");
        assert_eq!(
            as_of("2026-05-04"),
            "# as_of 2026-05-01",
            "the weekend has no session"
        );
        assert_eq!(
            as_of("2026-05-05"),
            "# as_of 2026-05-01",
            "no bar on the 4th in this file"
        );
        assert!(
            text.contains("no asset list")
                && text.contains("no etf list")
                && text.contains("no minute history"),
            "{text}"
        );
        // The snapshot is one a universe can read, with the day's own bars not in it.
        let snap = tf_universe::Snapshot::parse(
            &fs::read_to_string(format!("{out}/2026-04-30.snapshot")).unwrap(),
        )
        .unwrap();
        assert_eq!(snap.rows.len(), 1);
        // The first day has no bars before it: said, naming the day.
        let mut b = common.to_vec();
        b.extend_from_slice(&["--from", "2026-04-29", "--to", "2026-04-29"]);
        let e = run(&args(&b)).unwrap_err();
        assert!(
            e.contains("2026-04-29") && e.contains("bars start too late"),
            "{e}"
        );
        // A bad range or a date that is not one is refused.
        let mut c = common.to_vec();
        c.extend_from_slice(&["--from", "2026-02-30", "--to", "2026-05-05"]);
        assert!(
            run(&args(&c))
                .unwrap_err()
                .contains("--from 2026-02-30: expected YYYY-MM-DD")
        );
        let mut z = common.to_vec();
        z.extend_from_slice(&[
            "--from",
            "2026-04-30",
            "--to",
            "2026-05-05",
            "--window",
            "0",
        ]);
        assert!(run(&args(&z)).is_err());
    }

    #[test]
    fn flags_are_values_or_switches_and_a_latency_is_in_milliseconds() {
        let a = args(&["--out", "x", "--evidence", "--from", "2026-05-01"]);
        let f = Flags::parse(&a, &["--out", "--from"], &["--evidence"]).unwrap();
        assert!(f.on("--evidence") && !f.on("--other"));
        assert_eq!(
            (f.get("--out"), f.get("--from"), f.get("--to")),
            (Some("x"), Some("2026-05-01"), None)
        );
        let none = Flags::parse(&args(&["--out", "x"]), &["--out"], &["--evidence"]).unwrap();
        assert!(!none.on("--evidence"));
        assert_eq!(latency_ns("50").unwrap(), 50_000_000);
        assert_eq!(latency_ns("0").unwrap(), 0);
        assert!(latency_ns("-1").is_err() && latency_ns("1.5").is_err() && latency_ns("").is_err());
        assert!(
            latency_ns("18446744073709551615").is_err(),
            "it would overflow"
        );
    }

    #[test]
    fn windows_and_minimums_of_zero_are_refused_in_words() {
        let d = scratch("zero");
        let p = |n: &str| d.join(n).to_string_lossy().into_owned();
        fs::write(
            p("bars.csv"),
            "ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume\n",
        )
        .unwrap();
        fs::write(p("sym.json"), r#"{"result":{}}"#).unwrap();
        for flag in ["--window", "--min-days"] {
            let e = run(&args(&[
                "snapshots",
                "--bars",
                &p("bars.csv"),
                "--symbology",
                &p("sym.json"),
                "--from",
                "2026-05-01",
                "--to",
                "2026-05-05",
                "--out",
                &p("o"),
                flag,
                "0",
            ]))
            .unwrap_err();
            assert!(
                e.contains(&format!("{flag} 0: not a number above zero")),
                "{e}"
            );
        }
    }
}
