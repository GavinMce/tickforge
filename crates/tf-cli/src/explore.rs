//! `tf explore`: open stored backtest runs in the trade explorer.
//!
//! A stored result holds a manifest and metrics, not the tape or the trades. The explorer
//! rebuilds each run from its manifest, replays it (a run is deterministic) and checks the
//! replay against what was stored before showing anything: the event stream's hash and every
//! metric. A run that no longer reproduces (the code changed behaviour since it was stored)
//! is refused rather than shown as if it were the stored one.

use std::collections::BTreeMap;
use std::path::Path;

use tf_backtest::compare::{compare, to_json};
use tf_backtest::export::{ExportMeta, bundle, export_json, page, round_trips};
use tf_manifest::{Manifest, RunResult};
use tf_replay::{EventSink, HashSink};

use super::{
    BacktestArgs, backtest_config, backtest_manifest, git_sha, momentum_metrics, parse_backtest,
};

pub(crate) const DOLLAR: u64 = 1_000_000_000;

/// Why a stored run was not opened. The kind says what a person can do about it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// This kind of run cannot be opened at all.
    Unsupported(String),
    /// It used entry rules whose file was not given.
    NeedsRules(String),
    /// The replay does not match the stored result: the code's behaviour has changed.
    Drift(String),
    /// The run cannot be rebuilt exactly from its manifest.
    Rebuild(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Refusal::Unsupported(m)
        | Refusal::NeedsRules(m)
        | Refusal::Drift(m)
        | Refusal::Rebuild(m)) = self;
        f.write_str(m)
    }
}

impl From<String> for Refusal {
    fn from(m: String) -> Refusal {
        Refusal::Rebuild(m)
    }
}

impl From<&str> for Refusal {
    fn from(m: &str) -> Refusal {
        Refusal::Rebuild(m.to_owned())
    }
}

pub(crate) struct Replay {
    json: String,
    trades: Vec<tf_backtest::export::RoundTrip>,
    declines: Vec<tf_strategy::Decline>,
    rules_id: String,
    event_hash: u64,
    t0: u64,
    what: String,
}

/// The result stored under a hash or a unique prefix of one (at least 8 hex digits).
pub(crate) fn load_run(store: &Path, prefix: &str) -> Result<RunResult, String> {
    if prefix.len() < 8 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{prefix:?} is not a manifest hash (8 or more hex digits)"
        ));
    }
    let prefix = prefix.to_ascii_lowercase();
    let dir = store.join(&prefix[..2]);
    let mut found = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with(".tfrs") {
                found.push(e.path());
            }
        }
    }
    match found.as_slice() {
        [] => Err(format!(
            "no stored run starts with {prefix} in {}",
            store.display()
        )),
        [one] => {
            let text =
                std::fs::read_to_string(one).map_err(|e| format!("{}: {e}", one.display()))?;
            RunResult::parse(&text).map_err(|e| format!("{}: {e}", one.display()))
        }
        many => Err(format!(
            "{prefix} matches {} stored runs; give more digits",
            many.len()
        )),
    }
}

fn num(m: &Manifest, key: &str) -> Result<u64, String> {
    let v = m
        .config()
        .get(key)
        .ok_or_else(|| format!("the manifest has no `{key}`"))?;
    v.parse::<u64>()
        .map_err(|e| format!("`{key}` = {v:?}: {e}"))
}

fn whole(raw: u64, unit: u64, what: &str) -> Result<u64, String> {
    if raw % unit != 0 {
        return Err(format!(
            "{what} is not a whole number in the stored manifest"
        ));
    }
    Ok(raw / unit)
}

