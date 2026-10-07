//! The numbers pinned here are from `scripts/check_stats.py`, a separate implementation (standard library only), for the
//! data below; a few are worked by hand and said so.

use std::fs;
use std::path::PathBuf;

use crate::boot::{default_block, quantile};
use crate::norm::{cdf, erfc, inv_cdf};
use crate::registry::Registered;
use crate::rng::SplitMix64;
use crate::sharpe::variance;
use crate::*;

fn close(a: f64, b: f64, tol: f64) {
    assert!(
        (a - b).abs() <= tol * (1.0 + b.abs()),
        "{a} is not within {tol} of {b}"
    );
}

/// One basis-point result per trade, by day; day 3 has no trade.
fn plain_days() -> Vec<Vec<f64>> {
    vec![
        vec![12.0, -5.0, 8.0],
        vec![-20.0, 4.0],
        vec![7.0],
        vec![],
        vec![15.0, 9.0, -3.0, 6.0],
        vec![-11.0, -2.0],
        vec![5.0, 5.0, 10.0],
        vec![-8.0],
        vec![22.0, -14.0],
        vec![3.0, 1.0, 9.0],
    ]
}

/// The refinement: the plain trades without the losers it would not have taken, on the same signals.
fn refined_days() -> Vec<Vec<f64>> {
    vec![
        vec![12.0, 8.0],
        vec![4.0],
        vec![],
        vec![],
        vec![15.0, 9.0, 6.0],
        vec![],
        vec![10.0],
        vec![],
        vec![22.0],
        vec![9.0],
    ]
}

fn day_names(n: usize) -> Vec<String> {
    (1..=n).map(|d| format!("2026-05-{d:02}")).collect()
}

/// A variant from per-day results in basis points. Trade `k` of a day is in symbol `S{k}`, entered at `10 k` and closed
/// at `10 k + 5`; the trades of the refinement keep the numbers of the plain trades they are (`keep` lists those).
fn variant(fp: u64, name: &str, days: &[Vec<f64>], with_r: bool) -> Variant {
    variant_from(fp, name, days, with_r, None)
}

fn variant_from(
    fp: u64,
    name: &str,
    days: &[Vec<f64>],
    with_r: bool,
    slots: Option<&[Vec<usize>]>,
) -> Variant {
    let names = day_names(days.len());
    let mut outcomes = Vec::new();
    for (d, day) in days.iter().enumerate() {
        for (k, &v) in day.iter().enumerate() {
            let slot = slots.map_or(k, |s| s[d][k]);
            outcomes.push(Outcome {
                day: names[d].clone(),
                symbol: format!("S{slot}"),
                entry_ts: 10 * slot as u64,
                exit_ts: 10 * slot as u64 + 5,
                net_bps_x100: (v * 100.0).round() as i64,
                r_milli: with_r.then(|| (v * 10.0).round() as i64),
            });
        }
    }
    Variant {
        fingerprint: fp,
        name: name.to_owned(),
        outcomes,
    }
}

/// The slots of the plain trades the refinement of `refined_days` is.
fn refined_slots() -> Vec<Vec<usize>> {
    vec![
        vec![0, 2],
        vec![1],
        vec![],
        vec![],
        vec![0, 1, 3],
        vec![],
        vec![2],
        vec![],
        vec![0],
        vec![2],
    ]
}

fn registry_of(n: usize) -> Registry {
    let mut reg = Registry::new();
    for i in 0..n {
        reg.register(i as u64 + 1, &format!("v{i}"), "2026-10-07")
            .unwrap();
    }
    reg
}

fn cfg() -> Bootstrap {
    Bootstrap {
        replicates: 500,
        block: Some(2),
        seed: 42,
    }
}

// ---- the normal distribution ----

#[test]
fn the_distribution_function_matches_known_values_in_the_body_and_the_tails() {
    // From the independent check; the tails are where a series gives out and a fraction takes over (3.0 is the seam).
    for (x, want) in [
        (-5.0, 2.866515718791946e-07),
        (-1.0, 0.15865525393145707),
        (0.0, 0.5),
        (1.96, 0.9750021048517795),
        (3.0, 0.9986501019683699),
        (6.0, 0.9999999990134123),
    ] {
        close(cdf(x), want, 1e-12);
    }
    // erfc itself: erfc(0) = 1, erfc(inf) -> 0, and erfc(-x) = 2 - erfc(x).
    assert_eq!(erfc(0.0), 1.0);
    close(erfc(-1.0), 2.0 - erfc(1.0), 1e-15);
    // Either side of the seam between the series and the fraction agree.
    close(erfc(2.999_999_9), erfc(3.000_000_1), 1e-6);
    assert!(erfc(f64::NAN).is_nan());
    // Far in the tail the answer is tiny and not zero, and symmetric.
    assert!(cdf(-30.0) > 0.0 && cdf(-30.0) < 1e-190);
    close(cdf(0.7) + cdf(-0.7), 1.0, 1e-15);
}

