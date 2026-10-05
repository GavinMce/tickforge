use std::process::ExitCode;
use std::time::Instant;

use tf_core::{Event, NANOS_PER_SEC, SimClock};
use tf_manifest::{DataRange, DirStore, Manifest, Put, RunResult};
use tf_provider::{Channels, Subscription};
use tf_replay::{EventSink, HashSink, RunConfig, StatsSink, Tee, run};
use tf_synth::{Account, SynthConfig, SynthOptions, SynthProvider, SynthStream};

const USAGE: &str = "\
tf - tickforge

USAGE:
    tf synth [--seed N] [--symbols N] [--secs N] [--runners PERMILLE] [--dump N]

    Generate a deterministic synthetic session, run it through the engine
    loop, and print a summary plus a hash that is identical for identical
    inputs.

    --seed N          RNG seed (default 42)
    --symbols N       universe size (default 5000)
    --secs N          simulated session length (default 600)
    --runners N       permille of symbols with a runner scenario (default 20)
    --dump N          also print the first N events
    --store DIR       keep the result in DIR keyed by a manifest hash (seed, git sha,
                      config, data range); a rerun of the same manifest is skipped

    tf backtest [--seed N] [--secs N] [--healthy N] [--dangerous N] [--quiet N]
                [--latency-ms N] [--borrow-bps N] [--order-notional USD]
                [--daily-loss USD] [--max-orders N] [--higher-lows N] [--store DIR]

    Run Strategy 1 (long side) over a synthetic session of healthy and dangerous
    runners and quiet names, through the risk gateway and the simulated broker,
    and print the report: P&L, drawdown, slippage, hit rate, per-scenario
    breakdown, and what the gateway refused.

    --seed N          RNG seed (default 1)
    --secs N          simulated session length (default 400)
    --healthy N       healthy-pullback runners (default 2)
    --dangerous N     dangerous-pullback runners (default 2)
    --quiet N         quiet symbols (default 2)
    --latency-ms N    order latency to the venue (default 50)
    --borrow-bps N    annual borrow fee on shorts, basis points (default 0)
    --order-notional  gateway cap per order in dollars (default 5000)
    --daily-loss      gateway daily loss limit in dollars (default 1000)
    --max-orders N    gateway orders per 10 s (default 20)
    --higher-lows N   strategy: higher lows required before entering (default 1)
    --store DIR       keep the result keyed by a manifest hash of the whole setup
                      (seed, git sha, session, limits, parameters); reruns are skipped

    tf bench [--symbols N] [--secs N] [--seed N] [--runs N] [--out FILE]
             [--compare FILE] [--commit SHA] [--flag-drop PCT] [--flag-rise PCT]

    Measure events/s and per-event p50/p99/p99.9 latency through the run loop
    for four scenarios, and print a markdown table. Use a release build.

    --symbols N       universe size (default 5000)
    --secs N          simulated session length (default 400)
    --seed N          RNG seed (default 1)
    --runs N          repetitions per scenario (default 3)
    --out FILE        append the results as JSON lines (a per-commit history)
    --compare FILE    show the change against the results in FILE
    --commit SHA      label for the results (default: $GITHUB_SHA, else git HEAD)
    --flag-drop PCT   flag a throughput drop over PCT% against --compare (default 10)
    --flag-rise PCT   flag a p99 rise over PCT% against --compare (default 25)
                      The defaults suit a quiet machine; CI runners vary by 2x.
";

struct SynthArgs {
    seed: u64,
    symbols: usize,
    secs: u64,
    runners: u32,
    dump: usize,
    store: Option<String>,
}

