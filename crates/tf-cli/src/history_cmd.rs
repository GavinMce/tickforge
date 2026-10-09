//! `tf history`: the research history store (E19-S07).

use std::path::Path;

use tf_history::{Store, describe, index, verify};

const USAGE: &str = "usage:
    tf history index DIR --dataset NAME --schema NAME [--symbols LIST]
    tf history names DIR --dataset NAME --schema NAME --date DATE --symbology FILE
    tf history verify DIR
    tf history show DIR
    tf history screen DIR [--dataset NAME] [--from DATE] [--to DATE] [--quotes SCHEMA] [--min-price P] [--max-price P]
                          [--min-dollars N] [--min-bars N] [--max-spread-bp BP]";

pub(crate) fn history(args: &[String]) -> Result<(), String> {
    print!("{}", run(args)?);
    Ok(())
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    let dir = args.get(1).ok_or_else(|| USAGE.to_owned())?;
    let dir = Path::new(dir);
    match args.first().map(String::as_str) {
        Some("index") => {
            let (mut dataset, mut schema, mut symbols) = (None, None, "ALL_SYMBOLS".to_owned());
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                let mut v = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
                match a.as_str() {
                    "--dataset" => dataset = Some(v("--dataset")?),
                    "--schema" => schema = Some(v("--schema")?),
                    "--symbols" => symbols = v("--symbols")?,
                    o => return Err(format!("unknown argument {o}\n{USAGE}")),
                }
            }
            let (Some(dataset), Some(schema)) = (dataset, schema) else {
                return Err(USAGE.to_owned());
            };
            let (store, rep) =
                index(dir, &dataset, &schema, &symbols).map_err(|e| e.to_string())?;
            let mut s = format!(
                "indexed {} days of {dataset} {schema}: {} records, {} MB; manifest {}\n",
                rep.days,
                rep.records,
                rep.bytes / 1_000_000,
                dir.join(tf_history::MANIFEST).display()
            );
            if !rep.empty.is_empty() {
                s.push_str(&format!(
                    "no records on {} days (a holiday the pull asked for, or a failed pull): {}\n",
                    rep.empty.len(),
                    rep.empty.join(" ")
                ));
            }
            let (_, unknown) = store.cost();
            if unknown > 0 {
                s.push_str(&format!(
                    "{unknown} days have no cost recorded (no <date>.cost beside the file)\n"
                ));
            }
            Ok(s)
        }
        Some("names") => names(dir, &args[2..]),
        Some("verify") => {
            let r = verify(dir).map_err(|e| e.to_string())?;
            if r.is_clean() {
                Ok(format!(
                    "{} days, {} MB: every file is what the manifest says\n",
                    r.days,
                    r.bytes / 1_000_000
                ))
            } else {
                Err(format!(
                    "{} problems in {} days:\n{}",
                    r.problems.len(),
                    r.days,
                    r.problems.join("\n")
                ))
            }
        }
        Some("screen") => screen(dir, &args[2..]),
        Some("show") => Ok(describe(&Store::read(dir).map_err(|e| e.to_string())?)),
        _ => Err(USAGE.to_owned()),
    }
}