#[test]
fn the_quantile_function_inverts_it_and_is_exact_in_the_tail() {
    for (p, want) in [
        (1e-10, -6.3613409024040575),
        (0.001, -3.090232306167814),
        (0.025, -1.9599639845400545),
        (0.9, 1.2815515655446008),
        (0.975, 1.959963984540054),
        (0.999, 3.090232306167813),
    ] {
        close(inv_cdf(p), want, 1e-12);
    }
    assert!(inv_cdf(0.5).abs() < 1e-12);
    // A round trip: exact down the lower tail; on the upper side only as far as the probability itself is exact (past
    // 4 the distance of `p` from 1 is mostly rounding: that is what the complement is for).
    let mut x = -8.0;
    while x < 4.0 {
        close(inv_cdf(cdf(x)), x, 1e-9);
        x += 0.37;
    }
    // Where it matters, the complement: the upper 1e-12 point is minus the lower, and exact.
    close(-inv_cdf(1e-12), 7.034483825301132, 1e-12);
    close(inv_cdf(0.3), -inv_cdf(0.7), 1e-12);
    assert_eq!(inv_cdf(0.0), f64::NEG_INFINITY);
    assert_eq!(inv_cdf(1.0), f64::INFINITY);
    assert!(inv_cdf(-0.1).is_nan() && inv_cdf(1.1).is_nan() && inv_cdf(f64::NAN).is_nan());
}

// ---- the generator ----

#[test]
fn the_generator_gives_the_published_sequence_and_unbiased_indices() {
    // SplitMix64 from seed 1: the reference values (also in the independent check).
    let mut r = SplitMix64::new(1);
    assert_eq!(
        [r.next_u64(), r.next_u64(), r.next_u64()],
        [
            10451216379200822465,
            13757245211066428519,
            17911839290282890590
        ]
    );
    let mut r = SplitMix64::new(7);
    assert_eq!(
        (0..5).map(|_| r.below(10)).collect::<Vec<_>>(),
        [3, 0, 9, 5, 4]
    );
    // Every index comes up, none out of range, and roughly equally often.
    let mut r = SplitMix64::new(3);
    let mut seen = [0u32; 7];
    for _ in 0..70_000 {
        seen[r.below(7)] += 1;
    }
    assert!(
        seen.iter().all(|&c| (9_000..11_000).contains(&c)),
        "{seen:?}"
    );
    assert_eq!(SplitMix64::new(9).below(1), 0);
}

// ---- results by day ----

#[test]
fn a_mean_and_its_cluster_error_by_hand() {
    // Days of (sum, trades): (10, 2), none, (-4, 1). The mean is 6 / 3 = 2. Residuals s - m n: 6, 0, -6, so the
    // squares sum to 72; times D / (D - 1) = 3/2 is 108; its root over 3 trades is 2 sqrt(3).
    let s = DaySeries::from_pairs([(10.0, 2), (0.0, 0), (-4.0, 1)]);
    assert_eq!((s.days(), s.trades()), (3, 3));
    assert_eq!(s.mean(), Some(2.0));
    close(s.cluster_se().unwrap(), 2.0 * 3f64.sqrt(), 1e-15);
    // Nothing to average, and too few days for an error.
    assert_eq!(DaySeries::from_pairs([(0.0, 0), (0.0, 0)]).mean(), None);
    assert_eq!(
        DaySeries::from_pairs([(0.0, 0), (0.0, 0)]).cluster_se(),
        None
    );
    assert_eq!(DaySeries::from_pairs([(5.0, 1)]).cluster_se(), None);
    assert_eq!(DaySeries::from_pairs([(5.0, 1)]).mean(), Some(5.0));
}

fn plain_series() -> DaySeries {
    DaySeries::from_pairs(
        plain_days()
            .iter()
            .map(|d| (d.iter().sum(), d.len() as u32)),
    )
}

#[test]
fn the_bootstrap_of_a_variant_matches_the_independent_check() {
    let s = plain_series();
    // 53 over 21 trades, by hand.
    assert_eq!((s.trades(), s.days()), (21, 10));
    close(s.mean().unwrap(), 53.0 / 21.0, 1e-15);
    close(s.mean().unwrap(), 2.5238095238095237, 1e-15);
    close(s.cluster_se().unwrap(), 1.900203305786516, 1e-12);
    let b = s.bootstrap(cfg()).unwrap();
    close(b.estimate, 2.5238095238095237, 1e-12);
    close(b.se, 1.1381805081179102, 1e-9);
    close(b.t.unwrap(), 2.2174070859664283, 1e-9);
    close(b.lo, 0.21764705882352953, 1e-9);
    close(b.hi, 4.719619565217388, 1e-9);
    assert_eq!((b.replicates, b.block, b.days), (500, 2, 10));
}

