use std::cell::Cell;
use std::path::PathBuf;

use super::*;

fn data() -> DataRange {
    DataRange {
        source: "tape:2026-01-05".into(),
        from: 1_767_623_400_000_000_000,
        to: 1_767_646_800_000_000_000,
    }
}

fn manifest() -> Manifest {
    Manifest::new("0123abcd", "backtest", 42, data())
        .unwrap()
        .with_config("batch", "4096")
        .unwrap()
        .with_config("universe", "5000 symbols, wildcard")
        .unwrap()
        .with_param("pullback_depth_permille", "400")
        .unwrap()
        .with_param("stop_cents", "25")
        .unwrap()
}

#[test]
fn the_canonical_encoding_is_pinned() {
    // Both values were also computed independently in Python (hashlib) from the
    // documented layout. Changing the encoding changes every manifest hash and
    // orphans every stored result. If this fails, that is what you are about to
    // do: bump SCHEMA and say why.
    assert_eq!(
        manifest().hash().hex(),
        "f1ea1561d5d350e7408a06c10c5074483ec9127d65370074b53e2c26a1b82b26"
    );
    assert_eq!(
        manifest().params_hash().hex(),
        "fc0902afeac3742d364b27dc9693c53f99eb7b1df71dfd8d9496fc78e2d8e2c0"
    );
}

#[test]
fn the_hash_ignores_insertion_order_and_covers_every_field() {
    let reordered = Manifest::new("0123abcd", "backtest", 42, data())
        .unwrap()
        .with_param("stop_cents", "25")
        .unwrap()
        .with_config("universe", "5000 symbols, wildcard")
        .unwrap()
        .with_param("pullback_depth_permille", "400")
        .unwrap()
        .with_config("batch", "4096")
        .unwrap();
    assert_eq!(manifest().hash(), reordered.hash());

    let base = manifest().hash();
    let variants = [
        Manifest::new("0123abce", "backtest", 42, data()).unwrap(),
        Manifest::new("0123abcd", "replay", 42, data()).unwrap(),
        Manifest::new("0123abcd", "backtest", 43, data()).unwrap(),
        Manifest::new(
            "0123abcd",
            "backtest",
            42,
            DataRange {
                source: "tape:other".into(),
                ..data()
            },
        )
        .unwrap(),
        Manifest::new(
            "0123abcd",
            "backtest",
            42,
            DataRange {
                from: data().from + 1,
                ..data()
            },
        )
        .unwrap(),
        Manifest::new(
            "0123abcd",
            "backtest",
            42,
            DataRange {
                to: data().to + 1,
                ..data()
            },
        )
        .unwrap(),
    ];
    for v in variants {
        let v = v
            .with_config("batch", "4096")
            .unwrap()
            .with_config("universe", "5000 symbols, wildcard")
            .unwrap()
            .with_param("pullback_depth_permille", "400")
            .unwrap()
            .with_param("stop_cents", "25")
            .unwrap();
        assert_ne!(v.hash(), base);
    }
    let changed_config = manifest_with("batch", "8192", "stop_cents", "25");
    let changed_param = manifest_with("batch", "4096", "stop_cents", "26");
    assert_ne!(changed_config.hash(), base);
    assert_ne!(changed_param.hash(), base);
    // A key moved between config and params is a different manifest.
    let moved = Manifest::new("0123abcd", "backtest", 42, data())
        .unwrap()
        .with_param("batch", "4096")
        .unwrap();
    let kept = Manifest::new("0123abcd", "backtest", 42, data())
        .unwrap()
        .with_config("batch", "4096")
        .unwrap();
    assert_ne!(moved.hash(), kept.hash());
}

fn manifest_with(ck: &str, cv: &str, pk: &str, pv: &str) -> Manifest {
    Manifest::new("0123abcd", "backtest", 42, data())
        .unwrap()
        .with_config(ck, cv)
        .unwrap()
        .with_config("universe", "5000 symbols, wildcard")
        .unwrap()
        .with_param("pullback_depth_permille", "400")
        .unwrap()
        .with_param(pk, pv)
        .unwrap()
}

#[test]
fn params_hash_depends_only_on_the_params() {
    let a = manifest();
    let b = Manifest::new("ffff", "replay", 7, data())
        .unwrap()
        .with_config("other", "x")
        .unwrap()
        .with_param("stop_cents", "25")
        .unwrap()
        .with_param("pullback_depth_permille", "400")
        .unwrap();
    assert_eq!(a.params_hash(), b.params_hash());
    assert_ne!(a.hash(), b.hash());
    let c = manifest_with("batch", "4096", "stop_cents", "99");
    assert_ne!(a.params_hash(), c.params_hash());
}

