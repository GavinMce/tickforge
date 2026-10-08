//! `tf catalog`: every run of each strategy, newest first, with backtest, paper and live kept apart.
//!
//! Read-only. Backtests come from the run store; paper or live sessions come from an order ledger,
//! which does not say which of the two it is, so `--kind` does.

use std::fmt::Write as _;
use std::path::Path;

use tf_catalog::{Catalog, Kind, Run, Source, when};
use tf_ledger::FileStore;

use super::runs::{money, scan};

const USAGE: &str =
    "usage: tf catalog [--store DIR] [--ledger DIR --kind paper|live] [--strategy NAME] [--latest]";

pub(crate) fn catalog(args: &[String]) -> Result<(), String> {
    print!("{}", report(args)?);
    Ok(())
}

fn line(r: &Run) -> String {
    let net = r.net_pnl.map_or("-".to_owned(), |n| {
        money(i64::try_from(n).unwrap_or(i64::MAX))
    });
    let trades = r.trades.map_or("-".to_owned(), |t| t.to_string());
    // `built-in` or the first eight digits of a rule set's fingerprint.
    let rules = r
        .rules
        .as_ref()
        .map_or("-".to_owned(), |x| x.chars().take(8).collect());
    let budget = r.budget.map_or("-".to_owned(), |b| {
        money(i64::try_from(b).unwrap_or(i64::MAX))
    });
    let source = match &r.source {
        Source::Stored { hash } => format!(
            "{} (tf explore {})",
            &hash[..hash.len().min(8)],
            &hash[..hash.len().min(8)]
        ),
        Source::Ledger { ledger, session } => format!("{ledger} day {session}"),
        Source::Research { scenario, .. } => format!("scenario {scenario}"),
    };
    format!(
        "  {}  {:<8} {:>12} {:>6} trades  rules {:<8}  budget {:>12}  {}\n",
        when(r.started),
        r.kind.name(),
        net,
        trades,
        rules,
        budget,
        source
    )
}

