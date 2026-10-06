//! `tf ledger verify DIR`: replay an order ledger and say what it holds.
//!
//! Opening a ledger repairs a torn last record (and says so), exactly as a restarting engine
//! would. It takes the ledger's lock, so it refuses while an engine is writing the ledger.

use std::fmt::Write as _;
use std::path::Path;

use tf_ledger::{FileStore, Journal};

use super::runs::money;

pub(crate) fn ledger(args: &[String]) -> Result<(), String> {
    print!("{}", report(args)?);
    Ok(())
}

fn side_name(s: tf_strategy::intent::Side) -> &'static str {
    match s {
        tf_strategy::intent::Side::Buy => "buy",
        tf_strategy::intent::Side::Sell => "sell",
        tf_strategy::intent::Side::SellShort => "short",
    }
}

/// What `tf ledger verify` prints.
pub(crate) fn report(args: &[String]) -> Result<String, String> {
    let (mut dir, mut orders) = (None, false);
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("verify") {
        return Err("usage: tf ledger verify DIR [--orders]".to_owned());
    }
    for a in it {
        match a.as_str() {
            "--orders" => orders = true,
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}")),
            d if dir.is_none() => dir = Some(d.to_owned()),
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    let dir = dir.ok_or("usage: tf ledger verify DIR [--orders]")?;
    if !Path::new(&dir).join("ledger.log").exists() {
        return Err(format!("no ledger in {dir} (no ledger.log)"));
    }
    let store = FileStore::open(&dir).map_err(|e| e.to_string())?;
    let (j, rec) = Journal::open_recorded(store).map_err(|e| e.to_string())?;
    let snap = j.snapshot();
    let mut out = String::new();
    let _ = writeln!(out, "ledger   {dir}");
    let _ = writeln!(
        out,
        "replayed {} records; every recorded decision reproduced",
        rec.records
    );
    if let Some(r) = &rec.repaired {
        let _ = writeln!(out, "repaired a torn last record: {r}");
    }
    let all = j.orders().count();
    let _ = writeln!(
        out,
        "orders   {all} accepted ({} open); gateway has {} working",
        rec.open_orders,
        j.gateway().working_orders()
    );
    let _ = writeln!(
        out,
        "gateway  accepted {}  next order {}",
        snap.accepted, snap.next_order
    );
    if snap.rejected.is_empty() {
        let _ = writeln!(out, "refused  nothing");
    }
    for (why, n) in &snap.rejected {
        let _ = writeln!(out, "refused  {why:<22} {n}");
    }
    let clamp = |v: i128| v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    let _ = writeln!(
        out,
        "pnl      realised {}  day base {}",
        money(clamp(snap.realized)),
        money(clamp(snap.day_base))
    );
    for (strategy, r) in &snap.strategy_realized {
        let _ = writeln!(
            out,
            "pnl      strategy {strategy} realised {}",
            money(clamp(*r))
        );
    }
    let _ = writeln!(
        out,
        "state    kill switch {}  loss limit latched {}",
        if snap.killed { "ENGAGED" } else { "off" },
        if snap.loss_latched { "YES" } else { "no" }
    );
    if snap.positions.is_empty() {
        let _ = writeln!(out, "positions none");
    }
    for (strategy, i, qty, avg, mark) in &snap.positions {
        let _ = writeln!(
            out,
            "position strategy {strategy} instrument {i}: {qty} @ {} (mark {})",
            money(*avg),
            money(*mark)
        );
    }
    if orders {
        for o in j.open_orders() {
            let _ = writeln!(
                out,
                "open     order {} instrument {} {} {} of {} {:?}",
                o.id.0,
                o.intent.instrument,
                side_name(o.intent.side),
                o.filled_qty(),
                o.intent.qty,
                o.state()
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_core::Px;
    use tf_risk::Limits;
    use tf_strategy::intent::{
        Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif,
    };
    use tf_strategy::lifecycle::Decision;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tf-ledgercmd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn limits() -> Limits {
        Limits::new(
            5_000 * 1_000_000_000,
            1_000,
            20_000 * 1_000_000_000,
            400 * 1_000_000_000,
            6,
            10_000_000_000,
        )
        .unwrap()
    }

    fn buy(seq: u64, qty: u32) -> Intent {
        let px = Px::from_raw(5_000_000_000);
        Intent {
            id: IntentId {
                strategy: StrategyId(1),
                seq,
            },
            instrument: 1,
            side: Side::Buy,
            qty,
            purpose: Purpose::Open,
            pricing: Pricing::Limit(px),
            protect: Some(Protective {
                stop_trigger: Px::from_raw(4_500_000_000),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            ts: seq,
            reason: 1,
        }
    }

    /// A ledger with one filled order, one working order and one refusal.
    fn make(dir: &Path) {
        let (mut j, _) = Journal::open(FileStore::open(dir).unwrap(), limits(), 3).unwrap();
        let Decision::Accepted(a) = j.decide(&buy(1, 100), 1_000_000_000).unwrap() else {
            panic!()
        };
        j.ack(a, 1_000_000_001).unwrap();
        j.fill(a, 60, Px::from_raw(5_000_000_000), 1_000_000_002)
            .unwrap();
        let Decision::Accepted(_) = j.decide(&buy(2, 50), 2_000_000_000).unwrap() else {
            panic!()
        };
        let _ = j.decide(&buy(3, 100_000), 3_000_000_000).unwrap();
    }

    #[test]
    fn a_ledger_is_replayed_and_described() {
        let dir = scratch("describe");
        make(&dir);
        let d = dir.to_str().unwrap();
        let text = report(&args(&["verify", d, "--orders"])).unwrap();
        assert!(
            text.contains("replayed 6 records; every recorded decision reproduced"),
            "{text}"
        );
        assert!(
            text.contains("orders   2 accepted (2 open); gateway has 2 working"),
            "{text}"
        );
        assert!(text.contains("refused  max_notional"), "{text}");
        assert!(
            text.contains("position strategy 1 instrument 1: 60 @ $5.00"),
            "{text}"
        );
        assert!(
            text.contains("open     order 0 instrument 1 buy 60 of 100 PartiallyFilled"),
            "{text}"
        );
        assert!(
            text.contains("open     order 1 instrument 1 buy 0 of 50 Pending"),
            "{text}"
        );
        assert!(text.contains("kill switch off"));
        assert!(
            !report(&args(&["verify", d]))
                .unwrap()
                .contains("open     order"),
            "orders only on request"
        );
        assert!(!dir.join("ledger.lock").exists(), "the lock is released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_tail_is_repaired_and_said_and_damage_inside_or_a_held_lock_is_refused() {
        let dir = scratch("damage");
        make(&dir);
        let log = dir.join("ledger.log");
        let bytes = std::fs::read(&log).unwrap();
        std::fs::write(&log, &bytes[..bytes.len() - 9]).unwrap();
        let d = dir.to_str().unwrap();
        let text = report(&args(&["verify", d])).unwrap();
        assert!(text.contains("repaired a torn last record"), "{text}");
        assert!(text.contains("replayed 5 records"), "{text}");
        assert!(
            !report(&args(&["verify", d])).unwrap().contains("repaired"),
            "once"
        );
        // Damage before the last record.
        let text = std::fs::read_to_string(&log)
            .unwrap()
            .replacen("buy", "bux", 1);
        std::fs::write(&log, text).unwrap();
        let e = report(&args(&["verify", d])).unwrap_err();
        assert!(e.contains("damaged") || e.contains("cannot be read"), "{e}");
        // An engine holding the ledger.
        let held = FileStore::open(scratch("held")).unwrap();
        drop(held);
        let dir2 = scratch("lock");
        make(&dir2);
        let _w = FileStore::open(&dir2).unwrap();
        assert!(
            report(&args(&["verify", dir2.to_str().unwrap()]))
                .unwrap_err()
                .contains("held by another writer")
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn bad_use_is_refused() {
        let dir = scratch("usage");
        assert!(report(&args(&[])).unwrap_err().contains("usage"));
        assert!(
            report(&args(&["inspect", "x"]))
                .unwrap_err()
                .contains("usage")
        );
        assert!(report(&args(&["verify"])).unwrap_err().contains("usage"));
        assert!(
            report(&args(&["verify", "a", "b"]))
                .unwrap_err()
                .contains("unexpected")
        );
        assert!(
            report(&args(&["verify", "a", "--what"]))
                .unwrap_err()
                .contains("unknown flag")
        );
        assert!(
            report(&args(&["verify", dir.to_str().unwrap()]))
                .unwrap_err()
                .contains("no ledger")
        );
        assert!(!dir.exists(), "looking at a missing ledger creates nothing");
    }
}