#[test]
fn a_bootstrap_is_repeatable_follows_its_seed_and_has_defaults_that_are_sound() {
    let s = plain_series();
    assert_eq!(s.bootstrap(cfg()), s.bootstrap(cfg()));
    let other = Bootstrap { seed: 43, ..cfg() };
    assert_ne!(
        s.bootstrap(cfg()).unwrap().se,
        s.bootstrap(other).unwrap().se
    );
    // A block of one day is the ordinary bootstrap of days, and its error is near the textbook one.
    let one = s
        .bootstrap(Bootstrap {
            replicates: 4_000,
            block: Some(1),
            seed: 1,
        })
        .unwrap();
    let analytic = s.cluster_se().unwrap();
    assert!(
        (one.se / analytic - 1.0).abs() < 0.12,
        "{} against {analytic}",
        one.se
    );
    // The default block is the cube root of the days, rounded up; a block cannot exceed the days.
    assert_eq!(
        [1, 2, 8, 9, 10, 27, 28].map(default_block),
        [1, 2, 2, 3, 3, 3, 4]
    );
    assert_eq!(Bootstrap::default().replicates, 2_000);
    let wide = s
        .bootstrap(Bootstrap {
            block: Some(50),
            ..cfg()
        })
        .unwrap();
    assert_eq!(wide.block, 10);
    assert_eq!(s.bootstrap(Bootstrap::default()).unwrap().block, 3);
    // Too little to resample: one day, one replicate, or no trade at all.
    assert!(DaySeries::from_pairs([(5.0, 1)]).bootstrap(cfg()).is_none());
    assert!(
        s.bootstrap(Bootstrap {
            replicates: 1,
            ..cfg()
        })
        .is_none()
    );
    assert!(
        DaySeries::from_pairs([(0.0, 0), (0.0, 0), (0.0, 0)])
            .bootstrap(cfg())
            .is_none()
    );
}

#[test]
fn a_result_that_cannot_vary_has_an_error_of_nothing_and_no_t_statistic() {
    // Four days of one trade of 5: every resample gives 5.
    let s = DaySeries::from_pairs([(5.0, 1); 4]);
    let b = s.bootstrap(cfg()).unwrap();
    assert_eq!((b.estimate, b.se, b.lo, b.hi), (5.0, 0.0, 5.0, 5.0));
    assert_eq!(b.t, None);
    assert_eq!(s.cluster_se(), Some(0.0));
    // The same with a result the arithmetic cannot hold exactly: three trades of -10.29 on three days. The sums differ
    // from the product by a rounding, and that is not an error to divide an estimate by.
    let s = DaySeries::from_pairs([(-10.29, 1), (-10.29, 1), (-10.29, 1)]);
    let b = s.bootstrap(cfg()).unwrap();
    assert_eq!((b.se, b.t), (0.0, None));
    assert_eq!(s.cluster_se(), Some(0.0));
    let s = DaySeries::from_pairs([(-20.58, 2), (-10.29, 1), (-30.87, 3)]);
    assert_eq!(s.bootstrap(cfg()).unwrap().se, 0.0);
    assert_eq!(s.cluster_se(), Some(0.0));
    // A real spread, however small against a large mean, is kept.
    let s = DaySeries::from_pairs([(1_000.0, 1), (1_000.5, 1), (999.5, 1)]);
    assert!(s.cluster_se().unwrap() > 0.1 && s.bootstrap(cfg()).unwrap().se > 0.1);
}

#[test]
fn a_resample_with_no_trade_in_it_gives_no_estimate_and_is_not_counted() {
    // One day in ten has a trade: some resamples miss it.
    let mut pairs = [(0.0, 0); 10];
    pairs[4] = (3.0, 1);
    let b = DaySeries::from_pairs(pairs)
        .bootstrap(Bootstrap {
            replicates: 300,
            block: Some(1),
            seed: 5,
        })
        .unwrap();
    assert!(b.replicates < 300 && b.replicates > 150, "{}", b.replicates);
    assert_eq!(b.estimate, 3.0);
}

#[test]
fn quantiles_interpolate_between_the_two_nearest() {
    let v = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];
    assert_eq!(quantile(&v, 0.0), 0.0);
    assert_eq!(quantile(&v, 1.0), 9.0);
    close(quantile(&v, 0.5), 4.5, 1e-15);
    close(quantile(&v, 0.95), 8.55, 1e-15);
    close(quantile(&v, 0.025), 0.225, 1e-15);
    assert_eq!(quantile(&[7.0], 0.3), 7.0);
}

// ---- the daily Sharpe ratio ----

#[test]
fn moments_match_the_independent_check_and_refuse_what_has_no_spread() {
    let daily: Vec<f64> = plain_days().iter().map(|d| d.iter().sum()).collect();
    assert_eq!(
        daily,
        [15.0, -16.0, 7.0, 0.0, 27.0, -13.0, 20.0, -8.0, 8.0, 13.0]
    );
    let m = moments(&daily).unwrap();
    assert_eq!(m.n, 10);
    close(m.mean, 5.3, 1e-15);
    close(m.sd, 14.314328485821472, 1e-12);
    close(m.skew, -0.13996078325242423, 1e-12);
    close(m.kurt, 1.8552832199511016, 1e-12);
    close(m.sharpe(), 0.3702583746942596, 1e-12);
    // By hand: for 1, 2, 3 the mean is 2, the sample sd 1, no skew, and Pearson's kurtosis (1 + 0 + 1) / 3 / (2/3)^2 = 1.5.
    let m = moments(&[1.0, 2.0, 3.0]).unwrap();
    assert_eq!((m.mean, m.sd, m.skew), (2.0, 1.0, 0.0));
    close(m.kurt, 1.5, 1e-15);
    assert!(moments(&[1.0, 2.0]).is_none());
    assert!(moments(&[4.0, 4.0, 4.0]).is_none());
    // Two values of variance: 1, 3 around a mean of 2 is 2.
    assert_eq!(variance(&[1.0, 3.0]), Some(2.0));
    assert_eq!(variance(&[1.0]), None);
}

