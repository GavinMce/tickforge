//! Proposals and decisions kept as files beside the ledger, in `proposals/`.
//!
//! A proposal `NNNNNNNNNN.prop` is written whole and never changed: who proposed what, why, the
//! evidence, which policy judged it, the verdict, and the tree. A person's decision on a proposal
//! that needed one is a separate file `NNNNNNNNNN.dec` with the same number, created exclusively, so
//! a proposal is decided once. Nothing is ever edited or removed except a decision rolled back
//! because what it approved could not be queued.
//!
//! ```text
//! tfprop 1
//! by <agent>
//! at <event time, nanoseconds>
//! status auto|pending|refused
//! policy step=<bp> cooldown=<ns>
//! nodes <changed ids, comma separated>
//! reason <one line>
//! evidence <one line>
//! why <one line>          (zero or more)
//! ---
//! budgets v1 ...
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use tf_budget::Tree;
use tf_core::Nanos;

use crate::Policy;

/// What the policy said when the proposal came in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Reduced risk within bounds: it was scheduled without a person.
    Auto,
    /// Waiting for a person.
    Pending,
    Refused,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Auto => "auto",
            Status::Pending => "pending",
            Status::Refused => "refused",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub id: u64,
    pub by: String,
    pub at: Nanos,
    pub status: Status,
    pub policy: Policy,
    /// The nodes (groups and strategies) the tree changes.
    pub nodes: Vec<String>,
    pub reason: String,
    pub evidence: String,
    /// Why the policy decided as it did, one line each.
    pub why: Vec<String>,
    pub tree: Tree,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Approved,
    Declined,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub id: u64,
    pub by: String,
    pub at: Nanos,
    pub call: Call,
    pub note: String,
}

/// Where a proposal stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Scheduled for the next rebalance on its own.
    Scheduled,
    /// Waiting for a person.
    Waiting,
    /// A person approved it and it was scheduled.
    Approved,
    Declined,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub proposal: Proposal,
    pub decision: Option<Decision>,
}

impl Entry {
    pub fn state(&self) -> State {
        match (self.proposal.status, self.decision.as_ref().map(|d| d.call)) {
            (Status::Auto, _) => State::Scheduled,
            (Status::Refused, _) => State::Refused,
            (Status::Pending, None) => State::Waiting,
            (Status::Pending, Some(Call::Approved)) => State::Approved,
            (Status::Pending, Some(Call::Declined)) => State::Declined,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    /// A decision for a proposal that already has one.
    Decided(u64),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(m) => write!(f, "proposals: {m}"),
            StoreError::Decided(id) => write!(f, "proposal {id} already has a decision"),
        }
    }
}

fn io(p: &Path, e: std::io::Error) -> StoreError {
    StoreError::Io(format!("{}: {e}", p.display()))
}

pub fn dir(ledger: &Path) -> PathBuf {
    ledger.join("proposals")
}

/// One line of text: control characters become spaces, and it is cut to `max` characters.
fn line(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn proposal_text(p: &Proposal) -> String {
    let mut s = format!(
        "tfprop 1\nby {}\nat {}\nstatus {}\npolicy step={} cooldown={}\nnodes {}\nreason {}\nevidence {}\n",
        line(&p.by, 64),
        p.at,
        p.status.name(),
        p.policy.step,
        p.policy.cooldown,
        p.nodes.join(","),
        line(&p.reason, 300),
        line(&p.evidence, 600),
    );
    for w in &p.why {
        s.push_str(&format!("why {}\n", line(w, 300)));
    }
    s.push_str("---\n");
    s.push_str(&p.tree.render());
    s
}

fn field<'a>(l: Option<&'a str>, key: &str) -> Result<&'a str, String> {
    let missing = || format!("expected a `{key}` line");
    let l = l.ok_or_else(missing)?;
    if l == key {
        return Ok("");
    }
    l.strip_prefix(key)
        .and_then(|r| r.strip_prefix(' '))
        .ok_or_else(missing)
}

fn parse_proposal(id: u64, text: &str) -> Result<Proposal, String> {
    let (head, body) = text.split_once("\n---\n").ok_or("no `---` line")?;
    let mut lines = head.lines();
    if lines.next() != Some("tfprop 1") {
        return Err("not a proposal (no `tfprop 1` first line)".to_owned());
    }
    let by = field(lines.next(), "by")?.to_owned();
    let at = field(lines.next(), "at")?.parse().map_err(|_| "bad `at`")?;
    let status = match field(lines.next(), "status")? {
        "auto" => Status::Auto,
        "pending" => Status::Pending,
        "refused" => Status::Refused,
        other => return Err(format!("unknown status `{other}`")),
    };
    let pol = field(lines.next(), "policy")?;
    let policy = pol
        .strip_prefix("step=")
        .and_then(|r| r.split_once(" cooldown="))
        .and_then(|(a, b)| {
            Some(Policy {
                step: a.parse().ok()?,
                cooldown: b.parse().ok()?,
            })
        })
        .ok_or("bad `policy` line")?;
    let nodes_line = field(lines.next(), "nodes")?;
    let nodes = nodes_line
        .split(',')
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    let reason = field(lines.next(), "reason")?.to_owned();
    let evidence = field(lines.next(), "evidence")?.to_owned();
    let mut why = Vec::new();
    for l in lines {
        why.push(
            l.strip_prefix("why ")
                .ok_or("expected `why` lines")?
                .to_owned(),
        );
    }
    let tree = Tree::parse(body).map_err(|e| e.to_string())?;
    Ok(Proposal {
        id,
        by,
        at,
        status,
        policy,
        nodes,
        reason,
        evidence,
        why,
        tree,
    })
}

