use std::collections::BTreeMap;

use crate::review::*;

const D: i64 = 1_000_000_000;

fn o(net: i64, dd: i64, trades: i64, dangerous: i64) -> Outcome {
    Outcome {
        net: net * D,
        max_drawdown: dd * D,
        trades,
        dangerous_trades: dangerous,
    }
}

/// `n` sessions in which the candidate matches the base.
fn same(n: usize) -> Vec<Paired> {
    (0..n)
        .map(|i| Paired {
            seed: i as u64,
            base: o(100, 10, 2, 0),
            cand: o(100, 10, 2, 0),
        })
        .collect()
}

fn gate<'a>(r: &'a Review, name: &str) -> &'a Gate {
    r.gates.iter().find(|g| g.name == name).unwrap()
}

fn failed(r: &Review) -> Vec<&'static str> {
    r.gates.iter().filter(|g| !g.pass).map(|g| g.name).collect()
}

#[test]
fn a_candidate_that_does_the_same_in_enough_sessions_is_accepted() {
    let r = evaluate(&same(24), false, false, &Policy::default());
    assert_eq!(r.verdict, Verdict::Accepted);
    assert!(failed(&r).is_empty(), "{:?}", r.gates);
    assert_eq!(r.gates.len(), 8);
}

#[test]
fn each_gate_rejects_on_its_own_and_only_on_its_own() {
    let p = Policy::default();
    // Not different.
    assert_eq!(failed(&evaluate(&same(24), true, false, &p)), ["different"]);
    // Too few sessions (11 < 12), and exactly enough passes.
    assert_eq!(failed(&evaluate(&same(11), false, false, &p)), ["sessions"]);
    assert!(failed(&evaluate(&same(12), false, false, &p)).is_empty());
    // Too few trades: 12 sessions x (2 + 2) = 48 is fine; make them trade little.
    let quiet: Vec<_> = (0..12)
        .map(|i| Paired {
            seed: i,
            base: o(0, 0, 0, 0),
            cand: o(0, 0, 0, 0),
        })
        .collect();
    assert_eq!(failed(&evaluate(&quiet, false, false, &p)), ["trades"]);
    // A dangerous-scenario entry the base did not make.
    let mut v = same(24);
    v[3].cand.dangerous_trades = 1;
    assert_eq!(failed(&evaluate(&v, false, false, &p)), ["dangerous"]);
    // ...is fine if the base made as many.
    v[3].base.dangerous_trades = 1;
    assert!(failed(&evaluate(&v, false, false, &p)).is_empty());
    // Net P&L lower overall (and worse in one session, which breadth allows).
    let mut v = same(24);
    v[0].cand.net -= D;
    assert_eq!(failed(&evaluate(&v, false, false, &p)), ["net"]);
    // A deeper worst drawdown (net unchanged).
    let mut v = same(24);
    v[5].cand.max_drawdown += 1;
    assert_eq!(failed(&evaluate(&v, false, false, &p)), ["drawdown"]);
}

#[test]
fn breadth_catches_a_gain_that_comes_from_a_few_sessions_while_most_get_worse() {
    let p = Policy::default();
    let mut v = same(24);
    // 7 of 24 worse (limit is 24 * 250 / 1000 = 6), made up for by one big win.
    for s in v.iter_mut().take(7) {
        s.cand.net -= D;
    }
    v[20].cand.net += 100 * D;
    let r = evaluate(&v, false, false, &p);
    assert_eq!(failed(&r), ["breadth"], "{:?}", r.gates);
    assert!(
        gate(&r, "breadth")
            .detail
            .contains("better in 1, worse in 7 of 24")
    );
    // Six worse is allowed.
    v[6].cand.net += D;
    assert!(failed(&evaluate(&v, false, false, &p)).is_empty());
}