#[test]
fn the_text_form_round_trips_and_is_canonical() {
    let m = manifest();
    let text = m.to_text();
    assert!(
        text.starts_with("tfmf 1\ngit_sha 0123abcd\nkind backtest\nseed 42\n"),
        "{text}"
    );
    assert!(
        text.contains("config universe 5000 symbols, wildcard\n"),
        "values may hold spaces"
    );
    let back = Manifest::parse(&text).unwrap();
    assert_eq!(back, m);
    assert_eq!(back.hash(), m.hash());
    assert_eq!(back.to_text(), text);
}

#[test]
fn invalid_fields_and_malformed_text_are_rejected() {
    let bad = |r: Result<Manifest, Error>| matches!(r, Err(Error::Invalid(_)));
    assert!(bad(Manifest::new("", "k", 1, data())));
    assert!(bad(Manifest::new("has space", "k", 1, data())));
    assert!(bad(Manifest::new(
        "sha",
        "k",
        1,
        DataRange {
            source: String::new(),
            ..data()
        }
    )));
    assert!(bad(Manifest::new(
        "sha",
        "k",
        1,
        DataRange {
            from: 10,
            to: 9,
            ..data()
        }
    )));
    let m = Manifest::new("sha", "k", 1, data()).unwrap();
    assert!(bad(m.clone().with_config("bad key", "v")));
    assert!(bad(m.clone().with_config("k", "two\nlines")));
    assert!(bad(m.clone().with_config("k", " padded")));
    assert!(bad(m.clone().with_config("k", "")));
    assert!(
        bad(m
            .clone()
            .with_config("k", "a")
            .unwrap()
            .with_config("k", "b")),
        "a key given twice"
    );

    let good = manifest().to_text();
    let parse_err = |t: &str| match Manifest::parse(t) {
        Err(Error::Parse { line, .. }) => line,
        other => panic!("expected a parse error, got {other:?}"),
    };
    assert_eq!(parse_err(""), 0);
    assert_eq!(parse_err("tfmf 2\n"), 1);
    assert_eq!(parse_err(&good.replace("seed 42", "seed forty")), 4);
    assert_eq!(
        parse_err(&good.replace("kind backtest\n", "kind backtest\nkind replay\n")),
        4
    );
    assert_eq!(
        parse_err(&format!("{good}surprise x\n")),
        good.lines().count() + 1
    );
    assert_eq!(
        parse_err(&good.replace("param stop_cents 25", "param stop_cents 26")),
        8,
        "params_hash must match"
    );
    assert_eq!(parse_err(&good.replace("data_from", "data_frm")), 6);
    assert_eq!(
        parse_err(&good.replace("git_sha 0123abcd\n", "")),
        0,
        "a missing field"
    );
}

fn result() -> RunResult {
    RunResult::new(manifest(), 2_190_384, 0xa8f6_c9a1_9621_be32)
        .with_metric("trades", 1_400_000)
        .unwrap()
        .with_metric("pnl_cents", -12_345)
        .unwrap()
}

#[test]
fn a_result_round_trips_in_a_stable_text_form() {
    let r = result();
    let text = r.to_text();
    let head: Vec<&str> = text.lines().take(6).collect();
    assert_eq!(head[0], "tfrs 1");
    assert_eq!(head[1], format!("manifest_hash {}", manifest().hash()));
    assert_eq!(
        &head[2..],
        [
            "events 2190384",
            "event_hash a8f6c9a19621be32",
            "metric pnl_cents -12345",
            "metric trades 1400000"
        ],
        "metrics sorted by name"
    );
    assert_eq!(text.lines().nth(6), Some("manifest"));
    let back = RunResult::parse(&text).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.to_text(), text);
    assert_eq!(back.metric("pnl_cents"), Some(-12_345));
    assert_eq!(back.key(), manifest().hash());
}