fn decision_text(d: &Decision) -> String {
    format!(
        "tfdec 1\nby {}\nat {}\ncall {}\nnote {}\n",
        line(&d.by, 64),
        d.at,
        match d.call {
            Call::Approved => "approved",
            Call::Declined => "declined",
        },
        line(&d.note, 300)
    )
}

fn parse_decision(id: u64, text: &str) -> Result<Decision, String> {
    let mut lines = text.lines();
    if lines.next() != Some("tfdec 1") {
        return Err("not a decision (no `tfdec 1` first line)".to_owned());
    }
    let by = field(lines.next(), "by")?.to_owned();
    let at = field(lines.next(), "at")?.parse().map_err(|_| "bad `at`")?;
    let call = match field(lines.next(), "call")? {
        "approved" => Call::Approved,
        "declined" => Call::Declined,
        other => return Err(format!("unknown call `{other}`")),
    };
    let note = field(lines.next(), "note")?.to_owned();
    Ok(Decision {
        id,
        by,
        at,
        call,
        note,
    })
}

fn number(name: &str, ext: &str) -> Option<u64> {
    let n = name.strip_suffix(ext)?.strip_suffix('.')?;
    (n.len() == 10).then(|| n.parse().ok()).flatten()
}

fn write_unique(dir: &Path, tmp_name: &str, text: &str, name: &str) -> Result<bool, StoreError> {
    let tmp = dir.join(tmp_name);
    fs::write(&tmp, text).map_err(|e| io(&tmp, e))?;
    let made = match fs::hard_link(&tmp, dir.join(name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io(dir, e)),
    };
    let _ = fs::remove_file(&tmp);
    made
}

fn temp_name() -> String {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}.tmp",
        std::process::id(),
        CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Keep a proposal (its `id` is ignored) under the next number, and return that number.
pub fn save_proposal(ledger: &Path, p: &Proposal) -> Result<u64, StoreError> {
    let d = dir(ledger);
    fs::create_dir_all(&d).map_err(|e| io(&d, e))?;
    let mut next = fs::read_dir(&d)
        .map_err(|e| io(&d, e))?
        .flatten()
        .filter_map(|f| number(&f.file_name().to_string_lossy(), "prop"))
        .max()
        .map_or(1, |n| n + 1);
    loop {
        let mut mine = p.clone();
        mine.id = next;
        if write_unique(
            &d,
            &temp_name(),
            &proposal_text(&mine),
            &format!("{next:010}.prop"),
        )? {
            return Ok(next);
        }
        next += 1;
    }
}

/// Keep a person's decision. A proposal is decided once.
pub fn save_decision(ledger: &Path, dec: &Decision) -> Result<(), StoreError> {
    let d = dir(ledger);
    fs::create_dir_all(&d).map_err(|e| io(&d, e))?;
    if write_unique(
        &d,
        &temp_name(),
        &decision_text(dec),
        &format!("{:010}.dec", dec.id),
    )? {
        Ok(())
    } else {
        Err(StoreError::Decided(dec.id))
    }
}

/// Take back a decision whose consequence could not be carried out.
pub fn forget_decision(ledger: &Path, id: u64) -> Result<(), StoreError> {
    let p = dir(ledger).join(format!("{id:010}.dec"));
    fs::remove_file(&p).map_err(|e| io(&p, e))
}

/// The proposals and the files that could not be read as one (named, with why).
pub type Listing = (Vec<Entry>, Vec<(String, String)>);

/// Every proposal with its decision, oldest first, and the files that could not be read.
pub fn list(ledger: &Path) -> Result<Listing, StoreError> {
    let d = dir(ledger);
    let rd = match fs::read_dir(&d) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], vec![])),
        Err(e) => return Err(io(&d, e)),
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|f| f.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let (mut entries, mut bad) = (Vec::new(), Vec::new());
    let mut decisions = std::collections::BTreeMap::new();
    for name in names.iter().filter(|n| number(n, "dec").is_some()) {
        let id = number(name, "dec").expect("filtered");
        let parsed = fs::read_to_string(d.join(name))
            .map_err(|e| e.to_string())
            .and_then(|t| parse_decision(id, &t));
        match parsed {
            Ok(dec) => {
                decisions.insert(id, dec);
            }
            Err(why) => bad.push((name.clone(), why)),
        }
    }
    for name in names.iter().filter(|n| number(n, "prop").is_some()) {
        let id = number(name, "prop").expect("filtered");
        let parsed = fs::read_to_string(d.join(name))
            .map_err(|e| e.to_string())
            .and_then(|t| parse_proposal(id, &t));
        match parsed {
            Ok(proposal) => entries.push(Entry {
                decision: decisions.remove(&id),
                proposal,
            }),
            Err(why) => bad.push((name.clone(), why)),
        }
    }
    for (id, _) in decisions {
        bad.push((
            format!("{id:010}.dec"),
            "a decision with no proposal".to_owned(),
        ));
    }
    Ok((entries, bad))
}
