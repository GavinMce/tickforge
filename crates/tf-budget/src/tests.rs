use std::collections::BTreeMap;

use tf_synth::SplitMix64;

use crate::*;

const D: u128 = 1_000_000_000; // one dollar, raw

fn s(id: &str, share: Bp) -> Strategy {
    Strategy {
        id: id.to_owned(),
        share,
    }
}

fn g(id: &str, share: Bp, strategies: Vec<Strategy>) -> Group {
    Group {
        id: id.to_owned(),
        share,
        loss: LossLimits::default(),
        strategies,
    }
}

/// The workspace of the UI mock: Day 25% (three strategies), Swing 35%, ETF 40%.
fn mock() -> Tree {
    Tree::new(vec![
        g(
            "day",
            2500,
            vec![s("ml", 3000), s("ms", 3500), s("tr", 2500)],
        ),
        g("swing", 3500, vec![s("sw", 5000), s("nw", 5000)]),
        g("etf", 4000, vec![s("rot", 7000), s("core", 3000)]),
    ])
    .unwrap()
}

fn mock_usage() -> Usage {
    Usage::new()
        .with("ml", 4_800 * D)
        .with("ms", 3_600 * D)
        .with("tr", 2_800 * D)
        .with("sw", 9_400 * D)
        .with("nw", 9_000 * D)
        .with("rot", 22_000 * D)
        .with("core", 9_500 * D)
}

const BAL: u128 = 100_000 * D;

// ------------------------------------------------------------- building

#[test]
fn a_tree_is_only_built_when_every_rule_holds() {
    assert!(Tree::new(vec![]).is_ok());
    let e = |groups| Tree::new(groups).unwrap_err();
    assert_eq!(
        e(vec![g("", 100, vec![])]),
        BudgetError::BadId(String::new())
    );
    assert_eq!(
        e(vec![g("Day", 100, vec![])]),
        BudgetError::BadId("Day".into())
    );
    assert_eq!(
        e(vec![g("a b", 100, vec![])]),
        BudgetError::BadId("a b".into())
    );
    assert_eq!(
        e(vec![g(&"x".repeat(33), 100, vec![])]),
        BudgetError::BadId("x".repeat(33))
    );
    assert!(
        Tree::new(vec![
            g(&"x".repeat(32), 100, vec![]),
            g("a_b-9", 100, vec![])
        ])
        .is_ok()
    );
    assert_eq!(
        e(vec![g("a", 1, vec![]), g("a", 1, vec![])]),
        BudgetError::Duplicate("a".into())
    );
    assert_eq!(
        e(vec![g("a", 1, vec![s("x", 1)]), g("b", 1, vec![s("x", 1)])]),
        BudgetError::Duplicate("x".into()),
        "a strategy id is unique across the tree"
    );
    assert_eq!(
        e(vec![g("a", 1, vec![s("a", 1)])]),
        BudgetError::Duplicate("a".into()),
        "and not a group's id either"
    );
    assert_eq!(
        e(vec![g("a", FULL + 1, vec![])]),
        BudgetError::ShareTooBig {
            what: "a".into(),
            share: FULL + 1
        }
    );
    assert_eq!(
        e(vec![g("a", 1, vec![s("x", FULL + 1)])]),
        BudgetError::ShareTooBig {
            what: "x".into(),
            share: FULL + 1
        }
    );
    // The whole is allowed; one basis point more is not, at either level.
    assert!(
        Tree::new(vec![
            g("a", 6000, vec![s("x", 10_000)]),
            g("b", 4000, vec![])
        ])
        .is_ok()
    );
    assert_eq!(
        e(vec![g("a", 6000, vec![]), g("b", 4001, vec![])]),
        BudgetError::OverAllocated {
            parent: "workspace".into(),
            total: 10_001
        }
    );
    assert_eq!(
        e(vec![g("a", 6000, vec![s("x", 6000), s("y", 4001)])]),
        BudgetError::OverAllocated {
            parent: "a".into(),
            total: 10_001
        }
    );
    for (soft, hard) in [(0, 600), (600, 600), (700, 600), (300, 10_001)] {
        let mut gr = g("a", 1, vec![]);
        gr.loss = LossLimits { soft, hard };
        assert_eq!(
            e(vec![gr]),
            BudgetError::BadLossLimits {
                group: "a".into(),
                soft,
                hard
            },
            "{soft} {hard}"
        );
    }
    let mut ok = g("a", 1, vec![]);
    ok.loss = LossLimits {
        soft: 1,
        hard: FULL,
    };
    assert!(Tree::new(vec![ok]).is_ok());
}

