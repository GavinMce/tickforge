//! `tf live`: certify a strategy set on a stored day, check the gateway, and run a live day (E18-S13).
//!
//! The day itself is `tf_driver::run` (ADR 0050): this command is everything around it that a person would otherwise do by hand
//! at four in the morning, written down as a config file so that the same thing is done every day:
//!
//! ```text
//! live config v1
//! dataset EQUS.MINI
//! subscribe tbbo ALL_SYMBOLS
//! key_env DATABENTO_API_KEY
//! set month.set
//! snapshots snapshots
//! certificates certificates.txt
//! dir live
//! ```
//!
//! Paths are relative to the config's directory. `dir` holds the ledger (`dir/ledger`, one file ledger that continues from day to
//! day) and a directory for each day (`dir/YYYY-MM-DD`: the raw capture, the decision log, the report).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tf_calendar::{Calendar, Date, SessionTimes};
use tf_core::Nanos;
use tf_driver::{DriverConfig, Outcome, Reconnect, Warmup};
use tf_host::set::StrategySet;
use tf_host::{Certificate, StrategyDef, certify_files};
use tf_ledger::FileStore;
use tf_live::{ApiKey, Config as GatewayConfig, LiveProvider, Sub, Symbols};
use tf_provider::Provider;
use tf_universe::Snapshot;

pub(crate) const USAGE: &str = "usage:
    tf live certify --set FILE --store DIR --dataset NAME --schema NAME --date DATE --snapshots DIR --out FILE [--id-space N]
    tf live check --config FILE [--seconds N]
    tf live run --config FILE [--date DATE] [--start-at HH:MM]";

const HEADER: &str = "live config v1";

/// A live config, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveConfig {
    pub dataset: String,
    /// Each subscription: a schema and the symbols (none listed is all of them).
    pub subs: Vec<(String, Vec<String>)>,
    pub key_env: String,
    /// `host:port` of the gateway if not the dataset's own.
    pub gateway: Option<String>,
    pub set: PathBuf,
    pub snapshots: PathBuf,
    pub certificates: PathBuf,
    pub dir: PathBuf,
    /// Ask the gateway to replay from the start of the premarket, for a start later than that.
    pub replay_from_premarket: bool,
    /// End the day at the regular close (`close`) or at the end of after-hours trading, 20:00 (`after_hours`, the default).
    pub end_at_regular_close: bool,
    pub warmup_quiet_secs: u64,
    pub warmup_max_secs: u64,
    pub segment_secs: u64,
    pub reconnects: u32,
    pub id_space: usize,
}

fn word<'a>(n: usize, w: &[&'a str], what: &str) -> Result<&'a str, String> {
    match w {
        [_, v] => Ok(v),
        _ => Err(format!("line {n}: `{what}` takes one value")),
    }
}

fn number(n: usize, w: &[&str], what: &str) -> Result<u64, String> {
    word(n, w, what)?
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or(format!("line {n}: {what} is a whole number above zero"))
}

impl LiveConfig {
    pub(crate) fn parse(text: &str, base: &Path) -> Result<LiveConfig, String> {
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l.trim()))
            .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'));
        if lines.next().map(|(_, l)| l) != Some(HEADER) {
            return Err(format!("the first line must be `{HEADER}`"));
        }
        let mut c = LiveConfig {
            dataset: String::new(),
            subs: Vec::new(),
            key_env: "DATABENTO_API_KEY".to_owned(),
            gateway: None,
            set: PathBuf::new(),
            snapshots: PathBuf::new(),
            certificates: PathBuf::new(),
            dir: PathBuf::new(),
            replay_from_premarket: false,
            end_at_regular_close: false,
            warmup_quiet_secs: 5,
            warmup_max_secs: 60,
            segment_secs: 300,
            reconnects: 20,
            id_space: 16_384,
        };
        let mut seen = std::collections::BTreeSet::new();
        for (n, line) in lines {
            let w: Vec<&str> = line.split_whitespace().collect();
            let key = w[0];
            if key != "subscribe" && !seen.insert(key.to_owned()) {
                return Err(format!("line {n}: `{key}` is given twice"));
            }
            match key {
                "dataset" => c.dataset = word(n, &w, key)?.to_owned(),
                "subscribe" => {
                    let (Some(schema), Some(symbols)) = (w.get(1), w.get(2)) else {
                        return Err(format!(
                            "line {n}: `subscribe SCHEMA SYMBOLS` (ALL_SYMBOLS or a comma-separated list)"
                        ));
                    };
                    if w.len() != 3 {
                        return Err(format!("line {n}: `subscribe SCHEMA SYMBOLS`"));
                    }
                    let list = if *symbols == "ALL_SYMBOLS" {
                        Vec::new()
                    } else {
                        symbols.split(',').map(str::to_owned).collect()
                    };
                    if list.iter().any(String::is_empty) {
                        return Err(format!("line {n}: a symbol is empty"));
                    }
                    c.subs.push(((*schema).to_owned(), list));
                }
                "key_env" => c.key_env = word(n, &w, key)?.to_owned(),
                "gateway" => c.gateway = Some(word(n, &w, key)?.to_owned()),
                "set" => c.set = base.join(word(n, &w, key)?),
                "snapshots" => c.snapshots = base.join(word(n, &w, key)?),
                "certificates" => c.certificates = base.join(word(n, &w, key)?),
                "dir" => c.dir = base.join(word(n, &w, key)?),
                "replay_from_premarket" => {
                    c.replay_from_premarket = match word(n, &w, key)? {
                        "yes" => true,
                        "no" => false,
                        _ => return Err(format!("line {n}: replay_from_premarket is yes or no")),
                    }
                }
                "end" => {
                    c.end_at_regular_close = match word(n, &w, key)? {
                        "close" => true,
                        "after_hours" => false,
                        _ => return Err(format!("line {n}: end is close or after_hours")),
                    }
                }
                "warmup_quiet_secs" => c.warmup_quiet_secs = number(n, &w, key)?,
                "warmup_max_secs" => c.warmup_max_secs = number(n, &w, key)?,
                "segment_secs" => c.segment_secs = number(n, &w, key)?,
                "reconnects" => {
                    c.reconnects = u32::try_from(number(n, &w, key)?)
                        .map_err(|_| format!("line {n}: too many"))?
                }
                "id_space" => {
                    c.id_space = usize::try_from(number(n, &w, key)?)
                        .map_err(|_| format!("line {n}: too many"))?
                }
                other => {
                    return Err(format!(
                        "line {n}: `{other}` is not a setting of a live config"
                    ));
                }
            }
        }
        for (name, empty) in [
            ("dataset", c.dataset.is_empty()),
            ("subscribe", c.subs.is_empty()),
            ("set", c.set.as_os_str().is_empty()),
            ("snapshots", c.snapshots.as_os_str().is_empty()),
            ("certificates", c.certificates.as_os_str().is_empty()),
            ("dir", c.dir.as_os_str().is_empty()),
        ] {
            if empty {
                return Err(format!("`{name}` is missing"));
            }
        }
        if c.warmup_quiet_secs > c.warmup_max_secs {
            return Err("warmup_quiet_secs is longer than warmup_max_secs".to_owned());
        }
        Ok(c)
    }

    pub(crate) fn load(path: &Path) -> Result<LiveConfig, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        LiveConfig::parse(&text, path.parent().unwrap_or_else(|| Path::new(".")))
    }

    /// What to ask the gateway for; `start` is the replay start, if any.
    fn subs(&self, start: Option<Nanos>) -> Vec<Sub> {
        self.subs
            .iter()
            .map(|(schema, symbols)| Sub {
                schema: schema.clone(),
                stype_in: "raw_symbol".to_owned(),
                symbols: if symbols.is_empty() {
                    Symbols::All
                } else {
                    Symbols::List(symbols.clone())
                },
                start,
                snapshot: false,
            })
            .collect()
    }

    fn gateway_config(&self, key: ApiKey, start: Option<Nanos>) -> GatewayConfig {
        let mut g = GatewayConfig::new(key, &self.dataset, self.subs(start));
        g.addr = self.gateway.clone();
        g
    }
}