#[test]
fn the_expected_best_of_many_and_the_deflated_ratio_match_the_independent_check() {
    close(
        expected_max_sharpe(7, 0.04).unwrap(),
        0.27735489227083204,
        1e-12,
    );
    close(
        expected_max_sharpe(100, 0.01).unwrap(),
        0.2530602893201685,
        1e-12,
    );
    // More trials, a higher bar; no spread among them, no bar; fewer than two trials, no answer.
    assert!(expected_max_sharpe(1000, 0.04).unwrap() > expected_max_sharpe(7, 0.04).unwrap());
    assert_eq!(expected_max_sharpe(7, 0.0), Some(0.0));
    assert_eq!(expected_max_sharpe(1, 0.04), None);
    assert_eq!(expected_max_sharpe(0, 0.04), None);
    assert_eq!(expected_max_sharpe(7, -0.01), None);
    assert_eq!(expected_max_sharpe(7, f64::NAN), None);

    let daily: Vec<f64> = plain_days().iter().map(|d| d.iter().sum()).collect();
    let m = moments(&daily).unwrap();
    close(
        probabilistic_sharpe(m.sharpe(), 0.0, m.n, m.skew, m.kurt).unwrap(),
        0.8573035301264635,
        1e-12,
    );
    close(
        deflated_sharpe(&m, 7, 0.04).unwrap(),
        0.6056690020420549,
        1e-12,
    );
    // A bar of nothing and no skew or tail makes it the ordinary probability that the ratio is positive.
    close(
        probabilistic_sharpe(0.0, 0.0, 50, 0.0, 3.0).unwrap(),
        0.5,
        1e-15,
    );
    // A ratio the skew cannot support has no variance to speak of, and a single day has no standard error.
    assert_eq!(probabilistic_sharpe(2.0, 0.0, 30, 1.0, 1.0), None);
    assert_eq!(probabilistic_sharpe(0.5, 0.0, 1, 0.0, 3.0), None);
    assert_eq!(probabilistic_sharpe(f64::NAN, 0.0, 30, 0.0, 3.0), None);
    assert_eq!(deflated_sharpe(&m, 1, 0.04), None);
}

// ---- the registry ----

#[test]
fn a_variant_is_entered_once_and_keeps_its_first_date_and_name() {
    let mut reg = Registry::new();
    assert!(reg.is_empty());
    assert_eq!(
        reg.register(0xabc, "gap", "2026-10-07"),
        Ok(Registered::New)
    );
    assert_eq!(
        reg.register(0xdef, "vwap", "2026-10-08"),
        Ok(Registered::New)
    );
    // Run again later, under another name: nothing changes.
    assert_eq!(
        reg.register(0xabc, "gap2", "2026-11-01"),
        Ok(Registered::Known)
    );
    assert_eq!(reg.len(), 2);
    let t = reg.get(0xabc).unwrap();
    assert_eq!(
        (t.name.as_str(), t.first_run.as_str()),
        ("gap", "2026-10-07")
    );
    assert!(reg.contains(0xdef) && !reg.contains(1));
    assert_eq!(
        reg.trials()
            .iter()
            .map(|t| t.fingerprint)
            .collect::<Vec<_>>(),
        [0xabc, 0xdef]
    );
    // A name or a date that would not read back is refused, and nothing is entered.
    for (name, date) in [
        ("", "2026-10-07"),
        ("a\tb", "2026-10-07"),
        ("a\nb", "2026-10-07"),
        ("a\rb", "2026-10-07"),
        ("ok", "2026-10-7"),
        ("ok", "2026-13-01"),
        ("ok", "2026-00-01"),
        ("ok", "2026-10-32"),
        ("ok", "2026-10-00"),
        ("ok", "2026/10/07"),
        ("ok", "2026-10-071"),
        ("ok", "2026-1x-07"),
        ("ok", "2026x10-07"),
        ("ok", "2026-10x07"),
    ] {
        assert!(
            reg.register(0x999, name, date).is_err(),
            "{name:?} {date:?}"
        );
    }
    assert_eq!(reg.len(), 2);
    // Even a variant already in it is refused a bad name: the check comes first.
    assert!(reg.register(0xabc, "", "2026-10-07").is_err());
}