#[test]
fn what_is_unassigned_and_what_each_node_is_worth_in_dollars() {
    let t = mock();
    assert_eq!(
        (
            t.unassigned(),
            t.unassigned_in("day"),
            t.unassigned_in("swing")
        ),
        (0, Some(1000), Some(0))
    );
    assert_eq!(t.unassigned_in("nope"), None);
    assert_eq!(t.group_budget(BAL, "day"), Some(25_000 * D));
    assert_eq!(t.group_budget(BAL, "etf"), Some(40_000 * D));
    assert_eq!(t.group_budget(BAL, "nope"), None);
    assert_eq!(t.strategy_budget(BAL, "ml"), Some(7_500 * D));
    assert_eq!(t.strategy_budget(BAL, "ms"), Some(8_750 * D));
    assert_eq!(t.strategy_budget(BAL, "core"), Some(12_000 * D));
    assert_eq!(t.strategy_budget(BAL, "nope"), None);
    assert_eq!(t.group_of("nw").unwrap().id, "swing");
    assert!(t.group_of("swing").is_none(), "a group is not a strategy");
    assert_eq!(t.strategy("tr").unwrap().share, 2500);
    assert_eq!(share_of(7, 5000), 3, "rounded down");
    assert_eq!(share_of(0, FULL), 0);
    assert_eq!(share_of(123_456_789, FULL), 123_456_789);
    assert_eq!(Usage::new().with("ml", 5).strategy("ml"), 5);
    assert_eq!(Usage::new().strategy("ml"), 0);
    assert_eq!(mock_usage().group(t.group("day").unwrap()), 11_200 * D);
}

#[test]
fn loss_limits_are_three_and_six_percent_of_a_strategys_own_budget() {
    let t = mock();
    assert_eq!(
        LossLimits::default(),
        LossLimits {
            soft: 300,
            hard: 600
        }
    );
    assert_eq!(
        t.loss_amounts(BAL, "ml"),
        Some((225 * D, 450 * D)),
        "3% and 6% of $7,500"
    );
    assert_eq!(t.loss_amounts(BAL, "nope"), None);
    let t2 = t
        .with_loss(
            "day",
            LossLimits {
                soft: 200,
                hard: 1000,
            },
        )
        .unwrap();
    assert_eq!(t2.loss_amounts(BAL, "ml"), Some((150 * D, 750 * D)));
    assert_eq!(
        t2.loss_amounts(BAL, "sw"),
        t.loss_amounts(BAL, "sw"),
        "other groups are untouched"
    );
    assert_eq!(
        t.loss_amounts(BAL, "ml"),
        Some((225 * D, 450 * D)),
        "and the original"
    );
    assert!(
        t.with_loss(
            "day",
            LossLimits {
                soft: 600,
                hard: 600
            }
        )
        .is_err()
    );
    assert_eq!(
        t.with_loss("nope", LossLimits::default()).unwrap_err(),
        BudgetError::Unknown("nope".into())
    );
}

// ------------------------------------------------------------- ranges

#[test]
fn a_groups_share_cannot_go_below_what_its_strategies_have_in_use() {
    let t = mock();
    let r = t.group_range("day", BAL, &mock_usage()).unwrap();
    // Momentum long has $4,800 in use at a 30% share, so the group needs $16,000: 16% of the balance.
    assert_eq!(
        r.min,
        Bound {
            value: 1600,
            why: Why::InUse {
                who: "ml".into(),
                used: 4_800 * D
            }
        }
    );
    assert_eq!(
        r.max,
        Bound {
            value: 2500,
            why: Why::Unassigned
        },
        "nothing is unassigned at the top"
    );
    assert!(r.contains(1600) && r.contains(2500) && !r.contains(1599) && !r.contains(2501));
    let t2 = t.with_group_share("day", 1600, BAL, &mock_usage()).unwrap();
    assert_eq!(t2.group("day").unwrap().share, 1600);
    assert_eq!(
        t2.unassigned(),
        900,
        "what was given up is unassigned at the top"
    );
    assert_eq!(
        t.group("day").unwrap().share,
        2500,
        "the original is unchanged"
    );
    // The freed share can then go to another group, up to what is unassigned.
    let r2 = t2.group_range("etf", BAL, &mock_usage()).unwrap();
    assert_eq!(r2.max.value, 4900);
    assert!(t2.with_group_share("etf", 4900, BAL, &mock_usage()).is_ok());
    assert_eq!(
        t.with_group_share("day", 1599, BAL, &mock_usage())
            .unwrap_err(),
        BudgetError::OutOfRange {
            what: "day".into(),
            asked: 1599,
            range: Box::new(r.clone())
        }
    );
    assert!(matches!(
        t.with_group_share("day", 2501, BAL, &mock_usage()),
        Err(BudgetError::OutOfRange { .. })
    ));
    // With nothing in use any share down to zero is fine.
    let r0 = t.group_range("day", BAL, &Usage::new()).unwrap();
    assert_eq!(
        r0.min,
        Bound {
            value: 0,
            why: Why::Nothing
        }
    );
    // The strategy that needs the most is the one named: here Trend, $5,000 of a 25% share.
    let u = Usage::new()
        .with("ml", 100 * D)
        .with("ms", 100 * D)
        .with("tr", 5_000 * D);
    let r = t.group_range("day", BAL, &u).unwrap();
    assert_eq!(
        r.min,
        Bound {
            value: 2000,
            why: Why::InUse {
                who: "tr".into(),
                used: 5_000 * D
            }
        }
    );
    assert_eq!(
        t.group_range("nope", BAL, &u).unwrap_err(),
        BudgetError::Unknown("nope".into())
    );
}

