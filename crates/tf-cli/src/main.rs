mod budgets_cmd;
mod catalog_cmd;
mod explore;
mod history_cmd;
mod ledger_cmd;
mod reference_cmd;
mod rules_cmd;
mod runs;
mod serve_cmd;
mod universe_cmd;

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
                [--strategy momentum|trend] [--lead SECS] [--fast N] [--slow N]
                [--propose NAME=VALUE@SECS]... [--revert-drawdown USD] [--lockout SECS]
                [--export FILE.json|FILE.html] [--rules FILE]

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
    --higher-lows N   momentum: higher lows required before entering (default 1)
    --strategy NAME   momentum (the pullback strategy, default) or trend (the example
                      indicator strategy: EMA cross / VWAP reclaim / volume surge on
                      one-minute bars; it needs minutes to warm up, so its defaults are
                      a 1800 s session and a 420 s quiet lead-in)
    --lead SECS       quiet lead-in before the first runner (default 70, which the scanner's baseline needs; 420 for trend)
    --fast N          trend: fast EMA period in bars (default 3)
    --slow N          trend: slow EMA period in bars (default 6)
    --propose P       momentum: an agent's parameter proposal, NAME=VALUE@SECS after the
                      session starts (repeatable). Switches to an A/B run: the strategy
                      tuned through its bounded parameter store against a shadow copy
                      with fixed parameters on the same feed, each with its own simulated
                      broker and gateway. Prints both reports, the difference, the
                      changes applied and the proposals refused (with why).
    --rules FILE      momentum only: decide with the entry rules in FILE (see `tf_strategy::rules`)
    --export FILE     momentum only: also write the trade explorer's data (FILE.json) or a
                      standalone explorer page (FILE.html)
                      (bars, ticks, scanner hits, tier moves, orders with the gateway's
                      answer, fills, stop paths, and the strategy's own record of why it
                      entered or declined each symbol)
    --revert-drawdown USD   with --propose: the safety policy returns every tuned
                      parameter to baseline when the tuned side falls this many dollars
                      behind its own best showing against the shadow
    --lockout SECS    with --revert-drawdown: refuse all tuning for this long after a
                      revert (default 300)
    --store DIR       keep the result keyed by a manifest hash of the whole setup
                      (seed, git sha, session, limits, parameters); reruns are skipped

    tf explore HASH [HASH2] --store DIR [--rules FILE]... [--out FILE.html]

    Open stored backtest runs (kept by `tf backtest --store DIR`) in the trade explorer.
    The run is rebuilt from its manifest and replayed, and the replay is checked against
    the stored result (event hash and every metric) before anything is shown; a run the
    code no longer reproduces is refused. With two hashes, the page also compares them
    trade by trade. A run that used --rules needs the same file given here.

    HASH              a manifest hash, or a prefix of 8 or more digits
    --out FILE        where to write the page (default explorer.html)

    tf runs --store DIR [--kind K] [--strategy S] [--seed N] [--rules-id PREFIX] [--git PREFIX]
            [--sort hash|net|trades|seed] [--desc] [--check] [--rules FILE]...
            [--open ROW [ROW]] [--out FILE.html]

    List the runs kept by `tf backtest --store DIR`, numbered, with what a person picks by:
    short hash, strategy, seed, session size, the mix of scenarios, the entry rules (built-in
    or a fingerprint), the git revision they were stored at, trades and net P&L.

    --kind, --strategy, --seed, --rules-id, --git   keep only matching runs
    --sort KEY        order by hash (default), net, trades or seed; --desc reverses
    --check           add a column saying whether the current code still reproduces each
                      stored run (ok, drifted, needs --rules, cannot rebuild, n/a); this
                      replays every run, so it takes as long as running them
    --open ROW [ROW]  open that row of the list in the explorer, or compare two rows
                      (same rules as `tf explore`: pass --rules FILE for custom rules)

    tf rules diff BASE CAND
    tf rules review CAND --store DIR --proposer NAME --reason TEXT [--base FILE] [--seeds A..B]
                    [--min-sessions N] [--min-trades N] [--allow-net-drop USD]
                    [--allow-drawdown-rise USD] [--worse-pct N]
    tf rules show|approve|list ... --store DIR [--by NAME]

    Review a proposed change to the momentum strategy's entry rules before anything uses it.
    BASE and CAND are rules files (BASE may be `built-in`).

    diff      the structural change, flagging any loosening of a protective stage
              (too_old, dangerous): a condition removed, a threshold moved so it fires less
              often, or `any` made `all`
    review    runs base and candidate on the same held-out sessions (seeds 1000..1023 unless
              --seeds), keeps every run in the store (open them with `tf runs` / `tf explore`),
              applies the gates (a different rule set; enough sessions and trades; no more
              entries in the dangerous scenarios; net P&L and worst drawdown no worse; not
              worse in more than a quarter of the sessions) and writes a proposal record with
              the verdict: accepted, needs-human (it loosens a veto) or rejected. Exits with
              an error when rejected.
    show      the record and its status;  list  all records
    approve   record a named person's approval (appended to the record). A rejected proposal
              cannot be approved, and nothing is live until a person has approved it.
    The gates test internal consistency on synthetic sessions; they are not evidence of an edge.

    tf reference build --bars FILE --symbology FILE --out FILE [--assets FILE] [--etf-list FILE]
                       [--minutes FILE [--history-sessions N] [--wick-clip PERMILLE]] [--up-to YYYY-MM-DD] [--window N] [--min-days N]

    Builds the reference snapshot a universe is judged from: per symbol the last close, average
    dollar and share volume over the last --window sessions (default 20; needs --min-days of them,
    default 10) and average true range, from daily bars and a symbology file (fetched by
    scripts/fetch_reference.sh). Only bars dated on or before --up-to are read. --assets adds
    Alpaca's tradable, shortable, easy_to_borrow and exchange; --etf-list (a file of symbols you
    keep) adds etf. --minutes adds, from a one-minute bar history of the live feed (scripts/fetch_minutes.sh,
    ending on the same day as the daily bars), the previous session's high, low and close, the 14-session ATR,
    the first-minute, first-five-minute and premarket volume baselines, the cumulative volume at 09:35 to 15:30
    and the state of an EMA(100) over regular-session hourly closes (the last --history-sessions, default 60).
    A minute's high and low are capped at --wick-clip permille (default 5) beyond its open and close, because
    the feed's raw highs and lows carry off-market prints; the build says how many bars that changed.
    A column with no source is left out, so a universe or strategy that needs it refuses.

    tf history index DIR --dataset NAME --schema NAME [--symbols LIST]
    tf history verify DIR
    tf history show DIR

    The research history store: one zstd DBN file per day and schema under DIR/<dataset>/<schema>/<date>.dbn.zst,
    pulled by scripts/pull_history.sh (which asks the cost first and refuses above a cap), and a manifest. `index`
    builds the manifest from the files (size, SHA-256, records, symbols, cost) and records what the store is not
    (borrow flags are not point in time; whether names that left are present). `verify` rereads every file and names
    any that is missing, short, altered or not listed (and exits non-zero). `show` prints the symbols per day.

    tf universe show SPEC [--snapshot FILE]
    tf universe members SPEC --snapshot FILE [--out FILE]
    tf universe diff OLD NEW [--snapshot FILE]

    A universe spec says which symbols a strategy watches: static conditions on a reference
    snapshot (CSV, `# as_of DATE`, one row per symbol), then optionally a live top-N with
    hysteresis. `show` prints the canonical text and fingerprint; `members` lists who passes the
    static conditions (and refuses when the snapshot lacks a column the spec uses); `diff` lists
    what changed between two specs and, with a snapshot, which symbols that admits or drops.

    tf budgets propose DIR --by NAME --reason TEXT --evidence TEXT --tree FILE [--at NANOS]
                       [--step-bp N] [--cooldown-secs N]
    tf budgets proposals DIR
    tf budgets approve|decline DIR ID --by NAME [--note TEXT] [--at NANOS]

    An agent proposes a budget tree (the text form) with its reason and evidence. A change that
    only reduces risk (a share cut, loss limits tightened) by at most --step-bp (default 1000)
    on a node not changed within --cooldown-secs (default a day) is queued on its own; anything
    that raises a share or loosens a limit waits for a person (`approve` or `decline`); an
    increase for a strategy in drawdown, or anything the rules do not allow, is refused and this
    exits with an error. Nothing writes the ledger: what is queued goes in its inbox
    (`tf ledger apply-inbox`) and takes effect at the next rebalance.

    tf catalog [--store DIR] [--ledger DIR --kind paper|live] [--strategy NAME] [--latest]

    List the runs of each strategy, newest first, with backtest, paper and live kept apart:
    backtests from a run store, and one paper or live session per trading day of an order
    ledger (the ledger does not say which, so --kind does). A session's P&L is the change in
    the strategy's realised profit that day; trades are the orders that opened a position and
    got a fill. Only stored backtests can be opened with `tf explore`. --latest shows only
    each strategy's newest run. Read-only.

    tf serve --token-file FILE [--ledger DIR --kind paper|live] [--store DIR] [--addr HOST:PORT]

    Serve the workspace read-only over HTTP (default 127.0.0.1:8787): /api/overview (balance,
    groups, strategies with budget, use, day P&L, loss limits and state), /api/runs[?strategy=S],
    /health. The ledger is read without its lock, so it works while an engine writes it. Sign in
    with the token in FILE (at least 16 letters, digits, - or _) as `Authorization: Bearer ...`,
    or on the login page at /. Nothing it serves can place an order or change a budget. It speaks
    plain HTTP: put TLS and a real identity provider in front of it before exposing it.

    tf ledger apply-inbox DIR [--at NANOS]

    Record the budget changes people have requested (through the workspace app) as scheduled
    changes in the ledger, which take effect at the next rebalance. Each request is checked again
    against the budgets in force and what each strategy has in use now; one that no longer fits is
    refused with the reason and kept as `.rej`. Takes the ledger's lock, so it refuses while an
    engine is writing it (an engine applies its own inbox).

    tf ledger verify DIR [--orders]

    Replay an order ledger (the append-only log the risk gateway and order book write) and
    show what it holds: records, orders and which are open, positions, refusals by reason,
    realised P&L, the kill switch. Every recorded decision is re-decided and must come out the
    same, or this fails and says which record differs. A torn last record is repaired and
    reported, as a restart would; damage anywhere else is refused. It takes the ledger's lock,
    so it refuses while an engine is writing the ledger.

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
                Event::ParamChange(p) => println!(
                    "{:>16} param #{} -> {} ({:?}, proposer {}, reason {}, evidence {:#x})",
                    p.hdr.ts_recv, p.param, p.new_value, p.scope, p.proposer, p.reason, p.evidence
                ),
                Event::TierChange(t) => println!(
                    "{:>16} {name} tier {:?} (reason {}, score {})",
                    t.hdr.ts_recv, t.action, t.reason, t.score
                ),
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

