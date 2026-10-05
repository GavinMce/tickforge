use std::process::ExitCode;
use std::time::Instant;

use tf_core::{Event, NANOS_PER_SEC, SimClock};
use tf_provider::{Channels, Subscription};
use tf_replay::{HashSink, RunConfig, StatsSink, Tee, run};
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
}

fn parse_synth(args: &[String]) -> Result<SynthArgs, String> {
    let mut a = SynthArgs {
        seed: 42,
        symbols: 5000,
        secs: 600,
        runners: 20,
        dump: 0,
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
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(a)
}

fn synth(args: &[String]) -> Result<(), String> {
    let a = parse_synth(args)?;
    let cfg = SynthConfig::universe(a.seed, a.symbols, a.secs * NANOS_PER_SEC, a.runners);
    let table = cfg.symbol_table();

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