#[test]
fn a_strategys_share_is_bounded_by_its_use_below_and_its_groups_free_share_above() {
    let t = mock();
    let r = t.strategy_range("ml", BAL, &mock_usage()).unwrap();
    // $4,800 of a $25,000 group is 19.2%.
    assert_eq!(
        r.min,
        Bound {
            value: 1920,
            why: Why::InUse {
                who: "ml".into(),
                used: 4_800 * D
            }
        }
    );
    // 30% now plus the 10% unassigned in Day trading.
    assert_eq!(
        r.max,
        Bound {
            value: 4000,
            why: Why::Unassigned
        }
    );
    assert!(
        t.with_strategy_share("ml", 4000, BAL, &mock_usage())
            .is_ok()
    );
    assert!(matches!(
        t.with_strategy_share("ml", 4001, BAL, &mock_usage()),
        Err(BudgetError::OutOfRange { .. })
    ));
    assert!(matches!(
        t.with_strategy_share("ml", 1919, BAL, &mock_usage()),
        Err(BudgetError::OutOfRange { .. })
    ));
    let t2 = t
        .with_strategy_share("ml", 1920, BAL, &mock_usage())
        .unwrap();
    assert_eq!(t2.unassigned_in("day"), Some(2080));
    assert_eq!(
        t2.strategy("ms").unwrap().share,
        3500,
        "siblings are untouched"
    );
    // A full group has nothing to give: the ceiling is where it is.
    let r = t.strategy_range("sw", BAL, &mock_usage()).unwrap();
    assert_eq!(r.max.value, 5000);
    assert_eq!(
        t.strategy_range("nope", BAL, &Usage::new()).unwrap_err(),
        BudgetError::Unknown("nope".into())
    );
    // The group's size feeds in: halve Day trading and Momentum long needs a bigger share of it.
    let small = t.with_group_share("day", 1600, BAL, &mock_usage()).unwrap();
    assert_eq!(
        small
            .strategy_range("ml", BAL, &mock_usage())
            .unwrap()
            .min
            .value,
        3000,
        "$4,800 of $16,000"
    );
}

#[test]
fn a_node_that_is_already_over_budget_may_stay_or_grow_but_not_shrink() {
    let t = mock();
    // Momentum long has more in use than its $7,500 (prices moved against a short, say).
    let u = Usage::new().with("ml", 9_000 * D);
    let r = t.strategy_range("ml", BAL, &u).unwrap();
    assert_eq!(
        r.min,
        Bound {
            value: 3000,
            why: Why::OverBudget
        }
    );
    assert!(
        t.with_strategy_share("ml", 3000, BAL, &u).is_ok(),
        "staying is allowed"
    );
    assert!(
        t.with_strategy_share("ml", 3600, BAL, &u).is_ok(),
        "growing is allowed"
    );
    assert!(
        t.with_strategy_share("ml", 2999, BAL, &u).is_err(),
        "shrinking is not"
    );
    let r = t.group_range("day", BAL, &u).unwrap();
    assert_eq!((r.min.value, r.min.why.clone()), (2500, Why::OverBudget));
    assert!(t.with_group_share("day", 2499, BAL, &u).is_err());
    // More in use than the whole balance: still stay-or-grow, never a panic.
    let huge = Usage::new().with("ml", 1_000_000 * D);
    let r = t.group_range("day", BAL, &huge).unwrap();
    assert_eq!(r.min.value, 2500);
}

fn random_tree(rng: &mut SplitMix64) -> (Tree, Usage, u128) {
    let n = 1 + rng.below(4) as usize;
    let mut left = FULL;
    let mut groups = Vec::new();
    let mut usage = Usage::new();
    let balance = (1 + rng.below(2_000_000)) as u128 * (1 + rng.below(1000)) as u128;
    for gi in 0..n {
        let share = (rng.below(u64::from(left) + 1)) as Bp / 2;
        left -= share;
        let m = 1 + rng.below(4) as usize;
        let mut inner_left = FULL;
        let mut strategies = Vec::new();
        for si in 0..m {
            let sshare = rng.below(u64::from(inner_left) + 1) as Bp / 2;
            inner_left -= sshare;
            let id = format!("s{gi}_{si}");
            let sb = share_of(share_of(balance, share), sshare);
            usage = usage.with(
                &id,
                if sb == 0 {
                    0
                } else {
                    u128::from(rng.below(sb as u64 + 1)) * u128::from(rng.below(2))
                },
            );
            strategies.push(Strategy { id, share: sshare });
        }
        groups.push(Group {
            id: format!("g{gi}"),
            share,
            loss: LossLimits::default(),
            strategies,
        });
    }
    (Tree::new(groups).unwrap(), usage, balance)
}

