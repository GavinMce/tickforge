use std::collections::BTreeSet;

use tf_budget::{Tree, Usage};
use tf_synth::SplitMix64;

use super::*;

const D: u128 = 1_000_000_000;
const BAL: u128 = 100_000 * D;
const HOUR: Nanos = 3_600 * 1_000_000_000;

/// (group, share, soft, hard, [(strategy, share)])
type G<'a> = (&'a str, u32, u32, u32, &'a [(&'a str, u32)]);

fn tree(groups: &[G<'_>]) -> Tree {
    let mut s = String::from("budgets v1\n");
    for (g, share, soft, hard, strategies) in groups {
        s.push_str(&format!("group {g} {share} {soft} {hard}\n"));
        for (n, sh) in *strategies {
            s.push_str(&format!("strategy {g} {n} {sh}\n"));
        }
    }
    Tree::parse(&s).unwrap_or_else(|e| panic!("{e}\n{s}"))
}

/// day 50% (alpha 50%, beta 30%, 20% unassigned), swing 50% (gamma, delta 50% each).
fn base() -> Tree {
    tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 5_000), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ])
}

fn facts() -> Facts {
    Facts {
        now: 100 * 24 * HOUR,
        ..Facts::default()
    }
}

fn verdict(wanted: &Tree, f: &Facts, usage: &Usage) -> Verdict {
    decide(&Policy::default(), &base(), wanted, BAL, usage, f)
}

fn needs(v: Verdict) -> Vec<String> {
    match v {
        Verdict::NeedsPerson(w) => w,
        other => panic!("expected NeedsPerson, got {other:?}"),
    }
}

#[test]
fn a_small_cut_applies_on_its_own() {
    let w = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 4_500), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert_eq!(verdict(&w, &facts(), &Usage::new()), Verdict::Apply);
    // Several small cuts together, in a group and a strategy.
    let w = tree(&[
        ("day", 4_500, 300, 600, &[("alpha", 4_500), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 4_800), ("delta", 5_000)],
        ),
    ]);
    assert_eq!(verdict(&w, &facts(), &Usage::new()), Verdict::Apply);
}

#[test]
fn a_cut_larger_than_the_step_goes_to_a_person_and_says_which() {
    let w = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 3_900), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 4_900), ("delta", 5_000)],
        ),
    ]);
    let why = needs(verdict(&w, &facts(), &Usage::new()));
    assert_eq!(why.len(), 1, "{why:?}");
    assert!(
        why[0].starts_with("alpha: cut by 1100 basis points, more than the 1000"),
        "{why:?}"
    );
    // Exactly the step is allowed.
    let w = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 4_000), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert_eq!(verdict(&w, &facts(), &Usage::new()), Verdict::Apply);
}

#[test]
fn a_node_changed_recently_cannot_be_cut_again_by_an_agent() {
    let w = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 4_500), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    let mut f = facts();
    f.last_change.insert("alpha".into(), f.now - HOUR);
    let why = needs(verdict(&w, &f, &Usage::new()));
    assert_eq!(
        why,
        ["alpha: changed too recently for an agent to cut it again"]
    );
    // The day before the cooldown ends, and the very moment it does.
    f.last_change.insert("alpha".into(), f.now - 24 * HOUR + 1);
    assert!(matches!(
        verdict(&w, &f, &Usage::new()),
        Verdict::NeedsPerson(_)
    ));
    f.last_change.insert("alpha".into(), f.now - 24 * HOUR);
    assert_eq!(verdict(&w, &f, &Usage::new()), Verdict::Apply);
    // A change to another node does not count, and a clock before the change does not panic.
    f.last_change.clear();
    f.last_change.insert("beta".into(), f.now - HOUR);
    assert_eq!(verdict(&w, &f, &Usage::new()), Verdict::Apply);
    f.last_change.insert("alpha".into(), f.now + HOUR);
    assert!(matches!(
        verdict(&w, &f, &Usage::new()),
        Verdict::NeedsPerson(_)
    ));
}

#[test]
fn an_increase_always_needs_a_person() {
    let w = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 5_500), ("beta", 3_000)]), // room: 20% unassigned
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert_eq!(
        needs(verdict(&w, &facts(), &Usage::new())),
        ["alpha: an increase needs a person"]
    );
    // Even a tiny one, and a loosened loss limit, and a mixed loss change.
    let tiny = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 5_001), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert!(matches!(
        verdict(&tiny, &facts(), &Usage::new()),
        Verdict::NeedsPerson(_)
    ));
    for (soft, hard) in [(301, 600), (300, 601), (350, 500), (250, 700)] {
        let w = tree(&[
            (
                "day",
                5_000,
                soft,
                hard,
                &[("alpha", 5_000), ("beta", 3_000)],
            ),
            (
                "swing",
                5_000,
                300,
                600,
                &[("gamma", 5_000), ("delta", 5_000)],
            ),
        ]);
        let why = needs(verdict(&w, &facts(), &Usage::new()));
        assert_eq!(why, ["day: an increase needs a person"], "{soft}/{hard}");
    }
}

