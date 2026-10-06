//! `tf runs`: list the runs kept by `tf backtest --store`, filter and sort them, say whether the
//! current code still reproduces each, and open or compare runs by row number.
//!
//! Read-only over the store. A file that does not parse is counted and skipped, never fatal,
//! so one damaged run does not hide the rest.

use std::path::Path;

use tf_manifest::RunResult;

use super::explore::{Refusal, open_runs, replay};

/// One run as a table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub key: String,
    pub kind: String,
    pub strategy: String,
    pub seed: u64,
    pub secs: String,
    pub mix: String,
    /// `built-in`, the first eight digits of a custom rule set's fingerprint, or `-`.
    pub rules: String,
    pub git: String,
    pub trades: Option<i64>,
    /// Net P&L in 1e-9 dollars.
    pub net: Option<i64>,
}

pub(crate) fn row_of(r: &RunResult) -> Row {
    let m = r.manifest();
    let c = m.config();
    let get = |k: &str| c.get(k).map(String::as_str);
    let strategy = get("strategy").unwrap_or("-").to_owned();
    let mix = match (get("healthy"), get("dangerous"), get("quiet")) {
        (Some(h), Some(d), Some(q)) => format!("{h}h/{d}d/{q}q"),
        _ => "-".to_owned(),
    };
    let rules = match (get("rules"), strategy.as_str()) {
        (Some(id), _) => id.chars().take(8).collect(),
        (None, "momentum") => "built-in".to_owned(),
        _ => "-".to_owned(),
    };
    Row {
        key: r.key().hex(),
        kind: m.kind().to_owned(),
        strategy,
        seed: m.seed(),
        secs: get("secs").unwrap_or("-").to_owned(),
        mix,
        rules,
        git: m.git_sha().chars().take(7).collect(),
        trades: r.metric("trades"),
        net: r.metric("pnl_net"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sort {
    Hash,
    Net,
    Trades,
    Seed,
}

#[derive(Debug, Default)]
pub(crate) struct Query {
    pub kind: Option<String>,
    pub strategy: Option<String>,
    pub seed: Option<u64>,
    pub rules: Option<String>,
    pub git: Option<String>,
    pub sort: Option<Sort>,
    pub desc: bool,
}

/// The rows that pass the filters, in the requested order. Ties (and rows with no value for
/// the sort key, which come first) fall back to the hash, so the numbering is stable.
pub(crate) fn select(mut rows: Vec<Row>, q: &Query) -> Vec<Row> {
    rows.retain(|r| {
        q.kind.as_ref().is_none_or(|k| &r.kind == k)
            && q.strategy.as_ref().is_none_or(|s| &r.strategy == s)
            && q.seed.is_none_or(|s| r.seed == s)
            && q.rules
                .as_ref()
                .is_none_or(|p| r.rules.starts_with(p.as_str()))
            && q.git.as_ref().is_none_or(|p| r.git.starts_with(p.as_str()))
    });
    rows.sort_by(|a, b| {
        let ord = match q.sort.unwrap_or(Sort::Hash) {
            Sort::Hash => a.key.cmp(&b.key),
            Sort::Net => a.net.cmp(&b.net),
            Sort::Trades => a.trades.cmp(&b.trades),
            Sort::Seed => a.seed.cmp(&b.seed),
        };
        let ord = ord.then_with(|| a.key.cmp(&b.key));
        if q.desc { ord.reverse() } else { ord }
    });
    rows
}

/// Dollars from 1e-9 dollars, to the cent.
pub(crate) fn money(raw: i64) -> String {
    let cents = (i128::from(raw).abs() + 5_000_000) / 10_000_000;
    format!(
        "{}${}.{:02}",
        if raw < 0 && cents > 0 { "-" } else { "" },
        cents / 100,
        cents % 100
    )
}

/// The runs in `dir` and how many files were skipped because they did not parse.
pub(crate) fn scan(dir: &Path) -> Result<(Vec<RunResult>, usize), String> {
    tf_manifest::DirStore::new(dir)
        .list()
        .map_err(|e| e.to_string())
}

/// Whether the current code still gives the stored result.
pub(crate) fn status(r: &RunResult, rules: &[String]) -> &'static str {
    match replay(r, rules) {
        Ok(_) => "ok",
        Err(Refusal::Drift(_)) => "drifted",
        Err(Refusal::NeedsRules(_)) => "needs --rules",
        Err(Refusal::Rebuild(_)) => "cannot rebuild",
        Err(Refusal::Unsupported(_)) => "n/a",
    }
}

pub(crate) fn table(rows: &[Row], now: Option<&[&str]>) -> String {
    let mut out = format!(
        "{:>3}  {:<12}  {:<8}  {:<9}  {:>6}  {:>5}  {:<9}  {:<8}  {:<7}  {:>6}  {:>11}",
        "#", "hash", "kind", "strategy", "seed", "secs", "mix", "rules", "git", "trades", "net"
    );
    if now.is_some() {
        out.push_str("  now");
    }
    out.push('\n');
    for (i, r) in rows.iter().enumerate() {
        out.push_str(&format!(
            "{:>3}  {:<12}  {:<8}  {:<9}  {:>6}  {:>5}  {:<9}  {:<8}  {:<7}  {:>6}  {:>11}",
            i + 1,
            &r.key[..12],
            r.kind,
            r.strategy,
            r.seed,
            r.secs,
            r.mix,
            r.rules,
            r.git,
            r.trades.map_or("-".to_owned(), |t| t.to_string()),
            r.net.map_or("-".to_owned(), money),
        ));
        if let Some(now) = now {
            out.push_str(&format!("  {}", now[i]));
        }
        out.push('\n');
    }
    out
}

/// The row numbers (1-based, as printed) as indices, one or two of them.
pub(crate) fn pick(rows: &[Row], numbers: &[usize]) -> Result<Vec<String>, String> {
    if numbers.is_empty() || numbers.len() > 2 {
        return Err("--open takes one row number to open, or two to compare".to_owned());
    }
    numbers
        .iter()
        .map(|&n| {
            if n == 0 || n > rows.len() {
                Err(format!(
                    "row {n} does not exist (the list has {} row{})",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                ))
            } else {
                Ok(rows[n - 1].key.clone())
            }
        })
        .collect()
}

/// The table for `rows`, with the check column when asked for.
pub(crate) fn listing(
    rows: &[Row],
    by_key: &std::collections::BTreeMap<String, &RunResult>,
    rules: &[String],
    check: bool,
) -> String {
    let now: Option<Vec<&str>> =
        check.then(|| rows.iter().map(|r| status(by_key[&r.key], rules)).collect());
    table(rows, now.as_deref())
}

/// Runs to open, the rule files that may be needed for them, and where to write the page.
type OpenRequest = (Vec<RunResult>, Vec<String>, String);

pub(crate) fn runs(args: &[String]) -> Result<(), String> {
    let (text, open) = report(args)?;
    print!("{text}");
    if let Some((chosen, rules, out)) = open {
        open_runs(&chosen, &rules, &out)?;
    }
    Ok(())
}

/// What `tf runs` prints, and the runs to open afterwards if `--open` was given.
pub(crate) fn report(args: &[String]) -> Result<(String, Option<OpenRequest>), String> {
    let (mut store, mut rules, mut out) = (None, Vec::new(), None);
    let (mut q, mut check, mut open) = (Query::default(), false, Vec::new());
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--store" => store = Some(val("--store")?),
            "--rules" => rules.push(val("--rules")?),
            "--out" => out = Some(val("--out")?),
            "--kind" => q.kind = Some(val("--kind")?),
            "--strategy" => q.strategy = Some(val("--strategy")?),
            "--seed" => {
                q.seed = Some(val("--seed")?.parse().map_err(|e| format!("--seed: {e}"))?);
            }
            "--rules-id" => q.rules = Some(val("--rules-id")?),
            "--git" => q.git = Some(val("--git")?),
            "--sort" => {
                q.sort = Some(match val("--sort")?.as_str() {
                    "hash" => Sort::Hash,
                    "net" => Sort::Net,
                    "trades" => Sort::Trades,
                    "seed" => Sort::Seed,
                    other => return Err(format!("--sort {other}: use hash, net, trades or seed")),
                });
            }
            "--desc" => q.desc = true,
            "--check" => check = true,
            "--open" => {
                while let Some(n) = it.next_if(|n| n.parse::<usize>().is_ok()) {
                    open.push(n.parse::<usize>().expect("checked"));
                }
                if open.is_empty() {
                    return Err("--open needs a row number".to_owned());
                }
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let store =
        store.ok_or("tf runs needs --store DIR (where `tf backtest --store` kept the runs)")?;
    rules.extend(crate::rules_cmd::store_rules(Path::new(&store)));
    let (found, skipped) = scan(Path::new(&store))?;
    let by_key: std::collections::BTreeMap<String, &RunResult> =
        found.iter().map(|r| (r.key().hex(), r)).collect();
    let rows = select(found.iter().map(row_of).collect(), &q);
    let mut text = String::new();
    if rows.is_empty() {
        text.push_str(&format!(
            "no runs{} in {store}\n",
            if found.is_empty() { "" } else { " match" }
        ));
    } else {
        text.push_str(&listing(&rows, &by_key, &rules, check));
    }
    if skipped > 0 {
        text.push_str(&format!("skipped  {skipped} file(s) that did not parse\n"));
    }
    let open = if open.is_empty() {
        None
    } else {
        let keys = pick(&rows, &open)?;
        let chosen: Vec<RunResult> = keys.iter().map(|k| by_key[k].clone()).collect();
        Some((
            chosen,
            rules,
            out.unwrap_or_else(|| "explorer.html".to_owned()),
        ))
    };
    Ok((text, open))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explore::tests::stored;
    use tf_manifest::{DataRange, DirStore, Manifest};

    const FLAGS: [&str; 10] = [
        "--healthy",
        "2",
        "--dangerous",
        "2",
        "--quiet",
        "1",
        "--secs",
        "400",
        "--lead",
        "60",
    ];

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tf-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn with_flags(extra: &[&str]) -> RunResult {
        let mut f = FLAGS.to_vec();
        f.extend_from_slice(extra);
        stored(&f)
    }

    fn row(key: &str, seed: u64, net: Option<i64>, trades: Option<i64>, strategy: &str) -> Row {
        Row {
            key: format!("{key}{}", "0".repeat(64 - key.len())),
            kind: "backtest".into(),
            strategy: strategy.into(),
            seed,
            secs: "400".into(),
            mix: "1h/1d/1q".into(),
            rules: "built-in".into(),
            git: "abc1234".into(),
            trades,
            net,
        }
    }

    #[test]
    fn a_row_says_what_the_run_was() {
        let r = with_flags(&["--seed", "7"]);
        let row = row_of(&r);
        assert_eq!(row.key, r.key().hex());
        assert_eq!(
            (row.kind.as_str(), row.strategy.as_str(), row.seed),
            ("backtest", "momentum", 7)
        );
        assert_eq!(
            (row.secs.as_str(), row.mix.as_str(), row.rules.as_str()),
            ("400", "2h/2d/1q", "built-in")
        );
        assert_eq!(
            row.git,
            r.manifest().git_sha().chars().take(7).collect::<String>()
        );
        assert_eq!(row.trades, r.metric("trades"));
        assert_eq!(row.net, r.metric("pnl_net"));
        assert!(row.trades.is_some() && row.net.is_some());
        // Custom rules show their fingerprint; the trend strategy has none.
        let dir = scratch("rules");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("r.rules");
        let text = tf_strategy::rules::MOMENTUM_RULES
            .replace("higher_lows >= @min_higher_lows", "higher_lows >= 99");
        std::fs::write(&f, &text).unwrap();
        let custom = row_of(&with_flags(&["--rules", f.to_str().unwrap()]));
        let id = format!(
            "{:016x}",
            tf_strategy::RuleSet::parse(&text).unwrap().fingerprint()
        );
        assert_eq!(custom.rules, id[..8]);
        let trend = row_of(&stored(&["--strategy", "trend", "--secs", "1800"]));
        assert_eq!(
            (trend.strategy.as_str(), trend.rules.as_str()),
            ("trend", "-")
        );
        // A run of another kind has no scenario mix.
        let synth = Manifest::new(
            "abc",
            "synth",
            3,
            DataRange {
                source: "synth:x".into(),
                from: 0,
                to: 1,
            },
        )
        .unwrap();
        let s = row_of(&RunResult::new(synth, 10, 1));
        assert_eq!(
            (
                s.strategy.as_str(),
                s.mix.as_str(),
                s.rules.as_str(),
                s.trades
            ),
            ("-", "-", "-", None)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filters_keep_only_matching_rows_and_each_one_matters() {
        let mut other_rules = row("bb", 2, Some(5), Some(1), "momentum");
        other_rules.rules = "a1b2c3d4".into();
        let mut other_git = row("cc", 3, Some(6), Some(1), "momentum");
        other_git.git = "fff9999".into();
        let mut other_kind = row("dd", 4, Some(7), Some(1), "momentum");
        other_kind.kind = "synth".into();
        let rows = vec![
            row("aa", 1, Some(1), Some(1), "momentum"),
            other_rules,
            other_git,
            other_kind,
            row("ee", 5, Some(9), Some(1), "trend"),
        ];
        let keys = |q: Query| {
            select(rows.clone(), &q)
                .iter()
                .map(|r| r.key[..2].to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(Query::default()), ["aa", "bb", "cc", "dd", "ee"]);
        assert_eq!(
            keys(Query {
                strategy: Some("trend".into()),
                ..Query::default()
            }),
            ["ee"]
        );
        assert_eq!(
            keys(Query {
                kind: Some("synth".into()),
                ..Query::default()
            }),
            ["dd"]
        );
        assert_eq!(
            keys(Query {
                seed: Some(3),
                ..Query::default()
            }),
            ["cc"]
        );
        assert_eq!(
            keys(Query {
                rules: Some("a1b".into()),
                ..Query::default()
            }),
            ["bb"]
        );
        assert_eq!(
            keys(Query {
                rules: Some("built".into()),
                ..Query::default()
            }),
            ["aa", "cc", "dd", "ee"]
        );
        assert_eq!(
            keys(Query {
                git: Some("fff".into()),
                ..Query::default()
            }),
            ["cc"]
        );
        assert_eq!(
            keys(Query {
                strategy: Some("momentum".into()),
                git: Some("abc".into()),
                ..Query::default()
            }),
            ["aa", "bb", "dd"],
            "filters combine"
        );
        assert!(
            keys(Query {
                seed: Some(99),
                ..Query::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn sorting_is_stable_with_the_hash_as_tiebreak_and_desc_reverses_all_of_it() {
        let rows = vec![
            row("aa", 5, Some(300), Some(2), "momentum"),
            row("bb", 5, Some(-50), Some(7), "momentum"),
            row("cc", 2, Some(300), None, "momentum"),
            row("dd", 9, None, Some(2), "momentum"),
        ];
        let order = |sort, desc| {
            select(
                rows.clone(),
                &Query {
                    sort: Some(sort),
                    desc,
                    ..Query::default()
                },
            )
            .iter()
            .map(|r| r.key[..2].to_owned())
            .collect::<Vec<_>>()
        };
        assert_eq!(order(Sort::Hash, false), ["aa", "bb", "cc", "dd"]);
        assert_eq!(order(Sort::Hash, true), ["dd", "cc", "bb", "aa"]);
        assert_eq!(
            order(Sort::Net, false),
            ["dd", "bb", "aa", "cc"],
            "no value first, ties by hash"
        );
        assert_eq!(order(Sort::Net, true), ["cc", "aa", "bb", "dd"]);
        assert_eq!(order(Sort::Trades, false), ["cc", "aa", "dd", "bb"]);
        assert_eq!(order(Sort::Seed, false), ["cc", "aa", "bb", "dd"]);
    }

    #[test]
    fn money_rounds_to_the_cent_and_keeps_the_sign_of_what_it_shows() {
        assert_eq!(money(374_400_000_000), "$374.40");
        assert_eq!(money(-29_670_000_000), "-$29.67");
        assert_eq!(money(0), "$0.00");
        assert_eq!(money(4_999_999), "$0.00", "under half a cent");
        assert_eq!(money(5_000_000), "$0.01", "half a cent rounds up");
        assert_eq!(money(-4_000_000), "$0.00", "no minus on zero");
        assert_eq!(money(1_234_560_000_000), "$1234.56");
        assert_eq!(money(i64::MIN).chars().next(), Some('-'));
    }

    #[test]
    fn the_table_numbers_rows_from_one_and_the_pick_follows_the_numbers() {
        let rows = vec![
            row("aa", 1, Some(374_400_000_000), Some(1), "momentum"),
            row("bb", 2, None, None, "momentum"),
        ];
        let t = table(&rows, None);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("hash") && lines[0].contains("net") && !lines[0].contains("now"));
        assert!(
            lines[1].trim_start().starts_with("1  aa0000000000"),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains("$374.40"));
        assert!(lines[2].trim_start().starts_with("2  bb0000000000"));
        assert!(
            lines[2].trim_end().ends_with('-'),
            "no net shows a dash: {}",
            lines[2]
        );
        let t = table(&rows, Some(&["ok", "drifted"]));
        assert!(t.lines().next().unwrap().ends_with("now"));
        assert!(t.lines().nth(1).unwrap().ends_with("  ok"));
        assert!(t.lines().nth(2).unwrap().ends_with("  drifted"));
        assert_eq!(pick(&rows, &[2]).unwrap(), [rows[1].key.clone()]);
        assert_eq!(
            pick(&rows, &[2, 1]).unwrap(),
            [rows[1].key.clone(), rows[0].key.clone()]
        );
        for bad in [&[0usize][..], &[3], &[1, 2, 1], &[]] {
            assert!(pick(&rows, bad).is_err(), "{bad:?}");
        }
        assert!(
            pick(&rows, &[3])
                .unwrap_err()
                .contains("the list has 2 rows")
        );
        assert!(pick(&rows[..1], &[2]).unwrap_err().contains("1 row)"));
    }

    #[test]
    fn scanning_finds_every_run_and_counts_what_it_could_not_read() {
        let dir = scratch("scan");
        let store = DirStore::new(&dir);
        let (a, b) = (with_flags(&["--seed", "1"]), with_flags(&["--seed", "2"]));
        store.put(&a).unwrap();
        store.put(&b).unwrap();
        let p = store.path_for(a.manifest());
        std::fs::write(p.with_file_name("garbage.tfrs"), "not a result").unwrap();
        std::fs::write(p.with_file_name("notes.txt"), "ignored").unwrap();
        std::fs::write(dir.join("stray.tfrs"), "top level files are not runs").unwrap();
        let (mut runs, skipped) = scan(&dir).unwrap();
        assert_eq!(skipped, 1);
        runs.sort_by_key(|r| r.manifest().seed());
        assert_eq!(runs, [a, b]);
        assert!(scan(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_check_column_tells_ok_from_drifted_from_needs_rules_from_not_applicable() {
        let ok = with_flags(&[]);
        assert_eq!(status(&ok, &[]), "ok");
        let tampered = {
            let mut t = RunResult::new(ok.manifest().clone(), ok.events, ok.event_hash);
            for (k, v) in ok.metrics() {
                t = t
                    .with_metric(k, if k == "pnl_net" { v + 1 } else { *v })
                    .unwrap();
            }
            t
        };
        assert_eq!(status(&tampered, &[]), "drifted");
        assert_eq!(
            status(
                &RunResult::new(ok.manifest().clone(), ok.events, ok.event_hash ^ 1),
                &[]
            ),
            "drifted"
        );
        let dir = scratch("check");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("r.rules");
        std::fs::write(
            &f,
            tf_strategy::rules::MOMENTUM_RULES
                .replace("higher_lows >= @min_higher_lows", "higher_lows >= 99"),
        )
        .unwrap();
        let custom = with_flags(&["--rules", f.to_str().unwrap()]);
        assert_eq!(status(&custom, &[]), "needs --rules");
        assert_eq!(status(&custom, &[f.to_str().unwrap().to_owned()]), "ok");
        assert_eq!(
            status(&stored(&["--strategy", "trend", "--secs", "1800"]), &[]),
            "n/a"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_that_cannot_be_rebuilt_exactly_says_so_in_the_check_column() {
        let r = with_flags(&[]);
        let odd = RunResult::new(
            r.manifest().clone().with_config("mystery", "1").unwrap(),
            r.events,
            r.event_hash,
        );
        assert_eq!(status(&odd, &[]), "cannot rebuild");
    }

    #[test]
    fn the_command_lists_checks_and_opens_by_row_and_refuses_bad_use() {
        let dir = scratch("cmd");
        let store = DirStore::new(&dir);
        let (a, b) = (with_flags(&["--seed", "1"]), with_flags(&["--seed", "2"]));
        store.put(&a).unwrap();
        store.put(&b).unwrap();
        let d = dir.to_str().unwrap().to_owned();
        let args = |x: &[&str]| -> Vec<String> {
            let mut v = vec!["--store".to_owned(), d.clone()];
            v.extend(x.iter().map(|s| (*s).to_owned()));
            v
        };
        runs(&args(&[])).unwrap();
        runs(&args(&[
            "--check",
            "--sort",
            "net",
            "--desc",
            "--strategy",
            "momentum",
        ]))
        .unwrap();
        runs(&args(&["--seed", "99"])).unwrap(); // nothing matches: not an error
        // Open row 1 of the default order.
        let first = {
            let mut keys = [a.key().hex(), b.key().hex()];
            keys.sort();
            keys[0].clone()
        };
        let out = dir.join("one.html");
        runs(&args(&["--open", "1", "--out", out.to_str().unwrap()])).unwrap();
        let page = std::fs::read_to_string(&out).unwrap();
        assert!(page.starts_with("<!doctype html>"));
        let seed_of_first = if first == a.key().hex() { 1 } else { 2 };
        assert!(
            page.contains(&format!("\"seed\":{seed_of_first}")),
            "row 1 is the first hash"
        );
        assert!(!page.contains(&format!("\"seed\":{}", 3 - seed_of_first)));
        // Two rows: a comparison, but these are two sessions (different seeds), so it is refused.
        let e = runs(&args(&["--open", "1", "2", "--out", out.to_str().unwrap()])).unwrap_err();
        assert!(e.contains("different sessions"), "{e}");
        for (bad, why) in [
            (vec!["--bogus"], "unknown flag"),
            (vec!["--sort", "luck"], "use hash, net, trades or seed"),
            (vec!["--seed", "x"], "--seed"),
            (vec!["--open"], "needs a row number"),
            (vec!["--open", "3"], "row 3 does not exist"),
            (
                vec!["--open", "1", "2", "1"],
                "one row number to open, or two",
            ),
            (vec!["--kind"], "--kind needs a value"),
        ] {
            let e = runs(&args(&bad)).unwrap_err();
            assert!(e.contains(why), "{bad:?}: {e}");
        }
        assert!(runs(&[]).unwrap_err().contains("needs --store"));
        assert!(
            runs(&[
                "--store".to_owned(),
                dir.join("nope").to_str().unwrap().to_owned()
            ])
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_rows_open_in_the_order_given_so_a_is_the_first_one_named() {
        let dir = scratch("order");
        let rf = dir.join("r.rules");
        std::fs::create_dir_all(&dir).unwrap();
        let text = tf_strategy::rules::MOMENTUM_RULES
            .replace("higher_lows >= @min_higher_lows", "higher_lows >= 99");
        std::fs::write(&rf, &text).unwrap();
        let custom_id = format!(
            "{:016x}",
            tf_strategy::RuleSet::parse(&text).unwrap().fingerprint()
        );
        let builtin_id = format!("{:016x}", tf_strategy::RuleSet::momentum().fingerprint());
        let store = DirStore::new(&dir);
        let plain = with_flags(&[]);
        let custom = with_flags(&["--rules", rf.to_str().unwrap()]);
        store.put(&plain).unwrap();
        store.put(&custom).unwrap();
        let rows = select(vec![row_of(&plain), row_of(&custom)], &Query::default());
        let number = |r: &RunResult| rows.iter().position(|x| x.key == r.key().hex()).unwrap() + 1;
        let (np, nc) = (number(&plain).to_string(), number(&custom).to_string());
        let out = dir.join("cmp.html");
        let args = |a: &str, b: &str| -> Vec<String> {
            [
                "--store",
                dir.to_str().unwrap(),
                "--rules",
                rf.to_str().unwrap(),
                "--open",
                a,
                b,
                "--out",
                out.to_str().unwrap(),
            ]
            .map(str::to_owned)
            .to_vec()
        };
        let first_rules = |page: &str| {
            let at = page.find("\"runs\":[").unwrap();
            let k = page[at..].find("\"rules\":{\"id\":\"").unwrap() + at + 15;
            page[k..k + 16].to_owned()
        };
        runs(&args(&np, &nc)).unwrap();
        let page = std::fs::read_to_string(&out).unwrap();
        assert_eq!(first_rules(&page), builtin_id, "plain named first is run A");
        assert!(page.contains("\"compare\":"));
        runs(&args(&nc, &np)).unwrap();
        let page = std::fs::read_to_string(&out).unwrap();
        assert_eq!(first_rules(&page), custom_id, "custom named first is run A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_check_column_appears_only_when_asked_and_carries_each_runs_status() {
        let ok = with_flags(&[]);
        let tampered = {
            let mut t = RunResult::new(ok.manifest().clone(), ok.events, ok.event_hash ^ 1);
            for (k, v) in ok.metrics() {
                t = t.with_metric(k, *v).unwrap();
            }
            t
        };
        let tampered_seed = with_flags(&["--seed", "5"]);
        let tampered = RunResult::new(
            tampered_seed.manifest().clone(),
            tampered_seed.events,
            tampered.event_hash,
        );
        let by_key: std::collections::BTreeMap<String, &RunResult> =
            [(ok.key().hex(), &ok), (tampered.key().hex(), &tampered)]
                .into_iter()
                .collect();
        let rows = select(vec![row_of(&ok), row_of(&tampered)], &Query::default());
        let plain = listing(&rows, &by_key, &[], false);
        assert!(
            !plain.contains("now") && !plain.contains("drifted"),
            "{plain}"
        );
        let checked = listing(&rows, &by_key, &[], true);
        assert!(
            checked.lines().next().unwrap().ends_with("now"),
            "{checked}"
        );
        for (line, r) in checked.lines().skip(1).zip(&rows) {
            let want = if r.key == ok.key().hex() {
                "ok"
            } else {
                "drifted"
            };
            assert!(line.ends_with(&format!("  {want}")), "{line}");
        }
    }
}
