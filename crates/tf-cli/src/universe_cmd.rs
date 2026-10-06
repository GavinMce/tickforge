//! `tf universe`: read a universe spec, say who it admits from a reference snapshot, compare two.

use std::fmt::Write as _;
use std::fs;

use tf_universe::{Selection, Snapshot, Spec, diff, select};

const USAGE: &str = "usage:
    tf universe show SPEC [--snapshot FILE]
    tf universe members SPEC --snapshot FILE [--out FILE]
    tf universe diff OLD NEW [--snapshot FILE]";

pub(crate) fn universe(args: &[String]) -> Result<(), String> {
    print!("{}", run(args)?);
    Ok(())
}

fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

fn spec(path: &str) -> Result<Spec, String> {
    Spec::parse(&read(path)?).map_err(|e| format!("{path}: {e}"))
}

fn snapshot(path: &str) -> Result<Snapshot, String> {
    Snapshot::parse(&read(path)?).map_err(|e| format!("{path}: {e}"))
}

struct Flags {
    positional: Vec<String>,
    snapshot: Option<String>,
    out: Option<String>,
}

fn flags(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags {
        positional: Vec::new(),
        snapshot: None,
        out: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--snapshot" => f.snapshot = Some(it.next().cloned().ok_or("--snapshot needs a file")?),
            "--out" => f.out = Some(it.next().cloned().ok_or("--out needs a file")?),
            o if o.starts_with("--") => return Err(format!("unknown flag {o}\n{USAGE}")),
            _ => f.positional.push(a.clone()),
        }
    }
    Ok(f)
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    let (Some(cmd), rest) = (args.first(), args.get(1..).unwrap_or(&[])) else {
        return Err(USAGE.to_owned());
    };
    let f = flags(rest)?;
    let mut out = String::new();
    match (cmd.as_str(), f.positional.as_slice()) {
        ("show", [path]) => {
            let s = spec(path)?;
            out.push_str(&s.render());
            let _ = writeln!(out, "fingerprint {:016x}", s.fingerprint());
            if let Some(snap) = &f.snapshot {
                let snap = snapshot(snap)?;
                let sel = select(&s, &snap).map_err(|e| e.to_string())?;
                let _ = writeln!(
                    out,
                    "{} of {} symbols pass the static conditions (snapshot as of {})",
                    sel.symbols.len(),
                    snap.rows.len(),
                    snap.as_of
                );
            }
        }
        ("members", [path]) => {
            let (s, snap) = (spec(path)?, snapshot(f.snapshot.as_deref().ok_or(USAGE)?)?);
            let sel: Selection = select(&s, &snap).map_err(|e| e.to_string())?;
            match &f.out {
                Some(p) => {
                    fs::write(p, sel.render()).map_err(|e| format!("{p}: {e}"))?;
                    let _ = writeln!(
                        out,
                        "{} members written to {p} (fingerprint {:016x})",
                        sel.symbols.len(),
                        sel.fingerprint()
                    );
                }
                None => out.push_str(&sel.render()),
            }
        }
        ("diff", [old, new]) => {
            let snap = f.snapshot.as_deref().map(snapshot).transpose()?;
            let d = diff(&spec(old)?, &spec(new)?, snap.as_ref()).map_err(|e| e.to_string())?;
            if d.changes.is_empty() {
                out.push_str("no difference in the specs\n");
            }
            for c in &d.changes {
                let _ = writeln!(out, "{c}");
            }
            if snap.is_some() {
                let _ = writeln!(out, "admits {} more: {}", d.added.len(), d.added.join(" "));
                let _ = writeln!(
                    out,
                    "admits {} fewer: {}",
                    d.removed.len(),
                    d.removed.join(" ")
                );
                if d.widens {
                    out.push_str("WIDENS the universe: capacity and risk sized for the old one need another look\n");
                }
            }
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tf-universe-cli-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn show_members_and_diff() {
        let d = dir();
        let p = |n: &str| d.join(n).to_string_lossy().into_owned();
        fs::write(p("a.spec"), "universe v1\nstatic price >= 5; etf = no\n").unwrap();
        fs::write(p("b.spec"), "universe v1\nstatic price >= 5\n").unwrap();
        fs::write(
            p("snap.csv"),
            "# as_of 2026-10-02\nsymbol,price,etf\nAAA,10,no\nBBB,10,yes\nCCC,1,no\n",
        )
        .unwrap();
        let show = run(&args(&["show", &p("a.spec"), "--snapshot", &p("snap.csv")])).unwrap();
        assert!(show.contains("static price >= 5.00; etf = no\n"), "{show}");
        assert!(
            show.contains("fingerprint ") && show.contains("1 of 3 symbols pass"),
            "{show}"
        );
        let members = run(&args(&[
            "members",
            &p("a.spec"),
            "--snapshot",
            &p("snap.csv"),
        ]))
        .unwrap();
        assert!(
            members.starts_with("members v1\nas_of 2026-10-02\n")
                && members.ends_with("count 1\nAAA\n"),
            "{members}"
        );
        let w = run(&args(&[
            "members",
            &p("a.spec"),
            "--snapshot",
            &p("snap.csv"),
            "--out",
            &p("m.txt"),
        ]))
        .unwrap();
        assert!(w.starts_with("1 members written"), "{w}");
        assert_eq!(
            Selection::parse(&fs::read_to_string(p("m.txt")).unwrap())
                .unwrap()
                .symbols,
            ["AAA"]
        );
        let df = run(&args(&[
            "diff",
            &p("a.spec"),
            &p("b.spec"),
            "--snapshot",
            &p("snap.csv"),
        ]))
        .unwrap();
        assert!(
            df.contains("- static etf = no")
                && df.contains("admits 1 more: BBB")
                && df.contains("WIDENS"),
            "{df}"
        );
        assert!(
            run(&args(&["diff", &p("b.spec"), &p("a.spec")]))
                .unwrap()
                .contains("+ static etf = no")
        );
        assert!(
            run(&args(&["diff", &p("a.spec"), &p("a.spec")]))
                .unwrap()
                .contains("no difference")
        );
        // Errors name the file and the line.
        fs::write(p("bad.spec"), "universe v1\nstatic colour = red\n").unwrap();
        assert!(
            run(&args(&["show", &p("bad.spec")]))
                .unwrap_err()
                .contains("bad.spec: universe spec line 2")
        );
        assert!(run(&args(&["members", &p("a.spec")])).is_err());
        assert!(run(&args(&["show", &p("missing")])).is_err());
        assert!(run(&args(&[])).is_err());
        assert!(
            run(&args(&["show", "x", "--bogus"]))
                .unwrap_err()
                .contains("unknown flag")
        );
        let _ = fs::remove_dir_all(&d);
    }
}