#[test]
fn the_registry_reads_back_exactly_and_every_kind_of_damage_is_noticed() {
    let mut reg = Registry::new();
    reg.register(0xabc, "gap fade", "2026-10-07").unwrap();
    reg.register(u64::MAX, "vwap", "2026-10-08").unwrap();
    let text = reg.render();
    assert!(text.starts_with("trial registry v1\ntrial\t0000000000000abc\t2026-10-07\tgap fade\n"));
    assert_eq!(Registry::parse(&text).unwrap(), reg);
    assert_eq!(
        Registry::parse(&Registry::new().render()).unwrap(),
        Registry::new()
    );
    let seal = |body: &str| {
        // A body with a correct checksum, so that the damage under test is the only damage.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in body.as_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{body}end {h:016x}\n")
    };
    let body = &text[..text.rfind("end ").unwrap()];
    for (bad, what) in [
        (text.replacen("gap fade", "gap fadx", 1), "checksum"),
        (text.replace("end ", "ent "), "cut short"),
        (text[..text.len() - 1].to_owned(), "after `end`"),
        (format!("{text}trial\t1\t2026-10-07\tz\n"), "after `end`"),
        (text.replace("end ", "end zz"), "hexadecimal"),
        (
            seal(&body.replacen("v1", "v2", 1)),
            "not `trial registry v1`",
        ),
        (seal(&format!("{body}surprise\n")), "does not know"),
        (
            seal(&format!("{body}trial\tzz\t2026-10-07\tn\n")),
            "not a fingerprint",
        ),
        (
            seal(&format!("{body}trial\t1\t2026-10-07\n")),
            "does not know",
        ),
        (
            seal(&format!("{body}trial\t1\t2026-13-07\tn\n")),
            "not a date",
        ),
        (
            seal(&format!("{body}trial\t0000000000000abc\t2026-10-07\tn\n")),
            "entered twice",
        ),
    ] {
        let e = Registry::parse(&bad).unwrap_err().to_string();
        assert!(e.contains(what), "{what}: {e}");
    }
    // An `end` inside a name is not the end.
    let mut tricky = Registry::new();
    tricky.register(1, "the end of it", "2026-10-07").unwrap();
    assert_eq!(Registry::parse(&tricky.render()).unwrap(), tricky);
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-stats-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_registry_is_saved_whole_and_a_missing_file_is_an_empty_registry() {
    let dir = scratch("reg");
    let path = dir.join("trials.reg");
    assert_eq!(Registry::load(&path).unwrap(), Registry::new());
    let mut reg = Registry::new();
    reg.register(7, "gap", "2026-10-07").unwrap();
    reg.save(&path).unwrap();
    assert_eq!(Registry::load(&path).unwrap(), reg);
    // Nothing half-written is left beside it, and a second save replaces the first.
    reg.register(8, "vwap", "2026-10-08").unwrap();
    reg.save(&path).unwrap();
    assert_eq!(Registry::load(&path).unwrap().len(), 2);
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["trials.reg"]);
    // A damaged file is an error, not an empty registry, and so is a path that is not a file.
    fs::write(&path, "trial registry v1\n").unwrap();
    assert!(Registry::load(&path).is_err());
    assert!(Registry::load(&dir).is_err());
    assert!(reg.save(&dir.join("missing-dir").join("x")).is_err());
}

// ---- a variant's figures ----

#[test]
fn a_variants_figures_match_the_independent_check() {
    let days = day_names(10);
    let reg = registry_of(7);
    let v = variant(1, "plain", &plain_days(), true);
    let rep = report(&[v], &days, &reg, cfg()).unwrap();
    let s = &rep[0];
    assert_eq!((s.fingerprint, s.name.as_str()), (1, "plain"));
    assert_eq!((s.trials, s.days, s.trades), (7, 10, 21));
    assert_eq!((s.bp.trades, s.r.trades), (21, 21));
    close(s.bp.mean.unwrap(), 2.5238095238095237, 1e-12);
    close(s.bp.cluster_se.unwrap(), 1.900203305786516, 1e-12);
    let b = s.bp.boot.unwrap();
    close(b.se, 1.1381805081179102, 1e-9);
    close(b.t.unwrap(), 2.2174070859664283, 1e-9);
    // 14 of 21 made money; the average win over the average loss; the deepest fall of the running total (by hand: it
    // reaches 15 after the first day, then 4 less, then -20 on day 1: from a peak of 15 to -5 is 20).
    close(s.hit_rate.unwrap(), 2.0 / 3.0, 1e-15);
    close(s.payoff.unwrap(), 0.9206349206349207, 1e-12);
    assert_eq!(s.max_drawdown_bp, 20.0);
    close(s.daily.unwrap().sharpe(), 0.3702583746942596, 1e-12);
    // R is the same trades in other units (here a hundredth of the basis points), so the same ratios.
    close(s.r.mean.unwrap(), s.bp.mean.unwrap() / 100.0, 1e-12);
    close(s.r.boot.unwrap().t.unwrap(), b.t.unwrap(), 1e-9);
    // One variant in the report: no spread of Sharpe ratios to estimate, so no deflation.
    assert_eq!(s.deflated_sharpe, None);
}

#[test]
fn the_deflated_ratio_uses_the_registrys_count_and_the_spread_among_the_variants_reported() {
    let days = day_names(10);
    let a = variant(1, "plain", &plain_days(), false);
    let b = variant_from(2, "refined", &refined_days(), false, Some(&refined_slots()));
    let rep5 = report(&[a.clone(), b.clone()], &days, &registry_of(5), cfg()).unwrap();
    let rep50 = report(&[a, b], &days, &registry_of(50), cfg()).unwrap();
    let (ma, mb) = (rep5[0].daily.unwrap(), rep5[1].daily.unwrap());
    let var = variance(&[ma.sharpe(), mb.sharpe()]).unwrap();
    assert_eq!((rep5[0].trials, rep50[0].trials), (5, 50));
    close(
        rep5[0].deflated_sharpe.unwrap(),
        deflated_sharpe(&ma, 5, var).unwrap(),
        1e-12,
    );
    close(
        rep5[1].deflated_sharpe.unwrap(),
        deflated_sharpe(&mb, 5, var).unwrap(),
        1e-12,
    );
    // Fifty trials are a higher bar than five.
    assert!(rep50[0].deflated_sharpe.unwrap() < rep5[0].deflated_sharpe.unwrap());
    // With one trial in the registry there is nothing to deflate by.
    let one = report(
        &[
            variant(1, "plain", &plain_days(), false),
            variant(2, "x", &refined_days(), false),
        ],
        &days,
        &registry_of(2),
        cfg(),
    )
    .unwrap();
    assert!(one[0].deflated_sharpe.is_some());
    let reg1 = registry_of(1);
    let only = variant(1, "plain", &plain_days(), false);
    assert_eq!(
        report(&[only], &days, &reg1, cfg()).unwrap()[0].deflated_sharpe,
        None
    );
}