#[test]
fn tightening_loss_limits_applies_on_its_own() {
    for (soft, hard) in [(250, 600), (300, 500), (200, 400)] {
        let w = tree(&[
            (
                "day",
                5_000,
                soft,
                hard,
                &[("alpha", 5_000), ("beta", 3_000)],
            ),
            (
                "swing",
                5_000,
                300,
                600,
                &[("gamma", 5_000), ("delta", 5_000)],
            ),
        ]);
        assert_eq!(
            verdict(&w, &facts(), &Usage::new()),
            Verdict::Apply,
            "{soft}/{hard}"
        );
    }
    // A very large tightening is a cut larger than the step.
    let w = tree(&[
        (
            "day",
            5_000,
            300,
            1_900,
            &[("alpha", 5_000), ("beta", 3_000)],
        ),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert!(
        matches!(
            verdict(&w, &facts(), &Usage::new()),
            Verdict::NeedsPerson(_)
        ),
        "a raise of the hard limit is an increase"
    );
    let big_base = tree(&[
        (
            "day",
            5_000,
            300,
            3_000,
            &[("alpha", 5_000), ("beta", 3_000)],
        ),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    let tight = tree(&[
        (
            "day",
            5_000,
            300,
            1_500,
            &[("alpha", 5_000), ("beta", 3_000)],
        ),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    let v = decide(
        &Policy::default(),
        &big_base,
        &tight,
        BAL,
        &Usage::new(),
        &facts(),
    );
    assert!(
        matches!(v, Verdict::NeedsPerson(w) if w[0].contains("1500 basis points")),
        "tightened by more than the step"
    );
}

#[test]
fn an_increase_for_a_strategy_in_drawdown_is_refused_not_sent_to_a_person() {
    let mut f = facts();
    f.drawdown.insert("alpha".into());
    let up = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 5_500), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    let v = verdict(&up, &f, &Usage::new());
    assert!(
        matches!(&v, Verdict::Refused(m) if m.contains("alpha is in drawdown")),
        "{v:?}"
    );
    // Raising its group is the same; raising a different strategy is not blocked.
    let group_up = decide(
        &Policy::default(),
        &tree(&[
            ("day", 4_000, 300, 600, &[("alpha", 5_000), ("beta", 3_000)]),
            (
                "swing",
                5_000,
                300,
                600,
                &[("gamma", 5_000), ("delta", 5_000)],
            ),
        ]),
        &base(),
        BAL,
        &Usage::new(),
        &f,
    );
    assert!(
        matches!(&group_up, Verdict::Refused(m) if m.contains("alpha is in drawdown: no increase for day")),
        "{group_up:?}"
    );
    let other = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 5_000), ("beta", 3_500)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert!(matches!(
        verdict(&other, &f, &Usage::new()),
        Verdict::NeedsPerson(_)
    ));
    // A cut is still allowed for a strategy in drawdown: it is what one wants.
    let cut = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 4_500), ("beta", 3_000)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert_eq!(verdict(&cut, &f, &Usage::new()), Verdict::Apply);
    // A cut to one node paired with an increase to a node in drawdown is refused as a whole.
    let swap = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 5_500), ("beta", 2_500)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    assert!(matches!(
        verdict(&swap, &f, &Usage::new()),
        Verdict::Refused(_)
    ));
}

#[test]
fn what_the_rules_do_not_allow_is_refused_with_the_reason() {
    let u = Usage::new().with("alpha", 20_000 * D); // alpha's budget is $25,000
    let below = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 3_000), ("beta", 3_000)]), // $15,000 < $20,000 in use
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    let v = verdict(&below, &facts(), &u);
    assert!(
        matches!(&v, Verdict::Refused(m) if m.starts_with("alpha:") && m.contains("in use")),
        "{v:?}"
    );
    // A different shape.
    let shape = tree(&[("day", 5_000, 300, 600, &[("alpha", 5_000)])]);
    assert!(
        matches!(verdict(&shape, &facts(), &Usage::new()), Verdict::Refused(m) if m.contains("same groups"))
    );
    // No change.
    assert_eq!(
        verdict(&base(), &facts(), &Usage::new()),
        Verdict::Refused("it changes nothing".to_owned())
    );
}

