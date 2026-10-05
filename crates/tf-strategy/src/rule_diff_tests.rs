use crate::momentum::MomentumParams;
use crate::rule_diff::{Change, ChangeKind, diff, loosens_a_veto};
use crate::rules::{MOMENTUM_RULES, Mode, RuleSet, StageKind};

fn rules(body: &str) -> RuleSet {
    RuleSet::parse(&format!("rules v1\n{body}")).unwrap()
}

const BASE: &str = "\
too_old any: secs_since_high > 90
armed all: impulse >= 300
dangerous any: depth > 350; volume_ratio > 250
enter all: depth >= 30; higher_lows >= 1; bid_support >= 150
";

fn d(base: &str, cand: &str) -> Vec<Change> {
    diff(&rules(base), &rules(cand), &MomentumParams::default())
}

fn with(from: &str, to: &str) -> String {
    assert!(BASE.contains(from), "{from}");
    BASE.replacen(from, to, 1)
}

#[test]
fn identical_rules_have_no_changes_and_the_built_in_set_has_none_against_itself() {
    assert!(d(BASE, BASE).is_empty());
    let m = RuleSet::momentum();
    assert!(diff(&m, &m, &MomentumParams::default()).is_empty());
}

#[test]
fn adding_a_condition_to_a_veto_is_stricter_and_never_flagged() {
    let c = d(
        BASE,
        &with("depth > 350;", "depth > 350; retrace_now > 400;"),
    );
    assert_eq!(c.len(), 1);
    assert!(matches!(c[0].kind, ChangeKind::Added(_)));
    assert_eq!(c[0].stage, StageKind::Dangerous);
    assert!(!c[0].loosens && !loosens_a_veto(&c));
}

#[test]
fn removing_a_veto_condition_loosens_but_removing_an_entry_condition_does_not() {
    let c = d(BASE, &with("; volume_ratio > 250", ""));
    assert_eq!(c.len(), 1);
    assert!(matches!(c[0].kind, ChangeKind::Removed(_)) && c[0].loosens);
    let c = d(
        BASE,
        &with("too_old any: secs_since_high > 90\n", "too_old any:\n"),
    );
    assert!(c[0].loosens && c[0].stage == StageKind::TooOld);
    let c = d(BASE, &with("; bid_support >= 150", ""));
    assert!(matches!(c[0].kind, ChangeKind::Removed(_)));
    assert!(
        !c[0].loosens,
        "an entry condition: the outcomes will show it"
    );
    assert!(!loosens_a_veto(&c));
}

#[test]
fn moving_a_greater_than_threshold_up_loosens_and_down_does_not() {
    assert!(loosens_a_veto(&d(
        BASE,
        &with("depth > 350", "depth > 400")
    )));
    assert!(!loosens_a_veto(&d(
        BASE,
        &with("depth > 350", "depth > 300")
    )));
    // `>= 351` and `> 350` are the same test on integers: a change, but not a loosening.
    let same = d(BASE, &with("depth > 350", "depth >= 351"));
    assert_eq!(same.len(), 1);
    assert!(!same[0].loosens);
    assert!(loosens_a_veto(&d(
        BASE,
        &with("depth > 350", "depth >= 352")
    )));
    assert!(loosens_a_veto(&d(
        BASE,
        &with("depth > 350", "depth > 351")
    )));
    assert!(
        !loosens_a_veto(&d(BASE, &with("depth > 350", "depth >= 350"))),
        "inclusive fires more"
    );
}

#[test]
fn moving_a_less_than_threshold_down_loosens_and_up_does_not() {
    let base = with("depth > 350", "bid_support < 100");
    assert!(loosens_a_veto(&d(&base, &base.replace("< 100", "< 80"))));
    assert!(!loosens_a_veto(&d(&base, &base.replace("< 100", "< 120"))));
    assert!(!loosens_a_veto(&d(&base, &base.replace("< 100", "<= 100"))));
    assert!(loosens_a_veto(&d(&base.replace("< 100", "<= 100"), &base)));
}