/// The command line that made a manifest's run, rebuilt from the manifest. Only single
/// momentum runs can be opened.
pub(crate) fn args_from_manifest(
    m: &Manifest,
    rules_files: &[String],
) -> Result<BacktestArgs, Refusal> {
    if m.kind() != "backtest" {
        return Err(Refusal::Unsupported(format!(
            "this is a `{}` run, not a backtest",
            m.kind()
        )));
    }
    let strategy = m.config().get("strategy").map_or("", String::as_str);
    if strategy != "momentum" {
        return Err(Refusal::Unsupported(format!(
            "only momentum runs can be opened (this one is `{strategy}`)"
        )));
    }
    if m.config().get("ab").map(String::as_str) != Some("0") {
        return Err(Refusal::Unsupported(
            "A/B runs (with --propose) cannot be opened in the explorer".to_owned(),
        ));
    }
    let mut a = parse_backtest(&[])?;
    a.seed = m.seed();
    a.secs = num(m, "secs")?;
    a.lead = Some(num(m, "lead_secs")?);
    a.healthy = num(m, "healthy")? as u32;
    a.dangerous = num(m, "dangerous")? as u32;
    a.quiet = num(m, "quiet")? as u32;
    a.latency_ms = whole(num(m, "latency_ns")?, 1_000_000, "the latency")?;
    a.borrow_bps = num(m, "borrow_bps")? as u32;
    a.order_notional = whole(
        num(m, "limit_max_order_notional_raw")?,
        DOLLAR,
        "the order cap",
    )?;
    a.daily_loss = whole(
        num(m, "limit_max_daily_loss_raw")?,
        DOLLAR,
        "the daily loss limit",
    )?;
    a.max_orders = num(m, "limit_max_orders_per_window")? as u32;
    a.higher_lows = m
        .params()
        .get("min_higher_lows")
        .ok_or("the manifest has no `min_higher_lows`")?
        .parse::<u32>()
        .map_err(|e| format!("min_higher_lows: {e}"))?;
    if let Some(id) = m.config().get("rules") {
        let mut hit = None;
        for f in rules_files {
            let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
            let r = tf_strategy::RuleSet::parse(&text).map_err(|e| format!("{f}: {e}"))?;
            if format!("{:016x}", r.fingerprint()) == *id {
                hit = Some(f.clone());
            }
        }
        a.rules = Some(hit.ok_or_else(|| {
            Refusal::NeedsRules(format!(
                "this run used the entry rules {id}; give the file with --rules"
            ))
        })?);
    }
    Ok(a)
}

fn diff_maps(
    what: &str,
    stored: &BTreeMap<String, String>,
    now: &BTreeMap<String, String>,
) -> Option<String> {
    for (k, v) in stored {
        if now.get(k) != Some(v) {
            return Some(format!(
                "{what} `{k}`: stored {v:?}, rebuilt {:?}",
                now.get(k)
            ));
        }
    }
    now.keys()
        .find(|k| !stored.contains_key(*k))
        .map(|k| format!("{what} `{k}` is in the rebuilt run but not the stored one"))
}

pub(crate) fn replay(stored: &RunResult, rules_files: &[String]) -> Result<Replay, Refusal> {
    let m = stored.manifest();
    let a = args_from_manifest(m, rules_files)?;
    let cfg = backtest_config(&a)?;
    let (events, labels) = tf_backtest::demo_session_with_lead(
        a.seed,
        a.secs,
        a.healthy,
        a.dangerous,
        a.quiet,
        a.lead.unwrap_or(tf_backtest::DEMO_LEAD_SECS),
    );
    let t0 = events.first().map_or(0, |e| e.ts_recv());
    // The rebuilt setup must be the stored one, key by key, or the replay is of something else.
    let rebuilt = backtest_manifest(&a, &cfg, t0)?;
    if rebuilt.data() != m.data() {
        return Err("the rebuilt session covers a different data range than the stored one".into());
    }
    if let Some(d) = diff_maps("config", m.config(), rebuilt.config())
        .or_else(|| diff_maps("parameter", m.params(), rebuilt.params()))
    {
        return Err(Refusal::Rebuild(format!(
            "cannot rebuild this run exactly: {d}"
        )));
    }
    let mut hash = HashSink::new();
    for ev in &events {
        hash.on_event(ev);
    }
    let traced =
        tf_backtest::momentum_backtest_traced(events.iter().copied(), labels.clone(), &cfg)?;
    let event_hash = hash.finish();
    if stored.events != events.len() as u64 || stored.event_hash != event_hash {
        return Err(Refusal::Drift(format!(
            "the replayed tape differs from the stored run ({} events, hash {:016x}; stored {} events, hash {:016x})",
            events.len(),
            event_hash,
            stored.events,
            stored.event_hash
        )));
    }
    let metrics: BTreeMap<String, i64> = momentum_metrics(&traced.result).into_iter().collect();
    for (k, v) in stored.metrics() {
        if metrics.get(k) != Some(v) {
            return Err(Refusal::Drift(format!(
                "the replay does not reproduce the stored result: `{k}` was {v}, now {:?} (the code has changed behaviour since this run was stored at {})",
                metrics.get(k),
                m.git_sha()
            )));
        }
    }
    if let Some(k) = metrics.keys().find(|k| !stored.metrics().contains_key(*k)) {
        return Err(Refusal::Drift(format!(
            "the replay has a metric `{k}` the stored run does not"
        )));
    }
    let rules = cfg
        .rules
        .clone()
        .unwrap_or_else(tf_strategy::RuleSet::momentum);
    let json = export_json(
        &events,
        &labels,
        &cfg,
        &traced.result,
        &traced.entries,
        &traced.declines,
        &ExportMeta {
            strategy: &a.strategy,
            seed: a.seed,
            secs: a.secs,
        },
    );
    Ok(Replay {
        json,
        trades: round_trips(&traced.result),
        declines: traced.declines,
        rules_id: format!("{:016x}", rules.fingerprint()),
        event_hash,
        t0,
        what: format!(
            "{} events, tape hash {event_hash:016x}, {} metrics identical; stored at {}, replayed at {}",
            events.len(),
            stored.metrics().len(),
            &m.git_sha()[..m.git_sha().len().min(12)],
            git_sha().chars().take(12).collect::<String>()
        ),
    })
}