#[test]
fn a_person_sees_every_reason_not_only_the_first() {
    let w = tree(&[
        ("day", 5_000, 300, 600, &[("alpha", 3_000), ("beta", 3_500)]),
        (
            "swing",
            5_000,
            300,
            600,
            &[("gamma", 5_000), ("delta", 5_000)],
        ),
    ]);
    let mut f = facts();
    f.last_change.insert("alpha".into(), f.now - HOUR);
    let why = needs(verdict(&w, &f, &Usage::new()));
    assert_eq!(why.len(), 3, "{why:?}");
    assert!(why.iter().any(|m| m.contains("cut by 2000")));
    assert!(why.iter().any(|m| m.contains("changed too recently")));
    assert!(why.iter().any(|m| m == "beta: an increase needs a person"));
}

fn random_wanted(rng: &mut SplitMix64, t: &Tree, only_down: bool) -> Option<Tree> {
    let shift = |rng: &mut SplitMix64, span: u64| -> i64 {
        let v = (rng.next_u64() % (2 * span + 1)) as i64 - span as i64;
        if only_down { -v.abs() } else { v }
    };
    let mut s = String::from("budgets v1\n");
    let mut top = 0u32;
    for g in t.groups() {
        let share = (g.share as i64 + shift(rng, 1_000)).clamp(0, 10_000) as u32;
        top += share;
        let soft = (g.loss.soft as i64 + shift(rng, 100)).max(1) as u32;
        let hard = (g.loss.hard as i64 + shift(rng, 200)).max(2) as u32;
        s.push_str(&format!("group {} {share} {soft} {hard}\n", g.id));
        let mut inner = 0u32;
        for st in &g.strategies {
            let sh = (st.share as i64 + shift(rng, 1_000)).clamp(0, 10_000) as u32;
            inner += sh;
            s.push_str(&format!("strategy {} {} {sh}\n", g.id, st.id));
        }
        if inner > 10_000 {
            return None;
        }
    }
    if top > 10_000 {
        return None;
    }
    Tree::parse(&s).ok()
}

#[test]
fn whatever_applies_on_its_own_never_raises_a_budget_or_a_loss_limit() {
    let mut rng = SplitMix64::new(0xA6E27);
    let t = base();
    let (mut applied, mut person, mut refused) = (0, 0, 0);
    for _ in 0..4_000 {
        let only_down = rng.next_u64() % 2 == 0;
        let Some(w) = random_wanted(&mut rng, &t, only_down) else {
            continue;
        };
        let mut f = facts();
        for st in ["alpha", "beta", "gamma", "delta"] {
            match rng.next_u64() % 6 {
                0 => {
                    f.drawdown.insert(st.to_owned());
                }
                1 => {
                    f.last_change
                        .insert(st.to_owned(), f.now - (rng.next_u64() % 48) * HOUR);
                }
                _ => {}
            }
        }
        let mut usage = Usage::new();
        for st in ["alpha", "beta", "gamma", "delta"] {
            let b = t.strategy_budget(BAL, st).unwrap();
            usage = usage.with(st, b * (rng.next_u64() % 100) as u128 / 100);
        }
        match decide(&Policy::default(), &t, &w, BAL, &usage, &f) {
            Verdict::Apply => {
                applied += 1;
                assert!(check_edit(&t, &w, BAL, &usage).is_ok());
                for st in ["alpha", "beta", "gamma", "delta"] {
                    assert!(
                        w.strategy_budget(BAL, st) <= t.strategy_budget(BAL, st),
                        "{st} budget rose:\n{}",
                        w.render()
                    );
                    let (sw, hw) = w.loss_amounts(BAL, st).unwrap();
                    let (st0, ht0) = t.loss_amounts(BAL, st).unwrap();
                    assert!(
                        sw <= st0 && hw <= ht0,
                        "{st} loss limit loosened:\n{}",
                        w.render()
                    );
                }
                for c in diff(&t, &w) {
                    let (kind, node, size) = classify(&c);
                    assert_eq!(kind, Some(false));
                    assert!(size <= Policy::default().step, "{c:?}");
                    let last = f.last_change.get(&node);
                    assert!(
                        last.is_none_or(|l| f.now - l >= Policy::default().cooldown),
                        "{node} cooling down"
                    );
                }
            }
            Verdict::NeedsPerson(_) => person += 1,
            Verdict::Refused(_) => {
                refused += 1;
            }
        }
        // An increase for anything in drawdown is never left to a person.
        let v = decide(&Policy::default(), &t, &w, BAL, &usage, &f);
        if check_edit(&t, &w, BAL, &usage).is_ok() {
            for c in diff(&t, &w) {
                let (kind, node, _) = classify(&c);
                if kind == Some(true)
                    && strategies_of(&w, &node)
                        .iter()
                        .any(|s| f.drawdown.contains(*s))
                {
                    assert!(
                        matches!(v, Verdict::Refused(_)),
                        "{c:?} in drawdown got {v:?}"
                    );
                }
            }
        }
    }
    assert!(
        applied > 100 && person > 100 && refused > 100,
        "{applied} {person} {refused}"
    );
    let _: BTreeSet<u8> = BTreeSet::new();
}

