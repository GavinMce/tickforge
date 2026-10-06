//! `tf budgets`: proposals to change the budgets, and what is done with them.
//!
//! An agent proposes a whole budget tree with a reason and its evidence. What reduces risk within
//! bounds is queued on its own; an increase waits for a person; an increase for a strategy in
//! drawdown, or anything the rules do not allow, is refused (this command then exits with an
//! error, so an agent calling it sees the refusal). Nothing here writes the ledger: whatever is to
//! take effect goes in the ledger's inbox (`tf ledger apply-inbox`) and takes effect at the next
//! rebalance.

use std::fmt::Write as _;
use std::path::Path;

use tf_core::Nanos;
use tf_proposals::Policy;
use tf_proposals::flow::{self, FlowError};
use tf_proposals::store::{self, State, Status};

use tf_catalog::when;

const USAGE: &str = "usage:
    tf budgets propose DIR --by NAME --reason TEXT --evidence TEXT --tree FILE [--at NANOS] [--step-bp N] [--cooldown-secs N]
    tf budgets proposals DIR
    tf budgets approve DIR ID --by NAME [--note TEXT] [--at NANOS]
    tf budgets decline DIR ID --by NAME [--note TEXT] [--at NANOS]";

pub(crate) fn budgets(args: &[String]) -> Result<(), String> {
    print!("{}", run(args)?);
    Ok(())
}