#[test]
fn legal_ranges_match_a_brute_force_search_and_every_edit_at_a_bound_keeps_the_rules() {
    let mut rng = SplitMix64::new(11);
    let (mut group_checks, mut strategy_checks) = (0, 0);
    for _ in 0..300 {
        let (t, usage, balance) = random_tree(&mut rng);
        // The tree starts feasible (usage is at most each strategy's budget), so floors are real.
        for gr in t.groups() {
            let r = t.group_range(&gr.id, balance, &usage).unwrap();
            assert!(r.min.value <= gr.share && gr.share <= r.max.value, "{r:?}");
            assert_eq!(r.max.value, gr.share + t.unassigned());
            // Brute force the floor: the first share at which every use still fits.
            let fits = |x: Bp| {
                let b = share_of(balance, x);
                b >= usage.group(gr)
                    && gr
                        .strategies
                        .iter()
                        .all(|s| share_of(b, s.share) >= usage.strategy(&s.id))
            };
            let want = (0..=FULL).find(|x| fits(*x)).unwrap();
            assert_eq!(r.min.value, want.min(gr.share), "group {} of {t:?}", gr.id);
            for edge in [r.min.value, r.max.value] {
                let t2 = t.with_group_share(&gr.id, edge, balance, &usage).unwrap();
                assert_eq!(t2.group(&gr.id).unwrap().share, edge);
                assert!(t2.unassigned() <= FULL);
                group_checks += 1;
            }
            if r.min.value > 0 {
                assert!(
                    t.with_group_share(&gr.id, r.min.value - 1, balance, &usage)
                        .is_err()
                );
            }
            if r.max.value < FULL {
                assert!(
                    t.with_group_share(&gr.id, r.max.value + 1, balance, &usage)
                        .is_err()
                );
            }
            for st in &gr.strategies {
                let r = t.strategy_range(&st.id, balance, &usage).unwrap();
                assert!(r.min.value <= st.share && st.share <= r.max.value);
                let gb = share_of(balance, gr.share);
                let want = (0..=FULL)
                    .find(|x| share_of(gb, *x) >= usage.strategy(&st.id))
                    .unwrap();
                assert_eq!(r.min.value, want.min(st.share), "strategy {}", st.id);
                for edge in [r.min.value, r.max.value] {
                    let t2 = t
                        .with_strategy_share(&st.id, edge, balance, &usage)
                        .unwrap();
                    // Whatever the edit, the children never exceed the parent, in dollars either.
                    let sum: u128 = t2
                        .group(&gr.id)
                        .unwrap()
                        .strategies
                        .iter()
                        .map(|s| t2.strategy_budget(balance, &s.id).unwrap())
                        .sum();
                    assert!(sum <= t2.group_budget(balance, &gr.id).unwrap());
                    strategy_checks += 1;
                }
                if r.min.value > 0 {
                    assert!(
                        t.with_strategy_share(&st.id, r.min.value - 1, balance, &usage)
                            .is_err()
                    );
                }
                if r.max.value < FULL {
                    assert!(
                        t.with_strategy_share(&st.id, r.max.value + 1, balance, &usage)
                            .is_err()
                    );
                }
            }
        }
        // The dollar budgets of any tree never add up to more than the level above.
        let top: u128 = t
            .groups()
            .iter()
            .map(|g| t.group_budget(balance, &g.id).unwrap())
            .sum();
        assert!(top <= balance);
        for gr in t.groups() {
            let inner: u128 = gr
                .strategies
                .iter()
                .map(|s| t.strategy_budget(balance, &s.id).unwrap())
                .sum();
            assert!(inner <= t.group_budget(balance, &gr.id).unwrap());
        }
    }
    assert!(
        group_checks > 500 && strategy_checks > 1000,
        "{group_checks} {strategy_checks}"
    );
}

// ------------------------------------------------------------- text and diff