fn read_key(env_name: &str) -> Result<ApiKey, String> {
    let v = std::env::var(env_name).map_err(|_| {
        format!("the key is read from the environment variable {env_name}, which is not set")
    })?;
    ApiKey::new(&v).map_err(|e| format!("{env_name}: {e:?}"))
}

fn date_of(s: &str) -> Result<Date, String> {
    let (y, m, d) = (
        s.get(..4).and_then(|x| x.parse::<i32>().ok()),
        s.get(5..7).and_then(|x| x.parse::<u8>().ok()),
        s.get(8..10).and_then(|x| x.parse::<u8>().ok()),
    );
    match (y, m, d, s.len()) {
        (Some(y), Some(m), Some(d), 10) if s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' => {
            Date::new(y, m, d).ok_or(format!("{s} is not a date"))
        }
        _ => Err(format!("{s}: expected YYYY-MM-DD")),
    }
}

fn times_of(date: Date) -> Result<SessionTimes, String> {
    Calendar::us_equities()
        .times(date)
        .map_err(|e| format!("the calendar does not cover {date}: {e:?}"))?
        .ok_or(format!("the market is closed on {date}"))
}

fn snapshot_for(dir: &Path, date: &str) -> Result<Snapshot, String> {
    let path = dir.join(format!("{date}.snapshot"));
    let text = fs::read_to_string(&path).map_err(|e| {
        format!(
            "no reference snapshot for {date}: {}: {e} (`tf research snapshots` makes them)",
            path.display()
        )
    })?;
    let snap = Snapshot::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if snap.as_of.as_str() >= date {
        return Err(format!(
            "the snapshot for {date} is as of {}: it must be from before the day, or the strategies would know the day",
            snap.as_of
        ));
    }
    Ok(snap)
}

/// The certificates file: `certificates v1`, then a line `NUMBER NAME CERTIFICATE` for each strategy.
fn render_certificates(certs: &[(u16, String, Certificate)]) -> String {
    let mut s = String::from("certificates v1\n");
    for (id, name, c) in certs {
        s.push_str(&format!("{id} {name} {}\n", c.to_text()));
    }
    s
}

fn parse_certificates(text: &str) -> Result<Vec<(u16, String, Certificate)>, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    if lines.next() != Some("certificates v1") {
        return Err("the first line must be `certificates v1`".to_owned());
    }
    lines
        .enumerate()
        .map(|(i, l)| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let [id, name, cert] = w.as_slice() else {
                return Err(format!("line {}: `NUMBER NAME CERTIFICATE`", i + 2));
            };
            Ok((
                id.parse::<u16>()
                    .map_err(|_| format!("line {}: `{id}` is not a strategy number", i + 2))?,
                (*name).to_owned(),
                Certificate::from_text(cert).map_err(|e| format!("line {}: {e}", i + 2))?,
            ))
        })
        .collect()
}

/// Each definition with the certificate in `text` for its number and its current fingerprint; the ones with none are named.
fn admit(defs: Vec<StrategyDef>, text: &str) -> Result<Vec<(StrategyDef, Certificate)>, String> {
    let certs = parse_certificates(text)?;
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for d in defs {
        match certs
            .iter()
            .find(|(id, _, c)| *id == d.id && c.strategy_fp == d.fingerprint())
        {
            Some((_, _, c)) => out.push((d, c.clone())),
            None => missing.push(format!("{} ({})", d.name, d.id)),
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "no certificate for the current definition of: {} (`tf live certify` makes them, and again after any change of parameters, universe or priority)",
            missing.join(", ")
        ))
    }
}

pub(crate) fn live(args: &[String]) -> Result<(), String> {
    print!("{}", dispatch(args)?);
    Ok(())
}

pub(crate) fn dispatch(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("certify") => certify(&args[1..]),
        Some("check") => check(&args[1..]),
        Some("run") => run_cmd(&args[1..]),
        _ => Err(USAGE.to_owned()),
    }
}

fn flag_values(args: &[String], known: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if !known.contains(&a.as_str()) {
            return Err(format!("unexpected argument {a}\n{USAGE}"));
        }
        let v = it.next().ok_or(format!("{a} needs a value"))?;
        if out.iter().any(|(k, _)| k == a) {
            return Err(format!("{a} is given twice"));
        }
        out.push((a.clone(), v.clone()));
    }
    Ok(out)
}

fn get<'a>(f: &'a [(String, String)], k: &str) -> Option<&'a str> {
    f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn need<'a>(f: &'a [(String, String)], k: &str) -> Result<&'a str, String> {
    get(f, k).ok_or(format!("{k} is required\n{USAGE}"))
}

fn certify(args: &[String]) -> Result<String, String> {
    let f = flag_values(
        args,
        &[
            "--set",
            "--store",
            "--dataset",
            "--schema",
            "--date",
            "--snapshots",
            "--out",
            "--id-space",
        ],
    )?;
    let date_text = need(&f, "--date")?;
    let date = date_of(date_text)?;
    let times = times_of(date)?;
    let id_space = match get(&f, "--id-space") {
        None => 16_384,
        Some(n) => n
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(format!("--id-space {n}: not a number above zero"))?,
    };
    let (set, defs) =
        StrategySet::load(Path::new(need(&f, "--set")?)).map_err(|e| e.to_string())?;
    let mut cfg = set.host_config(id_space).map_err(|e| e.to_string())?;
    cfg.day = Some(times);
    let store = Path::new(need(&f, "--store")?);
    let (dataset, schema) = (need(&f, "--dataset")?, need(&f, "--schema")?);
    let manifest = tf_history::Store::read(store).map_err(|e| e.to_string())?;
    let day = manifest
        .of(dataset, schema)
        .find(|d| d.date == date_text)
        .ok_or(format!(
            "{dataset} {schema} {date_text} is not in the store"
        ))?;
    let files = tf_history::files(store, dataset, schema, Some(date_text), Some(date_text))
        .map_err(|e| e.to_string())?;
    let tape_id = u64::from_str_radix(day.sha256.get(..16).unwrap_or("0"), 16).unwrap_or(0);
    let snapshot = snapshot_for(Path::new(need(&f, "--snapshots")?), date_text)?;
    let mut certs = Vec::new();
    let mut text = String::new();
    for d in &defs {
        let c = certify_files(d, &cfg, snapshot.clone(), &files, tape_id)
            .map_err(|e| format!("{} ({}) is not certified: {e}", d.name, d.id))?;
        text.push_str(&format!(
            "certified {} ({}): {} events, {} intents, {} accepted\n",
            d.name, d.id, c.events, c.intents, c.accepted
        ));
        certs.push((d.id, d.name.clone(), c));
    }
    let out = need(&f, "--out")?;
    fs::write(out, render_certificates(&certs)).map_err(|e| format!("{out}: {e}"))?;
    text.push_str(&format!(
        "{} certificates written to {out}, from {dataset} {schema} {date_text}\n",
        certs.len()
    ));
    Ok(text)
}

