use tf_core::Px;
use tf_engine::PullbackFeatures;

use crate::momentum::MomentumParams;
use crate::rules::{Cmp, Condition, Feature, MOMENTUM_RULES, Mode, RuleSet, StageKind, Threshold};

fn features() -> PullbackFeatures {
    PullbackFeatures {
        impulse_low: Px::from_raw(1_000_000_000),
        impulse_high: Px::from_raw(1_400_000_000),
        pullback_low: Px::from_raw(1_300_000_000),
        last: Px::from_raw(1_350_000_000),
        impulse_secs: 20,
        secs_since_high: 40,
        depth_permille: 250,
        retrace_now_permille: 125,
        volume_ratio_permille: Some(180),
        higher_lows: 2,
        recent_trades_per_sec_x1000: 5_000,
        tape_ratio_permille: Some(700),
        tick_speed_x1000: 0,
        spread: Some(20_000_000),
        avg_spread: None,
        bid_support_permille: Some(400),
        avg_bid_support_permille: None,
    }
}

fn parse_err(text: &str) -> String {
    RuleSet::parse(text).expect_err(text).0
}

const BODY: &str = "\
too_old any: secs_since_high > 90
armed all: impulse >= 300
dangerous any: depth > 350
enter all: higher_lows >= 1
";

fn with_header(body: &str) -> String {
    format!("rules v1\n{body}")
}

#[test]
fn the_built_in_rules_parse_and_the_text_is_canonical() {
    let r = RuleSet::momentum();
    assert_eq!(r.render(), MOMENTUM_RULES, "render is the canonical text");
    assert_eq!(RuleSet::parse(&r.render()).unwrap(), r, "and round-trips");
    assert_eq!(r.too_old.mode, Mode::Any);
    assert_eq!(r.armed.conditions.len(), 2);
    assert_eq!(r.dangerous.conditions.len(), 2);
    assert_eq!(r.enter.conditions.len(), 5);
    // Every threshold of the built-in rules follows a tunable, so the store reaches them all.
    for k in [
        StageKind::TooOld,
        StageKind::Armed,
        StageKind::Dangerous,
        StageKind::Enter,
    ] {
        for c in &r.stage(k).conditions {
            let literal_zero = c.threshold == Threshold::Value(0);
            assert!(
                c.threshold.param_name().is_some() || literal_zero,
                "{k:?} {c:?}"
            );
        }
    }
}

#[test]
fn comments_blank_lines_and_spacing_do_not_change_the_rules_or_their_fingerprint() {
    let plain = RuleSet::parse(&with_header(BODY)).unwrap();
    let noisy = RuleSet::parse(&format!(
        "# a note\n\nrules v1   # header\n\n  too_old   any :  secs_since_high   >   90  \n{}",
        BODY.split_once('\n').unwrap().1
    ))
    .unwrap();
    assert_eq!(plain, noisy);
    assert_eq!(plain.fingerprint(), noisy.fingerprint());
    assert_eq!(plain.render(), noisy.render());
    // Any change to a threshold, a comparison, a feature or a mode is a different version.
    for changed in [
        BODY.replace("> 90", "> 91"),
        BODY.replace("> 90", ">= 90"),
        BODY.replace("depth > 350", "retrace_now > 350"),
        BODY.replace("dangerous any", "dangerous all"),
        BODY.replace("> 90", "> @max_pullback_secs"),
    ] {
        let other = RuleSet::parse(&with_header(&changed)).unwrap();
        assert_ne!(other.fingerprint(), plain.fingerprint(), "{changed}");
    }
}

#[test]
fn every_kind_of_bad_rule_text_is_refused_with_the_line_and_the_reason() {
    let cases: [(String, &str); 15] = [
        ("# only a comment\n".to_owned(), "missing `rules v1`"),
        (BODY.to_owned(), "line 1: expected `rules v1`"),
        ("rules v2\n".to_owned() + BODY, "expected `rules v1`"),
        (
            with_header(&BODY.replace("armed all", "armd all")),
            "line 3: unknown stage `armd`",
        ),
        (
            with_header(&format!("{BODY}enter all: depth >= 0\n")),
            "appears twice",
        ),
        (
            with_header(&BODY.replace("too_old any: secs_since_high > 90\n", "")),
            "stage `too_old` is missing",
        ),
        (
            with_header(&BODY.replace("armed all", "armed some")),
            "`some` is not `all` or `any`",
        ),
        (
            with_header(&BODY.replace("armed all", "armed")),
            "<stage> <all|any>",
        ),
        (
            with_header(&BODY.replace("impulse >=", "gap >=")),
            "unknown feature `gap`",
        ),
        (
            with_header(&BODY.replace("impulse >=", "impulse =>")),
            "unknown comparison `=>`",
        ),
        (
            with_header(&BODY.replace("300", "@nothing")),
            "`@nothing` is not a tunable parameter",
        ),
        (
            with_header(&BODY.replace("300", "3x")),
            "`3x` is not a number",
        ),
        (
            with_header(&BODY.replace("impulse >= 300", "impulse >= 300 now")),
            "is not `<feature> <comparison> <threshold>`",
        ),
        (
            with_header(&BODY.replace("enter all: higher_lows >= 1", "enter all:")),
            "`enter` needs at least one condition",
        ),
        (
            with_header(&BODY.replace("armed all: impulse >= 300", "armed all: impulse >=")),
            "line 3: `impulse >=`",
        ),
    ];
    for (text, want) in cases {
        let got = parse_err(&text);
        assert!(
            got.contains(want),
            "wanted `{want}` in `{got}` for:\n{text}"
        );
    }
    // Spending a parameter that exists is fine, including ones the strategy does not use in its defaults.
    assert!(RuleSet::parse(&with_header(&BODY.replace("350", "@max_depth_permille"))).is_ok());
}