#[derive(Debug)]
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
    propose: Vec<(String, i64, u64)>,
    export: Option<String>,
    rules: Option<String>,
    revert_drawdown: Option<u64>,
    lockout: u64,
    strategy: String,
    lead: Option<u64>,
    fast: u32,
    slow: u32,
    store: Option<String>,
}

fn parse_backtest(args: &[String]) -> Result<BacktestArgs, String> {
    let mut a = BacktestArgs {
        seed: 1,
        secs: 0,
        healthy: 2,
        dangerous: 2,
        quiet: 2,
        latency_ms: 50,
        borrow_bps: 0,
        order_notional: 5_000,
        daily_loss: 1_000,
        max_orders: 20,
        higher_lows: 1,
        propose: Vec::new(),
        export: None,
        rules: None,
        revert_drawdown: None,
        lockout: 300,
        strategy: "momentum".to_owned(),
        lead: None,
        fast: 3,
        slow: 6,
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
            "--lead" => a.lead = Some(val("--lead")?),
            "--rules" => a.rules = Some(it.next().cloned().ok_or("--rules needs a file")?),
            "--export" => a.export = Some(it.next().cloned().ok_or("--export needs a file")?),
            "--revert-drawdown" => a.revert_drawdown = Some(val("--revert-drawdown")?),
            "--lockout" => a.lockout = val("--lockout")?,
            "--propose" => {
                let v = it.next().ok_or("--propose needs NAME=VALUE@SECS")?;
                a.propose.push(parse_proposal(v)?);
            }
            "--fast" => a.fast = val("--fast")? as u32,
            "--slow" => a.slow = val("--slow")? as u32,
            "--strategy" => a.strategy = it.next().cloned().ok_or("--strategy needs a value")?,
            "--store" => a.store = Some(it.next().cloned().ok_or("--store needs a value")?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if !matches!(a.strategy.as_str(), "momentum" | "trend") {
        return Err(format!(
            "unknown strategy {:?} (momentum or trend)",
            a.strategy
        ));
    }
    let trend = a.strategy == "trend";
    if a.revert_drawdown.is_some() && a.propose.is_empty() {
        return Err(
            "--revert-drawdown needs at least one --propose (it watches a tuned side)".to_owned(),
        );
    }
    if a.revert_drawdown == Some(0) {
        return Err("--revert-drawdown must be positive".to_owned());
    }
    if a.rules.is_some() && trend {
        return Err("--rules is for the momentum strategy".to_owned());
    }
    if a.export.is_some() && (trend || !a.propose.is_empty()) {
        return Err(
            "--export is for a single momentum run (no --strategy trend, no --propose)".to_owned(),
        );
    }
    if trend && !a.propose.is_empty() {
        return Err(
            "--propose applies to the momentum strategy; trend has no tunable parameters yet"
                .to_owned(),
        );
    }
    if a.secs == 0 {
        a.secs = if trend { 1800 } else { 400 };
    }
    a.lead.get_or_insert(if trend {
        420
    } else {
        tf_backtest::DEMO_LEAD_SECS
    });
    Ok(a)
}

/// `NAME=VALUE@SECS`.
fn parse_proposal(v: &str) -> Result<(String, i64, u64), String> {
    let bad = || format!("--propose {v:?}: expected NAME=VALUE@SECS");
    let (name_value, secs) = v.rsplit_once('@').ok_or_else(bad)?;
    let (name, value) = name_value.split_once('=').ok_or_else(bad)?;
    let value = value
        .parse::<i64>()
        .map_err(|e| format!("--propose {v:?}: value: {e}"))?;
    let secs = secs
        .parse::<u64>()
        .map_err(|e| format!("--propose {v:?}: seconds: {e}"))?;
    if name.is_empty() {
        return Err(bad());
    }
    Ok((name.to_owned(), value, secs))
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
    let rules = match &a.rules {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Some(tf_strategy::RuleSet::parse(&text).map_err(|e| format!("{path}: {e}"))?)
        }
        None => None,
    };
    Ok(tf_backtest::BacktestConfig {
        rules,
        sim: tf_strategy::SimConfig {
            latency_ns: a.latency_ms * 1_000_000,
            borrow_bps_per_year: a.borrow_bps,
        },
        limits,
        params: tf_strategy::MomentumParams {
            min_higher_lows: a.higher_lows,
            ..tf_strategy::MomentumParams::default()
        },
        trend: tf_strategy::TrendParams {
            fast_period: a.fast,
            slow_period: a.slow,
            ..tf_strategy::TrendParams::default()
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
        ("strategy", a.strategy.clone()),
        ("secs", a.secs.to_string()),
        (
            "lead_secs",
            a.lead.unwrap_or(tf_backtest::DEMO_LEAD_SECS).to_string(),
        ),
        ("ab", u8::from(!a.propose.is_empty()).to_string()),
        ("healthy", a.healthy.to_string()),
        ("dangerous", a.dangerous.to_string()),
        ("quiet", a.quiet.to_string()),
        ("latency_ns", cfg.sim.latency_ns.to_string()),
        ("borrow_bps", cfg.sim.borrow_bps_per_year.to_string()),
    ];
    for (k, v) in session {
        m = m.with_config(k, &v).map_err(|e| e.to_string())?;
    }
    if let Some(r) = &cfg.rules {
        m = m
            .with_config("rules", &format!("{:016x}", r.fingerprint()))
            .map_err(|e| e.to_string())?;
    }
    for (k, v) in cfg.limits.pairs() {
        m = m
            .with_config(&format!("limit_{k}"), &v)
            .map_err(|e| e.to_string())?;
    }
    let params = if a.strategy == "trend" {
        cfg.trend.pairs()
    } else {
        cfg.params.pairs()
    };
    for (k, v) in params {
        m = m.with_param(k, &v).map_err(|e| e.to_string())?;
    }
    if let Some(d) = a.revert_drawdown {
        m = m
            .with_config("revert_drawdown_usd", &d.to_string())
            .and_then(|m| m.with_config("lockout_secs", &a.lockout.to_string()))
            .map_err(|e| e.to_string())?;
    }
    for (i, (name, value, secs)) in a.propose.iter().enumerate() {
        m = m
            .with_config(&format!("propose_{i}"), &format!("{name}={value}@{secs}"))
            .map_err(|e| e.to_string())?;
    }
    Ok(m)
}

fn backtest_single(
    a: &BacktestArgs,
    cfg: &tf_backtest::BacktestConfig,
    events: &[Event],
    labels: Vec<String>,
) -> Result<Vec<(String, i64)>, String> {
    let r = if a.strategy == "trend" {
        tf_backtest::trend_backtest(events.iter().copied(), labels, cfg)?
    } else if let Some(path) = &a.export {
        let traced =
            tf_backtest::momentum_backtest_traced(events.iter().copied(), labels.clone(), cfg)?;
        let json = tf_backtest::export::export_json(
            events,
            &labels,
            cfg,
            &traced.result,
            &traced.entries,
            &traced.declines,
            &tf_backtest::export::ExportMeta {
                strategy: &a.strategy,
                seed: a.seed,
                secs: a.secs,
            },
        );
        let out = if path.ends_with(".html") {
            tf_backtest::export::page(&json)
        } else {
            json
        };
        std::fs::write(path, &out).map_err(|e| format!("{path}: {e}"))?;
        println!("exported {path} ({} bytes)", out.len());
        traced.result
    } else {
        tf_backtest::momentum_backtest(events.iter().copied(), labels, cfg)?
    };
    println!(
        "{} | seed {} | {} healthy, {} dangerous, {} quiet | {} s simulated | latency {} ms",
        a.strategy, a.seed, a.healthy, a.dangerous, a.quiet, a.secs, a.latency_ms
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
    Ok(momentum_metrics(&r))
}

/// The integers a single backtest stores as its result.
fn momentum_metrics(r: &tf_backtest::BacktestResult) -> Vec<(String, i64)> {
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
    metrics
}

fn backtest_ab(
    a: &BacktestArgs,
    cfg: &tf_backtest::BacktestConfig,
    events: &[Event],
    labels: Vec<String>,
    start: u64,
) -> Result<Vec<(String, i64)>, String> {
    use tf_backtest::ab::{RevertConfig, Scheduled, momentum_ab_with};
    let params = tf_params::ParamStore::new(tf_strategy::tunable_specs(&cfg.params))
        .map_err(|e| format!("{e:?}"))?;
    let mut scheduled = Vec::new();
    for (name, value, secs) in &a.propose {
        let id = params.id_of(name).ok_or_else(|| {
            let names: Vec<&str> = params.specs().iter().map(|s| s.name).collect();
            format!("unknown parameter {name:?}; tunable: {}", names.join(", "))
        })?;
        scheduled.push(Scheduled {
            at: start + secs * NANOS_PER_SEC,
            proposal: tf_params::Proposal {
                param: id,
                target: tf_params::Target::Global,
                value: *value,
                proposer: 1,
                reason: 0,
                evidence: 0,
            },
        });
    }
    let revert = a.revert_drawdown.map(|d| RevertConfig {
        max_drawdown: tf_backtest::dollars(d),
        lockout: a.lockout * NANOS_PER_SEC,
    });
    let r = momentum_ab_with(events.iter().copied(), labels, cfg, &scheduled, revert)?;
    println!(
        "momentum A/B | seed {} | {} healthy, {} dangerous, {} quiet | {} s simulated | {} proposals",
        a.seed,
        a.healthy,
        a.dangerous,
        a.quiet,
        a.secs,
        scheduled.len()
    );
    println!("--- tuned");
    print!("{}", r.tuned.report.render());
    println!("--- shadow (fixed parameters, same feed)");
    print!("{}", r.shadow.report.render());
    let c = &r.comparison;
    let usd = |raw: i128| {
        let cents = (raw.abs() + 5_000_000) / 10_000_000;
        let sign = if raw < 0 && cents != 0 { '-' } else { '+' };
        format!("{sign}{}.{:02}", cents / 100, cents % 100)
    };
    println!("--- tuned minus shadow");
    println!(
        "net {}  realised {}  drawdown {}  slippage {}  trades {:+}  shares {:+}",
        usd(c.pnl_net),
        usd(c.pnl_realized),
        usd(c.max_drawdown),
        usd(c.slippage_cost),
        c.trades,
        c.fills
    );
    for (label, d) in &c.pnl_net_by_label {
        println!("  {label:<12} {}", usd(*d));
    }
    println!("--- parameters");
    let name = |p: u16| params.specs().get(usize::from(p)).map_or("?", |s| s.name);
    for ch in &r.changes {
        println!(
            "applied  +{:>4} s  {} {} -> {}",
            (ch.ts - start) / NANOS_PER_SEC,
            name(ch.param),
            ch.old,
            ch.new
        );
    }
    for rv in &r.reverts {
        println!(
            "REVERT   +{:>4} s  {} parameter(s) back to baseline: {} behind its best (limit ${})",
            (rv.at - start) / NANOS_PER_SEC,
            rv.parameters,
            usd(-(rv.trip.drawdown as i128)),
            a.revert_drawdown.unwrap_or(0)
        );
    }
    for rf in &r.refused {
        println!(
            "refused  +{:>4} s  {} = {}: {:?}",
            (rf.at - start) / NANOS_PER_SEC,
            name(rf.proposal.param),
            rf.proposal.value,
            rf.why
        );
    }
    let books = r.tuned.books_agree() && r.shadow.books_agree();
    println!(
        "books    {} | bookkeeping errors {} + {}",
        if books { "agree" } else { "DISAGREE" },
        r.tuned.bookkeeping_errors,
        r.shadow.bookkeeping_errors
    );
    println!(
        "outcome  tuned {:#x}  shadow {:#x}",
        r.tuned.outcome_hash, r.shadow.outcome_hash
    );
    let mut metrics = r.metrics();
    metrics.push(("books_agree".into(), i64::from(books)));
    Ok(metrics)
}

fn backtest(args: &[String]) -> Result<(), String> {
    let a = parse_backtest(args)?;
    let cfg = backtest_config(&a)?;
    let (events, labels) = tf_backtest::demo_session_with_lead(
        a.seed,
        a.secs,
        a.healthy,
        a.dangerous,
        a.quiet,
        a.lead.unwrap_or(tf_backtest::DEMO_LEAD_SECS),
    );
    let mut hash = HashSink::new();
    for ev in &events {
        hash.on_event(ev);
    }
    let start = events.first().map_or(0, |e| e.ts_recv());
    let manifest = backtest_manifest(&a, &cfg, start)?;
    let store = a.store.as_deref().map(DirStore::new);
    // An export needs the run itself, so it does not use a cached result.
    if let (Some(store), None) = (&store, &a.export) {
        if let Some(r) = store.get(&manifest).map_err(|e| e.to_string())? {
            println!("cached   {}  (not rerun)", manifest.hash());
            println!("events   {:>12}", r.events);
            for (k, v) in r.metrics() {
                println!("{k:<34} {v:>16}");
            }
            return Ok(());
        }
    }

    let metrics = if a.propose.is_empty() {
        backtest_single(&a, &cfg, &events, labels)?
    } else {
        backtest_ab(&a, &cfg, &events, labels, start)?
    };

    if let Some(store) = store {
        let key = manifest.hash();
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
        Some("explore") => explore::explore(&args[1..]),
        Some("runs") => runs::runs(&args[1..]),
        Some("rules") => rules_cmd::rules(&args[1..]),
        Some("budgets") => budgets_cmd::budgets(&args[1..]),
        Some("universe") => universe_cmd::universe(&args[1..]),
        Some("reference") => reference_cmd::reference(&args[1..]),
        Some("history") => history_cmd::history(&args[1..]),
        Some("serve") => serve_cmd::serve_cmd(&args[1..]),
        Some("catalog") => catalog_cmd::catalog(&args[1..]),
        Some("ledger") => ledger_cmd::ledger(&args[1..]),
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
            &["--strategy", "trend"],
            &["--lead", "100"],
            &["--propose", "min_higher_lows=2@1"],
            &[
                "--propose",
                "min_higher_lows=2@1",
                "--revert-drawdown",
                "100",
            ],
        ] {
            assert_ne!(
                base,
                hash(flags),
                "{flags:?} must change the key or a stale result would be reused"
            );
        }
    }

    #[test]
    fn every_trend_setting_is_part_of_its_manifest() {
        let base = hash(&["--strategy", "trend"]);
        assert_eq!(base, hash(&["--strategy", "trend"]));
        for flags in [
            &["--fast", "4"][..],
            &["--slow", "7"],
            &["--secs", "1700"],
            &["--lead", "400"],
            &["--seed", "2"],
            &["--daily-loss", "999"],
        ] {
            let mut a = vec!["--strategy", "trend"];
            a.extend_from_slice(flags);
            assert_ne!(base, hash(&a), "{flags:?}");
        }
        // The momentum-only flag does not matter to a trend run (and is not recorded for it).
        assert_eq!(base, hash(&["--strategy", "trend", "--higher-lows", "0"]));
    }

    #[test]
    fn proposals_are_part_of_the_manifest_in_value_time_and_order() {
        let one = hash(&["--propose", "min_higher_lows=2@1"]);
        assert_eq!(one, hash(&["--propose", "min_higher_lows=2@1"]));
        for flags in [
            &["--propose", "min_higher_lows=2@2"][..],
            &["--propose", "min_higher_lows=3@1"],
            &["--propose", "trail_permille=20@1"],
            &[
                "--propose",
                "min_higher_lows=2@1",
                "--propose",
                "min_higher_lows=3@62",
            ],
        ] {
            assert_ne!(one, hash(flags), "{flags:?}");
        }
        assert_ne!(
            hash(&[
                "--propose",
                "min_higher_lows=2@1",
                "--propose",
                "trail_permille=20@1"
            ]),
            hash(&[
                "--propose",
                "trail_permille=20@1",
                "--propose",
                "min_higher_lows=2@1"
            ]),
            "the order they are listed in is part of the setup"
        );
    }

    #[test]
    fn the_revert_policy_is_part_of_the_manifest_and_needs_a_tuned_side() {
        let base = hash(&[
            "--propose",
            "min_higher_lows=2@1",
            "--revert-drawdown",
            "100",
        ]);
        for flags in [
            &[
                "--propose",
                "min_higher_lows=2@1",
                "--revert-drawdown",
                "101",
            ][..],
            &[
                "--propose",
                "min_higher_lows=2@1",
                "--revert-drawdown",
                "100",
                "--lockout",
                "60",
            ],
        ] {
            assert_ne!(base, hash(flags), "{flags:?}");
        }
        let args =
            |v: &[&str]| parse_backtest(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert!(
            args(&["--revert-drawdown", "100"]).is_err(),
            "nothing is tuned"
        );
        assert!(args(&["--propose", "x=1@1", "--revert-drawdown", "0"]).is_err());
    }

    #[test]
    fn proposals_parse_strictly() {
        assert_eq!(
            parse_proposal("trail_permille=20@60"),
            Ok(("trail_permille".to_owned(), 20, 60))
        );
        assert_eq!(parse_proposal("x=-5@0"), Ok(("x".to_owned(), -5, 0)));
        for bad in ["", "x", "x=1", "x@1", "=1@1", "x=a@1", "x=1@a", "x=1@-1"] {
            assert!(parse_proposal(bad).is_err(), "{bad:?}");
        }
        let args =
            |v: &[&str]| parse_backtest(&v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert!(
            args(&["--strategy", "trend", "--propose", "x=1@1"]).is_err(),
            "trend has nothing to tune yet"
        );
        assert!(args(&["--propose"]).is_err());
    }

    #[test]
    fn bad_backtest_arguments_are_refused() {
        assert!(parse_backtest(&["--strategy".to_owned(), "nope".to_owned()]).is_err());
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
