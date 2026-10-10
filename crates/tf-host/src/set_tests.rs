use std::fs;

use crate::replay_tests::scratch;
use crate::set::{SetError, StrategySet, templates};

const UNIVERSE: &str = "universe v1\nstatic adv_shares >= 1000\n";

fn text(body: &str) -> String {
    format!("strategy set v1\n{body}")
}

fn refused(body: &str) -> String {
    StrategySet::parse(&text(body)).unwrap_err().0
}

const TWO: &str =
    "balance 100000\nstrategy 1 a t04 universe=u.txt names=3\nstrategy 2 b t04 universe=u.txt\n";

#[test]
fn a_set_names_its_strategies_their_parameters_and_the_budget_and_limits_around_them() {
    let s = StrategySet::parse(&text(
        "# the month\nbalance 250000\nloss 200 500\nlimits order=20000 position=5000 gross=900000 daily_loss=7000 orders=50 window_secs=2\n\nstrategy 7 reversal-a t04 universe=u/liquid.txt share=6000 names=3 dollars=1500 priority=4\nstrategy 9 null_1 t14 universe=u/liquid.txt seed=5\n",
    ))
    .unwrap();
    assert_eq!((s.balance, s.loss_soft, s.loss_hard), (250_000, 200, 500));
    assert_eq!(
        (
            s.limits.order,
            s.limits.position,
            s.limits.gross,
            s.limits.daily_loss,
            s.limits.orders,
            s.limits.window_secs
        ),
        (20_000, 5_000, 900_000, 7_000, 50, 2)
    );
    let a = &s.strategies[0];
    assert_eq!(
        (
            a.id,
            a.name.as_str(),
            a.template.as_str(),
            a.universe.as_str(),
            a.priority,
            a.share
        ),
        (7, "reversal-a", "t04", "u/liquid.txt", 4, Some(6_000))
    );
    // Every parameter of the template, the given ones over its defaults.
    assert!(
        a.params.contains("names=3")
            && a.params.contains("dollars=1500")
            && a.params.contains("stop_permille=100"),
        "{}",
        a.params
    );
    assert_eq!(a.params.split_whitespace().count(), 9);
    let b = &s.strategies[1];
    assert_eq!((b.priority, b.share), (1, None));
    assert!(b.params.contains("seed=5"), "{}", b.params);
    // The shares the line gave, and the rest to the other.
    assert_eq!(s.shares().unwrap(), [6_000, 4_000]);
}

#[test]
fn what_a_set_leaves_out_is_a_default_and_the_unassigned_shares_are_split_evenly() {
    let s = StrategySet::parse(&text("balance 90000\nstrategy 1 a t04 universe=u\nstrategy 2 b t04 universe=u\nstrategy 3 c t04 universe=u\n")).unwrap();
    assert_eq!((s.loss_soft, s.loss_hard), (300, 600));
    assert_eq!(
        (
            s.limits.order,
            s.limits.position,
            s.limits.gross,
            s.limits.orders,
            s.limits.window_secs
        ),
        (50_000, 100_000, 5_000_000, 10_000, 1)
    );
    assert_eq!(s.limits.daily_loss, 9_000, "a tenth of the balance");
    // Ten thousand over three: the first gets the remainder.
    assert_eq!(s.shares().unwrap(), [3_334, 3_333, 3_333]);
    let one = StrategySet::parse(&text("balance 100\nstrategy 1 a t04 universe=u\n")).unwrap();
    assert_eq!(one.limits.daily_loss, 10);
    assert_eq!(
        StrategySet::parse(&text("balance 5\nstrategy 1 a t04 universe=u\n"))
            .unwrap()
            .limits
            .daily_loss,
        1,
        "never zero"
    );
    let mixed = StrategySet::parse(&text("balance 1000\nstrategy 1 a t04 universe=u share=2500\nstrategy 2 b t04 universe=u\nstrategy 3 c t04 universe=u\n")).unwrap();
    assert_eq!(mixed.shares().unwrap(), [2_500, 3_750, 3_750]);
}