#[test]
fn a_threshold_that_follows_a_parameter_is_compared_at_the_parameters_value() {
    let p = with("depth > 350", "depth > @max_depth_permille");
    assert_eq!(MomentumParams::default().max_depth_permille, 350);
    let literal_same = d(&p, &with("depth > 350", "depth > 350"));
    assert_eq!(
        literal_same.len(),
        1,
        "a different form is shown as a change"
    );
    assert!(!literal_same[0].loosens, "of the same strictness");
    assert!(loosens_a_veto(&d(&p, &with("depth > 350", "depth > 360"))));
    assert!(!loosens_a_veto(&d(&with("depth > 350", "depth > 360"), &p)));
}

#[test]
fn changing_any_to_all_in_a_veto_loosens_it_but_all_to_any_and_a_single_condition_do_not() {
    let all = with("dangerous any", "dangerous all");
    let c = d(BASE, &all);
    assert_eq!(c.len(), 1);
    assert!(
        matches!(
            c[0].kind,
            ChangeKind::Mode {
                from: Mode::Any,
                to: Mode::All
            }
        ) && c[0].loosens
    );
    let c = d(&all, BASE);
    assert!(
        matches!(
            c[0].kind,
            ChangeKind::Mode {
                from: Mode::All,
                to: Mode::Any
            }
        ) && !c[0].loosens
    );
    // One condition: any and all are the same test.
    assert!(d(BASE, &with("too_old any", "too_old all")).is_empty());
    // Entry stage: shown, not flagged.
    let c = d(BASE, &with("enter all", "enter any"));
    assert!(c.len() == 1 && !c[0].loosens);
}

#[test]
fn conditions_pair_by_feature_and_direction_so_a_flipped_comparison_is_a_removal_and_an_addition() {
    let c = d(BASE, &with("depth > 350", "depth < 10"));
    assert_eq!(c.len(), 2);
    assert!(matches!(c[0].kind, ChangeKind::Removed(_)) && c[0].loosens);
    assert!(matches!(c[1].kind, ChangeKind::Added(_)) && !c[1].loosens);
    // Two conditions on one feature pair in order.
    let two = with("depth > 350", "depth > 350; depth > 500");
    let c = d(&two, &two.replace("depth > 500", "depth > 600"));
    assert_eq!(c.len(), 1);
    assert!(c[0].loosens, "the second one moved up");
}

#[test]
fn changes_read_as_lines_a_reviewer_can_follow() {
    let c = d(BASE, &with("depth > 350", "depth > 400"));
    assert_eq!(
        c[0].to_string(),
        "~ dangerous: depth > 350  ->  depth > 400   [LOOSENS A VETO]"
    );
    let c = d(BASE, &with("; bid_support >= 150", ""));
    assert_eq!(c[0].to_string(), "- enter: bid_support >= 150");
    let c = d(BASE, &with("depth >= 30", "depth >= 30; tape_ratio >= 100"));
    assert_eq!(c[0].to_string(), "+ enter: tape_ratio >= 100");
    let c = d(BASE, &with("dangerous any", "dangerous all"));
    assert_eq!(
        c[0].to_string(),
        "~ dangerous: any -> all   [LOOSENS A VETO]"
    );
}

#[test]
fn the_built_in_rules_against_a_dropped_veto_flag_the_dropped_veto() {
    let m = RuleSet::momentum();
    let none = RuleSet::parse(&MOMENTUM_RULES.replace(
        "dangerous any: depth > @max_depth_permille; volume_ratio > @max_volume_ratio_permille",
        "dangerous any:",
    ))
    .unwrap();
    let c = diff(&m, &none, &MomentumParams::default());
    assert_eq!(c.len(), 2);
    assert!(
        c.iter()
            .all(|c| c.loosens && c.stage == StageKind::Dangerous)
    );
}