#[test]
fn a_condition_compares_a_feature_and_a_missing_feature_never_passes() {
    let f = features();
    let p = MomentumParams::default();
    let cond = |feature, cmp, v| Condition {
        feature,
        cmp,
        threshold: Threshold::Value(v),
    };
    // Boundaries of each comparison.
    assert!(cond(Feature::Depth, Cmp::Ge, 250).holds(&f, 0, &p));
    assert!(!cond(Feature::Depth, Cmp::Gt, 250).holds(&f, 0, &p));
    assert!(cond(Feature::Depth, Cmp::Le, 250).holds(&f, 0, &p));
    assert!(!cond(Feature::Depth, Cmp::Lt, 250).holds(&f, 0, &p));
    assert!(cond(Feature::Depth, Cmp::Lt, 251).holds(&f, 0, &p));
    // The impulse comes from the caller; the others from the features.
    assert!(cond(Feature::Impulse, Cmp::Ge, 400).holds(&f, 400, &p));
    assert!(!cond(Feature::Impulse, Cmp::Ge, 400).holds(&f, 399, &p));
    assert_eq!(Feature::SecsSinceHigh.value(&f, 0), Some(40));
    assert_eq!(Feature::ImpulseSecs.value(&f, 0), Some(20));
    assert_eq!(Feature::HigherLows.value(&f, 0), Some(2));
    assert_eq!(Feature::TapeRatio.value(&f, 0), Some(700));
    assert_eq!(Feature::BidSupport.value(&f, 0), Some(400));
    assert_eq!(Feature::RetraceNow.value(&f, 0), Some(125));
    // No data: false for every comparison, including the ones a missing value "satisfies" in code.
    let mut none = f;
    none.volume_ratio_permille = None;
    none.bid_support_permille = None;
    none.tape_ratio_permille = None;
    for cmp in [Cmp::Ge, Cmp::Gt, Cmp::Le, Cmp::Lt] {
        for feature in [
            Feature::VolumeRatio,
            Feature::BidSupport,
            Feature::TapeRatio,
        ] {
            assert!(
                !cond(feature, cmp, 500).holds(&none, 0, &p),
                "{feature:?} {cmp:?}"
            );
            assert_eq!(feature.value(&none, 0), None);
        }
    }
}

#[test]
fn stages_combine_all_or_any_and_an_empty_stage_has_the_neutral_answer() {
    let f = features();
    let p = MomentumParams::default();
    let rules = RuleSet::parse(&with_header(
        "too_old any:\narmed all:\ndangerous any: depth > 500; higher_lows >= 2\nenter all: depth >= 250; higher_lows >= 3\n",
    ))
    .unwrap();
    assert!(!rules.too_old.holds(&f, 0, &p), "an empty `any` is false");
    assert!(rules.armed.holds(&f, 0, &p), "an empty `all` is true");
    assert!(
        rules.dangerous.holds(&f, 0, &p),
        "one of two is enough for `any`"
    );
    assert!(
        !rules.enter.holds(&f, 0, &p),
        "one of two is not enough for `all`"
    );
}

#[test]
fn a_parameter_threshold_follows_the_parameters_and_a_literal_does_not() {
    let f = features();
    let rules = RuleSet::parse(&with_header(
        &BODY.replace("depth > 350", "depth > @max_depth_permille"),
    ))
    .unwrap();
    let literal = RuleSet::parse(&with_header(BODY)).unwrap();
    let mut p = MomentumParams::default();
    assert_eq!(p.max_depth_permille, 350);
    assert!(!rules.dangerous.holds(&f, 0, &p), "250 is within 350");
    p.max_depth_permille = 200;
    assert!(rules.dangerous.holds(&f, 0, &p), "and beyond 200");
    assert!(
        !literal.dangerous.holds(&f, 0, &p),
        "the literal 350 did not move"
    );
}

#[test]
fn the_record_has_one_entry_per_condition_with_the_value_the_limit_and_the_verdict() {
    let f = features();
    let p = MomentumParams {
        max_depth_permille: 300,
        ..MomentumParams::default()
    };
    let rules = RuleSet::momentum();
    let ev = rules.evaluate(&f, 500, &p);
    assert_eq!(ev.len(), 1 + 2 + 2 + 5);
    let by = |stage, feature| {
        ev.iter()
            .find(|e| e.stage == stage && e.condition.feature == feature)
            .unwrap()
    };
    let d = by(StageKind::Dangerous, Feature::Depth);
    assert_eq!(
        (d.value, d.limit, d.pass),
        (Some(250), 300, false),
        "limit is the resolved parameter"
    );
    assert_eq!(
        d.condition.threshold.param_name(),
        Some("max_depth_permille")
    );
    let i = by(StageKind::Armed, Feature::Impulse);
    assert_eq!((i.value, i.limit, i.pass), (Some(500), 300, true));
    // The record agrees with the decision for every stage.
    for kind in [
        StageKind::TooOld,
        StageKind::Armed,
        StageKind::Dangerous,
        StageKind::Enter,
    ] {
        let st = rules.stage(kind);
        let mine: Vec<_> = ev.iter().filter(|e| e.stage == kind).collect();
        let verdict = match st.mode {
            Mode::All => mine.iter().all(|e| e.pass),
            Mode::Any => mine.iter().any(|e| e.pass),
        };
        assert_eq!(verdict, st.holds(&f, 500, &p), "{kind:?}");
    }
}
