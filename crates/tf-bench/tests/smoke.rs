//! The harness runs end to end and measures the right thing. No timing is
//! asserted (CI machines are noisy); only structure and agreement.

use tf_bench::{Env, Workload, from_jsonl, markdown, run_all, to_jsonl};

fn env() -> Env {
    Env::detect("testcommit".into())
}

#[test]
fn all_scenarios_run_on_the_same_events_and_report_sane_percentiles() {
    let w = Workload {
        symbols: 30,
        secs: 60,
        seed: 4,
    };
    let rows = run_all(&w, 1, &env()).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.scenario.as_str()).collect();
    assert_eq!(
        names,
        [
            "run-loop/null",
            "run-loop/tier0",
            "tape/tier0",
            "synth/tier0"
        ]
    );

    for r in &rows {
        assert!(
            r.events > 500,
            "{} saw only {} events",
            r.scenario,
            r.events
        );
        assert_eq!(r.events, rows[0].events, "{}", r.scenario);
        assert!(r.events_per_s > 0);
        assert!(
            r.p50_ns <= r.p99_ns && r.p99_ns <= r.p999_ns && r.p999_ns <= r.max_ns,
            "{r:?}"
        );
        assert_eq!((r.symbols, r.secs, r.seed), (30, 60, 4));
        assert_eq!(r.commit, "testcommit");
    }
    // Everything ran against the same timer floor.
    assert!(rows.iter().all(|r| r.timer_p50_ns == rows[0].timer_p50_ns));
}

#[test]
fn results_round_trip_and_render() {
    let w = Workload {
        symbols: 10,
        secs: 30,
        seed: 1,
    };
    let rows = run_all(&w, 2, &env()).unwrap();
    assert_eq!(from_jsonl(&to_jsonl(&rows)).unwrap(), rows);
    let table = markdown(&rows, Some(&rows));
    for r in &rows {
        assert!(table.contains(&r.scenario), "{table}");
    }
    assert!(
        table.contains("+0.0% events/s"),
        "comparing a run with itself shows no change:\n{table}"
    );
}

#[test]
fn an_empty_workload_is_an_error_not_a_zero() {
    let w = Workload {
        symbols: 0,
        secs: 10,
        seed: 1,
    };
    assert!(run_all(&w, 1, &env()).unwrap_err().contains("no events"));
}