/// What `tf catalog` prints.
pub(crate) fn report(args: &[String]) -> Result<String, String> {
    let (mut store, mut ledger, mut kind, mut only, mut latest) = (None, None, None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
        match a.as_str() {
            "--store" => store = Some(value("--store")?),
            "--ledger" => ledger = Some(value("--ledger")?),
            "--kind" => {
                let k = value("--kind")?;
                kind = Some(
                    Kind::parse_session(&k).ok_or(format!("--kind is paper or live, not {k}"))?,
                );
            }
            "--strategy" => only = Some(value("--strategy")?),
            "--latest" => latest = true,
            other => return Err(format!("unexpected argument {other}\n{USAGE}")),
        }
    }
    if store.is_none() && ledger.is_none() {
        return Err(USAGE.to_owned());
    }
    if ledger.is_some() && kind.is_none() {
        return Err(
            "--ledger needs --kind paper|live: the ledger does not say which it is".to_owned(),
        );
    }
    if ledger.is_none() && kind.is_some() {
        return Err("--kind only applies to --ledger".to_owned());
    }
    let mut runs = Vec::new();
    let mut notes = String::new();
    if let Some(dir) = &store {
        let (found, skipped) = scan(Path::new(dir))?;
        runs.extend(tf_catalog::from_results(&found).runs().iter().cloned());
        if skipped > 0 {
            let _ = writeln!(
                notes,
                "skipped  {skipped} file(s) in the store that did not parse"
            );
        }
    }
    if let (Some(dir), Some(kind)) = (&ledger, kind) {
        if !Path::new(dir).join("ledger.log").exists() {
            return Err(format!("no ledger in {dir} (no ledger.log)"));
        }
        let fs = FileStore::open(dir).map_err(|e| e.to_string())?;
        runs.extend(tf_catalog::sessions(fs, dir, kind).map_err(|e| e.to_string())?);
    }
    let cat = Catalog::new(runs);
    let mut out = String::new();
    let mut shown = 0;
    for s in cat.strategies() {
        if only.as_deref().is_some_and(|o| o != s) {
            continue;
        }
        let rows: Vec<&Run> = if latest {
            cat.latest(s).into_iter().collect()
        } else {
            cat.of(s).collect()
        };
        let _ = writeln!(out, "{s}  ({} run(s))", cat.of(s).count());
        for r in rows {
            out.push_str(&line(r));
        }
        shown += 1;
    }
    if shown == 0 {
        out.push_str(&match &only {
            Some(o) => format!("no runs of {o}\n"),
            None => "no runs\n".to_owned(),
        });
    }
    out.push_str(&notes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_core::Px;
    use tf_ledger::Journal;
    use tf_manifest::{DataRange, DirStore, Manifest, RunResult};
    use tf_risk::Limits;
    use tf_strategy::intent::{
        Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
    };
    use tf_strategy::lifecycle::Decision;

    const SEC: u64 = 1_000_000_000;
    const P: i64 = 1_000_000_000;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tf-catalogcmd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn dates_are_calendar_dates_in_utc() {
        assert_eq!(when(0), "1970-01-01 00:00");
        assert_eq!(when(1_791_158_400 * SEC), "2026-10-05 00:00");
        assert_eq!(
            when((1_791_158_400 + 14 * 3_600 + 30 * 60 + 59) * SEC),
            "2026-10-05 14:30"
        );
        // Leap days and the ends of years.
        assert_eq!(when(951_782_400 * SEC), "2000-02-29 00:00");
        assert_eq!(when(951_868_800 * SEC), "2000-03-01 00:00");
        assert_eq!(when(1_709_164_800 * SEC), "2024-02-29 00:00");
        assert_eq!(when(1_704_067_199 * SEC), "2023-12-31 23:59");
        assert_eq!(when(1_704_067_200 * SEC), "2024-01-01 00:00");
    }

    fn backtest(strategy: &str, from: u64, net: i64) -> RunResult {
        let m = Manifest::new(
            "abc1234",
            "backtest",
            from,
            DataRange {
                source: "synth:universe".into(),
                from,
                to: from + 10,
            },
        )
        .unwrap()
        .with_config("strategy", strategy)
        .unwrap();
        RunResult::new(m, 1, 2)
            .with_metric("trades", 3)
            .unwrap()
            .with_metric("pnl_net", net)
            .unwrap()
    }

    fn buy(seq: u64, side: Side, purpose: Purpose, px: i64) -> Intent {
        Intent {
            id: IntentId {
                strategy: StrategyId(1),
                seq,
            },
            instrument: 0,
            side,
            qty: 100,
            purpose,
            pricing: Pricing::Limit(Px::from_raw(px)),
            protect: (purpose == Purpose::Open).then(|| Protective {
                stop_trigger: Px::from_raw(px * 9 / 10),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            ts: seq * SEC,
            reason: 1,
        }
    }

    fn ledger(dir: &Path) {
        let limits = Limits::new(
            5_000 * 1_000_000_000,
            1_000,
            20_000 * 1_000_000_000,
            400 * 1_000_000_000,
            6,
            10 * SEC,
        )
        .unwrap();
        let (mut j, _) = Journal::open(FileStore::open(dir).unwrap(), limits, 1).unwrap();
        let base = 1_791_158_400;
        for (i, (seq, side, purpose, px)) in [
            (base + 10, Side::Buy, Purpose::Open, 5 * P),
            (base + 20, Side::Sell, Purpose::Close, 7 * P),
        ]
        .into_iter()
        .enumerate()
        {
            let now = seq * SEC;
            let Decision::Accepted(o) = j.decide(&buy(seq, side, purpose, px), now).unwrap() else {
                panic!("{i}")
            };
            j.ack(o, now).unwrap();
            j.fill(o, 100, Px::from_raw(px), now).unwrap();
        }
    }

    #[test]
    fn stored_backtests_and_ledger_sessions_are_listed_apart_newest_first() {
        let (store, led) = (scratch("store"), scratch("ledger"));
        let s = DirStore::new(&store);
        s.put(&backtest("momentum", 1_000 * SEC, 5 * P)).unwrap();
        s.put(&backtest("momentum", 2_000 * SEC, -P)).unwrap();
        ledger(&led);
        let (sd, ld) = (store.to_str().unwrap(), led.to_str().unwrap());

        let text = report(&args(&["--store", sd, "--ledger", ld, "--kind", "paper"])).unwrap();
        assert!(text.contains("momentum  (2 run(s))"), "{text}");
        assert!(text.contains("s1  (1 run(s))"), "{text}");
        let new = text.find("1970-01-01 00:33").unwrap();
        let old = text.find("1970-01-01 00:16").unwrap();
        assert!(new < old, "newest first: {text}");
        assert!(text.contains("backtest"), "{text}");
        assert!(text.contains("2026-10-05 00:00  paper"), "{text}");
        assert!(text.contains("$200.00      1 trades"), "{text}");
        assert!(text.contains(&format!("{ld} day 1")), "{text}");
        assert!(text.contains("(tf explore "), "{text}");

        let latest = report(&args(&["--store", sd, "--latest"])).unwrap();
        assert!(latest.contains("momentum  (2 run(s))"), "{latest}");
        assert_eq!(latest.matches("backtest").count(), 1, "{latest}");
        assert!(latest.contains("1970-01-01 00:33"), "{latest}");

        let one = report(&args(&[
            "--store",
            sd,
            "--ledger",
            ld,
            "--kind",
            "live",
            "--strategy",
            "s1",
        ]))
        .unwrap();
        assert!(!one.contains("momentum"), "{one}");
        assert!(one.contains("live"), "{one}");
        assert!(
            report(&args(&["--store", sd, "--strategy", "zzz"]))
                .unwrap()
                .contains("no runs of zzz")
        );
        let _ = std::fs::remove_dir_all(&store);
        let _ = std::fs::remove_dir_all(&led);
    }

    #[test]
    fn a_store_with_a_damaged_file_still_lists_the_rest_and_says_so() {
        let dir = scratch("damaged");
        let s = DirStore::new(&dir);
        let r = backtest("momentum", 1_000 * SEC, 0);
        s.put(&r).unwrap();
        let p = s.path_for(r.manifest());
        std::fs::write(p.with_file_name("junk.tfrs"), "junk").unwrap();
        let text = report(&args(&["--store", dir.to_str().unwrap()])).unwrap();
        assert!(text.contains("momentum"), "{text}");
        assert!(text.contains("skipped  1 file(s)"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_invocations_are_refused_with_the_reason() {
        let err = |a: &[&str]| report(&args(a)).unwrap_err();
        assert!(err(&[]).starts_with("usage:"));
        assert!(err(&["--store"]).contains("needs a value"));
        assert!(err(&["--store", "x", "--kind", "backtest"]).contains("paper or live"));
        assert!(err(&["--store", "x", "--kind", "paper"]).contains("only applies to --ledger"));
        assert!(err(&["--ledger", "x"]).contains("needs --kind"));
        assert!(
            err(&["--ledger", "/nonexistent-ledger-dir", "--kind", "paper"])
                .contains("no ledger in")
        );
        assert!(err(&["--bogus"]).contains("unexpected argument"));
        assert!(err(&["--store", "/nonexistent-store-dir"]).contains("nonexistent-store-dir"));
    }
}