#[test]
fn a_result_whose_manifest_was_edited_does_not_parse() {
    let text = result().to_text();
    let tampered = text.replace("seed 42", "seed 43");
    assert!(
        matches!(RunResult::parse(&tampered), Err(Error::Parse { .. })),
        "the stated hash no longer matches"
    );
    assert!(matches!(
        RunResult::parse(&text.replace("manifest_hash ", "manifest_hash 0")),
        Err(Error::Parse { .. })
    ));
    assert!(
        matches!(RunResult::parse("tfrs 1\n"), Err(Error::Parse { .. })),
        "no manifest section"
    );
    assert!(matches!(
        RunResult::parse(&text.replace("events 2190384\n", "")),
        Err(Error::Parse { .. })
    ));
    assert!(matches!(
        RunResult::parse(&text.replace("metric trades 1400000", "metric trades lots")),
        Err(Error::Parse { .. })
    ));
    assert!(
        RunResult::new(manifest(), 1, 1)
            .with_metric("bad name", 1)
            .is_err()
    );
    assert!(
        result().with_metric("trades", 1).is_err(),
        "a metric given twice"
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("tf-manifest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_rerun_is_deduped_by_manifest_hash() {
    let dir = TempDir::new("dedupe");
    let store = DirStore::new(&dir.0);
    let m = manifest();
    assert_eq!(store.get(&m).unwrap(), None);
    assert_eq!(store.put(&result()).unwrap(), Put::Written);
    assert_eq!(store.get(&m).unwrap(), Some(result()));
    assert_eq!(
        store.put(&result()).unwrap(),
        Put::Deduped,
        "the same result again changes nothing"
    );

    let hex = m.hash().hex();
    assert_eq!(
        store.path_for(&m),
        dir.0.join(&hex[..2]).join(format!("{hex}.tfrs"))
    );
    assert_eq!(
        std::fs::read_to_string(store.path_for(&m)).unwrap(),
        result().to_text()
    );
    // No temporary files are left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.0.join(&hex[..2]))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1);

    // A different manifest has a different slot.
    assert_eq!(
        store
            .get(&manifest_with("batch", "8192", "stop_cents", "25"))
            .unwrap(),
        None
    );
}

#[test]
fn a_rerun_that_disagrees_is_reported_not_overwritten() {
    let dir = TempDir::new("mismatch");
    let store = DirStore::new(&dir.0);
    store.put(&result()).unwrap();

    let different_hash = RunResult::new(manifest(), 2_190_384, 0xdead_beef)
        .with_metric("trades", 1_400_000)
        .unwrap()
        .with_metric("pnl_cents", -12_345)
        .unwrap();
    match store.put(&different_hash) {
        Err(Error::Mismatch { hash, what }) => {
            assert_eq!(hash, manifest().hash());
            assert!(what.contains("event hash"), "{what}");
        }
        other => panic!("expected a mismatch, got {other:?}"),
    }
    let different_metric = RunResult::new(manifest(), 2_190_384, 0xa8f6_c9a1_9621_be32)
        .with_metric("trades", 1_400_000)
        .unwrap()
        .with_metric("pnl_cents", 1)
        .unwrap();
    assert!(
        matches!(store.put(&different_metric), Err(Error::Mismatch { what, .. }) if what.contains("pnl_cents"))
    );
    assert_eq!(
        store.get(&manifest()).unwrap(),
        Some(result()),
        "the stored result is untouched"
    );
}

#[test]
fn get_or_run_runs_once_then_serves_from_the_store() {
    let dir = TempDir::new("get-or-run");
    let store = DirStore::new(&dir.0);
    let calls = Cell::new(0);
    let run = || {
        calls.set(calls.get() + 1);
        Ok(result())
    };
    let (first, src) = store.get_or_run(&manifest(), run).unwrap();
    assert_eq!((src, calls.get()), (Source::Fresh, 1));
    let (second, src) = store.get_or_run(&manifest(), run).unwrap();
    assert_eq!(
        (src, calls.get()),
        (Source::Cached, 1),
        "the closure is not called again"
    );
    assert_eq!(first, second);

    // A failing run stores nothing, and a result for the wrong manifest is refused.
    let other = manifest_with("batch", "1", "stop_cents", "25");
    assert!(
        matches!(store.get_or_run(&other, || Err("boom".into())), Err(Error::Failed(m)) if m == "boom")
    );
    assert_eq!(store.get(&other).unwrap(), None);
    assert!(matches!(
        store.get_or_run(&other, || Ok(result())),
        Err(Error::Failed(_))
    ));
    assert_eq!(store.get(&other).unwrap(), None);
}

#[test]
fn a_file_stored_under_the_wrong_hash_is_a_collision_not_a_hit() {
    let dir = TempDir::new("collision");
    let store = DirStore::new(&dir.0);
    let other = manifest_with("batch", "8192", "stop_cents", "25");
    store.put(&result()).unwrap();
    // Plant manifest A's result where manifest B's would live.
    let (from, to) = (store.path_for(&manifest()), store.path_for(&other));
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(from, to).unwrap();
    assert_eq!(store.get(&other), Err(Error::Collision(other.hash())));
}

#[test]
fn a_damaged_file_is_an_error() {
    let dir = TempDir::new("damaged");
    let store = DirStore::new(&dir.0);
    store.put(&result()).unwrap();
    let path = store.path_for(&manifest());
    std::fs::write(&path, "garbage").unwrap();
    assert!(matches!(store.get(&manifest()), Err(Error::Parse { .. })));
}

#[test]
fn digest_hex_round_trips_and_rejects_junk() {
    let d = manifest().hash();
    assert_eq!(Digest::from_hex(&d.hex()), Some(d));
    assert_eq!(Digest::from_hex("zz"), None);
    assert_eq!(
        Digest::from_hex(&d.hex().to_uppercase()),
        None,
        "lowercase only"
    );
    assert_eq!(Digest::from_hex(&d.hex()[..63]), None);
}