#[test]
fn a_set_that_is_not_well_formed_is_refused_with_the_line() {
    let r = |b: &str| refused(b);
    assert!(
        StrategySet::parse("balance 1\n")
            .unwrap_err()
            .0
            .contains("must be `strategy set v1`")
    );
    assert!(StrategySet::parse("").unwrap_err().0.contains("must be"));
    assert!(r("strategy 1 a t04 universe=u\n").contains("`balance DOLLARS` is missing"));
    assert!(r("balance 1000\n").contains("no strategy"));
    for (body, why) in [
        ("balance 0\n", "above zero"),
        ("balance x\n", "above zero"),
        ("balance 1\nbalance 2\n", "once"),
        ("balance 1000\nloss 600 300\n", "0 < soft < hard"),
        ("balance 1000\nloss 0 300\n", "0 < soft < hard"),
        ("balance 1000\nloss 300 10001\n", "0 < soft < hard"),
        ("balance 1000\nloss 300\n", "`loss SOFT HARD`"),
        ("balance 1000\nlimits order=0\n", "above zero"),
        ("balance 1000\nlimits order=x\n", "above zero"),
        ("balance 1000\nlimits wat=1\n", "not a limit"),
        ("balance 1000\nlimits order=5 order=6\n", "twice"),
        (
            "balance 1000\nlimits order=900 gross=800\n",
            "gross limit is below",
        ),
        ("balance 1000\nlimits order\n", "not key=value"),
        (
            "balance 1000\nlimits order=5\nlimits order=6\n",
            "given once",
        ),
        ("balance 1000\nwat 1\n", "not a line of a strategy set"),
        (
            "balance 1000\nstrategy 1 a\n",
            "`strategy NUMBER NAME TEMPLATE",
        ),
        (
            "balance 1000\nstrategy 0 a t04 universe=u\n",
            "not a strategy number",
        ),
        (
            "balance 1000\nstrategy x a t04 universe=u\n",
            "not a strategy number",
        ),
        (
            "balance 1000\nstrategy 1 A t04 universe=u\n",
            "not a strategy name",
        ),
        (
            "balance 1000\nstrategy 1 a.b t04 universe=u\n",
            "not a strategy name",
        ),
        (
            "balance 1000\nstrategy 1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa t04 universe=u\n",
            "not a strategy name",
        ),
        (
            "balance 1000\nstrategy 1 a t99 universe=u\n",
            "not a template (known: t04, t14, t25, t26)",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u wat=3\n",
            "not a parameter of t04",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u names\n",
            "not key=value",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u names=3 names=4\n",
            "twice",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u names=0\n",
            "names",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u names=x\n",
            "whole number",
        ),
        (
            "balance 1000\nstrategy 1 a t04 names=3\n",
            "`universe=FILE` is required",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u priority=300\n",
            "not a priority",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u share=0\n",
            "not a share",
        ),
        (
            "balance 1000\nstrategy 1 a t04 universe=u share=10001\n",
            "not a share",
        ),
    ] {
        let e = r(body);
        assert!(e.contains(why), "{body:?} gave {e:?}");
    }
    assert!(
        r("balance 1000\nstrategy 1 a t04 universe=u\nstrategy 1 b t04 universe=u\n")
            .contains("number 1 is used twice")
    );
    assert!(
        r("balance 1000\nstrategy 1 a t04 universe=u\nstrategy 2 a t04 universe=u\n")
            .contains("name `a` is used twice")
    );
    // The shares: over the whole, or leaving nothing for those that gave none.
    assert!(r("balance 1000\nstrategy 1 a t04 universe=u share=7000\nstrategy 2 b t04 universe=u share=4000\n").contains("over 10000"));
    assert!(
        r("balance 1000\nstrategy 1 a t04 universe=u share=10000\nstrategy 2 b t04 universe=u\n")
            .contains("leave nothing")
    );
    // A line number is where the error is.
    assert!(
        r("balance 1000\n\n# c\nwat 1\n").starts_with("line 5:")
            || r("balance 1000\n\n# c\nwat 1\n").starts_with("line 4:")
    );
}

#[test]
fn the_set_gives_the_host_its_limits_budgets_and_the_gateway_its_numbers() {
    let s = StrategySet::parse(&text(TWO)).unwrap();
    let cfg = s.host_config(4_096).unwrap();
    assert_eq!(cfg.id_space, 4_096);
    let l = &cfg.limits;
    assert_eq!(
        (
            l.max_order_notional(),
            l.max_position_shares(),
            l.max_gross_notional(),
            l.max_daily_loss(),
            l.max_orders_per_window(),
            l.rate_window_ns()
        ),
        (
            50_000 * 1_000_000_000,
            100_000,
            5_000_000 * 1_000_000_000,
            10_000 * 1_000_000_000,
            10_000,
            1_000_000_000
        )
    );
    let b = cfg.budgets.as_ref().unwrap();
    assert_eq!(b.balance(), 100_000 * 1_000_000_000);
    assert_eq!(
        b.ids()
            .iter()
            .map(|(n, id)| (*n, id.as_str()))
            .collect::<Vec<_>>(),
        [(1, "a"), (2, "b")]
    );
    assert_eq!(b.strategy_budget(1), Some(50_000 * 1_000_000_000));
    assert_eq!(b.tree().groups().len(), 1);
    assert_eq!(
        (
            b.tree().groups()[0].loss.soft,
            b.tree().groups()[0].loss.hard
        ),
        (300, 600)
    );
    assert_eq!(cfg.sim, crate::research::CostModel::published().sim());
    // A limit that cannot be (a gross below the order) is refused when the config is made, not when the day starts.
    let mut bad = s.clone();
    bad.limits.gross = 1;
    assert!(
        bad.host_config(1)
            .err()
            .expect("refused")
            .0
            .contains("limits are not valid")
    );
}