/// The names of a day's instruments, from the vendor's symbology, kept beside the day's file (`<date>.names`). A pull of every
/// symbol has no mappings in its metadata, so without these a replay of the day knows ids and no symbols.
fn names(dir: &Path, args: &[String]) -> Result<String, String> {
    let (mut dataset, mut schema, mut date, mut sym) = (None, None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut v = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
        match a.as_str() {
            "--dataset" => dataset = Some(v("--dataset")?),
            "--schema" => schema = Some(v("--schema")?),
            "--date" => date = Some(v("--date")?),
            "--symbology" => sym = Some(v("--symbology")?),
            o => return Err(format!("unknown argument {o}\n{USAGE}")),
        }
    }
    let (Some(dataset), Some(schema), Some(date), Some(sym)) = (dataset, schema, date, sym) else {
        return Err(USAGE.to_owned());
    };
    let day = tf_reference::date_days(&date).ok_or(format!("--date: `{date}` is not a date"))?;
    let text = std::fs::read_to_string(&sym).map_err(|e| format!("{sym}: {e}"))?;
    let symbology = tf_reference::Symbology::parse(&text).map_err(|e| format!("{sym}: {e}"))?;
    let names: Vec<(u32, String)> = symbology
        .names_on(day)
        .into_iter()
        .map(|(id, s)| (id, s.to_owned()))
        .collect();
    if names.is_empty() {
        return Err(format!("{sym} names no instrument on {date}"));
    }
    let file = dir
        .join(&dataset)
        .join(&schema)
        .join(format!("{date}.dbn.zst"));
    let path = tf_capture::names_path(&file);
    let part = path.with_extension("names.part");
    std::fs::write(&part, tf_capture::render_names(&names))
        .map_err(|e| format!("{}: {e}", part.display()))?;
    std::fs::rename(&part, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "{} names for {date}: {}\n",
        names.len(),
        path.display()
    ))
}

/// A decimal number of dollars (`12.5`) as raw price units.
fn dollars(text: &str, what: &str) -> Result<i64, String> {
    tf_core::Px::parse(text)
        .map(tf_core::Px::raw)
        .ok_or(format!("{what}: `{text}` is not a number of dollars"))
}