// ------------------------------------------------------------------ store and flow

use std::path::{Path, PathBuf};

use tf_core::Px;
use tf_ledger::{FileStore, Journal as Jnl};
use tf_risk::{Budgets, GapRule, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::Decision as Dec;

use crate::flow::{self, FlowError};
use crate::store::{self, Call, State, Status};

const P: i64 = 1_000_000_000;
const SEC: Nanos = 1_000_000_000;
const DAY: Nanos = 86_400 * SEC;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-proposals-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn limits() -> Limits {
    Limits::new(5_000 * D, 1_000, 20_000 * D, 400 * D, 6, 10 * SEC)
        .unwrap()
        .with_gap_rule(GapRule::new(100_000 * D, 20_000, 1000).unwrap())
}

/// day 100%: alpha 50%, beta 30%, gamma 20%, on a $30,000 balance.
fn ledger_tree() -> Tree {
    tree(&[(
        "day",
        10_000,
        300,
        600,
        &[("alpha", 5_000), ("beta", 3_000), ("gamma", 2_000)],
    )])
}

fn intent(strategy: u16, seq: u64, side: Side, purpose: Purpose, px: i64) -> Intent {
    Intent {
        id: IntentId {
            strategy: StrategyId(strategy),
            seq,
        },
        instrument: 0,
        side,
        qty: 100,
        purpose,
        pricing: Pricing::Limit(Px::from_raw(px)),
        protect: (purpose == Purpose::Open).then(|| Protective {
            stop_trigger: Px::from_raw(px * 9 / 10),
            stop_limit: None,
            take_profit: None,
        }),
        tif: Tif::Day,
        ts: seq * SEC,
        reason: 1,
    }
}

fn fill(j: &mut Jnl<FileStore>, i: &Intent, px: i64) {
    let Dec::Accepted(o) = j.decide(i, i.ts).unwrap() else {
        panic!("{i:?}")
    };
    j.ack(o, i.ts).unwrap();
    j.fill(o, 100, Px::from_raw(px), i.ts).unwrap();
}

/// A round trip for `strategy` that wins or loses, ending at second `seq + 10` of its day.
fn trip(j: &mut Jnl<FileStore>, strategy: u16, seq: u64, exit: i64) {
    fill(
        j,
        &intent(strategy, seq, Side::Buy, Purpose::Open, 5 * P),
        5 * P,
    );
    fill(
        j,
        &intent(strategy, seq + 10, Side::Sell, Purpose::Close, exit * P),
        exit * P,
    );
}

fn open_ledger(dir: &Path, with_budgets: bool) -> Jnl<FileStore> {
    let (mut j, _) = Jnl::open(FileStore::open(dir).unwrap(), limits(), 1).unwrap();
    if with_budgets {
        let ids = [(1, "alpha"), (2, "beta"), (3, "gamma")].map(|(n, s)| (n, s.to_owned()));
        j.set_budgets(
            Some(Budgets::new(ledger_tree(), 30_000 * D, ids).unwrap()),
            SEC,
        )
        .unwrap();
    }
    j
}

fn text(alpha: u32, beta: u32, gamma: u32) -> String {
    tree(&[(
        "day",
        10_000,
        300,
        600,
        &[("alpha", alpha), ("beta", beta), ("gamma", gamma)],
    )])
    .render()
}

fn submit(dir: &Path, by: &str, t: &str, at: Nanos) -> Result<flow::Submitted, FlowError> {
    flow::submit(dir, &Policy::default(), by, "because", "evidence", t, at)
}

#[test]
fn a_cut_within_bounds_is_queued_on_its_own_and_recorded() {
    let dir = scratch("auto");
    let j = open_ledger(&dir, true);
    let log = std::fs::read(dir.join("ledger.log")).unwrap();
    let r = submit(&dir, "risk-agent", &text(4_500, 3_000, 2_000), 100 * DAY).unwrap();
    assert_eq!((r.id, r.status), (1, Status::Auto));
    assert!(
        r.why.iter().any(|w| w.contains("queued as 0000000001.req")),
        "{:?}",
        r.why
    );
    // In the inbox, by the agent, waiting for the engine; the ledger itself is untouched.
    let (waiting, _) = tf_ledger::inbox::pending(&dir).unwrap();
    assert_eq!(waiting.len(), 1);
    assert!(
        waiting[0].by.starts_with("risk-agent (within bounds"),
        "{}",
        waiting[0].by
    );
    assert_eq!(
        waiting[0].tree.as_ref().unwrap().render(),
        text(4_500, 3_000, 2_000)
    );
    assert_eq!(std::fs::read(dir.join("ledger.log")).unwrap(), log);
    // And a record of it, with the evidence and the policy.
    let (entries, bad) = store::list(&dir).unwrap();
    assert!(bad.is_empty());
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.state(), State::Scheduled);
    assert_eq!(
        (
            e.proposal.by.as_str(),
            e.proposal.reason.as_str(),
            e.proposal.evidence.as_str()
        ),
        ("risk-agent", "because", "evidence")
    );
    assert_eq!(e.proposal.nodes, ["alpha"]);
    assert_eq!(e.proposal.policy, Policy::default());
    assert_eq!(e.proposal.at, 100 * DAY);
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_increase_waits_for_a_person_who_can_approve_or_decline_it_once() {
    let dir = scratch("person");
    let mut j = open_ledger(&dir, true);
    let _ = &mut j;
    // Room for an increase: lower gamma first by proposing alpha up and gamma down.
    let r = submit(&dir, "growth-agent", &text(5_500, 3_000, 1_500), 100 * DAY).unwrap();
    assert_eq!(r.status, Status::Pending);
    assert!(
        r.why
            .iter()
            .any(|w| w == "alpha: an increase needs a person"),
        "{:?}",
        r.why
    );
    assert_eq!(
        tf_ledger::inbox::pending(&dir).unwrap().0.len(),
        0,
        "nothing is queued until a person approves"
    );
    assert_eq!(store::list(&dir).unwrap().0[0].state(), State::Waiting);
    // Approve: queued under the person's name, and decided.
    let name = flow::approve(&dir, 1, "gavin", "looks right", 101 * DAY).unwrap();
    assert_eq!(name, "0000000001.req");
    let (waiting, _) = tf_ledger::inbox::pending(&dir).unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].by, "gavin, approving a proposal by growth-agent");
    assert_eq!(
        waiting[0].tree.as_ref().unwrap().render(),
        text(5_500, 3_000, 1_500)
    );
    let e = &store::list(&dir).unwrap().0[0];
    assert_eq!(e.state(), State::Approved);
    let d = e.decision.as_ref().unwrap();
    assert_eq!(
        (d.by.as_str(), d.call, d.note.as_str(), d.at),
        ("gavin", Call::Approved, "looks right", 101 * DAY)
    );
    // Decided once: neither approving nor declining again.
    assert_eq!(
        flow::approve(&dir, 1, "gavin", "", 0),
        Err(FlowError::NotWaiting(1, State::Approved))
    );
    assert_eq!(
        flow::decline(&dir, 1, "gavin", "", 0),
        Err(FlowError::NotWaiting(1, State::Approved))
    );
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0.len(), 1);
    // Declining another leaves nothing queued and cannot be approved afterwards.
    let r = submit(&dir, "growth-agent", &text(5_000, 3_500, 1_500), 102 * DAY).unwrap();
    assert_eq!((r.id, r.status), (2, Status::Pending));
    flow::decline(&dir, 2, "gavin", "not now", 103 * DAY).unwrap();
    assert_eq!(store::list(&dir).unwrap().0[1].state(), State::Declined);
    assert_eq!(
        flow::approve(&dir, 2, "gavin", "", 0),
        Err(FlowError::NotWaiting(2, State::Declined))
    );
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0.len(), 1);
    // No such proposal.
    assert_eq!(
        flow::approve(&dir, 9, "g", "", 0),
        Err(FlowError::NotFound(9))
    );
    assert_eq!(
        flow::decline(&dir, 9, "g", "", 0),
        Err(FlowError::NotFound(9))
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_increase_for_a_strategy_that_lost_its_limit_is_refused_and_so_is_approving_one_made_before() {
    let dir = scratch("latched");
    let mut j = open_ledger(&dir, true);
    // Proposed while beta is fine: waits for a person.
    let before = submit(&dir, "growth-agent", &text(5_000, 3_500, 1_500), 100 * DAY).unwrap();
    assert_eq!(before.status, Status::Pending);
    // Then beta loses past its soft limit ($270 of $9,000): two trips of -$150.
    trip(&mut j, 2, 1_000, 3);
    trip(&mut j, 2, 1_100, 3);
    assert_eq!(j.check_loss_limits(1_200 * SEC).unwrap().len(), 1);
    assert_eq!(
        flow::drawdown(&dir)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        ["beta"]
    );
    // A new increase for beta is refused outright, and recorded as such.
    let r = submit(&dir, "growth-agent", &text(5_000, 3_600, 1_400), 101 * DAY).unwrap();
    assert_eq!(r.status, Status::Refused);
    assert!(r.why[0].contains("beta is in drawdown"), "{:?}", r.why);
    let entries = store::list(&dir).unwrap().0;
    assert_eq!(entries[1].state(), State::Refused);
    assert_eq!(
        flow::approve(&dir, 2, "gavin", "", 0),
        Err(FlowError::NotWaiting(2, State::Refused))
    );
    // The earlier proposal can no longer be approved either, and nothing was decided or queued.
    let e = flow::approve(&dir, 1, "gavin", "", 102 * DAY).unwrap_err();
    assert!(
        matches!(&e, FlowError::Refused(m) if m.contains("beta is in drawdown")),
        "{e:?}"
    );
    assert_eq!(store::list(&dir).unwrap().0[0].state(), State::Waiting);
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0.len(), 0);
    // A cut for beta is still welcome.
    let cut = submit(&dir, "risk-agent", &text(5_000, 2_800, 2_000), 103 * DAY).unwrap();
    assert_eq!(cut.status, Status::Auto, "{:?}", cut.why);
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_strategy_down_over_its_last_three_sessions_is_in_drawdown_without_being_stopped() {
    let dir = scratch("sessions");
    let mut j = open_ledger(&dir, true);
    let mut t = 1_000;
    for loss in [4, 4, 4] {
        trip(&mut j, 2, t, loss); // -$100 each, below the $270 soft limit
        j.new_day((t + 200) * SEC).unwrap();
        t += 100_000;
    }
    assert!(flow::drawdown(&dir).unwrap().contains("beta"));
    assert!(
        !flow::drawdown(&dir).unwrap().contains("alpha"),
        "alpha never traded"
    );
    // One good session among the last three is not enough if the three still sum below zero; a
    // strong enough one is.
    trip(&mut j, 2, t, 9); // +$400
    assert!(
        !flow::drawdown(&dir).unwrap().contains("beta"),
        "-100 -100 +400 is up"
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_agents_cooldown_is_kept_from_what_was_applied_and_approved() {
    let dir = scratch("cooldown");
    let j = open_ledger(&dir, true);
    let t0 = 100 * DAY;
    assert_eq!(
        submit(&dir, "a", &text(4_800, 3_000, 2_000), t0)
            .unwrap()
            .status,
        Status::Auto
    );
    // An hour later alpha is still cooling down; another node is not.
    let r = submit(&dir, "a", &text(4_600, 3_000, 2_000), t0 + 3_600 * SEC).unwrap();
    assert_eq!(r.status, Status::Pending);
    assert!(
        r.why[0].contains("alpha: changed too recently"),
        "{:?}",
        r.why
    );
    assert_eq!(
        submit(&dir, "a", &text(5_000, 2_800, 2_000), t0 + 3_600 * SEC)
            .unwrap()
            .status,
        Status::Auto
    );
    // A day later it may be cut again.
    assert_eq!(
        submit(&dir, "a", &text(4_600, 3_000, 2_000), t0 + DAY)
            .unwrap()
            .status,
        Status::Auto
    );
    // A change a person approved counts too, from the moment of approval, not of the proposal.
    let up = submit(&dir, "b", &text(5_200, 3_000, 1_800), t0 + 5 * DAY).unwrap();
    assert_eq!(up.status, Status::Pending);
    flow::approve(&dir, up.id, "gavin", "", t0 + 7 * DAY).unwrap();
    let after = submit(
        &dir,
        "a",
        &text(4_000, 3_000, 2_000),
        t0 + 7 * DAY + 3_600 * SEC,
    )
    .unwrap();
    assert_eq!(after.status, Status::Pending);
    assert!(
        after
            .why
            .iter()
            .any(|w| w.contains("alpha: changed too recently")),
        "{:?}",
        after.why
    );
    // The latest change counts, even when an older proposal was approved after a newer one applied.
    let dir2 = scratch("cooldown-latest");
    let j2 = open_ledger(&dir2, true);
    let first = submit(&dir2, "b", &text(5_200, 3_000, 1_800), t0).unwrap(); // pending
    assert_eq!(
        submit(&dir2, "a", &text(4_800, 3_000, 2_000), t0 + DAY)
            .unwrap()
            .status,
        Status::Auto
    );
    flow::approve(&dir2, first.id, "gavin", "", t0 + 10 * DAY).unwrap();
    let r = submit(
        &dir2,
        "a",
        &text(4_000, 3_000, 2_000),
        t0 + 10 * DAY + 3_600 * SEC,
    )
    .unwrap();
    assert_eq!(r.status, Status::Pending, "{:?}", r.why);
    drop(j2);
    let _ = std::fs::remove_dir_all(&dir2);
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn what_is_not_a_budget_tree_or_cannot_be_applied_is_turned_away_and_a_ledger_without_budgets_has_nothing_to_propose()
 {
    let dir = scratch("turned-away");
    let j = open_ledger(&dir, true);
    for bad in [
        "junk",
        "budgets v1\ngroup day 10000 300 600\nstrategy day alpha 6000\nstrategy day beta 6000\n",
        "",
    ] {
        assert!(
            matches!(submit(&dir, "a", bad, 1), Err(FlowError::Unreadable(_))),
            "{bad:?}"
        );
    }
    let over = submit(
        &dir,
        "a",
        "budgets v1\ngroup day 10000 300 600\nstrategy day alpha 6000\nstrategy day beta 6000\nstrategy day gamma 0\n",
        1,
    );
    assert!(
        matches!(&over, Err(FlowError::Unreadable(m)) if m.contains("more than the whole")),
        "children over the parent: {over:?}"
    );
    assert!(
        store::list(&dir).unwrap().0.is_empty(),
        "nothing is recorded for text that is not a tree"
    );
    // A tree that parses but the rules refuse is recorded as refused.
    let r = submit(
        &dir,
        "a",
        "budgets v1\ngroup other 10000 300 600\nstrategy other alpha 5000\n",
        1,
    )
    .unwrap();
    assert_eq!(r.status, Status::Refused);
    assert!(r.why[0].contains("same groups"), "{:?}", r.why);
    let none = submit(&dir, "a", &text(5_000, 3_000, 2_000), 1).unwrap();
    assert_eq!(
        (none.status, none.why[0].as_str()),
        (Status::Refused, "it changes nothing")
    );
    drop(j);
    let bare = scratch("no-budgets");
    let jb = open_ledger(&bare, false);
    assert!(
        matches!(submit(&bare, "a", &text(5_000, 3_000, 2_000), 1), Err(FlowError::Ledger(m)) if m.contains("no budgets"))
    );
    drop(jb);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bare);
}

#[test]
fn the_files_are_one_line_per_field_numbered_decided_once_and_damage_is_reported() {
    let dir = scratch("files");
    let t = ledger_tree();
    let mk = |reason: &str, evidence: &str| store::Proposal {
        id: 0,
        by: "agent".into(),
        at: 7,
        status: Status::Pending,
        policy: Policy {
            step: 500,
            cooldown: 9,
        },
        nodes: vec!["alpha".into(), "beta".into()],
        reason: reason.into(),
        evidence: evidence.into(),
        why: vec!["one".into(), "two".into()],
        tree: t.clone(),
    };
    let a = store::save_proposal(&dir, &mk("line one\nline two", &"e".repeat(900))).unwrap();
    let b = store::save_proposal(&dir, &mk("", "")).unwrap();
    assert_eq!((a, b), (1, 2));
    let (entries, bad) = store::list(&dir).unwrap();
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(
        entries[0].proposal.reason, "line one line two",
        "a line break cannot start another field"
    );
    assert_eq!(
        entries[0].proposal.evidence.len(),
        600,
        "evidence is kept to a length"
    );
    assert_eq!(entries[1].proposal.reason, "");
    assert_eq!(
        entries[0].proposal.policy,
        Policy {
            step: 500,
            cooldown: 9
        }
    );
    assert_eq!(entries[0].proposal.nodes, ["alpha", "beta"]);
    assert_eq!(entries[0].proposal.why, ["one", "two"]);
    assert_eq!(entries[0].proposal.tree, t);
    // Decided once; a decision can be taken back; the number is not reused.
    let d = store::Decision {
        id: 1,
        by: "p".into(),
        at: 8,
        call: Call::Declined,
        note: "n".into(),
    };
    store::save_decision(&dir, &d).unwrap();
    assert_eq!(
        store::save_decision(&dir, &d),
        Err(store::StoreError::Decided(1))
    );
    assert_eq!(store::list(&dir).unwrap().0[0].decision, Some(d.clone()));
    store::forget_decision(&dir, 1).unwrap();
    assert_eq!(store::list(&dir).unwrap().0[0].decision, None);
    assert!(store::forget_decision(&dir, 1).is_err());
    assert_eq!(store::save_proposal(&dir, &mk("c", "c")).unwrap(), 3);
    // Damage is named and the rest still list.
    let d2 = store::dir(&dir);
    std::fs::write(d2.join("0000000004.prop"), "hello").unwrap();
    std::fs::write(d2.join("0000000005.prop"), "tfprop 1\nby x\nat y\n").unwrap();
    std::fs::write(
        d2.join("0000000006.dec"),
        "tfdec 1\nby x\nat 1\ncall maybe\nnote\n",
    )
    .unwrap();
    std::fs::write(
        d2.join("0000000002.dec"),
        "tfdec 1\nby x\nat 1\ncall approved\nnote\n",
    )
    .unwrap();
    std::fs::write(
        d2.join("0000000009.dec"),
        "tfdec 1\nby x\nat 1\ncall approved\nnote\n",
    )
    .unwrap();
    std::fs::write(d2.join("notes.txt"), "ignored").unwrap();
    let (entries, bad) = store::list(&dir).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].decision.as_ref().unwrap().call, Call::Approved);
    let names: Vec<&str> = bad.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "0000000006.dec",
            "0000000004.prop",
            "0000000005.prop",
            "0000000009.dec"
        ]
    );
    assert!(bad[3].1.contains("no proposal"), "{bad:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 100 shares bought at `entry` dollars and sold at `exit` dollars.
fn swing(j: &mut Jnl<FileStore>, strategy: u16, seq: u64, entry: i64, exit: i64) {
    fill(
        j,
        &intent(strategy, seq, Side::Buy, Purpose::Open, entry * P),
        entry * P,
    );
    fill(
        j,
        &intent(strategy, seq + 10, Side::Sell, Purpose::Close, exit * P),
        exit * P,
    );
}

#[test]
fn a_strategy_stopped_by_a_loss_limit_is_in_drawdown_even_if_it_is_up_over_three_sessions() {
    // Beta (budget $9,000: soft limit $270, hard $540) made $1,000 yesterday and loses today.
    for (entry, exit, what) in [(5, 2, "soft"), (20, 14, "hard")] {
        let dir = scratch(&format!("latched-{what}"));
        let mut j = open_ledger(&dir, true);
        swing(&mut j, 2, 1_000, 5, 15);
        j.new_day(1_200 * SEC).unwrap();
        swing(&mut j, 2, 100_000, entry, exit);
        assert!(
            !j.check_loss_limits(100_200 * SEC).unwrap().is_empty(),
            "{what}"
        );
        assert!(
            flow::drawdown(&dir).unwrap().contains("beta"),
            "{what}: stopped, though three sessions are up"
        );
        drop(j);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn drawdown_is_the_last_three_sessions_taken_together_and_zero_is_not_down() {
    let beta_in_drawdown = |sessions: &[(i64, i64)], name: &str| {
        let dir = scratch(name);
        let mut j = open_ledger(&dir, true);
        let mut t = 1_000;
        for (k, (entry, exit)) in sessions.iter().enumerate() {
            swing(&mut j, 2, t, *entry, *exit);
            if k + 1 < sessions.len() {
                j.new_day((t + 200) * SEC).unwrap();
            }
            t += 100_000;
        }
        let down = flow::drawdown(&dir).unwrap().contains("beta");
        drop(j);
        let _ = std::fs::remove_dir_all(&dir);
        down
    };
    // The newest two lost but the third (+$400) makes the three positive.
    assert!(!beta_in_drawdown(&[(5, 9), (5, 4), (5, 4)], "dd-a"));
    // Exactly zero is not down.
    assert!(!beta_in_drawdown(&[(5, 6), (5, 4)], "dd-b"));
    // Only the last three count: an old big loss is forgiven.
    assert!(!beta_in_drawdown(
        &[(10, 5), (5, 6), (5, 6), (5, 6)],
        "dd-c"
    ));
    // Down is down.
    assert!(beta_in_drawdown(&[(5, 6), (5, 4), (5, 3)], "dd-d"));
}

#[test]
fn an_approval_that_cannot_be_queued_is_not_left_as_decided() {
    let dir = scratch("rollback");
    let j = open_ledger(&dir, true);
    let r = submit(&dir, "g", &text(5_200, 3_000, 1_800), 100 * DAY).unwrap();
    assert_eq!(r.status, Status::Pending);
    // The inbox cannot be made: something that is not a directory is in its place.
    std::fs::write(dir.join("inbox"), "in the way").unwrap();
    let e = flow::approve(&dir, r.id, "gavin", "", 101 * DAY).unwrap_err();
    assert!(matches!(e, FlowError::Store(_)), "{e:?}");
    assert_eq!(
        store::list(&dir).unwrap().0[0].state(),
        State::Waiting,
        "still waiting, not approved without being queued"
    );
    // Cleared, it can be approved.
    std::fs::remove_file(dir.join("inbox")).unwrap();
    flow::approve(&dir, r.id, "gavin", "", 102 * DAY).unwrap();
    assert_eq!(store::list(&dir).unwrap().0[0].state(), State::Approved);
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}
