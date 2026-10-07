//! `tf history`: the research history store (E19-S07).

use std::path::Path;

use tf_history::{Store, describe, index, verify};

const USAGE: &str = "usage:
    tf history index DIR --dataset NAME --schema NAME [--symbols LIST]
    tf history verify DIR
    tf history show DIR";

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
        Some("show") => Ok(describe(&Store::read(dir).map_err(|e| e.to_string())?)),
        _ => Err(USAGE.to_owned()),
    }
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
        assert!(run(&a(&["verify", "/nonexistent-tf-history"])).is_err());
        assert!(
            run(&a(&["frobnicate", "/tmp"]))
                .unwrap_err()
                .contains("usage")
        );
    }
}