#[test]
fn the_text_form_is_canonical_and_round_trips() {
    let t = mock();
    let text = t.render();
    assert_eq!(
        text,
        "budgets v1\ngroup day 2500 300 600\nstrategy day ml 3000\nstrategy day ms 3500\nstrategy day tr 2500\n\
         group swing 3500 300 600\nstrategy swing sw 5000\nstrategy swing nw 5000\n\
         group etf 4000 300 600\nstrategy etf rot 7000\nstrategy etf core 3000\n"
    );
    assert_eq!(Tree::parse(&text).unwrap(), t);
    assert_eq!(
        Tree::parse(&Tree::new(vec![]).unwrap().render()).unwrap(),
        Tree::default()
    );
    let noisy = format!(
        "# a note\n\n{}  # trailing\n",
        text.replace("budgets v1", "budgets   v1  # header")
    );
    assert_eq!(Tree::parse(&noisy).unwrap(), t);
    assert_eq!(Tree::parse(&noisy).unwrap().fingerprint(), t.fingerprint());
    // Each kind of edit is a different version.
    let edits = [
        t.with_group_share("day", 2400, BAL, &Usage::new()).unwrap(),
        t.with_strategy_share("ml", 2900, BAL, &Usage::new())
            .unwrap(),
        t.with_loss(
            "etf",
            LossLimits {
                soft: 301,
                hard: 600,
            },
        )
        .unwrap(),
        t.with_loss(
            "etf",
            LossLimits {
                soft: 300,
                hard: 601,
            },
        )
        .unwrap(),
    ];
    let mut seen = vec![t.fingerprint()];
    for e in &edits {
        assert!(!seen.contains(&e.fingerprint()));
        seen.push(e.fingerprint());
    }
    // Group order matters (it is the order the screen shows).
    let mut rev = t.groups().to_vec();
    rev.reverse();
    assert_ne!(Tree::new(rev).unwrap().fingerprint(), t.fingerprint());
}

#[test]
fn bad_text_is_refused_with_the_line_and_the_reason() {
    let good = mock().render();
    let cases: Vec<(String, &str)> = vec![
        ("# nothing\n".into(), "missing `budgets v1`"),
        ("budgets v2\n".into(), "expected `budgets v1`"),
        (
            good.replacen("budgets v1", "group day 1 300 600\nbudgets v1", 1),
            "expected `budgets v1`",
        ),
        (
            format!("{good}colour red 1\n"),
            "line 12: `colour red 1` is not a group or strategy line",
        ),
        (
            good.replace("group day 2500 300 600", "group day 25x 300 600"),
            "line 2: `25x` is not a whole number",
        ),
        (
            good.replace("strategy day ml 3000", "strategy day ml -1"),
            "is not a whole number",
        ),
        (
            good.replace("group day 2500 300 600\n", ""),
            "names group `day`, which has not been declared",
        ),
        (
            good.replace("strategy day ml 3000", "strategy day ml"),
            "is not a group or strategy line",
        ),
        (
            good.replace("group swing 3500", "group swing 7500"),
            "add up to 14000",
        ),
        (
            good.replace("strategy swing nw 5000", "strategy swing nw 5001"),
            "add up to 10001",
        ),
        (
            good.replace("strategy day ms 3500", "strategy day ml 3500"),
            "`ml` appears twice",
        ),
        (
            good.replace("group etf 4000 300 600", "group etf 4000 600 600"),
            "loss limits need",
        ),
        (good.replace("etf", "ETF"), "not a valid id"),
    ];
    for (text, want) in cases {
        let e = Tree::parse(&text).unwrap_err().to_string();
        assert!(e.contains(want), "wanted `{want}` in `{e}` for\n{text}");
    }
}

#[test]
fn a_diff_says_what_changed_in_the_order_the_screen_shows_it() {
    let a = mock();
    assert!(diff(&a, &a).is_empty());
    let b = a
        .with_group_share("day", 2000, BAL, &Usage::new())
        .unwrap()
        .with_strategy_share("ml", 2500, BAL, &Usage::new())
        .unwrap()
        .with_loss(
            "etf",
            LossLimits {
                soft: 200,
                hard: 800,
            },
        )
        .unwrap();
    assert_eq!(
        diff(&a, &b),
        vec![
            Change::GroupShare {
                id: "day".into(),
                from: 2500,
                to: 2000
            },
            Change::StrategyShare {
                group: "day".into(),
                id: "ml".into(),
                from: 3000,
                to: 2500
            },
            Change::Loss {
                id: "etf".into(),
                from: LossLimits {
                    soft: 300,
                    hard: 600
                },
                to: LossLimits {
                    soft: 200,
                    hard: 800
                }
            },
        ]
    );
    // Added and removed nodes, at both levels.
    let c = Tree::new(vec![
        g(
            "day",
            2500,
            vec![s("ml", 3000), s("ms", 3500), s("new", 100)],
        ),
        g("etf", 4000, vec![s("rot", 7000), s("core", 3000)]),
        g("fx", 100, vec![]),
    ])
    .unwrap();
    assert_eq!(
        diff(&a, &c),
        vec![
            Change::StrategyRemoved {
                group: "day".into(),
                id: "tr".into()
            },
            Change::StrategyAdded {
                group: "day".into(),
                id: "new".into()
            },
            Change::GroupRemoved("swing".into()),
            Change::GroupAdded("fx".into()),
        ]
    );
}

// ------------------------------------------------------------- rebalance

fn pnl(items: &[(&str, i128)]) -> BTreeMap<String, i128> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), *v * D as i128))
        .collect()
}

fn dollars(t: &Tree, balance: u128, id: &str) -> u128 {
    t.strategy_budget(balance, id).unwrap()
}