/// The bar-level screen (E19-S13): the symbols of a store's one-minute bars (and quotes) that meet the limits.
fn screen(dir: &Path, args: &[String]) -> Result<String, String> {
    use tf_history::screen::{Limits, bars, select, spreads};
    let (mut dataset, mut from, mut to, mut quotes) = ("XNAS.BASIC".to_owned(), None, None, None);
    let mut limits = Limits::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut v = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
        match a.as_str() {
            "--dataset" => dataset = v("--dataset")?,
            "--from" => from = Some(v("--from")?),
            "--to" => to = Some(v("--to")?),
            "--quotes" => quotes = Some(v("--quotes")?),
            "--min-price" => limits.min_price = Some(dollars(&v(a)?, a)?),
            "--max-price" => limits.max_price = Some(dollars(&v(a)?, a)?),
            "--min-dollars" => limits.min_dollars = Some(dollars(&v(a)?, a)?.max(0) as u128),
            "--min-bars" => {
                limits.min_bars = Some(v(a)?.parse().map_err(|_| format!("{a}: not a count"))?);
            }
            // Basis points, to hundredths: raw price units of 1e-9 over 1e7.
            "--max-spread-bp" => {
                limits.max_spread_bp_x100 = Some((dollars(&v(a)?, a)?.max(0) / 10_000_000) as u64);
            }
            o => return Err(format!("unknown argument {o}\n{USAGE}")),
        }
    }
    if limits.max_spread_bp_x100.is_some() && quotes.is_none() {
        return Err(
            "--max-spread-bp needs --quotes SCHEMA (a stored quote schema, e.g. tcbbo)".to_owned(),
        );
    }
    let (from, to) = (from.as_deref(), to.as_deref());
    let b =
        bars(&tf_history::files(dir, &dataset, "ohlcv-1m", from, to).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let q = match &quotes {
        Some(schema) => {
            spreads(&tf_history::files(dir, &dataset, schema, from, to).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        }
        None => Vec::new(),
    };
    let chosen = select(&b, &q, &limits);
    let mut s = format!(
        "{:<8} {:>8} {:>12} {:>16} {:>10}  \n",
        "symbol", "bars", "last close", "dollars", "spread bp"
    );
    for r in &b {
        let spread = q
            .iter()
            .find(|x| x.symbol == r.symbol)
            .and_then(tf_history::screen::SpreadRow::mean_bp_x100)
            .map_or("-".to_owned(), |m| format!("{}.{:02}", m / 100, m % 100));
        s.push_str(&format!(
            "{:<8} {:>8} {:>12} {:>16} {:>10}  {}\n",
            r.symbol,
            r.bars,
            tf_core::Px::from_raw(r.last_close).to_decimal(),
            r.dollars / 1_000_000_000,
            spread,
            if chosen.contains(&r.symbol) { "*" } else { "" }
        ));
    }
    s.push_str(&format!(
        "selected {} of {}: {}\n",
        chosen.len(),
        b.len(),
        chosen.join(",")
    ));
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commands_need_their_arguments_and_a_store_that_exists() {
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(run(&a(&[])).is_err());
        assert!(run(&a(&["verify"])).is_err());
        assert!(
            run(&a(&["index", "/nonexistent-tf-history"]))
                .unwrap_err()
                .contains("usage")
        );
        assert!(
            run(&a(&[
                "index",
                "/nonexistent-tf-history",
                "--dataset",
                "X",
                "--schema",
                "trades",
                "--bogus"
            ]))
            .is_err()
        );
        assert!(run(&a(&["show", "/nonexistent-tf-history"])).is_err());
        assert!(run(&a(&["screen", "/nonexistent-tf-history"])).is_err());
        assert!(
            run(&a(&["screen", "/tmp", "--max-spread-bp", "5"]))
                .unwrap_err()
                .contains("--quotes")
        );
        assert!(run(&a(&["screen", "/tmp", "--min-price", "lots"])).is_err());
        assert!(run(&a(&["screen", "/tmp", "--bogus"])).is_err());
        assert!(run(&a(&["verify", "/nonexistent-tf-history"])).is_err());
        assert!(
            run(&a(&["frobnicate", "/tmp"]))
                .unwrap_err()
                .contains("usage")
        );
    }

    #[test]
    fn names_for_a_day_come_from_the_symbology_and_sit_beside_the_days_file() {
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let dir = std::env::temp_dir().join(format!("tf-history-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("X").join("tbbo")).unwrap();
        let sym = dir.join("sym.json");
        std::fs::write(
            &sym,
            r#"{"result":{"AAPL":[{"d0":"2026-10-08","d1":"2026-10-09","s":"38"}],
                "OLD":[{"d0":"2026-10-01","d1":"2026-10-08","s":"39"}]}}"#,
        )
        .unwrap();
        let d = dir.to_str().unwrap();
        let s = sym.to_str().unwrap();
        let ok = run(&a(&[
            "names",
            d,
            "--dataset",
            "X",
            "--schema",
            "tbbo",
            "--date",
            "2026-10-08",
            "--symbology",
            s,
        ]))
        .unwrap();
        assert!(ok.starts_with("1 names for 2026-10-08"), "{ok}");
        let text = std::fs::read_to_string(dir.join("X/tbbo/2026-10-08.names")).unwrap();
        assert_eq!(
            tf_capture::parse_names(&text).unwrap(),
            [(38, "AAPL".to_owned())]
        );
        assert!(!dir.join("X/tbbo/2026-10-08.names.part").exists());
        for bad in [
            vec!["names", d],
            vec![
                "names",
                d,
                "--dataset",
                "X",
                "--schema",
                "tbbo",
                "--date",
                "soon",
                "--symbology",
                s,
            ],
            vec![
                "names",
                d,
                "--dataset",
                "X",
                "--schema",
                "tbbo",
                "--date",
                "2026-11-01",
                "--symbology",
                s,
            ],
            vec![
                "names",
                d,
                "--dataset",
                "X",
                "--schema",
                "tbbo",
                "--date",
                "2026-10-08",
                "--symbology",
                "/nonexistent.json",
            ],
            vec![
                "names",
                d,
                "--dataset",
                "X",
                "--schema",
                "tbbo",
                "--date",
                "2026-10-08",
                "--symbology",
                s,
                "--bogus",
            ],
        ] {
            assert!(run(&a(&bad)).is_err(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