fn check(args: &[String]) -> Result<String, String> {
    let f = flag_values(args, &["--config", "--seconds"])?;
    let cfg = LiveConfig::load(Path::new(need(&f, "--config")?))?;
    let secs = match get(&f, "--seconds") {
        None => 10,
        Some(s) => s
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(format!("--seconds {s}: not a number above zero"))?,
    };
    let key = read_key(&cfg.key_env)?;
    check_gateway(&cfg, key, Duration::from_secs(secs))
}

/// Count one thing off the queue: an event, or a gap marker (the engine saw less than the feed sent).
fn count(d: &tf_ingest::Delivery, events: &mut u64, gaps: &mut u64) {
    match d {
        tf_ingest::Delivery::Event(_) => *events += 1,
        tf_ingest::Delivery::Gap(_) => *gaps += 1,
    }
}

/// Log in, subscribe as the config says, and read for `how_long`, placing nothing and keeping nothing.
pub(crate) fn check_gateway(
    cfg: &LiveConfig,
    key: ApiKey,
    how_long: Duration,
) -> Result<String, String> {
    let mut provider =
        LiveProvider::new(cfg.gateway_config(key, None), tf_ingest::Config::default())
            .map_err(|e| format!("{e:?}"))?;
    provider
        .connect()
        .map_err(|e| format!("could not log in and start the session: {e}"))?;
    let session = provider.session_id().unwrap_or("?").to_owned();
    let start = std::time::Instant::now();
    let (mut events, mut gaps) = (0u64, 0u64);
    while start.elapsed() < how_long {
        if let Some(d) = provider.recv_timeout(Duration::from_millis(100)) {
            count(&d, &mut events, &mut gaps);
        }
        if let Some(shared) = provider.shared() {
            if shared.state() != tf_live::State::Streaming && events == 0 {
                break;
            }
        }
    }
    let (names, state, error) = provider
        .shared()
        .map_or((0, "no session".to_owned(), None), |s| {
            (
                s.names().iter().filter(|n| n.is_some()).count(),
                format!("{:?}", s.state()),
                s.error(),
            )
        });
    let st = provider.stats();
    provider.disconnect();
    let mut s = format!(
        "logged in to {} (session {session}); subscribed to {} for {:.0} s\nstate: {state}{}\n{events} events read, {gaps} gaps, {names} instruments named; the queue offered {}, dropped {} trades and {} quotes\n",
        cfg.dataset,
        cfg.subs
            .iter()
            .map(|(sc, _)| sc.as_str())
            .collect::<Vec<_>>()
            .join(" + "),
        start.elapsed().as_secs_f64(),
        error.map_or(String::new(), |e| format!(" ({e})")),
        st.offered,
        st.dropped_trades,
        st.dropped_quotes,
    );
    if events == 0 {
        s.push_str("nothing arrived: the market may be closed, or the plan's entitlement does not cover this dataset or schema\n");
    }
    Ok(s)
}

/// What a live day came to.
pub(crate) struct LiveDay {
    pub outcome: Outcome,
    pub dir: PathBuf,
    pub text: String,
    pub replay_equal: Option<bool>,
}

/// The first of `date`, `date-2`, `date-3`... that does not exist under `dir`: a restart in the day does not overwrite the
/// morning's log.
fn day_dir(dir: &Path, date: &str) -> PathBuf {
    let first = dir.join(date);
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{date}-{n}")))
        .find(|p| !p.exists())
        .expect("a number is free")
}

fn now_ns() -> Nanos {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as Nanos)
}

fn wait_until(ts: Nanos, stop: &AtomicBool) {
    while now_ns() < ts && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(
            Duration::from_millis(200).min(Duration::from_nanos(ts - now_ns().min(ts))),
        );
    }
}

/// Run one trading day: everything is checked before the gateway is touched, so a refusal costs nothing.
pub(crate) fn live_day(
    cfg: &LiveConfig,
    date_text: &str,
    key: ApiKey,
    stop: Arc<AtomicBool>,
    start_at: Option<Nanos>,
) -> Result<LiveDay, String> {
    let date = date_of(date_text)?;
    let times = times_of(date)?;
    let (set, defs) = StrategySet::load(&cfg.set).map_err(|e| e.to_string())?;
    let certs_text = fs::read_to_string(&cfg.certificates)
        .map_err(|e| format!("{}: {e}", cfg.certificates.display()))?;
    let strategies = admit(defs, &certs_text)?;
    let snapshot = snapshot_for(&cfg.snapshots, date_text)?;
    let mut host = set.host_config(cfg.id_space).map_err(|e| e.to_string())?;
    host.day = Some(times);
    let stop_file = cfg.dir.join("STOP");
    if stop_file.exists() {
        return Err(format!(
            "{} exists: remove it, or the day would end at once",
            stop_file.display()
        ));
    }
    fs::create_dir_all(&cfg.dir).map_err(|e| format!("{}: {e}", cfg.dir.display()))?;
    let store =
        FileStore::open(cfg.dir.join("ledger")).map_err(|e| format!("the ledger: {e:?}"))?;
    let dir = day_dir(&cfg.dir, date_text);
    let replay_start = cfg.replay_from_premarket.then_some(times.premarket);
    let driver = DriverConfig {
        live: cfg.gateway_config(key, replay_start),
        ingest: tf_ingest::Config::default(),
        host,
        snapshot,
        dir: dir.clone(),
        segment_secs: cfg.segment_secs,
        warmup: Warmup {
            quiet: Duration::from_secs(cfg.warmup_quiet_secs),
            max: Duration::from_secs(cfg.warmup_max_secs),
        },
        reconnect: Reconnect {
            max: cfg.reconnects,
            first_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        },
        close_ts: Some(if cfg.end_at_regular_close {
            times.close
        } else {
            times.after_hours_end
        }),
        label: format!("live {date_text}"),
        replay_check: true,
    };
    // A file named STOP in the directory ends the day cleanly: the flag is set, the capture finished and the report written.
    let finished = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (stop, finished) = (stop.clone(), finished.clone());
        std::thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                if stop_file.exists() {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    };
    if let Some(at) = start_at {
        wait_until(at, &stop);
    }
    let result = if stop.load(Ordering::Relaxed) {
        None
    } else {
        Some(tf_driver::run(driver, strategies, store, stop.clone()))
    };
    finished.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    match result {
        None => Err("stopped before the gateway was opened".to_owned()),
        Some(Err(e)) => Err(format!("the day failed: {e}")),
        Some(Ok(day)) => Ok(LiveDay {
            outcome: day.outcome.clone(),
            dir,
            text: day.text.clone(),
            replay_equal: day.replay_equal,
        }),
    }
}