#[test]
fn with_no_profit_and_no_loss_nothing_moves_however_many_times_it_is_run() {
    let t = mock();
    let r = rebalance(&t, &t, BAL, &BTreeMap::new(), Bounds::default()).unwrap();
    assert_eq!((&r.tree, r.balance), (&t, BAL));
    let mut rng = SplitMix64::new(5);
    for _ in 0..300 {
        let (t, _, balance) = random_tree(&mut rng);
        let r = rebalance(&t, &t, balance, &BTreeMap::new(), Bounds::default()).unwrap();
        assert_eq!(
            (&r.tree, r.balance),
            (&t, balance),
            "no drift from rounding"
        );
        let again = rebalance(&r.tree, &t, r.balance, &BTreeMap::new(), Bounds::default()).unwrap();
        assert_eq!(again, r);
    }
}

#[test]
fn a_winner_takes_its_profit_into_its_own_budget_and_the_others_keep_theirs() {
    let t = mock();
    let r = rebalance(&t, &t, BAL, &pnl(&[("ml", 600)]), Bounds::default()).unwrap();
    assert_eq!(
        r.balance,
        100_600 * D,
        "the balance is the old one plus the profit"
    );
    let tol = 2 * r.balance / 10_000; // a share is whole basis points of its parent
    let near = |got: u128, want: u128| got.abs_diff(want) <= tol;
    assert!(
        near(dollars(&r.tree, r.balance, "ml"), 8_100 * D),
        "7,500 + 600"
    );
    for (id, want) in [
        ("ms", 8_750),
        ("tr", 6_250),
        ("sw", 17_500),
        ("nw", 17_500),
        ("rot", 28_000),
        ("core", 12_000),
    ] {
        assert!(
            near(dollars(&r.tree, r.balance, id), want * D),
            "{id} keeps its dollars: {}",
            dollars(&r.tree, r.balance, id)
        );
    }
    // Day trading's unassigned $2,500 is still $2,500.
    let day = r.tree.group_budget(r.balance, "day").unwrap();
    let assigned: u128 = ["ml", "ms", "tr"]
        .iter()
        .map(|s| dollars(&r.tree, r.balance, s))
        .sum();
    assert!(near(day - assigned, 2_500 * D));
    // The group's share moved up, and so did the strategy's within it.
    assert!(r.tree.group("day").unwrap().share > 2500);
    assert!(r.tree.strategy("ml").unwrap().share > 3000);
    assert_eq!(r.tree.group("etf").unwrap().loss, LossLimits::default());
}

#[test]
fn a_loser_gives_up_its_loss_and_cannot_go_below_nothing() {
    let t = mock();
    let r = rebalance(&t, &t, BAL, &pnl(&[("sw", -700)]), Bounds::default()).unwrap();
    assert_eq!(r.balance, 99_300 * D);
    let tol = 2 * r.balance / 10_000;
    assert!(
        dollars(&r.tree, r.balance, "sw").abs_diff(16_800 * D) <= tol,
        "17,500 - 700"
    );
    assert!(dollars(&r.tree, r.balance, "nw").abs_diff(17_500 * D) <= tol);
    // A loss bigger than its budget would take it below zero: it ends at the floor of its bounds,
    // and the balance still falls by the whole loss.
    let big = rebalance(&t, &t, BAL, &pnl(&[("ml", -9_000)]), Bounds::default()).unwrap();
    assert_eq!(big.balance, 91_000 * D);
    assert_eq!(
        big.tree.strategy("ml").unwrap().share,
        1500,
        "half of its 30% target"
    );
}

#[test]
fn the_bounds_stop_a_node_running_away_from_its_target_in_either_direction() {
    let t = mock();
    // ml and tr each make a fortune: left alone they would take most of the group.
    let r = rebalance(
        &t,
        &t,
        BAL,
        &pnl(&[("ml", 90_000), ("tr", 80_000)]),
        Bounds::default(),
    )
    .unwrap();
    let day = r.tree.group("day").unwrap();
    let shares: Vec<Bp> = day.strategies.iter().map(|s| s.share).collect();
    assert_eq!(
        shares.iter().sum::<u32>(),
        FULL,
        "the clamped shares are trimmed to fit: {shares:?}"
    );
    assert_eq!(
        shares[1], 1750,
        "ms was clamped up to half its 35% target and stays there"
    );
    // The two winners are trimmed evenly from their ceilings (60% and 50%).
    assert!(shares[0] <= 6000 && shares[2] <= 5000);
    assert!(
        (i64::from(shares[0]) - 1500 - (i64::from(shares[2]) - 1250)).abs() <= 1,
        "{shares:?}"
    );
    // The group itself is held to twice its 25% target.
    assert!(day.share <= 5000, "{}", day.share);
    // Every share is within bounds of its target, whatever happened.
    let (lo, hi) = (
        |t: Bp| (t * 5000).div_ceil(FULL),
        |t: Bp| (t * 20_000 / FULL).min(FULL),
    );
    for (g, tg) in r.tree.groups().iter().zip(t.groups()) {
        assert!(g.share >= lo(tg.share) && g.share <= hi(tg.share));
        for (s, ts) in g.strategies.iter().zip(&tg.strategies) {
            assert!(
                s.share >= lo(ts.share) && s.share <= hi(ts.share),
                "{}: {}",
                s.id,
                s.share
            );
        }
    }
    assert!(r.tree.unassigned() <= FULL);
}