#[test]
fn the_definitions_have_their_universe_priority_and_parameters_and_each_variant_is_its_own() {
    let dir = scratch("set-defs");
    fs::create_dir_all(dir.join("u")).unwrap();
    fs::write(dir.join("u").join("liquid.txt"), UNIVERSE).unwrap();
    let path = dir.join("month.set");
    fs::write(
        &path,
        text("balance 100000\nstrategy 1 a t04 universe=u/liquid.txt names=3 priority=5\nstrategy 2 b t04 universe=u/liquid.txt names=4\nstrategy 3 n t14 universe=u/liquid.txt seed=9\n"),
    )
    .unwrap();
    let (set, defs) = StrategySet::load(&path).unwrap();
    assert_eq!(set.strategies.len(), 3);
    assert_eq!(
        defs.iter()
            .map(|d| (d.id, d.name.as_str(), d.priority))
            .collect::<Vec<_>>(),
        [(1, "a", 5), (2, "b", 1), (3, "n", 1)]
    );
    assert!(
        defs[0].params.contains("names=3")
            && defs[1].params.contains("names=4")
            && defs[2].params.contains("seed=9")
    );
    // Another parameter is another variant; the same line twice is the same variant.
    assert_ne!(defs[0].fingerprint(), defs[1].fingerprint());
    let (_, again) = StrategySet::load(&path).unwrap();
    assert_eq!(defs[0].fingerprint(), again[0].fingerprint());
    // A universe file that is not there, or does not read, names the strategy.
    fs::write(
        &path,
        text("balance 1000\nstrategy 1 a t04 universe=u/nope.txt\n"),
    )
    .unwrap();
    let e = StrategySet::load(&path).err().expect("refused").0;
    assert!(e.contains("strategy a") && e.contains("nope.txt"), "{e}");
    fs::write(dir.join("u").join("bad.txt"), "this is not a universe").unwrap();
    fs::write(
        &path,
        text("balance 1000\nstrategy 1 a t04 universe=u/bad.txt\n"),
    )
    .unwrap();
    assert!(
        StrategySet::load(&path)
            .err()
            .expect("refused")
            .0
            .contains("strategy a")
    );
    assert!(
        StrategySet::load(&dir.join("missing.set"))
            .err()
            .expect("refused")
            .0
            .contains("missing.set")
    );
    // The templates and their defaults are listed.
    let t = templates();
    assert_eq!(
        t.iter().map(|x| x.0).collect::<Vec<_>>(),
        ["t04", "t14", "t25", "t26"]
    );
    assert!(t[0].1.contains("names=20") && t[1].1.contains("seed="));
    assert!(t[2].1.contains("spike_x10=30") && t[2].1.contains("max_pullback_permille=300"));
    let _: Option<SetError> = None;
}