/// The instant `hhmm` (New York time) is on the day whose premarket begins at `premarket`, which is 04:00 there.
pub(crate) fn start_instant(premarket: Nanos, hhmm: &str) -> Result<Nanos, String> {
    let (h, m) = hhmm
        .split_once(':')
        .filter(|(h, m)| h.len() == 2 && m.len() == 2)
        .and_then(|(h, m)| Some((h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)))
        .filter(|(h, m)| (0..24).contains(h) && (0..60).contains(m))
        .ok_or(format!("--start-at {hhmm}: expected HH:MM, New York time"))?;
    let from_premarket_secs = (h * 60 + m - 4 * 60) * 60;
    u64::try_from(i128::from(premarket) + i128::from(from_premarket_secs) * 1_000_000_000)
        .map_err(|_| format!("--start-at {hhmm}: before the epoch"))
}

fn run_cmd(args: &[String]) -> Result<String, String> {
    let f = flag_values(args, &["--config", "--date", "--start-at"])?;
    let cfg = LiveConfig::load(Path::new(need(&f, "--config")?))?;
    let date_text = match get(&f, "--date") {
        Some(d) => d.to_owned(),
        None => {
            let (d, _) = Calendar::us_equities()
                .local(now_ns())
                .map_err(|e| format!("{e:?}"))?;
            d.to_string()
        }
    };
    let times = times_of(date_of(&date_text)?)?;
    let start_at = get(&f, "--start-at")
        .map(|hhmm| start_instant(times.premarket, hhmm))
        .transpose()?;
    let key = read_key(&cfg.key_env)?;
    run_day(&cfg, &date_text, key, start_at)
}

/// Run the day and say how it went: an error if it did not close normally.
pub(crate) fn run_day(
    cfg: &LiveConfig,
    date_text: &str,
    key: ApiKey,
    start_at: Option<Nanos>,
) -> Result<String, String> {
    let stop = Arc::new(AtomicBool::new(false));
    summary(live_day(cfg, date_text, key, stop, start_at)?)
}