#[test]
fn a_node_with_no_target_share_stays_at_none_and_a_wiped_out_balance_is_reported_as_zero() {
    let t = Tree::new(vec![
        g("a", 5000, vec![s("x", 0), s("y", 10_000)]),
        g("b", 5000, vec![s("z", 10_000)]),
    ])
    .unwrap();
    let r = rebalance(&t, &t, BAL, &pnl(&[("y", 1_000)]), Bounds::default()).unwrap();
    assert_eq!(
        r.tree.strategy("x").unwrap().share,
        0,
        "it cannot grow from nothing"
    );
    let wiped = rebalance(&t, &t, BAL, &pnl(&[("y", -200_000)]), Bounds::default()).unwrap();
    assert_eq!(
        (wiped.balance, &wiped.tree),
        (0, &t),
        "nothing to divide: the tree is left as it was"
    );
    let exactly = rebalance(
        &t,
        &t,
        BAL,
        &pnl(&[("y", -50_000), ("z", -50_000)]),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!(exactly.balance, 0);
    assert_eq!(
        exactly.tree, t,
        "nothing to divide: the tree is left as it was"
    );
}

#[test]
fn a_rebalance_needs_matching_trees_and_sensible_bounds() {
    let t = mock();
    let other = Tree::new(vec![g("day", 2500, vec![s("ml", 3000)])]).unwrap();
    assert_eq!(
        rebalance(&t, &other, BAL, &BTreeMap::new(), Bounds::default()).unwrap_err(),
        BudgetError::ShapeMismatch
    );
    let renamed = Tree::new(
        t.groups()
            .iter()
            .map(|gr| Group {
                id: format!("{}x", gr.id),
                ..gr.clone()
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(
        rebalance(&t, &renamed, BAL, &BTreeMap::new(), Bounds::default()).unwrap_err(),
        BudgetError::ShapeMismatch
    );
    for (floor, ceiling) in [(0, 20_000), (10_001, 20_000), (5_000, 9_999)] {
        assert_eq!(
            Bounds::new(floor, ceiling).unwrap_err(),
            BudgetError::BadBounds { floor, ceiling }
        );
        assert!(rebalance(&t, &t, BAL, &BTreeMap::new(), Bounds { floor, ceiling }).is_err());
    }
    assert_eq!(
        Bounds::new(10_000, 10_000).unwrap(),
        Bounds {
            floor: 10_000,
            ceiling: 10_000
        }
    );
    // Bounds of exactly the target pin every share: profit changes the balance and nothing else.
    let pinned = rebalance(
        &t,
        &t,
        BAL,
        &pnl(&[("ml", 5_000), ("rot", -3_000)]),
        Bounds::new(FULL, FULL).unwrap(),
    )
    .unwrap();
    assert_eq!(pinned.tree, t);
    assert_eq!(pinned.balance, 102_000 * D);
}

#[test]
fn across_random_trees_profits_and_bounds_a_rebalance_always_gives_a_valid_bounded_tree() {
    let mut rng = SplitMix64::new(99);
    let (mut clamped, mut moved) = (0, 0);
    for _ in 0..500 {
        let (t, _, balance) = random_tree(&mut rng);
        let bounds = Bounds::new(
            1 + rng.below(u64::from(FULL)) as Bp,
            FULL + rng.below(30_000) as Bp,
        )
        .unwrap();
        let mut p = BTreeMap::new();
        for gr in t.groups() {
            for st in &gr.strategies {
                let scale = i128::try_from(balance / 4).unwrap().max(1);
                p.insert(
                    st.id.clone(),
                    (i128::from(rng.below(2 * scale as u64 + 1) as i64) - scale)
                        * i128::from(rng.below(3) as i64),
                );
            }
        }
        let r = rebalance(&t, &t, balance, &p, bounds).unwrap();
        let want = (i128::try_from(balance).unwrap() + p.values().sum::<i128>()).max(0) as u128;
        assert_eq!(r.balance, want, "the balance is the old plus every profit");
        let top: u128 = r
            .tree
            .groups()
            .iter()
            .map(|gr| r.tree.group_budget(r.balance, &gr.id).unwrap())
            .sum();
        assert!(top <= r.balance);
        for (gr, tg) in r.tree.groups().iter().zip(t.groups()) {
            let (lo, hi) = (
                (u64::from(tg.share) * u64::from(bounds.floor)).div_ceil(u64::from(FULL)) as Bp,
                ((u64::from(tg.share) * u64::from(bounds.ceiling)) / u64::from(FULL))
                    .min(u64::from(FULL)) as Bp,
            );
            if r.balance > 0 {
                assert!(
                    gr.share >= lo && gr.share <= hi,
                    "group {} {} not in {lo}..={hi}",
                    gr.id,
                    gr.share
                );
            }
            clamped += usize::from(gr.share == lo || gr.share == hi);
            moved += usize::from(gr.share != tg.share);
            let inner: u128 = gr
                .strategies
                .iter()
                .map(|x| r.tree.strategy_budget(r.balance, &x.id).unwrap())
                .sum();
            assert!(inner <= r.tree.group_budget(r.balance, &gr.id).unwrap());
        }
        // Deterministic.
        assert_eq!(rebalance(&t, &t, balance, &p, bounds).unwrap(), r);
    }
    assert!(
        clamped > 50 && moved > 100,
        "clamped {clamped}, moved {moved}"
    );
}

#[test]
fn when_clamped_siblings_tie_the_trimming_starts_with_the_lowest_index() {
    // a and b make the same fortune and c is clamped up to half its target (1,501), so the three
    // overfill the whole by an odd number of basis points. The excess is trimmed one at a time from
    // whichever is furthest above its floor, the first on a tie: a gives up the odd one.
    let t = Tree::new(vec![g(
        "g",
        10_000,
        vec![s("a", 3000), s("b", 3000), s("c", 3001)],
    )])
    .unwrap();
    let r = rebalance(
        &t,
        &t,
        BAL,
        &pnl(&[("a", 900_000), ("b", 900_000)]),
        Bounds::default(),
    )
    .unwrap();
    let shares: Vec<Bp> = r
        .tree
        .group("g")
        .unwrap()
        .strategies
        .iter()
        .map(|x| x.share)
        .collect();
    assert_eq!(shares[2], 1501, "{shares:?}");
    assert_eq!(shares.iter().sum::<u32>(), FULL, "{shares:?}");
    assert_eq!(
        shares[0] + 1,
        shares[1],
        "a gave up the odd basis point: {shares:?}"
    );
}

#[test]
fn bounds_are_measured_from_the_targets_not_from_where_the_last_rebalance_left_a_node() {
    let t = mock(); // the targets: ml 30% of Day trading
    let first = rebalance(&t, &t, BAL, &pnl(&[("ml", 90_000)]), Bounds::default()).unwrap();
    assert_eq!(
        first.tree.strategy("ml").unwrap().share,
        6000,
        "held to twice its target"
    );
    // Another fortune: still held to twice the *target*, though it is already at 60%.
    let second = rebalance(
        &first.tree,
        &t,
        first.balance,
        &pnl(&[("ml", 90_000)]),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!(second.tree.strategy("ml").unwrap().share, 6000);
    // A big loss brings it back down, and no further than half its target.
    let back = rebalance(
        &second.tree,
        &t,
        second.balance,
        &pnl(&[("ml", -190_000)]),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!(back.tree.strategy("ml").unwrap().share, 1500);
    // The same holds for a whole group: here g1 is held to twice its 30% target, round after round
    // (the other group sits at its floor and the rest is unassigned, so nothing else caps it).
    let u = Tree::new(vec![
        g("g1", 3000, vec![s("x", 10_000)]),
        g("g2", 3000, vec![s("y", 10_000)]),
    ])
    .unwrap();
    let g1 = rebalance(&u, &u, BAL, &pnl(&[("x", 900_000)]), Bounds::default()).unwrap();
    assert_eq!(g1.tree.group("g1").unwrap().share, 6000);
    let g2 = rebalance(
        &g1.tree,
        &u,
        g1.balance,
        &pnl(&[("x", 900_000)]),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!(
        g2.tree.group("g1").unwrap().share,
        6000,
        "not twice where it already stood"
    );
    // With no profit it stays where it was, inside its bounds.
    let still = rebalance(
        &first.tree,
        &t,
        first.balance,
        &BTreeMap::new(),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!(still.tree, first.tree);
}

#[test]
fn a_rebalance_keeps_each_groups_loss_limits_and_leaves_a_zeroed_balance_alone() {
    let custom = mock()
        .with_loss(
            "etf",
            LossLimits {
                soft: 100,
                hard: 200,
            },
        )
        .unwrap();
    let r = rebalance(
        &custom,
        &custom,
        BAL,
        &pnl(&[("rot", 1_000)]),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!(
        r.tree.group("etf").unwrap().loss,
        LossLimits {
            soft: 100,
            hard: 200
        }
    );
    assert_eq!(r.tree.group("day").unwrap().loss, LossLimits::default());
    // A wiped-out balance gives back the same tree, not a tree of zeros.
    let wiped = rebalance(
        &custom,
        &custom,
        BAL,
        &pnl(&[("rot", -1_000_000)]),
        Bounds::default(),
    )
    .unwrap();
    assert_eq!((wiped.balance, wiped.tree), (0, custom));
}