#[test]
fn a_variant_that_is_not_in_the_registry_cannot_be_reported() {
    let days = day_names(10);
    let reg = registry_of(1);
    let known = variant(1, "plain", &plain_days(), false);
    let stranger = variant(99, "sneaky", &plain_days(), false);
    let e = report(&[known.clone(), stranger.clone()], &days, &reg, cfg()).unwrap_err();
    assert_eq!(
        e,
        StatsError::NotRegistered {
            name: "sneaky".into(),
            fingerprint: 99
        }
    );
    assert!(e.to_string().contains("sneaky") && e.to_string().contains("0000000000000063"));
    // Nor can it be one side of a comparison.
    assert!(paired(&stranger, &known, &days, &reg, cfg()).is_err());
    assert!(paired(&known, &stranger, &days, &reg, cfg()).is_err());
    // And a report of nothing is empty, not an error.
    assert!(report(&[], &days, &reg, cfg()).unwrap().is_empty());
}

#[test]
fn the_days_of_a_run_must_be_in_order_and_hold_every_trade() {
    let reg = registry_of(1);
    let v = variant(1, "plain", &plain_days(), false);
    let mut days = day_names(10);
    days.swap(2, 3);
    assert!(matches!(
        report(std::slice::from_ref(&v), &days, &reg, cfg()),
        Err(StatsError::Days(_))
    ));
    let mut twice = day_names(10);
    twice[4] = twice[3].clone();
    assert!(report(std::slice::from_ref(&v), &twice, &reg, cfg()).is_err());
    // A trade on a day that is not among them is not dropped.
    let short = day_names(9);
    let e = report(std::slice::from_ref(&v), &short, &reg, cfg())
        .unwrap_err()
        .to_string();
    assert!(e.contains("plain") && e.contains("2026-05-10"), "{e}");
    assert!(paired(&v, &v, &short, &reg, cfg()).is_err());
    assert!(paired(&v, &v, &days, &reg, cfg()).is_err());
}

#[test]
fn drawdown_follows_the_order_trades_closed_whatever_order_they_are_given_in() {
    // Results in the order they closed: +5, -3, -4, +2, -1: the total goes 5, 2, -2, 0, -1, so from the peak of 5 the
    // deepest it falls is to -2: 7.
    let mk = |day: &str, exit: u64, bp: i64| Outcome {
        day: day.into(),
        symbol: "S".into(),
        entry_ts: exit - 1,
        exit_ts: exit,
        net_bps_x100: bp * 100,
        r_milli: None,
    };
    let trades = vec![
        mk("2026-05-01", 10, 5),
        mk("2026-05-01", 20, -3),
        mk("2026-05-02", 10, -4),
        mk("2026-05-02", 20, 2),
        mk("2026-05-02", 30, -1),
    ];
    let days = day_names(3);
    let reg = registry_of(1);
    let dd = |outcomes: Vec<Outcome>| {
        let v = Variant {
            fingerprint: 1,
            name: "p".into(),
            outcomes,
        };
        report(&[v], &days, &reg, cfg()).unwrap()[0].max_drawdown_bp
    };
    assert_eq!(dd(trades.clone()), 7.0);
    let mut shuffled = trades.clone();
    shuffled.reverse();
    shuffled.swap(1, 3);
    assert_eq!(dd(shuffled), 7.0);
    // Only losses: the fall is from the starting nothing. Only wins: no fall.
    assert_eq!(
        dd(vec![mk("2026-05-01", 10, -2), mk("2026-05-01", 20, -3)]),
        5.0
    );
    assert_eq!(
        dd(vec![mk("2026-05-01", 10, 2), mk("2026-05-01", 20, 3)]),
        0.0
    );
    assert_eq!(dd(vec![]), 0.0);
}

#[test]
fn a_variant_with_no_trades_or_one_kind_of_result_has_figures_that_say_so() {
    let days = day_names(4);
    let reg = registry_of(1);
    let none = Variant {
        fingerprint: 1,
        name: "idle".into(),
        outcomes: vec![],
    };
    let s = &report(&[none], &days, &reg, cfg()).unwrap()[0];
    assert_eq!(s.trades, 0);
    assert_eq!(
        (s.bp.mean, s.hit_rate, s.payoff, s.daily),
        (None, None, None, None)
    );
    assert!(s.bp.boot.is_none() && s.r.boot.is_none() && s.bp.cluster_se.is_none());
    // Only winners: a hit rate of one and no payoff ratio (nothing lost); a trade of nothing is not a win.
    let mut v = variant(1, "w", &[vec![4.0], vec![0.0], vec![6.0], vec![]], false);
    let s = &report(std::slice::from_ref(&v), &days, &reg, cfg()).unwrap()[0];
    close(s.hit_rate.unwrap(), 2.0 / 3.0, 1e-15);
    assert_eq!(s.payoff, None);
    v.outcomes[0].net_bps_x100 = -400;
    let s = &report(&[v], &days, &reg, cfg()).unwrap()[0];
    assert_eq!(s.payoff, Some(1.5));
    // Trades without a stop have no R: counted in basis points, absent in R.
    let v = variant(1, "nor", &plain_days(), false);
    let s = &report(&[v], &day_names(10), &reg, cfg()).unwrap()[0];
    assert_eq!((s.bp.trades, s.r.trades), (21, 0));
    assert!(s.r.mean.is_none() && s.r.boot.is_none());
}