fn summary(day: LiveDay) -> Result<String, String> {
    let mut s = format!("{}\nfiles in {}\n", day.text, day.dir.display());
    if day.replay_equal == Some(false) {
        s.push_str("THE REPLAY OF THE CAPTURE DID NOT REPRODUCE THE DAY'S DECISIONS\n");
    }
    match day.outcome {
        Outcome::Closed => Ok(s),
        other => Err(format!("{s}the day did not close normally: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_live::testing::{Conn, Then, gateway, key, market_records, stream_of};

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-live-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    const SEC: Nanos = 1_000_000_000;
    const DAY: &str = "2026-05-04";

    fn config_text(extra: &str) -> String {
        format!(
            "live config v1\ndataset XNAS.BASIC\nsubscribe tcbbo ALL_SYMBOLS\nsubscribe status ALL_SYMBOLS\nset month.set\nsnapshots snaps\ncertificates certs.txt\ndir live\n{extra}"
        )
    }

    #[test]
    fn a_config_names_what_to_subscribe_to_and_where_everything_is() {
        let base = Path::new("/etc/tf");
        let c = LiveConfig::parse(&config_text("# a comment\nkey_env MY_KEY\ngateway 127.0.0.1:9\nreplay_from_premarket yes\nend close\nwarmup_quiet_secs 2\nwarmup_max_secs 30\nsegment_secs 60\nreconnects 3\nid_space 4096\n"), base).unwrap();
        assert_eq!(
            (c.dataset.as_str(), c.key_env.as_str(), c.gateway.as_deref()),
            ("XNAS.BASIC", "MY_KEY", Some("127.0.0.1:9"))
        );
        assert_eq!(
            c.subs,
            [("tcbbo".to_owned(), vec![]), ("status".to_owned(), vec![])]
        );
        assert_eq!(
            (
                c.set.as_path(),
                c.snapshots.as_path(),
                c.certificates.as_path(),
                c.dir.as_path()
            ),
            (
                Path::new("/etc/tf/month.set"),
                Path::new("/etc/tf/snaps"),
                Path::new("/etc/tf/certs.txt"),
                Path::new("/etc/tf/live")
            )
        );
        assert!(c.replay_from_premarket && c.end_at_regular_close);
        assert_eq!(
            (
                c.warmup_quiet_secs,
                c.warmup_max_secs,
                c.segment_secs,
                c.reconnects,
                c.id_space
            ),
            (2, 30, 60, 3, 4096)
        );
        // The defaults of what is left out.
        let d = LiveConfig::parse(&config_text(""), base).unwrap();
        assert_eq!(d.key_env, "DATABENTO_API_KEY");
        assert!(d.gateway.is_none() && !d.replay_from_premarket && !d.end_at_regular_close);
        assert_eq!(
            (
                d.warmup_quiet_secs,
                d.warmup_max_secs,
                d.segment_secs,
                d.reconnects,
                d.id_space
            ),
            (5, 60, 300, 20, 16_384)
        );
        // A list of symbols is a list.
        let l = LiveConfig::parse(
            &config_text("").replace("subscribe status ALL_SYMBOLS", "subscribe trades AAPL,MSFT"),
            base,
        )
        .unwrap();
        assert_eq!(
            l.subs[1],
            (
                "trades".to_owned(),
                vec!["AAPL".to_owned(), "MSFT".to_owned()]
            )
        );
        // What is asked of the gateway.
        let subs = c.subs(Some(5));
        assert_eq!(
            (
                subs[0].schema.as_str(),
                subs[0].start,
                subs[0].symbols == Symbols::All,
                subs[0].stype_in.as_str()
            ),
            ("tcbbo", Some(5), true, "raw_symbol")
        );
        assert_eq!(
            l.subs(None)[1].symbols,
            Symbols::List(vec!["AAPL".into(), "MSFT".into()])
        );
        assert_eq!(l.subs(None)[0].start, None);
    }

    #[test]
    fn a_config_that_is_not_well_formed_is_refused_with_the_line() {
        let base = Path::new(".");
        let e = |t: &str| LiveConfig::parse(t, base).unwrap_err();
        assert!(e("dataset X\n").contains("must be `live config v1`") && e("").contains("must be"));
        for (extra, why) in [
            ("wat 1\n", "not a setting"),
            ("dataset TWO\n", "given twice"),
            ("end never\n", "end is close or after_hours"),
            ("replay_from_premarket maybe\n", "yes or no"),
            ("segment_secs 0\n", "above zero"),
            ("reconnects x\n", "above zero"),
            ("id_space -1\n", "above zero"),
            ("warmup_quiet_secs 99\nwarmup_max_secs 5\n", "longer than"),
            ("subscribe tcbbo\n", "`subscribe SCHEMA SYMBOLS`"),
            ("subscribe tcbbo A B\n", "`subscribe SCHEMA SYMBOLS`"),
            ("subscribe tcbbo A,,B\n", "a symbol is empty"),
            ("key_env\n", "takes one value"),
            ("key_env A B\n", "takes one value"),
        ] {
            let got = e(&config_text(extra));
            assert!(got.contains(why), "{extra:?} gave {got:?}");
        }
        // Each thing that must be there.
        for (drop, name) in [
            ("dataset XNAS.BASIC\n", "dataset"),
            ("set month.set\n", "set"),
            ("snapshots snaps\n", "snapshots"),
            ("certificates certs.txt\n", "certificates"),
            ("dir live\n", "dir"),
        ] {
            assert!(
                e(&config_text("").replace(drop, "")).contains(&format!("`{name}` is missing")),
                "{name}"
            );
        }
        assert!(
            e("live config v1\ndataset X\nset a\nsnapshots b\ncertificates c\ndir d\n")
                .contains("`subscribe` is missing")
        );
        assert!(
            LiveConfig::load(Path::new("/no/such/live.cfg"))
                .unwrap_err()
                .contains("live.cfg")
        );
    }

    #[test]
    fn a_start_time_is_in_new_york_time_counted_from_the_premarket() {
        let t = times_of(date_of(DAY).unwrap()).unwrap();
        assert_eq!(start_instant(t.premarket, "04:00").unwrap(), t.premarket);
        assert_eq!(
            start_instant(t.premarket, "03:55").unwrap(),
            t.premarket - 300 * SEC
        );
        assert_eq!(start_instant(t.premarket, "09:30").unwrap(), t.open);
        assert_eq!(
            start_instant(t.premarket, "20:00").unwrap(),
            t.after_hours_end
        );
        assert_eq!(
            start_instant(t.premarket, "00:00").unwrap(),
            t.premarket - 4 * 3600 * SEC
        );
        for bad in [
            "", "4:00", "04:0", "24:00", "04:60", "ab:cd", "04-00", "04:00:00",
        ] {
            assert!(start_instant(t.premarket, bad).is_err(), "{bad}");
        }
        assert!(start_instant(1, "00:00").is_err(), "before the epoch");
    }

    #[test]
    fn dates_are_dates_and_a_closed_day_has_no_session() {
        assert!(date_of("2026-05-04").is_ok());
        for bad in [
            "",
            "2026-5-4",
            "2026/05/04",
            "2026-02-30",
            "2026-05-04x",
            "today",
        ] {
            assert!(date_of(bad).is_err(), "{bad}");
        }
        assert!(
            times_of(date_of("2026-05-02").unwrap())
                .unwrap_err()
                .contains("closed"),
            "a Saturday"
        );
        assert!(times_of(date_of("2026-05-04").unwrap()).is_ok());
        let early = times_of(date_of("2026-11-27").unwrap()).unwrap();
        let normal = times_of(date_of("2026-11-25").unwrap()).unwrap();
        assert!(
            early.close - early.premarket < normal.close - normal.premarket,
            "the day after Thanksgiving closes early"
        );
    }

    #[test]
    fn the_first_free_directory_for_a_day_is_the_day_and_then_the_day_with_a_number() {
        let d = scratch("daydir");
        assert_eq!(day_dir(&d, DAY), d.join(DAY));
        fs::create_dir_all(d.join(DAY)).unwrap();
        assert_eq!(day_dir(&d, DAY), d.join(format!("{DAY}-2")));
        fs::create_dir_all(d.join(format!("{DAY}-2"))).unwrap();
        assert_eq!(day_dir(&d, DAY), d.join(format!("{DAY}-3")));
        assert_eq!(day_dir(&d, "2026-05-05"), d.join("2026-05-05"));
    }

    // ---- a world to run in: a set, a store with a stored day, snapshots, certificates, a config ----

    struct World {
        dir: PathBuf,
        cfg: PathBuf,
    }

    /// The records of one afternoon of six names, in seven bursts of ten seconds: at the start of the hour before the close and
    /// at the top of it (the reference price), at half past three (the entry), just before the exit, and at the close, whose
    /// first record ends the day. The fake gateway writes slowly, so a whole afternoon second by second would take a minute.
    fn afternoon(close: Nanos) -> Vec<u8> {
        let mut recs = Vec::new();
        for before in [3_700, 3_600, 1_810, 1_800, 35, 5, 0] {
            recs.extend(market_records(6, 10, close - before * SEC, 5));
        }
        recs.sort_by_key(|r| r.0);
        recs.dedup_by_key(|r| (r.0, r.1.clone()));
        stream_of(6, &recs)
    }

    fn world(name: &str) -> World {
        let dir = scratch(name);
        let close = times_of(date_of(DAY).unwrap()).unwrap().close;
        let tape = dir.join("store").join("XNAS.BASIC").join("tcbbo");
        fs::create_dir_all(&tape).unwrap();
        fs::write(
            tape.join(format!("{DAY}.dbn.zst")),
            zstd::encode_all(&afternoon(close)[..], 0).unwrap(),
        )
        .unwrap();
        tf_history::index(&dir.join("store"), "XNAS.BASIC", "tcbbo", "ALL_SYMBOLS").unwrap();
        fs::create_dir_all(dir.join("snaps")).unwrap();
        let mut snap = String::from("# as_of 2026-05-01\nsymbol,price,adv_shares\n");
        for i in 0..6 {
            snap.push_str(&format!("S{i:02},20.00,100000\n"));
        }
        fs::write(dir.join("snaps").join(format!("{DAY}.snapshot")), snap).unwrap();
        fs::write(
            dir.join("u.txt"),
            "universe v1\nstatic adv_shares >= 1000\n",
        )
        .unwrap();
        fs::write(
            dir.join("month.set"),
            "strategy set v1\nbalance 100000\nstrategy 1 rev t04 universe=u.txt names=3\n",
        )
        .unwrap();
        let cfg = dir.join("live.cfg");
        fs::write(
            &cfg,
            config_text("end close\nwarmup_quiet_secs 1\nwarmup_max_secs 10\nreconnects 1\n"),
        )
        .unwrap();
        World { dir, cfg }
    }

    fn certify_args(w: &World) -> Vec<String> {
        let p = |n: &str| w.dir.join(n).to_string_lossy().into_owned();
        args(&[
            "certify",
            "--set",
            &p("month.set"),
            "--store",
            &p("store"),
            "--dataset",
            "XNAS.BASIC",
            "--schema",
            "tcbbo",
            "--date",
            DAY,
            "--snapshots",
            &p("snaps"),
            "--out",
            &p("certs.txt"),
            "--id-space",
            "64",
        ])
    }

    #[test]
    fn a_set_is_certified_on_a_stored_day_and_the_certificates_are_for_its_current_definitions() {
        let w = world("certify");
        let text = dispatch(&certify_args(&w)).unwrap();
        assert!(
            text.contains("certified rev (1):") && text.contains("1 certificates written"),
            "{text}"
        );
        let file = fs::read_to_string(w.dir.join("certs.txt")).unwrap();
        assert!(file.starts_with("certificates v1\n1 rev cert1:"), "{file}");
        let certs = parse_certificates(&file).unwrap();
        assert_eq!((certs[0].0, certs[0].1.as_str()), (1, "rev"));
        assert!(certs[0].2.events > 1_000 && certs[0].2.is_intact());
        // They admit the set they were made for, and not a definition that has changed since.
        let (_, defs) = StrategySet::load(&w.dir.join("month.set")).unwrap();
        assert_eq!(admit(defs, &file).unwrap().len(), 1);
        fs::write(
            w.dir.join("month.set"),
            "strategy set v1\nbalance 100000\nstrategy 1 rev t04 universe=u.txt names=4\n",
        )
        .unwrap();
        let (_, changed) = StrategySet::load(&w.dir.join("month.set")).unwrap();
        let e = admit(changed, &file).err().unwrap();
        assert!(
            e.contains("no certificate for the current definition of: rev (1)"),
            "{e}"
        );
        // A certificate for another number or an altered one is not taken, and the file must be one.
        assert!(
            parse_certificates("nonsense\n")
                .unwrap_err()
                .contains("certificates v1")
        );
        assert!(
            parse_certificates("certificates v1\n1 rev\n")
                .unwrap_err()
                .contains("NUMBER NAME CERTIFICATE")
        );
        assert!(
            parse_certificates("certificates v1\nx rev cert1:0\n")
                .unwrap_err()
                .contains("not a strategy number")
        );
        let mut touched = file.trim_end().to_owned();
        let last = touched.pop().unwrap();
        touched.push(if last == '0' { '1' } else { '0' });
        assert!(
            parse_certificates(&touched)
                .unwrap_err()
                .contains("altered")
        );
        assert_eq!(parse_certificates("certificates v1\n\n").unwrap().len(), 0);
        let (_, defs) = StrategySet::load(&w.dir.join("month.set")).unwrap();
        assert!(admit(defs, "certificates v1\n").is_err());
    }

    #[test]
    fn certifying_is_refused_without_writing_anything_when_a_strategy_fails_or_the_inputs_are_wrong()
     {
        let w = world("certify-bad");
        let run = |extra: &[(&str, &str)]| {
            let mut a = certify_args(&w);
            for (k, v) in extra {
                let i = a.iter().position(|x| x == k).unwrap();
                a[i + 1] = (*v).to_owned();
            }
            dispatch(&a).unwrap_err()
        };
        assert!(run(&[("--date", "2026-05-05")]).contains("is not in the store"));
        assert!(run(&[("--date", "2026-05-02")]).contains("closed"));
        assert!(run(&[("--date", "soon")]).contains("expected YYYY-MM-DD"));
        assert!(run(&[("--schema", "trades")]).contains("is not in the store"));
        assert!(run(&[("--id-space", "0")]).contains("not a number above zero"));
        assert!(run(&[("--snapshots", "/no/snaps")]).contains("no reference snapshot"));
        // A snapshot that is as of the day itself is refused.
        let p = w.dir.join("snaps").join(format!("{DAY}.snapshot"));
        let t = fs::read_to_string(&p).unwrap();
        fs::write(&p, t.replace("2026-05-01", DAY)).unwrap();
        assert!(run(&[]).contains("must be from before the day"));
        fs::write(&p, t).unwrap();
        // A set whose universe has no members on the tape cannot be set up, and nothing is written.
        fs::write(w.dir.join("u.txt"), "universe v1\nstatic adv_dollar >= 1\n").unwrap();
        let e = run(&[]);
        assert!(e.contains("rev (1) is not certified"), "{e}");
        assert!(!w.dir.join("certs.txt").exists());
        // Wrong or missing flags are named.
        assert!(
            dispatch(&args(&["certify"]))
                .unwrap_err()
                .contains("--date is required")
        );
        assert!(
            dispatch(&args(&["certify", "--wat", "1"]))
                .unwrap_err()
                .contains("unexpected argument --wat")
        );
        assert!(
            dispatch(&args(&["certify", "--set"]))
                .unwrap_err()
                .contains("needs a value")
        );
        assert!(
            dispatch(&args(&["certify", "--set", "a", "--set", "b"]))
                .unwrap_err()
                .contains("twice")
        );
        assert!(
            dispatch(&args(&[])).unwrap_err().starts_with("usage:")
                && dispatch(&args(&["wat"])).unwrap_err().starts_with("usage:")
        );
    }

    fn day_records(close: Nanos) -> Vec<u8> {
        afternoon(close)
    }

    #[test]
    fn the_gateway_check_logs_in_reads_for_a_while_and_says_what_it_saw() {
        let w = world("check");
        let close = times_of(date_of(DAY).unwrap()).unwrap().close;
        let g = gateway(vec![Conn::new(day_records(close), Then::Close)]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let text = check_gateway(&cfg, key(), Duration::from_secs(3)).unwrap();
        assert!(
            text.contains("logged in to XNAS.BASIC (session 4242)")
                && text.contains("tcbbo + status"),
            "{text}"
        );
        assert!(
            !text.contains("nothing arrived") && text.contains("6 instruments named"),
            "{text}"
        );
        let events: u64 = text
            .split(" events read")
            .next()
            .unwrap()
            .rsplit('\n')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(events > 500, "{text}");
        // What it asked for: the dataset's two subscriptions, then the start.
        let lines = &g.lines()[0];
        assert!(
            lines.iter().filter(|l| l.contains("schema=tcbbo")).count() == 1
                && lines.iter().filter(|l| l.contains("schema=status")).count() == 1,
            "{lines:?}"
        );
        assert!(lines.last().unwrap() == "start_session\n");
        // A session that sends nothing says so; a key the gateway refuses is an error, and the key is not in the message.
        let quiet = gateway(vec![Conn::new(stream_of(0, &[]), Then::Close)]);
        cfg.gateway = Some(quiet.addr.to_string());
        let t = check_gateway(&cfg, key(), Duration::from_secs(1)).unwrap();
        assert!(t.contains("nothing arrived"), "{t}");
        let refused = gateway(vec![Conn {
            refuse: Some("Authentication failed.".to_owned()),
            ..Conn::new(vec![], Then::Close)
        }]);
        cfg.gateway = Some(refused.addr.to_string());
        let wrong = ApiKey::new("db-0123456789abcdef0123456789abc").unwrap();
        let e = check_gateway(&cfg, wrong, Duration::from_secs(1)).unwrap_err();
        assert!(
            e.contains("could not log in") && !e.contains("0123456789abcdef"),
            "{e}"
        );
    }

    #[test]
    fn a_key_comes_from_the_environment_variable_the_config_names() {
        let e = read_key("TF_LIVE_TEST_NO_SUCH_KEY_VARIABLE").unwrap_err();
        assert!(
            e.contains("TF_LIVE_TEST_NO_SUCH_KEY_VARIABLE") && e.contains("not set"),
            "{e}"
        );
        let c = dispatch(&args(&["check", "--config", "/no/such.cfg"])).unwrap_err();
        assert!(c.contains("such.cfg"), "{c}");
        assert!(dispatch(&args(&["check", "--config", "x", "--seconds", "0"])).is_err());
    }

    fn certified(w: &World) {
        dispatch(&certify_args(w)).unwrap();
    }

    #[test]
    fn a_live_day_runs_from_the_config_to_the_close_and_leaves_its_ledger_capture_log_and_report() {
        let w = world("day");
        certified(&w);
        let close = times_of(date_of(DAY).unwrap()).unwrap().close;
        let g = gateway(vec![Conn::new(day_records(close), Then::Close)]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let day = live_day(&cfg, DAY, key(), Arc::new(AtomicBool::new(false)), None).unwrap();
        assert_eq!(day.outcome, Outcome::Closed);
        assert_eq!(day.dir, w.dir.join("live").join(DAY));
        // The day's files, and the ledger that continues from day to day.
        for f in ["decisions.log", "report.txt"] {
            assert!(day.dir.join(f).is_file(), "{f}");
        }
        assert!(day.dir.join("capture").is_dir());
        assert!(
            w.dir
                .join("live")
                .join("ledger")
                .join("ledger.log")
                .is_file()
        );
        assert!(
            day.text.contains("THE DAY CLOSED") && day.text.contains("live 2026-05-04"),
            "{}",
            day.text
        );
        // The capture replays to the day's decisions.
        assert_eq!(day.replay_equal, Some(true), "{}", day.text);
        // The strategy acted, and the ledger says so, as the workspace reads a live ledger.
        let runs = tf_catalog::sessions(
            tf_ledger::ReadOnlyStore::open(w.dir.join("live").join("ledger")),
            "live",
            tf_catalog::Kind::Live,
        )
        .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].strategy, "rev");
        assert!(runs[0].trades.unwrap_or(0) > 0, "{runs:?}");
        // What it asked for: both subscriptions, no replay, from this config.
        let lines = &g.lines()[0];
        assert!(
            lines.iter().any(|l| l.contains("schema=tcbbo"))
                && lines.iter().all(|l| !l.contains("start=")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_day_is_refused_before_the_gateway_is_touched_when_something_it_needs_is_missing_or_wrong()
    {
        let w = world("refuse");
        certified(&w);
        let close = times_of(date_of(DAY).unwrap()).unwrap().close;
        let g = gateway(vec![Conn::new(day_records(close), Then::Close)]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let go = |cfg: &LiveConfig, date: &str| {
            live_day(cfg, date, key(), Arc::new(AtomicBool::new(false)), None)
                .err()
                .unwrap()
        };
        assert!(go(&cfg, "2026-05-02").contains("closed"));
        assert!(go(&cfg, "soon").contains("expected YYYY-MM-DD"));
        assert!(
            go(&cfg, "2026-05-05").contains("no reference snapshot"),
            "no snapshot for that day"
        );
        // A certificate for a definition that has changed.
        fs::write(
            w.dir.join("month.set"),
            "strategy set v1\nbalance 100000\nstrategy 1 rev t04 universe=u.txt names=5\n",
        )
        .unwrap();
        assert!(go(&cfg, DAY).contains("no certificate for the current definition of: rev (1)"));
        // No certificates, no set, no ledger directory to write to.
        let mut none = cfg.clone();
        none.certificates = w.dir.join("none.txt");
        assert!(go(&none, DAY).contains("none.txt"));
        let mut noset = cfg.clone();
        noset.set = w.dir.join("none.set");
        assert!(go(&noset, DAY).contains("none.set"));
        // A stop file left behind would end the day at once.
        certified_again(&w);
        fs::create_dir_all(w.dir.join("live")).unwrap();
        fs::write(w.dir.join("live").join("STOP"), "").unwrap();
        assert!(go(&cfg, DAY).contains("STOP exists: remove it"));
        // Nothing connected to the gateway for any of that.
        assert!(g.lines().is_empty(), "{:?}", g.lines());
    }

    fn certified_again(w: &World) {
        certified(w);
    }

    #[test]
    fn a_stop_file_ends_the_day_cleanly_with_the_capture_finished_and_the_report_written() {
        let w = world("stop");
        certified(&w);
        // A gateway that keeps sending heartbeats and no data: the day would go on until someone stops it.
        let g = gateway(vec![Conn::new(stream_of(6, &[]), Then::Heartbeat(20_000))]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let dir = w.dir.join("live");
        let stopper = {
            let file = dir.join("STOP");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(2_500));
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, "").unwrap();
            })
        };
        let day = live_day(&cfg, DAY, key(), Arc::new(AtomicBool::new(false)), None).unwrap();
        stopper.join().unwrap();
        assert_eq!(day.outcome, Outcome::Stopped);
        assert!(day.text.contains("THE DAY WAS STOPPED"), "{}", day.text);
        assert!(day.dir.join("report.txt").is_file() && day.dir.join("decisions.log").is_file());
        // The day directory is not reused by a restart in the same day.
        assert_eq!(day_dir(&dir, DAY), dir.join(format!("{DAY}-2")));
    }

    #[test]
    fn a_start_time_in_the_future_waits_and_a_stop_during_the_wait_means_the_gateway_is_never_opened()
     {
        let w = world("wait");
        certified(&w);
        let g = gateway(vec![]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let stop = Arc::new(AtomicBool::new(false));
        let setter = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                stop.store(true, Ordering::Relaxed);
            })
        };
        let began = std::time::Instant::now();
        let e = live_day(&cfg, DAY, key(), stop, Some(now_ns() + 3_600 * SEC))
            .err()
            .unwrap();
        setter.join().unwrap();
        assert!(e.contains("stopped before the gateway was opened"), "{e}");
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "it waited the hour"
        );
        assert!(g.lines().is_empty());
    }

    #[test]
    fn the_settings_that_say_no_or_after_hours_are_what_they_say() {
        let base = Path::new(".");
        let c = LiveConfig::parse(&config_text("replay_from_premarket no\nend after_hours\nwarmup_quiet_secs 5\nwarmup_max_secs 5\n"), base).unwrap();
        assert!(!c.replay_from_premarket && !c.end_at_regular_close);
        assert_eq!((c.warmup_quiet_secs, c.warmup_max_secs), (5, 5));
        assert!(
            c.subs(None).iter().all(|s| !s.snapshot),
            "no snapshot is asked for"
        );
        let yes = LiveConfig::parse(&config_text("replay_from_premarket yes\nend close\n"), base)
            .unwrap();
        assert!(yes.replay_from_premarket && yes.end_at_regular_close);
    }

    #[test]
    fn a_date_has_dashes_in_the_right_places() {
        for bad in [
            "2026/05-04",
            "2026-05/04",
            "2026_05_04",
            "2026-05-044",
            "2026-0504",
        ] {
            assert!(date_of(bad).is_err(), "{bad}");
        }
        assert_eq!(date_of("2026-05-04").unwrap().to_string(), "2026-05-04");
    }

    #[test]
    fn the_check_names_what_is_wrong_with_how_long_to_read() {
        let w = world("check-args");
        let cfg = w.cfg.to_string_lossy().into_owned();
        for bad in ["0", "-1", "x", ""] {
            let e = dispatch(&args(&["check", "--config", &cfg, "--seconds", bad])).unwrap_err();
            assert!(
                e.contains(&format!("--seconds {bad}: not a number above zero")),
                "{e}"
            );
        }
    }

    #[test]
    fn events_and_gaps_are_counted_apart() {
        use tf_ingest::{Delivery, Gap, Lost};
        let ev = Delivery::Event(tf_core::Event::Trade(tf_core::Trade {
            hdr: tf_core::Header {
                ts_event: 1,
                ts_recv: 1,
                seq: 1,
                instrument: 0,
                provider: tf_core::ProviderId::Databento,
            },
            px: tf_core::Px::from_raw(1),
            size: 1,
            flags: tf_core::TradeFlags::NONE,
        }));
        let gap = Delivery::Gap(Gap {
            lost: Lost::Trades,
            count: 3,
            first_ts: 1,
            last_ts: 2,
        });
        let (mut e, mut g) = (0u64, 0u64);
        count(&ev, &mut e, &mut g);
        count(&ev, &mut e, &mut g);
        count(&gap, &mut e, &mut g);
        assert_eq!((e, g), (2, 1));
    }

    #[test]
    fn a_session_that_closes_at_once_ends_the_check_early_and_says_so() {
        let w = world("check-early");
        let g = gateway(vec![Conn::new(stream_of(0, &[]), Then::Close)]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let began = std::time::Instant::now();
        let t = check_gateway(&cfg, key(), Duration::from_secs(60)).unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "it read for the whole minute: {t}"
        );
        assert!(t.contains("nothing arrived"), "{t}");
    }

    #[test]
    fn a_day_that_did_not_close_or_did_not_replay_is_said_to_have_not() {
        let day = |outcome, replay_equal| LiveDay {
            outcome,
            dir: PathBuf::from("run/d"),
            text: "report".to_owned(),
            replay_equal,
        };
        let ok = summary(day(Outcome::Closed, Some(true))).unwrap();
        assert!(
            ok.starts_with("report\nfiles in run/d\n") && !ok.contains("DID NOT"),
            "{ok}"
        );
        assert!(
            summary(day(Outcome::Closed, None))
                .unwrap()
                .find("DID NOT")
                .is_none()
        );
        let bad = summary(day(Outcome::Closed, Some(false))).unwrap();
        assert!(
            bad.contains("THE REPLAY OF THE CAPTURE DID NOT REPRODUCE"),
            "{bad}"
        );
        for outcome in [
            Outcome::Stopped,
            Outcome::GaveUp("down".into()),
            Outcome::CaptureFailed("disk".into()),
        ] {
            let e = summary(day(outcome.clone(), Some(true))).unwrap_err();
            assert!(
                e.contains("the day did not close normally") && e.contains("report"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_day_is_run_and_reported_from_the_config_with_a_key() {
        let w = world("rundays");
        certified(&w);
        let close = times_of(date_of(DAY).unwrap()).unwrap().close;
        let g = gateway(vec![Conn::new(day_records(close), Then::Close)]);
        let mut cfg = LiveConfig::load(&w.cfg).unwrap();
        cfg.gateway = Some(g.addr.to_string());
        let text = run_day(&cfg, DAY, key(), None).unwrap();
        assert!(
            text.contains("THE DAY CLOSED") && text.contains("files in "),
            "{text}"
        );
        assert!(!text.contains("DID NOT REPRODUCE"), "{text}");
        // A day that cannot close (the gateway is gone and the one reconnect is refused) is an error that carries the report.
        let dead = gateway(vec![Conn::new(stream_of(6, &[]), Then::Close)]);
        cfg.gateway = Some(dead.addr.to_string());
        let again = run_day(&cfg, DAY, key(), None).unwrap_err();
        assert!(
            again.contains("the day did not close normally")
                && again.contains("THE GATEWAY WAS LOST"),
            "{again}"
        );
    }

    #[test]
    fn the_example_files_in_the_documents_are_a_config_a_set_and_a_universe_that_read() {
        let base = Path::new("docs/examples/live");
        let cfg =
            LiveConfig::parse(include_str!("../../../docs/examples/live/live.cfg"), base).unwrap();
        assert_eq!(
            (cfg.dataset.as_str(), cfg.subs.len(), cfg.key_env.as_str()),
            ("EQUS.MINI", 1, "DATABENTO_API_KEY")
        );
        let set =
            StrategySet::parse(include_str!("../../../docs/examples/live/month.set")).unwrap();
        assert_eq!(set.strategies.len(), 4);
        assert!(set.host_config(16_384).is_ok());
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/live");
        let defs = set.definitions(&dir).unwrap();
        assert_eq!(
            defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["rev3", "rev10", "rev3b", "null1"]
        );
        // Four variants with four fingerprints.
        let mut fps: Vec<u64> = defs.iter().map(StrategyDef::fingerprint).collect();
        fps.sort();
        fps.dedup();
        assert_eq!(fps.len(), 4);
    }

    #[test]
    fn the_clusters_config_files_read_and_agree_with_the_manifests_and_the_prepare_script() {
        let cfg = LiveConfig::parse(
            include_str!("../../../deploy/k8s/config/live.cfg"),
            Path::new("/config"),
        )
        .unwrap();
        assert_eq!(
            (cfg.dataset.as_str(), cfg.key_env.as_str()),
            ("EQUS.MINI", "DATABENTO_API_KEY")
        );
        assert_eq!(
            cfg.subs,
            [("tbbo".to_owned(), vec![])],
            "EQUS.MINI has no status and no tcbbo"
        );
        assert_eq!(cfg.set, Path::new("/config/month.set"));
        assert_eq!(
            (
                cfg.snapshots.as_path(),
                cfg.certificates.as_path(),
                cfg.dir.as_path()
            ),
            (
                Path::new("/data/live/snapshots"),
                Path::new("/data/live/certificates.txt"),
                Path::new("/data/live/run")
            )
        );
        // The set reads, with its universe beside it, and is the example's.
        let set = StrategySet::parse(include_str!("../../../deploy/k8s/config/month.set")).unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/k8s/config");
        assert!(set.definitions(&dir).is_ok());
        assert_eq!(
            include_str!("../../../deploy/k8s/config/month.set"),
            include_str!("../../../docs/examples/live/month.set")
        );
        // The workspace reads the ledger the live job writes; the prepare script writes where the live job reads.
        let workspace = include_str!("../../../deploy/k8s/workspace.yaml");
        assert!(
            workspace.contains(&format!("--ledger={}/ledger", cfg.dir.display())),
            "the workspace reads another ledger"
        );
        let script = include_str!("../../../scripts/prepare-day.sh");
        assert!(
            script.contains(&format!("SNAPS=${{SNAPS:-{}}}", cfg.snapshots.display())),
            "the snapshots are not where the config reads them"
        );
        assert!(
            script.contains(&format!("CERTS=${{CERTS:-{}}}", cfg.certificates.display())),
            "the certificates are not where the config reads them"
        );
        assert!(
            script.contains("SET=${SET:-/config/month.set}")
                && script.contains("SCHEMA=${SCHEMA:-tbbo}")
                && script.contains("DATASET=${DATASET:-EQUS.MINI}")
        );
        // The live job is told to start where the config is, five minutes before the premarket.
        let jobs = include_str!("../../../deploy/k8s/jobs.yaml");
        assert!(jobs.contains("--config=/config/live.cfg") && jobs.contains("--start-at=03:55"));
    }
}
