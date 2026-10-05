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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("synth") => synth(&args[1..]),
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