pub(crate) fn explore(args: &[String]) -> Result<(), String> {
    let (mut hashes, mut store, mut rules, mut out) = (Vec::new(), None, Vec::new(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => store = Some(it.next().cloned().ok_or("--store needs a directory")?),
            "--rules" => rules.push(it.next().cloned().ok_or("--rules needs a file")?),
            "--out" => out = Some(it.next().cloned().ok_or("--out needs a file")?),
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            hash => hashes.push(hash.to_owned()),
        }
    }
    let store =
        store.ok_or("tf explore needs --store DIR (where `tf backtest --store` kept the runs)")?;
    if hashes.is_empty() || hashes.len() > 2 {
        return Err(
            "give one run to open, or two to compare (by manifest hash or a prefix of 8+ digits)"
                .to_owned(),
        );
    }
    let out = out.unwrap_or_else(|| "explorer.html".to_owned());
    let mut stored = Vec::new();
    for h in &hashes {
        stored.push(load_run(Path::new(&store), h)?);
    }
    open_runs(&stored, &rules, &out)
}

/// Replay one stored run, or two of one session, and write the explorer page for them.
pub(crate) fn open_runs(stored: &[RunResult], rules: &[String], out: &str) -> Result<(), String> {
    let mut runs = Vec::new();
    for (k, s) in stored.iter().enumerate() {
        let r = replay(s, rules).map_err(|e| format!("run {}: {e}", &s.key().hex()[..12]))?;
        println!(
            "run {}  {}  rules {}  reproduced: {}",
            if k == 0 { "A" } else { "B" },
            &s.key().hex()[..12],
            &r.rules_id[..8],
            r.what
        );
        runs.push(r);
    }
    let cmp = if let [a, b] = runs.as_slice() {
        if a.event_hash != b.event_hash || a.t0 != b.t0 {
            return Err(
                "the two runs are on different sessions; only runs of one session can be compared"
                    .to_owned(),
            );
        }
        let pairs = compare(&a.trades, &a.declines, &b.trades, &b.declines, a.t0);
        for kind in ["same", "changed", "only_a", "only_b"] {
            let n = pairs.iter().filter(|p| p.kind.name() == kind).count();
            println!("compare  {kind:<8} {n}");
        }
        Some(to_json(&pairs))
    } else {
        None
    };
    let jsons: Vec<String> = runs.into_iter().map(|r| r.json).collect();
    let html = page(&bundle(&jsons, cmp.as_deref()));
    std::fs::write(out, &html).map_err(|e| format!("{out}: {e}"))?;
    println!("wrote    {out} ({} bytes)", html.len());
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn strs(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    /// What `tf backtest --store` would keep for these flags.
    pub(crate) fn stored(flags: &[&str]) -> RunResult {
        let a = parse_backtest(&strs(flags)).unwrap();
        let cfg = backtest_config(&a).unwrap();
        let (events, labels) = tf_backtest::demo_session_with_lead(
            a.seed,
            a.secs,
            a.healthy,
            a.dangerous,
            a.quiet,
            a.lead.unwrap_or(tf_backtest::DEMO_LEAD_SECS),
        );
        let start = events.first().map_or(0, |e| e.ts_recv());
        let m = backtest_manifest(&a, &cfg, start).unwrap();
        let mut hash = HashSink::new();
        for ev in &events {
            hash.on_event(ev);
        }
        let r = tf_backtest::momentum_backtest(events.iter().copied(), labels, &cfg).unwrap();
        let mut out = RunResult::new(m, events.len() as u64, hash.finish());
        for (k, v) in momentum_metrics(&r) {
            out = out.with_metric(&k, v).unwrap();
        }
        out
    }

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
        let d = std::env::temp_dir().join(format!("tf-explore-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_manifest_gives_back_the_command_line_that_made_it() {
        let sets: [&[&str]; 4] = [
            &FLAGS,
            &[
                "--seed",
                "7",
                "--latency-ms",
                "120",
                "--borrow-bps",
                "300",
                "--secs",
                "300",
            ],
            &[
                "--order-notional",
                "3000",
                "--daily-loss",
                "500",
                "--max-orders",
                "9",
                "--higher-lows",
                "2",
            ],
            &[
                "--healthy",
                "5",
                "--dangerous",
                "0",
                "--quiet",
                "0",
                "--lead",
                "45",
                "--seed",
                "99",
            ],
        ];
        for flags in sets {
            let r = stored(flags);
            let a = args_from_manifest(r.manifest(), &[]).unwrap();
            let cfg = backtest_config(&a).unwrap();
            let again = backtest_manifest(&a, &cfg, r.manifest().data().from).unwrap();
            assert_eq!(again.config(), r.manifest().config(), "{flags:?}");
            assert_eq!(again.params(), r.manifest().params(), "{flags:?}");
            assert_eq!(again.seed(), r.manifest().seed());
        }
    }

    #[test]
    fn only_single_momentum_runs_open_and_the_reason_is_given() {
        let trend = stored(&["--strategy", "trend", "--secs", "1800"]);
        let e = args_from_manifest(trend.manifest(), &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("only momentum runs"), "{e}");
        // A/B runs record `ab 1`.
        let ab = {
            let r = stored(&FLAGS);
            Manifest::new(
                r.manifest().git_sha(),
                "backtest",
                1,
                r.manifest().data().clone(),
            )
            .unwrap()
            .with_config("strategy", "momentum")
            .unwrap()
            .with_config("ab", "1")
            .unwrap()
        };
        assert!(
            args_from_manifest(&ab, &[])
                .unwrap_err()
                .to_string()
                .contains("A/B")
        );
        let synth =
            Manifest::new("x", "synth", 1, stored(&FLAGS).manifest().data().clone()).unwrap();
        assert!(
            args_from_manifest(&synth, &[])
                .unwrap_err()
                .to_string()
                .contains("not a backtest")
        );
    }

    #[test]
    fn a_run_with_custom_rules_needs_that_file_and_the_right_one() {
        let dir = scratch("rules");
        std::fs::create_dir_all(&dir).unwrap();
        let strict = tf_strategy::rules::MOMENTUM_RULES
            .replace("higher_lows >= @min_higher_lows", "higher_lows >= 99");
        let other = tf_strategy::rules::MOMENTUM_RULES
            .replace("higher_lows >= @min_higher_lows", "higher_lows >= 98");
        let (fa, fb) = (dir.join("a.rules"), dir.join("b.rules"));
        std::fs::write(&fa, &strict).unwrap();
        std::fs::write(&fb, &other).unwrap();
        let (pa, pb) = (fa.to_str().unwrap(), fb.to_str().unwrap());
        let mut flags = FLAGS.to_vec();
        flags.extend(["--rules", pa]);
        let r = stored(&flags);
        let none = args_from_manifest(r.manifest(), &[])
            .unwrap_err()
            .to_string();
        assert!(none.contains("give the file with --rules"), "{none}");
        let wrong = args_from_manifest(r.manifest(), &[pb.to_owned()])
            .unwrap_err()
            .to_string();
        assert!(
            wrong.contains("give the file with --rules"),
            "a different file does not satisfy it: {wrong}"
        );
        let right = args_from_manifest(r.manifest(), &[pb.to_owned(), pa.to_owned()]).unwrap();
        assert_eq!(
            right.rules.as_deref(),
            Some(pa),
            "the file is found by its fingerprint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stored_value_that_is_not_whole_in_its_unit_is_refused_not_rounded() {
        let r = stored(&FLAGS);
        for (key, why) in [
            ("latency_ns", "the latency"),
            ("limit_max_order_notional_raw", "the order cap"),
        ] {
            let v: u64 = r.manifest().config()[key].parse().unwrap();
            let old = r.manifest();
            let mut m =
                Manifest::new(old.git_sha(), old.kind(), old.seed(), old.data().clone()).unwrap();
            for (k, val) in old.config() {
                let val = if k == key {
                    (v + 1).to_string()
                } else {
                    val.clone()
                };
                m = m.with_config(k, &val).unwrap();
            }
            for (k, val) in old.params() {
                m = m.with_param(k, val).unwrap();
            }
            let e = args_from_manifest(&m, &[]).unwrap_err().to_string();
            assert!(e.contains(why) && e.contains("not a whole number"), "{e}");
        }
    }

    #[test]
    fn a_stored_run_is_replayed_and_checked_before_it_is_shown() {
        let r = stored(&FLAGS);
        let ok = replay(&r, &[]).unwrap();
        assert!(ok.what.contains("metrics identical"), "{}", ok.what);
        assert!(
            ok.json.contains("\"trades\":[{"),
            "the replay produced its trades"
        );
        assert!(!ok.trades.is_empty());
        // A stored metric that the replay does not give: refused, naming it.
        let tampered = {
            let mut t = RunResult::new(r.manifest().clone(), r.events, r.event_hash);
            for (k, v) in r.metrics() {
                let v = if k == "pnl_net" { v + 1 } else { *v };
                t = t.with_metric(k, v).unwrap();
            }
            t
        };
        let e = replay(&tampered, &[])
            .err()
            .expect("must refuse")
            .to_string();
        assert!(
            e.contains("`pnl_net`") && e.contains("does not reproduce"),
            "{e}"
        );
        // A different tape hash.
        let wrong_tape = RunResult::new(r.manifest().clone(), r.events, r.event_hash ^ 1);
        assert!(
            replay(&wrong_tape, &[])
                .err()
                .unwrap()
                .to_string()
                .contains("replayed tape differs")
        );
        // A metric the stored run never had.
        let mut fewer = RunResult::new(r.manifest().clone(), r.events, r.event_hash);
        for (k, v) in r.metrics().iter().filter(|(k, _)| k.as_str() != "shares") {
            fewer = fewer.with_metric(k, *v).unwrap();
        }
        assert!(
            replay(&fewer, &[])
                .err()
                .unwrap()
                .to_string()
                .contains("`shares`")
        );
        // A manifest whose setup the rebuild cannot match (an extra, unknown config key).
        let extra = RunResult::new(
            r.manifest().clone().with_config("mystery", "1").unwrap(),
            r.events,
            r.event_hash,
        );
        assert!(
            replay(&extra, &[])
                .err()
                .unwrap()
                .to_string()
                .contains("cannot rebuild this run exactly")
        );
    }

    #[test]
    fn runs_are_found_by_a_unique_prefix_of_eight_or_more_digits() {
        let dir = scratch("store");
        let store = tf_manifest::DirStore::new(&dir);
        let r = stored(&FLAGS);
        store.put(&r).unwrap();
        let hex = r.key().hex();
        assert_eq!(load_run(&dir, &hex).unwrap(), r);
        assert_eq!(load_run(&dir, &hex[..8]).unwrap(), r);
        assert_eq!(load_run(&dir, &hex[..8].to_uppercase()).unwrap(), r);
        assert!(
            load_run(&dir, &hex[..7])
                .unwrap_err()
                .to_string()
                .contains("8 or more")
        );
        assert!(
            load_run(&dir, "zzzzzzzzzz")
                .unwrap_err()
                .to_string()
                .contains("not a manifest hash")
        );
        let missing = format!("{}{}", &hex[..2], "0".repeat(10));
        let missing = if hex.starts_with(&missing) {
            format!("{}{}", &hex[..2], "f".repeat(10))
        } else {
            missing
        };
        assert!(
            load_run(&dir, &missing)
                .unwrap_err()
                .to_string()
                .contains("no stored run")
        );
        // A second run whose hash shares the first eight digits would be ambiguous: make the
        // situation by copying the file under a sibling name.
        let p = store.path_for(r.manifest());
        let sibling = p.with_file_name(format!("{}{}.tfrs", &hex[..8], "0".repeat(56)));
        std::fs::copy(&p, &sibling).unwrap();
        assert!(
            load_run(&dir, &hex[..8])
                .unwrap_err()
                .to_string()
                .contains("matches 2")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