fn parse_synth(args: &[String]) -> Result<SynthArgs, String> {
    let mut a = SynthArgs {
        seed: 42,
        symbols: 5000,
        secs: 600,
        runners: 20,
        dump: 0,
        store: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = |name: &str| -> Result<u64, String> {
            it.next()
                .ok_or_else(|| format!("{name} needs a value"))?
                .parse::<u64>()
                .map_err(|e| format!("{name}: {e}"))
        };
        match flag.as_str() {
            "--seed" => a.seed = val("--seed")?,
            "--symbols" => a.symbols = val("--symbols")? as usize,
            "--secs" => a.secs = val("--secs")?,
            "--runners" => a.runners = val("--runners")? as u32,
            "--dump" => a.dump = val("--dump")? as usize,
            "--store" => {
                a.store = Some(it.next().cloned().ok_or("--store needs a value")?);
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

/// The commit being run, with `-dirty` if the working tree has changes, since
/// results from uncommitted code should not share a key with the commit.
fn git_sha() -> String {
    let Some(head) = git_head() else {
        return "unknown".to_owned();
    };
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.status.success() || !o.stdout.is_empty())
        .unwrap_or(true);
    if dirty { format!("{head}-dirty") } else { head }
}

fn synth_manifest(a: &SynthArgs, cfg: &SynthConfig) -> Result<Manifest, String> {
    let data = DataRange {
        source: "synth:universe".to_owned(),
        from: cfg.session_start,
        to: cfg.session_start + cfg.duration,
    };
    Manifest::new(&git_sha(), "synth", a.seed, data)
        .and_then(|m| m.with_config("symbols", &a.symbols.to_string()))
        .and_then(|m| m.with_config("secs", &a.secs.to_string()))
        .and_then(|m| m.with_config("runners_permille", &a.runners.to_string()))
        .map_err(|e| e.to_string())
}

fn synth(args: &[String]) -> Result<(), String> {
    let a = parse_synth(args)?;
    let cfg = SynthConfig::universe(a.seed, a.symbols, a.secs * NANOS_PER_SEC, a.runners);
    let table = cfg.symbol_table();

    let stored = match &a.store {
        Some(dir) => {
            let manifest = synth_manifest(&a, &cfg)?;
            let store = DirStore::new(dir);
            if let Some(r) = store.get(&manifest).map_err(|e| e.to_string())? {
                println!("cached   {}  (not rerun)", manifest.hash());
                println!(
                    "seed {} | {} symbols | {} s simulated",
                    a.seed, a.symbols, a.secs
                );
                println!("events   {:>12}", r.events);
                for (k, v) in r.metrics() {
                    println!("{k:<8} {v:>12}");
                }
                println!("hash     {:>#18x}", r.event_hash);
                return Ok(());
            }
            Some((store, manifest))
        }
        None => None,
    };

    if a.dump > 0 {
        for ev in SynthStream::new(&cfg).take(a.dump) {
            let name = table.name(ev.instrument()).unwrap_or("?");
            match ev {
                Event::Trade(t) => {
                    println!("{:>16} {name} trade {} x {}", t.hdr.ts_recv, t.px, t.size)
                }
                Event::Quote(q) => {
                    println!(
                        "{:>16} {name} quote {} x {} / {} x {}",
                        q.hdr.ts_recv, q.bid_px, q.bid_sz, q.ask_px, q.ask_sz
                    );
                }
                Event::Status(s) => println!("{:>16} {name} status {:?}", s.hdr.ts_recv, s.kind),
                Event::Correction(c) => println!(
                    "{:>16} {name} correction {} x {} -> {} x {}",
                    c.hdr.ts_recv, c.orig_px, c.orig_size, c.px, c.size
                ),
                Event::CancelError(c) => println!(
                    "{:>16} {name} {:?} {} x {}",
                    c.hdr.ts_recv, c.kind, c.px, c.size
                ),
                Event::News(n) => {
                    println!("{:>16} {name} news #{}", n.hdr.ts_recv, n.article_id)
                }
            }
        }
        println!();
    }

    let mut provider = SynthProvider::new(cfg.clone(), SynthOptions::default(), Account::new(1));
    let clock = SimClock::new(cfg.session_start);
    let mut sink = Tee(HashSink::new(), StatsSink::default());
    let started = Instant::now();
    let report = run(
        &mut provider,
        &Subscription::all(Channels::ALL),
        &clock,
        &mut sink,
        &RunConfig::default(),
    )
    .map_err(|e| e.to_string())?;
    let wall = started.elapsed().as_secs_f64();

    let Tee(hash, stats) = sink;
    let mut busiest: Vec<(usize, u64)> = stats.per_instrument.iter().copied().enumerate().collect();
    busiest.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));

    println!(
        "seed {} | {} symbols | {} s simulated",
        a.seed, a.symbols, a.secs
    );
    println!(
        "events   {:>12}  ({} trades, {} quotes, {} status)",
        report.events, stats.trades, stats.quotes, stats.status
    );
    println!("shares   {:>12}", stats.shares);
    if let Some(px) = stats.max_trade_px {
        println!("max px   {px:>12}");
    }
    println!(
        "rate     {:>12.0} events/s simulated",
        report.events as f64 / a.secs.max(1) as f64
    );
    println!("hash     {:>#18x}", hash.finish());
    println!(
        "wall     {wall:>11.3}s  ({:.2}M events/s through run loop)",
        report.events as f64 / wall.max(1e-9) / 1e6
    );
    println!("busiest:");
    for (id, n) in busiest.iter().take(5) {
        println!("  {:<10} {n}", table.name(*id as u32).unwrap_or("?"));
    }

    if let Some((store, manifest)) = stored {
        let key = manifest.hash();
        let result = RunResult::new(manifest, report.events, hash.finish())
            .with_metric("trades", stats.trades as i64)
            .and_then(|r| r.with_metric("quotes", stats.quotes as i64))
            .and_then(|r| r.with_metric("status", stats.status as i64))
            .and_then(|r| r.with_metric("shares", stats.shares as i64))
            .and_then(|r| r.with_metric("max_px_raw", stats.max_trade_px.map_or(0, |p| p.raw())))
            .map_err(|e| e.to_string())?;
        let what = match store.put(&result).map_err(|e| e.to_string())? {
            Put::Written => "stored",
            Put::Deduped => "already stored (identical)",
        };
        println!("{what} {key}");
    }
    Ok(())
}

struct BacktestArgs {
    seed: u64,
    secs: u64,
    healthy: u32,
    dangerous: u32,
    quiet: u32,
    latency_ms: u64,
    borrow_bps: u32,
    order_notional: u64,
    daily_loss: u64,
    max_orders: u32,
    higher_lows: u32,
    store: Option<String>,
}

fn parse_backtest(args: &[String]) -> Result<BacktestArgs, String> {
    let mut a = BacktestArgs {
        seed: 1,
        secs: 400,
        healthy: 2,
        dangerous: 2,
        quiet: 2,
        latency_ms: 50,
        borrow_bps: 0,
        order_notional: 5_000,
        daily_loss: 1_000,
        max_orders: 20,
        higher_lows: 1,
        store: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = |name: &str| -> Result<u64, String> {
            it.next()
                .ok_or_else(|| format!("{name} needs a value"))?
                .parse::<u64>()
                .map_err(|e| format!("{name}: {e}"))
        };
        match flag.as_str() {
            "--seed" => a.seed = val("--seed")?,
            "--secs" => a.secs = val("--secs")?,
            "--healthy" => a.healthy = val("--healthy")? as u32,
            "--dangerous" => a.dangerous = val("--dangerous")? as u32,
            "--quiet" => a.quiet = val("--quiet")? as u32,
            "--latency-ms" => a.latency_ms = val("--latency-ms")?,
            "--borrow-bps" => a.borrow_bps = val("--borrow-bps")? as u32,
            "--order-notional" => a.order_notional = val("--order-notional")?,
            "--daily-loss" => a.daily_loss = val("--daily-loss")?,
            "--max-orders" => a.max_orders = val("--max-orders")? as u32,
            "--higher-lows" => a.higher_lows = val("--higher-lows")? as u32,
            "--store" => a.store = Some(it.next().cloned().ok_or("--store needs a value")?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

fn backtest_config(a: &BacktestArgs) -> Result<tf_backtest::BacktestConfig, String> {
    use tf_backtest::dollars;
    use tf_risk::{GapRule, Limits};
    let limits = Limits::new(
        dollars(a.order_notional),
        5_000,
        dollars(a.order_notional.max(20_000)),
        dollars(a.daily_loss),
        a.max_orders,
        10 * NANOS_PER_SEC,
    )
    .map_err(|e| format!("{e:?}"))?
    .with_gap_rule(GapRule::new(dollars(100_000), 20_000, 1000).map_err(|e| format!("{e:?}"))?);
    Ok(tf_backtest::BacktestConfig {
        sim: tf_strategy::SimConfig {
            latency_ns: a.latency_ms * 1_000_000,
            borrow_bps_per_year: a.borrow_bps,
        },
        limits,
        params: tf_strategy::MomentumParams {
            min_higher_lows: a.higher_lows,
            ..tf_strategy::MomentumParams::default()
        },
    })
}

fn backtest_manifest(
    a: &BacktestArgs,
    cfg: &tf_backtest::BacktestConfig,
    from: u64,
) -> Result<Manifest, String> {
    let data = DataRange {
        source: "synth:demo".to_owned(),
        from,
        to: from + a.secs * NANOS_PER_SEC,
    };
    let mut m = Manifest::new(&git_sha(), "backtest", a.seed, data).map_err(|e| e.to_string())?;
    let session = [
        ("secs", a.secs.to_string()),
        ("healthy", a.healthy.to_string()),
        ("dangerous", a.dangerous.to_string()),
        ("quiet", a.quiet.to_string()),
        ("latency_ns", cfg.sim.latency_ns.to_string()),
        ("borrow_bps", cfg.sim.borrow_bps_per_year.to_string()),
    ];
    for (k, v) in session {
        m = m.with_config(k, &v).map_err(|e| e.to_string())?;
    }
    for (k, v) in cfg.limits.pairs() {
        m = m
            .with_config(&format!("limit_{k}"), &v)
            .map_err(|e| e.to_string())?;
    }
    for (k, v) in cfg.params.pairs() {
        m = m.with_param(k, &v).map_err(|e| e.to_string())?;
    }
    Ok(m)
}

fn backtest(args: &[String]) -> Result<(), String> {
    let a = parse_backtest(args)?;
    let cfg = backtest_config(&a)?;
    let (events, labels) =
        tf_backtest::demo_session(a.seed, a.secs, a.healthy, a.dangerous, a.quiet);
    let mut hash = HashSink::new();
    for ev in &events {
        hash.on_event(ev);
    }
    let start = events.first().map_or(0, |e| e.ts_recv());
    let manifest = backtest_manifest(&a, &cfg, start)?;
    let store = a.store.as_deref().map(DirStore::new);
    if let Some(store) = &store {
        if let Some(r) = store.get(&manifest).map_err(|e| e.to_string())? {
            println!("cached   {}  (not rerun)", manifest.hash());
            println!("events   {:>12}", r.events);
            for (k, v) in r.metrics() {
                println!("{k:<34} {v:>16}");
            }
            return Ok(());
        }
    }

    let r = tf_backtest::momentum_backtest(events.iter().copied(), labels, &cfg)?;
    println!(
        "seed {} | {} healthy, {} dangerous, {} quiet | {} s simulated | latency {} ms",
        a.seed, a.healthy, a.dangerous, a.quiet, a.secs, a.latency_ms
    );
    print!("{}", r.report.render());
    println!(
        "gateway  intents {}  accepted {}  fills {}",
        r.intents, r.accepted, r.fills
    );
    if r.rejections.is_empty() {
        println!("refused  nothing");
    }
    for (why, n) in &r.rejections {
        println!("refused  {why:<22} {n}");
    }
    println!(
        "books    gateway and broker positions {}  | bookkeeping errors {}",
        if r.books_agree() { "agree" } else { "DISAGREE" },
        r.bookkeeping_errors
    );
    println!("outcome  {:>#18x}", r.outcome_hash);

    if let Some(store) = store {
        let key = manifest.hash();
        let mut metrics = r.report.metrics();
        metrics.push(("gateway.intents".into(), r.intents as i64));
        metrics.push(("gateway.accepted".into(), r.accepted as i64));
        metrics.push((
            "gateway.bookkeeping_errors".into(),
            r.bookkeeping_errors as i64,
        ));
        metrics.push(("books_agree".into(), i64::from(r.books_agree())));
        metrics.push(("outcome_hash".into(), r.outcome_hash as i64));
        for (why, n) in &r.rejections {
            metrics.push((format!("refused.{why}"), *n as i64));
        }
        let mut result = RunResult::new(manifest, events.len() as u64, hash.finish());
        for (k, v) in metrics {
            result = result.with_metric(&k, v).map_err(|e| e.to_string())?;
        }
        let what = match store.put(&result).map_err(|e| e.to_string())? {
            Put::Written => "stored",
            Put::Deduped => "already stored (identical)",
        };
        println!("{what} {key}");
    }
    Ok(())
}

struct BenchArgs {
    symbols: usize,
    secs: u64,
    seed: u64,
    runs: usize,
    out: Option<String>,
    compare: Option<String>,
    commit: Option<String>,
    flag_drop: u64,
    flag_rise: u64,
}

fn parse_bench(args: &[String]) -> Result<BenchArgs, String> {
    let mut a = BenchArgs {
        symbols: 5000,
        secs: 400,
        seed: 1,
        runs: 3,
        out: None,
        compare: None,
        commit: None,
        flag_drop: 10,
        flag_rise: 25,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut text = |name: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        let num = |name: &str, v: String| v.parse::<u64>().map_err(|e| format!("{name}: {e}"));
        match flag.as_str() {
            "--symbols" => a.symbols = num("--symbols", text("--symbols")?)? as usize,
            "--secs" => a.secs = num("--secs", text("--secs")?)?,
            "--seed" => a.seed = num("--seed", text("--seed")?)?,
            "--runs" => a.runs = num("--runs", text("--runs")?)? as usize,
            "--out" => a.out = Some(text("--out")?),
            "--compare" => a.compare = Some(text("--compare")?),
            "--commit" => a.commit = Some(text("--commit")?),
            "--flag-drop" => a.flag_drop = num("--flag-drop", text("--flag-drop")?)?,
            "--flag-rise" => a.flag_rise = num("--flag-rise", text("--flag-rise")?)?,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

fn git_head() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn bench(args: &[String]) -> Result<(), String> {
    let a = parse_bench(args)?;
    if cfg!(debug_assertions) {
        eprintln!("warning: this is a debug build; the timings mean nothing. Use a release build.");
    }
    let commit = a
        .commit
        .or_else(|| std::env::var("GITHUB_SHA").ok())
        .or_else(git_head)
        .unwrap_or_else(|| "unknown".to_owned());
    let workload = tf_bench::Workload {
        symbols: a.symbols,
        secs: a.secs,
        seed: a.seed,
    };
    let rows = tf_bench::run_all(&workload, a.runs, &tf_bench::Env::detect(commit))?;

    let base = match &a.compare {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Some(tf_bench::from_jsonl(&text).map_err(|e| format!("{path}: {e}"))?)
        }
        None => None,
    };
    let thresholds = tf_bench::Thresholds {
        drop_permille: (a.flag_drop * 10) as i64,
        rise_permille: (a.flag_rise * 10) as i64,
    };
    print!(
        "{}",
        tf_bench::markdown(&rows, base.as_deref(), &thresholds)
    );

    if let Some(path) = &a.out {
        use std::io::Write as _;
        if let Some(dir) = std::path::Path::new(path)
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            std::fs::create_dir_all(dir).map_err(|e| format!("{path}: {e}"))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("{path}: {e}"))?;
        f.write_all(tf_bench::to_jsonl(&rows).as_bytes())
            .map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("synth") => synth(&args[1..]),
        Some("bench") => bench(&args[1..]),
        Some("backtest") => backtest(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command {other}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let a = parse_backtest(&args).unwrap();
        let cfg = backtest_config(&a).unwrap();
        backtest_manifest(&a, &cfg, 1_000)
            .unwrap()
            .hash()
            .to_string()
    }

    #[test]
    fn every_backtest_setting_is_part_of_the_manifest() {
        let base = hash(&[]);
        assert_eq!(base, hash(&[]), "same settings, same key");
        for flags in [
            &["--seed", "2"][..],
            &["--secs", "300"],
            &["--healthy", "3"],
            &["--dangerous", "3"],
            &["--quiet", "3"],
            &["--latency-ms", "51"],
            &["--borrow-bps", "1"],
            &["--order-notional", "4000"],
            &["--daily-loss", "999"],
            &["--max-orders", "19"],
            &["--higher-lows", "0"],
        ] {
            assert_ne!(
                base,
                hash(flags),
                "{flags:?} must change the key or a stale result would be reused"
            );
        }
    }

    #[test]
    fn bad_backtest_arguments_are_refused() {
        let bad = |a: &[&str]| {
            parse_backtest(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).is_err()
        };
        assert!(bad(&["--nope"]));
        assert!(bad(&["--seed"]));
        assert!(bad(&["--seed", "x"]));
        let a = parse_backtest(&["--daily-loss".to_owned(), "0".to_owned()]).unwrap();
        assert!(backtest_config(&a).is_err(), "a zero limit is refused");
    }
}