// ---- a refinement against its plain version ----

#[test]
fn a_refinement_is_a_paired_difference_from_its_plain_version_with_its_own_error() {
    let days = day_names(10);
    let reg = registry_of(2);
    let plain = variant(1, "plain", &plain_days(), true);
    let refined = variant_from(2, "refined", &refined_days(), true, Some(&refined_slots()));
    let p = paired(&refined, &plain, &days, &reg, cfg()).unwrap();
    // Six days on which both traded; 95 over 9 trades against 67 over 17: 10.5556 - 3.9412 = 6.6144 (by hand).
    assert_eq!(p.common_days, 6);
    let b = p.bp.unwrap();
    close(b.estimate, 95.0 / 9.0 - 67.0 / 17.0, 1e-12);
    close(b.estimate, 6.61437908496732, 1e-12);
    close(b.se, 1.580096089891783, 1e-9);
    close(b.t.unwrap(), 4.18606129543699, 1e-9);
    close(b.lo, 4.25, 1e-9);
    close(b.hi, 10.7, 1e-9);
    assert_eq!((b.replicates, b.block, b.days), (500, 2, 6));
    // The same in R is the same trades in other units, so the same ratios.
    close(p.r.unwrap().estimate, b.estimate / 100.0, 1e-12);
    close(p.r.unwrap().t.unwrap(), b.t.unwrap(), 1e-9);
    // Every trade of the refinement is a signal the plain version traded.
    assert_eq!((p.shared_signals, p.other_signals), (9, 0));
}

#[test]
fn trades_the_plain_version_did_not_take_are_counted_apart() {
    let days = day_names(10);
    let reg = registry_of(2);
    let plain = variant(1, "plain", &plain_days(), false);
    // The same results in other symbols: not the plain version's signals.
    let elsewhere: Vec<Vec<usize>> = refined_slots()
        .into_iter()
        .map(|d| d.into_iter().map(|s| s + 10).collect())
        .collect();
    let other = variant_from(2, "other", &refined_days(), false, Some(&elsewhere));
    let p = paired(&other, &plain, &days, &reg, cfg()).unwrap();
    assert_eq!((p.shared_signals, p.other_signals), (0, 9));
    // One of nine moved: eight shared.
    let mut slots = refined_slots();
    slots[0][0] = 11;
    let one = variant_from(2, "one", &refined_days(), false, Some(&slots));
    let p = paired(&one, &plain, &days, &reg, cfg()).unwrap();
    assert_eq!((p.shared_signals, p.other_signals), (8, 1));
    // R: none of these trades has one, so there is no difference in R.
    assert!(p.r.is_none());
}

#[test]
fn a_difference_needs_days_on_which_both_traded() {
    let days = day_names(10);
    let reg = registry_of(2);
    let plain = variant(1, "plain", &[vec![1.0], vec![], vec![2.0], vec![]], false);
    let mut late = variant(2, "late", &[vec![], vec![3.0], vec![], vec![4.0]], false);
    late.outcomes.iter_mut().for_each(|o| o.symbol.push('x'));
    let p = paired(&late, &plain, &day_names(4), &reg, cfg()).unwrap();
    assert_eq!(
        (p.common_days, p.bp, p.shared_signals, p.other_signals),
        (0, None, 0, 2)
    );
    // One common day cannot be resampled.
    let one = variant(2, "one", &[vec![3.0], vec![], vec![], vec![]], false);
    let p = paired(&one, &plain, &day_names(4), &reg, cfg()).unwrap();
    assert_eq!((p.common_days, p.bp), (1, None));
    let _ = days;
    // Series of different lengths cannot be paired.
    let a = DaySeries::from_pairs([(1.0, 1), (2.0, 1), (3.0, 1)]);
    let b = DaySeries::from_pairs([(1.0, 1), (2.0, 1)]);
    assert!(paired_bootstrap(&a, &b, cfg()).is_none());
}

// ---- the null ----

#[test]
fn a_result_against_the_null_by_hand() {
    let null: Vec<f64> = (0..10).map(f64::from).collect();
    // Eight of the ten null runs are at or above 2: (8 + 1) / (10 + 1).
    let v = against_null(2.0, &null).unwrap();
    assert_eq!((v.runs, v.at_or_above), (10, 8));
    close(v.p_value, 9.0 / 11.0, 1e-15);
    close(v.median, 4.5, 1e-15);
    close(v.p95, 8.55, 1e-15);
    close(v.p99, 8.91, 1e-15);
    // Above every null run: the smallest p-value this many runs allow. At a run's value counts as not beating it.
    let best = against_null(100.0, &null).unwrap();
    assert_eq!(best.at_or_above, 0);
    close(best.p_value, 1.0 / 11.0, 1e-15);
    assert_eq!(against_null(9.0, &null).unwrap().at_or_above, 1);
    // Order does not matter, and no runs is no answer.
    let mut shuffled = null.clone();
    shuffled.reverse();
    assert_eq!(against_null(2.0, &shuffled), against_null(2.0, &null));
    assert_eq!(against_null(2.0, &[]), None);
}

