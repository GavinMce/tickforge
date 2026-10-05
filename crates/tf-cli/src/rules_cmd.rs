//! `tf rules`: look at, review and approve proposed entry rules.
//!
//! - `diff BASE CAND` shows what changed and flags any loosening of a protective stage.
//! - `review CAND` backtests base and candidate on the same held-out sessions, stores every run
//!   (so `tf runs` and `tf explore` can open and compare them), applies the gates and writes the
//!   proposal record with its verdict.
//! - `show`, `list` read the records; `approve` appends a named person's approval.
//!
//! Nothing here changes what runs live. See `tf_backtest::review` for the gates.

use std::path::{Path, PathBuf};

use tf_backtest::review::{Outcome, Paired, Policy, Record, Verdict, evaluate};
use tf_manifest::{DirStore, RunResult};
use tf_replay::{EventSink, HashSink};
use tf_strategy::RuleSet;
use tf_strategy::rule_diff::{diff, loosens_a_veto};

use super::explore::DOLLAR;
use super::{backtest_config, backtest_manifest, momentum_metrics, parse_backtest};

/// The held-out sessions: seeds that a proposer's own tuning does not use.
const DEFAULT_SEEDS: (u64, u64) = (1000, 1023);
const SESSION: [&str; 10] = [
    "--healthy",
    "3",
    "--dangerous",
    "3",
    "--quiet",
    "2",
    "--secs",
    "520",
    "--lead",
    "120",
];

fn load_rules(spec: &str) -> Result<RuleSet, String> {
    if spec == "built-in" {
        return Ok(RuleSet::momentum());
    }
    let text = std::fs::read_to_string(spec).map_err(|e| format!("{spec}: {e}"))?;
    RuleSet::parse(&text).map_err(|e| format!("{spec}: {e}"))
}

fn fp(r: &RuleSet) -> String {
    format!("{:016x}", r.fingerprint())
}