#[test]
fn the_tolerances_in_the_policy_are_what_they_say() {
    let mut v = same(24);
    v[0].cand.net -= 5 * D;
    v[1].cand.max_drawdown += 3 * D;
    let strict = Policy::default();
    assert_eq!(
        failed(&evaluate(&v, false, false, &strict)),
        ["net", "drawdown"]
    );
    let loose = Policy {
        allow_net_drop: 5 * D,
        allow_drawdown_rise: 3 * D,
        ..strict
    };
    assert!(failed(&evaluate(&v, false, false, &loose)).is_empty());
    let almost = Policy {
        allow_net_drop: 5 * D - 1,
        allow_drawdown_rise: 3 * D - 1,
        ..strict
    };
    assert_eq!(
        failed(&evaluate(&v, false, false, &almost)),
        ["net", "drawdown"]
    );
    let wide = Policy {
        min_sessions: 25,
        ..strict
    };
    assert_eq!(
        failed(&evaluate(&same(24), false, false, &wide)),
        ["sessions"]
    );
}

#[test]
fn loosening_a_veto_sends_a_clean_candidate_to_a_person_but_never_rescues_a_failing_one() {
    let p = Policy::default();
    let r = evaluate(&same(24), false, true, &p);
    assert_eq!(r.verdict, Verdict::NeedsHuman);
    assert_eq!(failed(&r), ["veto"]);
    let mut v = same(24);
    v[0].cand.dangerous_trades = 2;
    let r = evaluate(&v, false, true, &p);
    assert_eq!(
        r.verdict,
        Verdict::Rejected,
        "a failed gate rejects even when it also loosens"
    );
    assert_eq!(failed(&r), ["dangerous", "veto"]);
}

#[test]
fn outcomes_come_from_stored_metrics_and_a_missing_one_is_not_guessed() {
    let mut m: BTreeMap<String, i64> = BTreeMap::new();
    for (k, v) in [
        ("pnl_net", 5),
        ("max_drawdown", 7),
        ("trades", 3),
        ("group.dangerous.trades", 1),
    ] {
        m.insert(k.into(), v);
    }
    assert_eq!(
        Outcome::from_metrics(&m),
        Some(Outcome {
            net: 5,
            max_drawdown: 7,
            trades: 3,
            dangerous_trades: 1
        })
    );
    m.remove("group.dangerous.trades");
    assert_eq!(
        Outcome::from_metrics(&m).unwrap().dangerous_trades,
        0,
        "no such scenario in the session"
    );
    for k in ["pnl_net", "max_drawdown", "trades"] {
        let mut m2 = m.clone();
        m2.remove(k);
        assert_eq!(Outcome::from_metrics(&m2), None, "{k}");
    }
}

fn record(verdict_loosens: bool, fail: bool) -> Record {
    let mut v = same(24);
    if fail {
        v[0].cand.dangerous_trades = 1;
    }
    Record {
        proposer: "agent-7".into(),
        reason: "entries look late on the dangerous names".into(),
        base: "a89f772d86aac01e".into(),
        candidate: "c6518c6f0a49be69".into(),
        suite: "seeds 1000..1023, 3h/3d/2q, 520 s".into(),
        review: evaluate(&v, false, verdict_loosens, &Policy::default()),
        changes: vec!["~ dangerous: depth > 350  ->  depth > 400   [LOOSENS A VETO]".into()],
        runs: vec![(1000, "aa".repeat(32), "bb".repeat(32))],
        approvals: Vec::new(),
    }
}

#[test]
fn a_record_round_trips_and_keeps_every_gate_change_and_run() {
    for (loosens, fail) in [(false, false), (true, false), (false, true)] {
        let mut r = record(loosens, fail);
        if !fail {
            r.approve("gavin").unwrap();
        }
        let back = Record::parse(&r.to_text()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.id(), "c6518c6f0a49be69-a89f772d86aac01e");
    }
}