// ---- found by mutation: each of these fails if the line it names is changed ----

#[test]
fn two_days_are_enough_for_an_error_and_a_bootstrap_of_two_resamples_is_one() {
    // Two days of one trade each, 2 and 4: the mean is 3, the squares of the residuals sum to 2, and 2 / (2 - 1) times 2
    // is 4, whose root over 2 trades is 1 (by hand).
    let two = DaySeries::from_pairs([(2.0, 1), (4.0, 1)]);
    assert_eq!(two.cluster_se(), Some(1.0));
    let b = two
        .bootstrap(Bootstrap {
            replicates: 2,
            block: Some(1),
            seed: 1,
        })
        .unwrap();
    assert_eq!((b.replicates, b.days, b.estimate), (2, 2, 3.0));
    // One resample, or one day, is not enough.
    assert!(
        two.bootstrap(Bootstrap {
            replicates: 1,
            block: Some(1),
            seed: 1
        })
        .is_none()
    );
    assert!(DaySeries::from_pairs([(2.0, 1)]).bootstrap(cfg()).is_none());
}

#[test]
fn only_the_days_on_which_both_traded_are_in_a_difference() {
    // Day 1 has only the first, day 3 only the second. Both traded on days 0 and 2: 20 against 2 is 18. (Counting day 1
    // would make the first 30 and the difference 28; counting day 3 would make the second 3 and the difference 17.)
    let a = DaySeries::from_pairs([(10.0, 1), (50.0, 1), (30.0, 1), (0.0, 0)]);
    let b = DaySeries::from_pairs([(1.0, 1), (0.0, 0), (3.0, 1), (5.0, 1)]);
    let p = paired_bootstrap(&a, &b, cfg()).unwrap();
    assert_eq!((p.estimate, p.days), (18.0, 2));
    // The same with the roles swapped is the same days and the opposite sign.
    let q = paired_bootstrap(&b, &a, cfg()).unwrap();
    assert_eq!((q.estimate, q.days), (-18.0, 2));
}

#[test]
fn an_error_that_is_only_rounding_is_nothing_at_any_scale_and_a_real_one_is_kept() {
    // The error of these is 2.5e-10 on a mean near 1000: below a part in 1e12 of it, so nothing. (At a threshold
    // scaled the wrong way it would be kept and a t-statistic of 4e12 made of it.)
    let s = DaySeries::from_pairs([(1000.000000001, 1), (1000.0, 1), (1000.0, 1), (1000.0, 1)]);
    assert_eq!(s.cluster_se(), Some(0.0));
    assert_eq!(s.bootstrap(cfg()).unwrap().t, None);
    // And a spread of 0.5 on 1000 is real.
    let s = DaySeries::from_pairs([(1000.0, 1), (1000.5, 1), (999.5, 1), (1000.0, 1)]);
    assert!(s.cluster_se().unwrap() > 0.1);
}

#[test]
fn the_probability_of_a_positive_ratio_over_two_days_and_where_it_has_no_variance() {
    // Two days is the least there is a standard error for: cdf(0.5 / sqrt(1 + 0.5 * 0.25)) (independent check).
    close(
        probabilistic_sharpe(0.5, 0.0, 2, 0.0, 3.0).unwrap(),
        0.6813240558830315,
        1e-12,
    );
    // Where the variance of the estimate comes out as exactly nothing there is no probability: 1 - 1 x 1 + 0 = 0.
    assert_eq!(probabilistic_sharpe(1.0, 0.0, 30, 1.0, 1.0), None);
    // The sample variance divides by n - 1: 1, 2, 6 around 3 is 14 / 2 (not 14 x 2).
    assert_eq!(variance(&[1.0, 2.0, 6.0]), Some(7.0));
}

#[test]
fn a_run_cannot_name_a_day_twice_even_at_the_end() {
    let reg = registry_of(1);
    let v = variant(1, "plain", &plain_days(), false);
    let mut days = day_names(10);
    days.push(days[9].clone());
    assert!(matches!(
        report(std::slice::from_ref(&v), &days, &reg, cfg()),
        Err(StatsError::Days(_))
    ));
}

#[test]
fn a_variant_that_only_lost_has_a_hit_rate_of_nothing_and_no_payoff() {
    let reg = registry_of(1);
    let v = variant(1, "l", &[vec![-4.0], vec![-6.0], vec![]], false);
    let s = &report(&[v], &day_names(3), &reg, cfg()).unwrap()[0];
    assert_eq!((s.hit_rate, s.payoff), (Some(0.0), None));
    assert_eq!(s.max_drawdown_bp, 10.0);
}

#[test]
fn a_registry_whose_trailer_is_gone_is_cut_short_not_misread_from_a_name_that_says_end() {
    let mut reg = Registry::new();
    reg.register(1, "the end of it", "2026-10-07").unwrap();
    let text = reg.render();
    let cut = &text[..text.rfind("end ").unwrap()];
    assert!(cut.contains("the end of it"));
    let e = Registry::parse(cut).unwrap_err().to_string();
    assert!(e.contains("cut short"), "{e}");
}