#[test]
fn the_edges_of_what_a_set_allows_are_in_and_one_past_them_is_out() {
    let ok = |b: &str| StrategySet::parse(&text(b)).map_err(|e| e.0);
    // Loss limits: soft below hard, hard up to the whole.
    assert!(ok("balance 1000\nloss 9999 10000\nstrategy 1 a t04 universe=u\n").is_ok());
    assert!(ok("balance 1000\nloss 1 2\nstrategy 1 a t04 universe=u\n").is_ok());
    let e = ok("balance 1000\nloss 300 300\nstrategy 1 a t04 universe=u\n").unwrap_err();
    assert!(e.contains("0 < soft < hard"), "{e}");
    assert!(ok("balance 1000\nloss 300 10001\nstrategy 1 a t04 universe=u\n").is_err());
    // Limits: a gross equal to the order limit is allowed, one dollar under is not.
    assert!(ok("balance 1000\nlimits order=900 gross=900\nstrategy 1 a t04 universe=u\n").is_ok());
    assert!(ok("balance 1000\nlimits order=900 gross=899\nstrategy 1 a t04 universe=u\n").is_err());
    // A name of 32 characters is allowed, of 33 is not; a line with just a number, name and template has no universe.
    let name32 = "a".repeat(32);
    assert!(
        ok(&format!(
            "balance 1000\nstrategy 1 {name32} t04 universe=u\n"
        ))
        .is_ok()
    );
    assert!(
        ok(&format!(
            "balance 1000\nstrategy 1 {name32}a t04 universe=u\n"
        ))
        .is_err()
    );
    let e = ok("balance 1000\nstrategy 1 a t04\n").unwrap_err();
    assert!(e.contains("`universe=FILE` is required"), "{e}");
    assert!(
        ok("balance 1000\nstrategy 1 a\n")
            .unwrap_err()
            .contains("`strategy NUMBER NAME TEMPLATE")
    );
    // Shares that add to exactly the whole are fine, all given or some left over.
    let whole = ok("balance 1000\nstrategy 1 a t04 universe=u share=6000\nstrategy 2 b t04 universe=u share=4000\n").unwrap();
    assert_eq!(whole.shares().unwrap(), [6_000, 4_000]);
    let one_left =
        ok("balance 1000\nstrategy 1 a t04 universe=u share=9999\nstrategy 2 b t04 universe=u\n")
            .unwrap();
    assert_eq!(one_left.shares().unwrap(), [9_999, 1]);
    // The rest split evenly, the remainder to the first: 7,500 over three is 2,500 each, 7,501 would leave one over.
    let even = ok("balance 1000\nstrategy 1 a t04 universe=u share=2500\nstrategy 2 b t04 universe=u\nstrategy 3 c t04 universe=u\nstrategy 4 d t04 universe=u\n").unwrap();
    assert_eq!(even.shares().unwrap(), [2_500, 2_500, 2_500, 2_500]);
    let odd = ok("balance 1000\nstrategy 1 a t04 universe=u share=2499\nstrategy 2 b t04 universe=u\nstrategy 3 c t04 universe=u\nstrategy 4 d t04 universe=u\n").unwrap();
    assert_eq!(odd.shares().unwrap(), [2_499, 2_501, 2_500, 2_500]);
    assert_eq!(odd.shares().unwrap().iter().sum::<u32>(), 10_000);
}

#[test]
fn the_premarket_template_is_a_template_with_its_parameters_checked_when_the_set_is_read() {
    let ok = StrategySet::parse(&text(
        "balance 100000\nstrategy 1 pm t25 universe=u.txt names=2 spike_x10=40 max_pullback_permille=250\n",
    ))
    .unwrap();
    assert_eq!(ok.strategies[0].template, "t25");
    assert!(ok.strategies[0].params.contains("spike_x10=40"));
    assert!(
        ok.strategies[0]
            .params
            .contains("max_pullback_permille=250")
    );
    // Values the template refuses are refused here, not when the day starts; so are keys it does not have.
    let e = refused("balance 100000\nstrategy 1 pm t25 universe=u.txt min_pullback_permille=400\n");
    assert!(e.contains("pullback bounds"), "{e}");
    let e = refused("balance 100000\nstrategy 1 pm t25 universe=u.txt extreme_bp=1\n");
    assert!(e.contains("not a parameter of t25"), "{e}");
    // It builds into a definition whose parameters are the text of its variant.
    let dir = scratch("t25-set");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("u.txt"), UNIVERSE).unwrap();
    fs::write(
        dir.join("m.set"),
        text("balance 100000\nstrategy 3 pm t25 universe=u.txt dollars=500\n"),
    )
    .unwrap();
    let (_, defs) = StrategySet::load(&dir.join("m.set")).unwrap();
    assert_eq!((defs[0].id, defs[0].name.as_str()), (3, "pm"));
    assert!(defs[0].params.contains("dollars=500") && defs[0].params.contains("names=3"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_sets_the_cluster_is_deployed_with_read_and_build() {
    // The files of deploy/k8s/config, as the jobs read them: a typo in one is found here and not on the cluster.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/k8s/config");
    for (file, strategies) in [("month.set", 4), ("premarket.set", 10)] {
        let (_, defs) =
            StrategySet::load(&dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(defs.len(), strategies, "{file}");
    }
    // The premarket variants differ from one another, and each is a variant of its own for the registry.
    let (_, defs) = StrategySet::load(&dir.join("premarket.set")).unwrap();
    let mut params: Vec<&str> = defs.iter().map(|d| d.params.as_str()).collect();
    params.sort_unstable();
    params.dedup();
    assert_eq!(params.len(), 10);
    assert!(defs.iter().all(|d| d.name.starts_with("pm-")));
}