fn wall_clock() -> Nanos {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

struct Flags {
    positional: Vec<String>,
    by: Option<String>,
    reason: String,
    evidence: String,
    note: String,
    tree: Option<String>,
    at: Option<Nanos>,
    policy: Policy,
}

fn flags(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags {
        positional: Vec::new(),
        by: None,
        reason: String::new(),
        evidence: String::new(),
        note: String::new(),
        tree: None,
        at: None,
        policy: Policy::default(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| it.next().cloned().ok_or(format!("{what} needs a value"));
        let number = |what: &str, v: String| {
            v.parse::<u64>()
                .map_err(|_| format!("{what} {v}: not a number"))
        };
        match a.as_str() {
            "--by" => f.by = Some(value("--by")?),
            "--reason" => f.reason = value("--reason")?,
            "--evidence" => f.evidence = value("--evidence")?,
            "--note" => f.note = value("--note")?,
            "--tree" => f.tree = Some(value("--tree")?),
            "--at" => f.at = Some(number("--at", value("--at")?)?),
            "--step-bp" => {
                f.policy.step = u32::try_from(number("--step-bp", value("--step-bp")?)?)
                    .map_err(|_| "--step-bp is too large".to_owned())?;
            }
            "--cooldown-secs" => {
                f.policy.cooldown = number("--cooldown-secs", value("--cooldown-secs")?)?
                    .checked_mul(1_000_000_000)
                    .ok_or("--cooldown-secs is too large")?;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag {flag}\n{USAGE}")),
            other => f.positional.push(other.to_owned()),
        }
    }
    Ok(f)
}

fn state_name(s: State) -> &'static str {
    match s {
        State::Scheduled => "scheduled (on its own)",
        State::Waiting => "waiting for a person",
        State::Approved => "approved",
        State::Declined => "declined",
        State::Refused => "refused",
    }
}

fn flow_err(e: FlowError) -> String {
    e.to_string()
}

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err(USAGE.to_owned());
    };
    let f = flags(rest)?;
    let need_by = || f.by.clone().ok_or(format!("--by NAME is needed\n{USAGE}"));
    let mut out = String::new();
    match cmd.as_str() {
        "propose" => {
            let [dir] = f.positional.as_slice() else {
                return Err(USAGE.to_owned());
            };
            let by = need_by()?;
            let path = f
                .tree
                .as_deref()
                .ok_or(format!("--tree FILE is needed\n{USAGE}"))?;
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            let r = flow::submit(
                Path::new(dir),
                &f.policy,
                &by,
                &f.reason,
                &f.evidence,
                &text,
                f.at.unwrap_or_else(wall_clock),
            )
            .map_err(flow_err)?;
            let verdict = match r.status {
                Status::Auto => "scheduled on its own",
                Status::Pending => "waiting for a person",
                Status::Refused => "refused",
            };
            let _ = writeln!(out, "proposal {} {verdict}", r.id);
            for w in &r.why {
                let _ = writeln!(out, "  {w}");
            }
            if r.status == Status::Refused {
                return Err(out.trim_end().to_owned());
            }
        }
        "proposals" => {
            let [dir] = f.positional.as_slice() else {
                return Err(USAGE.to_owned());
            };
            let (entries, bad) = store::list(Path::new(dir)).map_err(|e| e.to_string())?;
            if entries.is_empty() && bad.is_empty() {
                out.push_str("no proposals\n");
            }
            for e in &entries {
                let p = &e.proposal;
                let _ = writeln!(
                    out,
                    "{:>4}  {}  {:<22}  by {}  nodes {}",
                    p.id,
                    when(p.at),
                    state_name(e.state()),
                    p.by,
                    p.nodes.join(",")
                );
                let _ = writeln!(out, "      why:      {}", p.reason);
                let _ = writeln!(out, "      evidence: {}", p.evidence);
                for w in &p.why {
                    let _ = writeln!(out, "      policy:   {w}");
                }
                if let Some(d) = &e.decision {
                    let _ = writeln!(
                        out,
                        "      decided:  {} by {} {}",
                        if d.call == store::Call::Approved {
                            "approved"
                        } else {
                            "declined"
                        },
                        d.by,
                        d.note
                    );
                }
            }
            for (name, why) in &bad {
                let _ = writeln!(out, "unreadable {name}: {why}");
            }
        }
        "approve" | "decline" => {
            let [dir, id] = f.positional.as_slice() else {
                return Err(USAGE.to_owned());
            };
            let id: u64 = id
                .parse()
                .map_err(|_| format!("`{id}` is not a proposal number"))?;
            let by = need_by()?;
            let at = f.at.unwrap_or_else(wall_clock);
            if cmd == "approve" {
                let name = flow::approve(Path::new(dir), id, &by, &f.note, at).map_err(flow_err)?;
                let _ = writeln!(
                    out,
                    "approved proposal {id}; queued as {name} (tf ledger apply-inbox records it; it takes effect at the next rebalance)"
                );
            } else {
                flow::decline(Path::new(dir), id, &by, &f.note, at).map_err(flow_err)?;
                let _ = writeln!(out, "declined proposal {id}");
            }
        }
        other => return Err(format!("unknown command {other}\n{USAGE}")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tf_budget::{Group, LossLimits as L, Strategy as S, Tree};
    use tf_ledger::{FileStore, Journal};
    use tf_risk::{Budgets, Limits};

    const DAY: u64 = 86_400 * 1_000_000_000;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tf-budgetscmd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn tree(a: u32, b: u32) -> Tree {
        Tree::new(vec![Group {
            id: "g".into(),
            share: 10_000,
            loss: L::default(),
            strategies: vec![
                S {
                    id: "s".into(),
                    share: a,
                },
                S {
                    id: "t".into(),
                    share: b,
                },
            ],
        }])
        .unwrap()
    }

    fn ledger(dir: &std::path::Path) {
        let limits = Limits::new(
            5_000 * 1_000_000_000,
            1_000,
            20_000 * 1_000_000_000,
            400 * 1_000_000_000,
            6,
            10_000_000_000,
        )
        .unwrap();
        let (mut j, _) = Journal::open(FileStore::open(dir).unwrap(), limits, 2).unwrap();
        let ids = [(1, "s".to_owned()), (2, "t".to_owned())];
        j.set_budgets(
            Some(Budgets::new(tree(4_000, 4_000), 10_000 * 1_000_000_000, ids).unwrap()),
            1_000_000_000,
        )
        .unwrap();
    }

    #[test]
    fn proposals_are_made_listed_approved_and_declined_from_the_command_line() {
        let dir = scratch("flow");
        ledger(&dir);
        let d = dir.to_str().unwrap();
        let file = |name: &str, t: &Tree| {
            let p = dir.join(name);
            std::fs::write(&p, t.render()).unwrap();
            p.to_str().unwrap().to_owned()
        };
        let at = (100 * DAY).to_string();
        let propose = |tree_file: &str, extra: &[&str]| {
            let mut a = vec![
                "propose",
                d,
                "--by",
                "risk-agent",
                "--reason",
                "lost 3 sessions",
                "--evidence",
                "run abc, review def",
                "--tree",
                tree_file,
                "--at",
                &at,
            ];
            a.extend_from_slice(extra);
            run(&args(&a))
        };
        // A small cut: scheduled on its own.
        let out = propose(&file("cut.txt", &tree(3_700, 4_000)), &[]).unwrap();
        assert!(
            out.contains("proposal 1 scheduled on its own")
                && out.contains("queued as 0000000001.req"),
            "{out}"
        );
        // A bigger cut than the step given on the command line: waits for a person.
        let out = propose(&file("big.txt", &tree(4_000, 3_500)), &["--step-bp", "300"]).unwrap();
        assert!(
            out.contains("proposal 2 waiting for a person")
                && out.contains("cut by 500 basis points, more than the 300"),
            "{out}"
        );
        // An increase waits.
        let out = propose(&file("up.txt", &tree(4_000, 4_500)), &[]).unwrap();
        assert!(
            out.contains("proposal 3 waiting for a person")
                && out.contains("t: an increase needs a person"),
            "{out}"
        );
        // Nothing to do, or not allowed: an error, so an agent notices.
        let e = propose(&file("same.txt", &tree(4_000, 4_000)), &[]).unwrap_err();
        assert!(
            e.contains("proposal 4 refused") && e.contains("it changes nothing"),
            "{e}"
        );
        // The list says where each stands, why, and the evidence.
        let out = run(&args(&["proposals", d])).unwrap();
        assert!(
            out.contains("scheduled (on its own)")
                && out.contains("waiting for a person")
                && out.contains("refused"),
            "{out}"
        );
        assert!(
            out.contains("why:      lost 3 sessions")
                && out.contains("evidence: run abc, review def"),
            "{out}"
        );
        assert!(out.contains("by risk-agent  nodes s"), "{out}");
        // A person approves one and declines another.
        let out = run(&args(&[
            "approve", d, "3", "--by", "gavin", "--note", "ok", "--at", &at,
        ]))
        .unwrap();
        assert!(
            out.contains("approved proposal 3; queued as 0000000002.req"),
            "{out}"
        );
        assert_eq!(
            run(&args(&[
                "decline", d, "2", "--by", "gavin", "--note", "no", "--at", &at
            ]))
            .unwrap(),
            "declined proposal 2\n"
        );
        let out = run(&args(&["proposals", d])).unwrap();
        assert!(
            out.contains("decided:  approved by gavin ok")
                && out.contains("decided:  declined by gavin no"),
            "{out}"
        );
        assert!(
            out.contains("approved ") && out.contains("declined"),
            "{out}"
        );
        // Decided once.
        assert!(
            run(&args(&["approve", d, "3", "--by", "gavin"]))
                .unwrap_err()
                .contains("not waiting")
        );
        // Both queued changes are what the engine will record.
        let text = crate::ledger_cmd::apply_inbox(&args(&[d, "--at", "5"])).unwrap();
        assert!(
            text.contains("scheduled 0000000001.req") && text.contains("scheduled 0000000002.req"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_invocations_are_refused_with_the_reason() {
        let err = |a: &[&str]| run(&args(a)).unwrap_err();
        assert!(err(&[]).starts_with("usage:"));
        assert!(err(&["frobnicate"]).contains("unknown command"));
        assert!(err(&["propose"]).starts_with("usage:"));
        assert!(err(&["propose", "d"]).contains("--by NAME"));
        assert!(err(&["propose", "d", "--by", "a"]).contains("--tree FILE"));
        assert!(
            err(&["propose", "d", "--by", "a", "--tree", "/nonexistent"]).contains("/nonexistent")
        );
        assert!(err(&["approve", "d", "x", "--by", "a"]).contains("not a proposal number"));
        assert!(err(&["approve", "d", "1"]).contains("--by NAME"));
        assert!(err(&["approve", "d"]).starts_with("usage:"));
        assert!(err(&["proposals"]).starts_with("usage:"));
        assert!(err(&["proposals", "d", "--wat"]).contains("unknown flag"));
        assert!(err(&["proposals", "d", "--at", "x"]).contains("not a number"));
        assert!(err(&["proposals", "d", "--step-bp", "99999999999"]).contains("too large"));
        assert!(
            err(&["proposals", "d", "--cooldown-secs", "99999999999999999999"])
                .contains("not a number")
        );
        assert!(err(&["proposals", "d", "--reason"]).contains("needs a value"));
        let empty = scratch("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            run(&args(&["proposals", empty.to_str().unwrap()])).unwrap(),
            "no proposals\n"
        );
        let _ = std::fs::remove_dir_all(&empty);
    }
}