/// The rule files kept under a store, for opening runs that used them.
pub(crate) fn store_rules(store: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(store.join("rules"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rules"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Keep `rules` in the store under its fingerprint; refuse to overwrite a file that differs.
fn keep_rules(store: &Path, rules: &RuleSet) -> Result<PathBuf, String> {
    let dir = store.join("rules");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.rules", fp(rules)));
    let text = rules.render();
    match std::fs::read_to_string(&path) {
        Ok(old) if old == text => {}
        Ok(_) => return Err(format!("{} exists with different text", path.display())),
        Err(_) => std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?,
    }
    Ok(path)
}

/// Run one session under the given rules (a file, or the built-in set) and keep the result.
fn run_session(store: &DirStore, seed: u64, rules: Option<&Path>) -> Result<RunResult, String> {
    let mut flags: Vec<String> = vec!["--seed".into(), seed.to_string()];
    flags.extend(SESSION.iter().map(|s| (*s).to_owned()));
    if let Some(p) = rules {
        flags.extend(["--rules".to_owned(), p.to_string_lossy().into_owned()]);
    }
    let a = parse_backtest(&flags)?;
    let cfg = backtest_config(&a)?;
    let (events, labels) = tf_backtest::demo_session_with_lead(
        a.seed,
        a.secs,
        a.healthy,
        a.dangerous,
        a.quiet,
        a.lead.unwrap_or(tf_backtest::DEMO_LEAD_SECS),
    );
    let start = events.first().map_or(0, |e| e.ts_recv());
    let manifest = backtest_manifest(&a, &cfg, start)?;
    if let Some(r) = store.get(&manifest).map_err(|e| e.to_string())? {
        return Ok(r);
    }
    let mut hash = HashSink::new();
    for ev in &events {
        hash.on_event(ev);
    }
    let r = tf_backtest::momentum_backtest(events.iter().copied(), labels, &cfg)?;
    let mut out = RunResult::new(manifest, events.len() as u64, hash.finish());
    for (k, v) in momentum_metrics(&r) {
        out = out.with_metric(&k, v).map_err(|e| e.to_string())?;
    }
    store.put(&out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn proposals_dir(store: &Path) -> PathBuf {
    store.join("proposals")
}

fn find_record(store: &Path, id: &str) -> Result<(PathBuf, Record), String> {
    if id.len() < 8 {
        return Err(format!(
            "{id:?} is too short for a proposal id (8 or more characters)"
        ));
    }
    let mut hits = Vec::new();
    for e in std::fs::read_dir(proposals_dir(store))
        .map_err(|e| format!("{}: {e}", proposals_dir(store).display()))?
        .flatten()
    {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(id) && name.ends_with(".tfpr") {
            hits.push(e.path());
        }
    }
    match hits.as_slice() {
        [] => Err(format!("no proposal starts with {id}")),
        [p] => {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            let r = Record::parse(&text).map_err(|e| format!("{}: {e}", p.display()))?;
            Ok((p.clone(), r))
        }
        many => Err(format!(
            "{id} matches {} proposals; give more of it",
            many.len()
        )),
    }
}

fn status(r: &Record) -> String {
    match (r.review.verdict, r.approvals.is_empty()) {
        (Verdict::Rejected, _) => "rejected".to_owned(),
        (v, true) => format!("{} (not approved)", v.name()),
        (v, false) => format!("{}, approved by {}", v.name(), r.approvals.join(", ")),
    }
}

fn print_diff(changes: &[tf_strategy::rule_diff::Change]) {
    if changes.is_empty() {
        println!("no changes");
    }
    for c in changes {
        println!("{c}");
    }
}

pub(crate) fn rules(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("diff") => cmd_diff(&args[1..]),
        Some("review") => cmd_review(&args[1..]),
        Some("show") => cmd_show(&args[1..]),
        Some("approve") => cmd_approve(&args[1..]),
        Some("list") => cmd_list(&args[1..]),
        _ => Err("usage: tf rules diff|review|show|approve|list (see `tf help`)".to_owned()),
    }
}

fn cmd_diff(args: &[String]) -> Result<(), String> {
    let [base, cand] = args else {
        return Err("usage: tf rules diff BASE CAND (a rules file, or `built-in`)".to_owned());
    };
    let (b, c) = (load_rules(base)?, load_rules(cand)?);
    let changes = diff(&b, &c, &tf_strategy::MomentumParams::default());
    println!("base {}  candidate {}", fp(&b), fp(&c));
    print_diff(&changes);
    if loosens_a_veto(&changes) {
        println!("this loosens a protective stage: a review will need a person");
    }
    Ok(())
}

fn usd_raw(s: &str, flag: &str) -> Result<i64, String> {
    let v = s.parse::<u64>().map_err(|e| format!("{flag}: {e}"))?;
    i64::try_from(v.saturating_mul(DOLLAR)).map_err(|_| format!("{flag}: too large"))
}

struct ReviewArgs {
    cand: String,
    base: String,
    store: PathBuf,
    proposer: String,
    reason: String,
    seeds: (u64, u64),
    policy: Policy,
}

fn parse_review(args: &[String]) -> Result<ReviewArgs, String> {
    let (mut cand, mut base, mut store) = (None, "built-in".to_owned(), None);
    let (mut proposer, mut reason, mut seeds) = (None, None, DEFAULT_SEEDS);
    let mut policy = Policy::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |n: &str| it.next().cloned().ok_or(format!("{n} needs a value"));
        match a.as_str() {
            "--store" => store = Some(val("--store")?),
            "--base" => base = val("--base")?,
            "--proposer" => proposer = Some(val("--proposer")?),
            "--reason" => reason = Some(val("--reason")?),
            "--seeds" => {
                let v = val("--seeds")?;
                let (lo, hi) = v.split_once("..").ok_or("--seeds is A..B")?;
                let p = |s: &str| s.parse::<u64>().map_err(|e| format!("--seeds: {e}"));
                seeds = (p(lo)?, p(hi)?);
                if seeds.0 > seeds.1 {
                    return Err("--seeds A..B needs A <= B".to_owned());
                }
            }
            "--min-sessions" => {
                policy.min_sessions = val("--min-sessions")?
                    .parse()
                    .map_err(|e| format!("--min-sessions: {e}"))?;
            }
            "--min-trades" => {
                policy.min_trades = val("--min-trades")?
                    .parse()
                    .map_err(|e| format!("--min-trades: {e}"))?;
            }
            "--allow-net-drop" => {
                policy.allow_net_drop = usd_raw(&val("--allow-net-drop")?, "--allow-net-drop")?
            }
            "--allow-drawdown-rise" => {
                policy.allow_drawdown_rise =
                    usd_raw(&val("--allow-drawdown-rise")?, "--allow-drawdown-rise")?;
            }
            "--worse-pct" => {
                let pct: u32 = val("--worse-pct")?
                    .parse()
                    .map_err(|e| format!("--worse-pct: {e}"))?;
                if pct > 100 {
                    return Err("--worse-pct is 0 to 100".to_owned());
                }
                policy.worse_sessions_permille = pct * 10;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            file if cand.is_none() => cand = Some(file.to_owned()),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    let cand = cand.ok_or("tf rules review needs the candidate rules file")?;
    let store = PathBuf::from(store.ok_or("tf rules review needs --store DIR")?);
    let proposer = proposer.ok_or("tf rules review needs --proposer NAME")?;
    let reason = reason.ok_or("tf rules review needs --reason TEXT")?;
    if cand == "built-in" {
        return Err("the candidate must be a rules file".to_owned());
    }
    Ok(ReviewArgs {
        cand,
        base,
        store,
        proposer,
        reason,
        seeds,
        policy,
    })
}

fn cmd_review(args: &[String]) -> Result<(), String> {
    let ReviewArgs {
        cand: cand_spec,
        base,
        store: store_dir,
        proposer,
        reason,
        seeds,
        policy,
    } = parse_review(args)?;
    let (b, c) = (load_rules(&base)?, load_rules(&cand_spec)?);
    let id = format!("{}-{}", fp(&c), fp(&b));
    let record_path = proposals_dir(&store_dir).join(format!("{id}.tfpr"));
    if record_path.exists() {
        let (_, old) = find_record(&store_dir, &id)?;
        return Err(format!(
            "this candidate was already reviewed against this base: {} (see `tf rules show {}`)",
            status(&old),
            &id[..16]
        ));
    }
    let changes = diff(&b, &c, &tf_strategy::MomentumParams::default());
    println!("base {}  candidate {}", fp(&b), fp(&c));
    print_diff(&changes);

    let cand_file = keep_rules(&store_dir, &c)?;
    let base_file = if base == "built-in" {
        None
    } else {
        Some(keep_rules(&store_dir, &b)?)
    };
    let store = DirStore::new(&store_dir);
    let (mut pairs, mut runs) = (Vec::new(), Vec::new());
    for seed in seeds.0..=seeds.1 {
        let rb = run_session(&store, seed, base_file.as_deref())?;
        let rc = run_session(&store, seed, Some(&cand_file))?;
        let (ob, oc) = (
            Outcome::from_metrics(rb.metrics())
                .ok_or("a stored run lacks the metrics the review needs")?,
            Outcome::from_metrics(rc.metrics())
                .ok_or("a stored run lacks the metrics the review needs")?,
        );
        pairs.push(Paired {
            seed,
            base: ob,
            cand: oc,
        });
        runs.push((seed, rb.key().hex(), rc.key().hex()));
    }
    let identical = pairs.iter().all(|p| p.base == p.cand);
    let loosens = loosens_a_veto(&changes);
    let review = evaluate(&pairs, b.fingerprint() == c.fingerprint(), loosens, &policy);
    let record = Record {
        proposer,
        reason,
        base: fp(&b),
        candidate: fp(&c),
        suite: format!(
            "seeds {}..{}; 3 healthy, 3 dangerous, 2 quiet; 520 s; 120 s lead-in; {} sessions",
            seeds.0,
            seeds.1,
            pairs.len()
        ),
        review,
        changes: changes.iter().map(ToString::to_string).collect(),
        runs,
        approvals: Vec::new(),
    };
    std::fs::create_dir_all(proposals_dir(&store_dir)).map_err(|e| e.to_string())?;
    std::fs::write(&record_path, record.to_text())
        .map_err(|e| format!("{}: {e}", record_path.display()))?;
    for g in &record.review.gates {
        println!(
            "gate  {:<10} {}  {}",
            g.name,
            if g.pass { "pass" } else { "FAIL" },
            g.detail
        );
    }
    if identical {
        println!(
            "note  the two rule sets gave identical outcomes in every session: the edit changed nothing these sessions can show"
        );
    }
    println!(
        "verdict {}   record {}",
        record.review.verdict.name(),
        record_path.display()
    );
    if let Some((_, rb, rc)) = record.runs.first() {
        println!(
            "compare one session: tf explore {} {} --store {}",
            &rb[..12],
            &rc[..12],
            store_dir.display()
        );
    }
    match record.review.verdict {
        Verdict::Rejected => Err("the proposal was rejected".to_owned()),
        Verdict::NeedsHuman => {
            println!(
                "a person must read the changes above before approving: `tf rules approve {} --by NAME --store ...`",
                &id[..16]
            );
            Ok(())
        }
        Verdict::Accepted => {
            println!(
                "passes every gate; still needs a person's approval: `tf rules approve {} --by NAME --store ...`",
                &id[..16]
            );
            Ok(())
        }
    }
}

fn store_arg(args: &[String]) -> Result<(Vec<String>, PathBuf, Option<String>), String> {
    let (mut rest, mut store, mut by) = (Vec::new(), None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => store = Some(it.next().cloned().ok_or("--store needs a directory")?),
            "--by" => by = Some(it.next().cloned().ok_or("--by needs a name")?),
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            other => rest.push(other.to_owned()),
        }
    }
    Ok((rest, PathBuf::from(store.ok_or("needs --store DIR")?), by))
}

fn cmd_show(args: &[String]) -> Result<(), String> {
    let (rest, store, _) = store_arg(args)?;
    let [id] = rest.as_slice() else {
        return Err("usage: tf rules show ID --store DIR".to_owned());
    };
    let (_, r) = find_record(&store, id)?;
    println!("proposal {}", r.id());
    println!("status   {}", status(&r));
    print!("{}", r.to_text());
    Ok(())
}

fn cmd_approve(args: &[String]) -> Result<(), String> {
    use std::io::Write as _;
    let (rest, store, by) = store_arg(args)?;
    let [id] = rest.as_slice() else {
        return Err("usage: tf rules approve ID --by NAME --store DIR".to_owned());
    };
    let by = by.ok_or("tf rules approve needs --by NAME")?;
    let (path, mut r) = find_record(&store, id)?;
    r.approve(&by).map_err(|e| e.to_string())?;
    let name = r.approvals.last().expect("just approved").clone();
    // Append only: the rest of the record is never rewritten.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(f, "approval {name}").map_err(|e| format!("{}: {e}", path.display()))?;
    println!("approved {} by {name}: {}", r.id(), status(&r));
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    let (rest, store, _) = store_arg(args)?;
    if !rest.is_empty() {
        return Err("usage: tf rules list --store DIR".to_owned());
    }
    let dir = proposals_dir(&store);
    let mut rows = Vec::new();
    for e in std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
    {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "tfpr") {
            continue;
        }
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        match Record::parse(&text) {
            Ok(r) => rows.push(r),
            Err(e) => println!("skipped  {}: {e}", p.display()),
        }
    }
    rows.sort_by_key(Record::id);
    if rows.is_empty() {
        println!("no proposals in {}", dir.display());
    }
    for r in &rows {
        println!(
            "{}  {:<12} {}  {} change(s)",
            &r.id()[..16],
            r.proposer.chars().take(12).collect::<String>(),
            status(r),
            r.changes.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-rules-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn file(dir: &Path, name: &str, text: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p.to_string_lossy().into_owned()
    }

    const BUILT: &str = tf_strategy::rules::MOMENTUM_RULES;

    fn variants(dir: &Path) -> [String; 4] {
        [
            // Drops the bounce and bid conditions: loses on every session.
            file(
                dir,
                "loose.rules",
                &BUILT.replace(
                    "; higher_lows >= @min_higher_lows; bid_support >= @min_bid_support_permille",
                    "",
                ),
            ),
            // An extra veto that never fires on these sessions: changes nothing.
            file(
                dir,
                "tight.rules",
                &BUILT.replace(
                    "volume_ratio > @max_volume_ratio_permille",
                    "volume_ratio > @max_volume_ratio_permille; retrace_now > 600",
                ),
            ),
            // A veto moved out of reach: same outcomes here, but it loosens.
            file(
                dir,
                "veto.rules",
                &BUILT.replace("depth > @max_depth_permille", "depth > 1500"),
            ),
            file(dir, "same.rules", BUILT),
        ]
    }

    fn review(dir: &Path, cand: &str, extra: &[&str]) -> Result<(), String> {
        let mut a = args(&[
            cand,
            "--store",
            dir.to_str().unwrap(),
            "--proposer",
            "agent-7",
            "--reason",
            "test",
        ]);
        a.extend(extra.iter().map(|s| (*s).to_owned()));
        cmd_review(&a)
    }

    fn records(dir: &Path) -> Vec<Record> {
        let mut v: Vec<_> = std::fs::read_dir(proposals_dir(dir))
            .unwrap()
            .flatten()
            .map(|e| Record::parse(&std::fs::read_to_string(e.path()).unwrap()).unwrap())
            .collect();
        v.sort_by_key(Record::id);
        v
    }

    #[test]
    fn rules_load_from_a_file_or_the_built_in_name_and_say_what_is_wrong() {
        let dir = scratch("load");
        assert_eq!(load_rules("built-in").unwrap(), RuleSet::momentum());
        let f = file(&dir, "r.rules", BUILT);
        assert_eq!(fp(&load_rules(&f).unwrap()), fp(&RuleSet::momentum()));
        assert!(
            load_rules(&file(&dir, "bad.rules", "rules v1\n"))
                .unwrap_err()
                .contains("stage")
        );
        assert!(load_rules(dir.join("missing").to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_files_are_kept_once_under_their_fingerprint_and_never_overwritten() {
        let dir = scratch("keep");
        let r = RuleSet::momentum();
        let p = keep_rules(&dir, &r).unwrap();
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            format!("{}.rules", fp(&r))
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r.render());
        assert_eq!(keep_rules(&dir, &r).unwrap(), p, "again is fine");
        std::fs::write(&p, "tampered").unwrap();
        assert!(keep_rules(&dir, &r).unwrap_err().contains("different text"));
        std::fs::write(dir.join("rules").join("notes.txt"), "x").unwrap();
        let listed = store_rules(&dir);
        assert_eq!(listed.len(), 1, "only .rules files: {listed:?}");
        assert!(store_rules(&dir.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_review_rejects_the_edit_that_loses_money_in_every_session() {
        let dir = scratch("reject");
        let [loose, ..] = variants(&dir);
        let e = review(&dir, &loose, &["--seeds", "1000..1011"]).unwrap_err();
        assert!(e.contains("rejected"), "{e}");
        let r = &records(&dir)[0];
        assert_eq!(r.review.verdict, Verdict::Rejected);
        let failed: Vec<_> = r
            .review
            .gates
            .iter()
            .filter(|g| !g.pass)
            .map(|g| g.name)
            .collect();
        assert_eq!(failed, ["net", "drawdown", "breadth"]);
        assert_eq!(
            (r.proposer.as_str(), r.reason.as_str()),
            ("agent-7", "test")
        );
        assert_eq!(r.runs.len(), 12);
        assert_eq!(r.changes.len(), 2);
        assert!(r.suite.contains("seeds 1000..1011"));
        // Both sides of every session were kept, and the rules with them.
        let (runs, skipped) = crate::runs::scan(&dir).unwrap();
        assert_eq!((runs.len(), skipped), (24, 0));
        assert!(store_rules(&dir).iter().any(|f| f.contains(&r.candidate)));
        // Reviewing the same candidate against the same base again is refused, with the verdict.
        let again = review(&dir, &loose, &["--seeds", "1000..1011"]).unwrap_err();
        assert!(
            again.contains("already reviewed") && again.contains("rejected"),
            "{again}"
        );
        // A rejected proposal cannot be approved.
        let id = &r.id()[..16];
        let e = cmd_approve(&args(&[
            id,
            "--by",
            "gavin",
            "--store",
            dir.to_str().unwrap(),
        ]))
        .unwrap_err();
        assert!(e.contains("rejected"), "{e}");
        assert!(records(&dir)[0].approvals.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_harmless_edit_is_accepted_a_loosened_veto_goes_to_a_person_and_approval_only_appends() {
        let dir = scratch("verdicts");
        let [_, tight, veto, same] = variants(&dir);
        let seeds = ["--seeds", "1000..1011"];
        review(&dir, &tight, &seeds).unwrap();
        review(&dir, &veto, &seeds).unwrap();
        let recs = records(&dir);
        let by_verdict = |v| recs.iter().find(|r| r.review.verdict == v).unwrap();
        let (acc, nh) = (
            by_verdict(Verdict::Accepted),
            by_verdict(Verdict::NeedsHuman),
        );
        assert!(acc.review.gates.iter().all(|g| g.pass));
        assert_eq!(
            nh.review
                .gates
                .iter()
                .filter(|g| !g.pass)
                .map(|g| g.name)
                .collect::<Vec<_>>(),
            ["veto"]
        );
        assert!(nh.changes[0].contains("LOOSENS A VETO"));
        // The built-in set against itself: nothing to review.
        let e = review(&dir, &same, &seeds).unwrap_err();
        assert!(e.contains("rejected"));
        let same_rec = records(&dir)
            .into_iter()
            .find(|r| r.candidate == r.base)
            .unwrap();
        assert_eq!(same_rec.review.gates[0].name, "different");
        assert!(!same_rec.review.gates[0].pass);
        // Approval appends one line and nothing else changes.
        let path = proposals_dir(&dir).join(format!("{}.tfpr", nh.id()));
        let before = std::fs::read_to_string(&path).unwrap();
        let d = dir.to_str().unwrap();
        cmd_approve(&args(&[&nh.id()[..16], "--by", "gavin", "--store", d])).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, format!("{before}approval gavin\n"));
        assert!(find_record(&dir, &nh.id()).unwrap().1.approved());
        assert!(
            cmd_approve(&args(&[&nh.id()[..16], "--by", "gavin", "--store", d]))
                .unwrap_err()
                .contains("already")
        );
        assert!(
            cmd_approve(&args(&[&nh.id()[..16], "--store", d]))
                .unwrap_err()
                .contains("--by")
        );
        assert!(status(&find_record(&dir, &nh.id()).unwrap().1).contains("approved by gavin"));
        assert_eq!(status(acc), "accepted (not approved)");
        cmd_show(&args(&[&acc.id()[..16], "--store", d])).unwrap();
        cmd_list(&args(&["--store", d])).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn too_few_sessions_is_a_rejection_and_the_policy_flags_move_the_gates() {
        let dir = scratch("policy");
        let [_, tight, ..] = variants(&dir);
        let e = review(&dir, &tight, &["--seeds", "1000..1004"]).unwrap_err();
        assert!(e.contains("rejected"));
        let r = &records(&dir)[0];
        assert_eq!(
            r.review
                .gates
                .iter()
                .filter(|g| !g.pass)
                .map(|g| g.name)
                .collect::<Vec<_>>(),
            ["sessions"]
        );
        assert!(
            r.review.gates[1]
                .detail
                .contains("5 paired sessions, at least 12")
        );
        // Asking for fewer is the proposer's choice to make only with a person watching: it works.
        let dir2 = scratch("policy2");
        let [_, tight2, ..] = variants(&dir2);
        review(
            &dir2,
            &tight2,
            &["--seeds", "1000..1004", "--min-sessions", "5"],
        )
        .unwrap();
        assert_eq!(records(&dir2)[0].review.verdict, Verdict::Accepted);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn bad_use_is_refused_with_a_reason() {
        let dir = scratch("usage");
        let [loose, ..] = variants(&dir);
        let d = dir.to_str().unwrap();
        let base = |extra: &[&str]| {
            let mut a = args(&[&loose, "--store", d, "--proposer", "p", "--reason", "r"]);
            a.extend(extra.iter().map(|s| (*s).to_owned()));
            cmd_review(&a).unwrap_err()
        };
        assert!(base(&["--seeds", "5"]).contains("A..B"));
        assert!(base(&["--seeds", "9..3"]).contains("A <= B"));
        assert!(base(&["--seeds", "a..3"]).contains("--seeds"));
        assert!(base(&["--worse-pct", "101"]).contains("0 to 100"));
        assert!(base(&["--bogus"]).contains("unknown flag"));
        assert!(base(&["--min-sessions"]).contains("needs a value"));
        assert!(base(&["extra"]).contains("unexpected argument"));
        assert!(base(&["--allow-net-drop", "x"]).contains("--allow-net-drop"));
        assert!(
            cmd_review(&args(&["--store", d, "--proposer", "p", "--reason", "r"]))
                .unwrap_err()
                .contains("candidate")
        );
        assert!(
            cmd_review(&args(&[&loose, "--proposer", "p", "--reason", "r"]))
                .unwrap_err()
                .contains("--store")
        );
        assert!(
            cmd_review(&args(&[&loose, "--store", d, "--reason", "r"]))
                .unwrap_err()
                .contains("--proposer")
        );
        assert!(
            cmd_review(&args(&[&loose, "--store", d, "--proposer", "p"]))
                .unwrap_err()
                .contains("--reason")
        );
        assert!(
            cmd_review(&args(&[
                "built-in",
                "--store",
                d,
                "--proposer",
                "p",
                "--reason",
                "r"
            ]))
            .unwrap_err()
            .contains("rules file")
        );
        assert!(rules(&args(&["nonsense"])).is_err());
        assert!(cmd_diff(&args(&["only-one"])).is_err());
        assert!(
            find_record(&dir, "short")
                .unwrap_err()
                .contains("too short")
        );
        assert!(cmd_show(&args(&["--store", d])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_proposals_runs_open_in_the_explorer_without_passing_the_rules_files() {
        let dir = scratch("explore");
        let [_, tight, ..] = variants(&dir);
        review(&dir, &tight, &["--seeds", "1000..1011"]).unwrap();
        let r = &records(&dir)[0];
        let (_, b, c) = &r.runs[0];
        let out = dir.join("x.html");
        crate::explore::explore(&args(&[
            &b[..12],
            &c[..12],
            "--store",
            dir.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]))
        .unwrap();
        assert!(
            std::fs::read_to_string(&out)
                .unwrap()
                .contains(&r.candidate[..8])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_flags_set_the_policy_in_the_units_they_name() {
        let dir = scratch("flags");
        let [loose, ..] = variants(&dir);
        let parse = |extra: &[&str]| {
            let mut a = args(&[&loose, "--store", "S", "--proposer", "p", "--reason", "r"]);
            a.extend(extra.iter().map(|s| (*s).to_owned()));
            parse_review(&a).unwrap()
        };
        let d = parse(&[]);
        assert_eq!(
            (d.seeds, d.base.as_str(), d.policy),
            ((1000, 1023), "built-in", Policy::default())
        );
        assert_eq!(
            (d.proposer.as_str(), d.reason.as_str(), d.store),
            ("p", "r", PathBuf::from("S"))
        );
        let a = parse(&[
            "--seeds",
            "7..9",
            "--base",
            "b.rules",
            "--min-sessions",
            "3",
            "--min-trades",
            "4",
            "--allow-net-drop",
            "5",
            "--allow-drawdown-rise",
            "2",
            "--worse-pct",
            "40",
        ]);
        assert_eq!((a.seeds, a.base.as_str()), ((7, 9), "b.rules"));
        assert_eq!(
            a.policy,
            Policy {
                min_sessions: 3,
                min_trades: 4,
                allow_net_drop: 5 * 1_000_000_000,
                allow_drawdown_rise: 2 * 1_000_000_000,
                worse_sessions_permille: 400,
            }
        );
        assert_eq!(
            parse(&["--worse-pct", "0"]).policy.worse_sessions_permille,
            0
        );
        assert_eq!(
            parse(&["--worse-pct", "100"])
                .policy
                .worse_sessions_permille,
            1000
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_review_runs_the_base_as_the_base_and_a_named_base_file_as_that_file() {
        let dir = scratch("bases");
        let [_, tight, veto, _] = variants(&dir);
        let rules_of = |dir: &Path| {
            let (runs, _) = crate::runs::scan(dir).unwrap();
            let mut v: Vec<Option<String>> = runs
                .iter()
                .map(|r| {
                    r.manifest()
                        .config()
                        .get("rules")
                        .map(|s| s[..8].to_owned())
                })
                .collect();
            v.sort();
            v
        };
        // Against the built-in set: half the runs carry no rules key at all.
        review(
            &dir,
            &tight,
            &["--seeds", "1000..1005", "--min-sessions", "6"],
        )
        .unwrap();
        let v = rules_of(&dir);
        assert_eq!(
            v.iter().filter(|r| r.is_none()).count(),
            6,
            "the built-in base: {v:?}"
        );
        let tight_fp = fp(&load_rules(&tight).unwrap());
        assert_eq!(
            v.iter()
                .filter(|r| **r == Some(tight_fp[..8].to_owned()))
                .count(),
            6
        );
        // Against a named base: both sides run under a file, and the built-in one is not run again.
        let dir2 = scratch("bases2");
        review(
            &dir2,
            &veto,
            &[
                "--base",
                &tight,
                "--seeds",
                "1000..1005",
                "--min-sessions",
                "6",
            ],
        )
        .unwrap();
        let v = rules_of(&dir2);
        assert!(v.iter().all(Option::is_some), "{v:?}");
        let veto_fp = fp(&load_rules(&veto).unwrap());
        let mut want = vec![Some(tight_fp[..8].to_owned()); 6];
        want.extend(vec![Some(veto_fp[..8].to_owned()); 6]);
        want.sort();
        assert_eq!(v, want);
        let r = &records(&dir2)[0];
        assert_eq!(
            (r.base.as_str(), r.candidate.as_str()),
            (tight_fp.as_str(), veto_fp.as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn the_run_browser_checks_runs_of_a_proposal_without_being_given_its_rules_files() {
        let dir = scratch("browse");
        let [_, tight, ..] = variants(&dir);
        review(&dir, &tight, &["--seeds", "1000..1011"]).unwrap();
        let d = dir.to_str().unwrap();
        let (text, open) = crate::runs::report(&args(&["--store", d, "--check"])).unwrap();
        assert!(open.is_none());
        let rows: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(rows.len(), 24);
        assert!(rows.iter().all(|l| l.ends_with("  ok")), "{text}");
        // And it opens a row, carrying the store's rule files with it.
        let (_, open) = crate::runs::report(&args(&["--store", d, "--open", "1"])).unwrap();
        let (chosen, rules, _) = open.unwrap();
        assert_eq!(chosen.len(), 1);
        assert!(
            rules
                .iter()
                .any(|f| f.contains(&fp(&load_rules(&tight).unwrap())))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