#[test]
fn approval_needs_a_name_refuses_the_rejected_and_the_repeat_and_is_only_ever_appended() {
    let mut r = record(false, false);
    assert!(!r.approved());
    assert!(r.approve("  ").is_err());
    r.approve("gavin").unwrap();
    assert!(r.approved());
    assert!(
        r.approve("gavin")
            .unwrap_err()
            .0
            .contains("already approved")
    );
    r.approve("sam").unwrap();
    let before = record(false, false).to_text();
    assert!(
        r.to_text().starts_with(&before),
        "approvals are appended lines, nothing else moves"
    );
    assert_eq!(
        &r.to_text()[before.len()..],
        "approval gavin\napproval sam\n"
    );
    let mut rej = record(false, true);
    assert_eq!(rej.review.verdict, Verdict::Rejected);
    assert!(rej.approve("gavin").unwrap_err().0.contains("rejected"));
    assert!(!rej.approved());
    // Needs-human can be approved.
    let mut nh = record(true, false);
    nh.approve("gavin").unwrap();
    assert!(nh.approved());
}

#[test]
fn a_record_cannot_claim_a_verdict_its_gates_do_not_support() {
    let good = record(false, false).to_text();
    let lie = good.replace("verdict accepted", "verdict needs-human");
    assert!(
        Record::parse(&lie)
            .unwrap_err()
            .0
            .contains("gates say `accepted`")
    );
    let rej = record(false, true).to_text();
    let lie = rej.replace("verdict rejected", "verdict accepted");
    assert!(
        Record::parse(&lie)
            .unwrap_err()
            .0
            .contains("gates say `rejected`")
    );
    let flipped = good.replace("dangerous pass", "dangerous fail");
    assert!(
        Record::parse(&flipped)
            .unwrap_err()
            .0
            .contains("says `accepted` but its gates say `rejected`")
    );
    let nh = record(true, false).to_text();
    assert!(Record::parse(&nh.replace("verdict needs-human", "verdict accepted")).is_err());
    // An approval on a rejected record is refused at read time.
    let sneaky = format!("{rej}approval gavin\n");
    assert!(
        Record::parse(&sneaky)
            .unwrap_err()
            .0
            .contains("rejected proposal has an approval")
    );
}

#[test]
fn bad_records_are_refused_with_the_line_and_the_reason() {
    let good = record(false, false).to_text();
    let cases = [
        ("tfpr 2\n".to_owned(), "expected `tfpr 1`"),
        (good.replace("tfpr 1", "tfpr 1\nnonsense"), "line 2"),
        (good.replace("proposer agent-7\n", ""), "no `proposer`"),
        (good.replace("verdict accepted\n", ""), "no `verdict`"),
        (
            good.replace("verdict accepted", "verdict maybe"),
            "unknown verdict `maybe`",
        ),
        (
            good.replace("gate net pass", "gate nett pass"),
            "unknown gate `nett`",
        ),
        (
            good.replace("gate net pass", "gate net maybe"),
            "`maybe` is not pass or fail",
        ),
        (good.replace("run 1000", "run x"), "seed"),
        (
            good.replace("\nrun 1000 ", "\nrun 1000 extra "),
            "a run is `seed base candidate`",
        ),
        (format!("{good}colour red\n"), "unknown key `colour`"),
        (
            good.replace(
                "gate veto pass no protective stage is loosened",
                "gate veto pass",
            ),
            "a gate is `name pass|fail detail`",
        ),
    ];
    for (text, want) in cases {
        let e = Record::parse(&text).unwrap_err().0;
        assert!(e.contains(want), "wanted `{want}` in `{e}`");
    }
    // Free text cannot break the format.
    let mut r = record(false, false);
    r.reason = "line one\nverdict rejected\napproval mallory".into();
    r.proposer = "a\nb".into();
    let back = Record::parse(&r.to_text()).unwrap();
    assert_eq!(back.reason, "line one verdict rejected approval mallory");
    assert!(back.approvals.is_empty() && back.review.verdict == Verdict::Accepted);
}
