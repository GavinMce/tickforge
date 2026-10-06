use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use tf_budget::{Group, LossLimits, Strategy as S, Tree};
use tf_catalog::Kind;
use tf_core::{Nanos, Px};
use tf_ledger::{FileStore, Journal};
use tf_risk::{Budgets, GapRule, Limits};
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::Decision;

use crate::http::{Request, decode, handle, serve, valid_token};
use crate::{Source, dollars, js, overview, run_detail, runs};

const P: i64 = 1_000_000_000;
const SEC: Nanos = 1_000_000_000;
const TOKEN: &str = "correct-horse-battery";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tf-workspace-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn limits() -> Limits {
    Limits::new(
        5_000 * 1_000_000_000u128,
        1_000,
        20_000 * 1_000_000_000u128,
        400 * 1_000_000_000u128,
        6,
        10 * SEC,
    )
    .unwrap()
    .with_gap_rule(GapRule::new(100_000 * 1_000_000_000u128, 20_000, 1000).unwrap())
}

fn budgets() -> Budgets {
    let tree = Tree::new(vec![Group {
        id: "day".into(),
        share: 10_000,
        loss: LossLimits::default(),
        strategies: vec![
            S {
                id: "alpha".into(),
                share: 5_000,
            },
            S {
                id: "beta".into(),
                share: 3_000,
            },
            S {
                id: "gamma".into(),
                share: 2_000,
            },
        ],
    }])
    .unwrap();
    Budgets::new(
        tree,
        30_000 * P as u128,
        [
            (1, "alpha".to_owned()),
            (2, "beta".to_owned()),
            (3, "gamma".to_owned()),
        ],
    )
    .unwrap()
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
        reason: 7,
    }
}

fn fill(j: &mut Journal<FileStore>, i: &Intent, px: i64) {
    let Decision::Accepted(o) = j.decide(i, i.ts).unwrap() else {
        panic!("{i:?}")
    };
    j.ack(o, i.ts).unwrap();
    j.fill(o, 100, Px::from_raw(px), i.ts).unwrap();
}

/// alpha holds 100 shares bought at $5; beta lost $100 today and gamma did nothing.
fn account(dir: &Path, with_budgets: bool) -> Journal<FileStore> {
    let (mut j, _) = Journal::open(FileStore::open(dir).unwrap(), limits(), 1).unwrap();
    if with_budgets {
        j.set_budgets(Some(budgets()), SEC).unwrap();
    }
    fill(
        &mut j,
        &intent(1, 10, Side::Buy, Purpose::Open, 5 * P),
        5 * P,
    );
    fill(
        &mut j,
        &intent(2, 20, Side::Buy, Purpose::Open, 5 * P),
        5 * P,
    );
    fill(
        &mut j,
        &intent(2, 30, Side::Sell, Purpose::Close, 4 * P),
        4 * P,
    );
    j
}

fn src(dir: &Path) -> Source {
    Source {
        ledger: Some((dir.to_owned(), Kind::Paper)),
        store: None,
    }
}

#[test]
fn money_is_text_to_the_cent_and_never_negative_zero() {
    assert_eq!(dollars(0), "0.00");
    assert_eq!(dollars(5 * P as i128), "5.00");
    assert_eq!(dollars(1_234_567_890_000), "1234.57");
    assert_eq!(dollars(-100 * P as i128), "-100.00");
    assert_eq!(dollars(-4_000_000), "0.00");
    assert_eq!(dollars(-5_000_000), "-0.01");
    assert_eq!(dollars(4_999_999), "0.00");
    assert_eq!(dollars(5_000_000), "0.01");
}

#[test]
fn the_overview_shows_balance_budgets_use_and_day_pnl_while_the_engine_holds_the_ledger() {
    let dir = scratch("overview");
    let j = account(&dir, true); // the writer stays open: the lock is held
    assert!(dir.join("ledger.lock").exists());
    let text = overview(&src(&dir)).unwrap();
    let has = |s: &str| assert!(text.contains(s), "missing {s} in {text}");
    has("\"kind\":\"paper\"");
    has("\"balance\":\"30000.00\"");
    has("\"killed\":false");
    has("\"scheduled_change\":false");
    // The group: all of the balance, 500 in use (alpha's 100 shares at $5), -100 today.
    has(
        "\"id\":\"day\",\"share_bp\":10000,\"budget\":\"30000.00\",\"used\":\"500.00\",\"day_pnl\":\"-100.00\"",
    );
    has("\"loss_soft_bp\":300,\"loss_hard_bp\":600");
    has(
        "\"name\":\"alpha\",\"number\":1,\"group\":\"day\",\"share_bp\":5000,\"budget\":\"15000.00\",\"used\":\"500.00\",\"day_pnl\":\"0.00\"",
    );
    has("\"loss_soft\":\"450.00\",\"loss_hard\":\"900.00\"");
    has(
        "\"name\":\"beta\",\"number\":2,\"group\":\"day\",\"share_bp\":3000,\"budget\":\"9000.00\",\"used\":\"0.00\",\"day_pnl\":\"-100.00\"",
    );
    has(
        "\"name\":\"gamma\",\"number\":3,\"group\":\"day\",\"share_bp\":2000,\"budget\":\"6000.00\",\"used\":\"0.00\",\"day_pnl\":\"0.00\"",
    );
    has("\"state\":\"active\"");
    // The account's totals.
    has("\"used\":\"500.00\",\"day_pnl\":\"-100.00\"}");
    // Runs: today's session of alpha and beta, none of gamma; the latest is the strategy's one.
    has("\"name\":\"alpha\"");
    assert_eq!(
        text.matches("\"runs\":1,\"latest_run\":{").count(),
        2,
        "{text}"
    );
    has(
        "\"name\":\"gamma\",\"number\":3,\"group\":\"day\",\"share_bp\":2000,\"budget\":\"6000.00\",\"used\":\"0.00\",\"day_pnl\":\"0.00\",\"loss_soft\":\"180.00\",\"loss_hard\":\"360.00\",\"state\":\"active\",\"runs\":0,\"latest_run\":null",
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_strategy_past_a_loss_limit_says_so_and_a_kill_switch_says_so_for_all() {
    let dir = scratch("states");
    let mut j = account(&dir, true);
    // beta lost $100 of a $270 soft limit; a second round trip loses $200 more.
    fill(
        &mut j,
        &intent(2, 40, Side::Buy, Purpose::Open, 5 * P),
        5 * P,
    );
    fill(
        &mut j,
        &intent(2, 50, Side::Sell, Purpose::Close, 3 * P),
        3 * P,
    );
    assert_eq!(j.check_loss_limits(60 * SEC).unwrap().len(), 1);
    let text = overview(&src(&dir)).unwrap();
    assert!(text.contains("\"name\":\"beta\""), "{text}");
    assert!(text.contains("\"day_pnl\":\"-300.00\",\"loss_soft\":\"270.00\",\"loss_hard\":\"540.00\",\"state\":\"no new entries\""), "{text}");
    assert_eq!(text.matches("\"state\":\"active\"").count(), 2, "{text}");
    j.engage_kill_switch(70 * SEC).unwrap();
    let text = overview(&src(&dir)).unwrap();
    assert!(text.contains("\"killed\":true"));
    assert_eq!(text.matches("\"state\":\"killed\"").count(), 3, "{text}");
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hard_limit_says_flatten() {
    let dir = scratch("hard");
    let mut j = account(&dir, true);
    // Another $450 lost: $550 today against a $540 hard limit.
    fill(
        &mut j,
        &intent(2, 40, Side::Buy, Purpose::Open, 5 * P),
        5 * P,
    );
    fill(
        &mut j,
        &intent(2, 50, Side::Sell, Purpose::Close, P / 2),
        P / 2,
    );
    j.check_loss_limits(90 * SEC).unwrap();
    let text = overview(&src(&dir)).unwrap();
    assert!(text.contains("\"state\":\"flatten\""), "{text}");
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_budgets_or_without_a_ledger_the_overview_says_what_is_missing() {
    let dir = scratch("nobudgets");
    let j = account(&dir, false);
    let text = overview(&src(&dir)).unwrap();
    assert!(text.contains("\"budgets\":false"), "{text}");
    assert!(text.ends_with("\"groups\":[],\"strategies\":[]}"), "{text}");
    drop(j);
    let none = Source {
        ledger: None,
        store: None,
    };
    assert_eq!(
        overview(&none).unwrap(),
        "{\"account\":null,\"groups\":[],\"strategies\":[]}"
    );
    assert_eq!(runs(&none, None).unwrap(), "{\"runs\":[]}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn runs_can_be_filtered_by_strategy_and_say_whether_they_open() {
    let dir = scratch("runs");
    let j = account(&dir, true);
    let all = runs(&src(&dir), None).unwrap();
    assert_eq!(all.matches("\"strategy\":").count(), 2, "{all}");
    let one = runs(&src(&dir), Some("beta")).unwrap();
    assert_eq!(one.matches("\"strategy\":").count(), 1);
    assert!(
        one.contains("\"strategy\":\"beta\",\"kind\":\"paper\""),
        "{one}"
    );
    assert!(one.contains("\"net_pnl\":\"-100.00\",\"trades\":1,\"rules\":null,\"budget\":\"9000.00\",\"source\":\"ledger\""), "{one}");
    assert!(one.contains("\"explorable\":false"));
    assert!(one.contains("\"started_ns\":\"10000000000\""), "{one}");
    assert_eq!(runs(&src(&dir), Some("nobody")).unwrap(), "{\"runs\":[]}");
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

fn get(path: &str) -> Request {
    Request {
        method: "GET".into(),
        path: path.into(),
        query: String::new(),
        authorization: None,
        cookie: None,
        body: String::new(),
    }
}

fn signed(path: &str) -> Request {
    Request {
        authorization: Some(format!("Bearer {TOKEN}")),
        ..get(path)
    }
}

#[test]
fn nothing_but_health_and_the_login_page_answers_without_the_token() {
    let dir = scratch("auth");
    let j = account(&dir, true);
    let s = src(&dir);
    let r = |q: Request| handle(&s, TOKEN, &q);
    assert_eq!(r(get("/health")).status, 200);
    for path in ["/api/overview", "/api/runs"] {
        let denied = r(get(path));
        assert_eq!(denied.status, 401, "{path}");
        assert!(!denied.body.contains("alpha"));
        assert_eq!(denied.extra, [("WWW-Authenticate", "Bearer".to_owned())]);
        assert_eq!(r(signed(path)).status, 200, "{path}");
    }
    // Wrong tokens: a different one, a prefix, an extension, empty, and a wrong scheme.
    for bad in [
        "Bearer wrong-horse-battery",
        "Bearer correct-horse-batter",
        "Bearer correct-horse-batteryy",
        "Bearer ",
        &format!("Bearer {TOKEN}\0"),
        &format!("Basic {TOKEN}"),
        TOKEN,
    ] {
        let q = Request {
            authorization: Some(bad.to_owned()),
            ..get("/api/overview")
        };
        assert_eq!(r(q).status, 401, "{bad}");
    }
    // The cookie works, a cookie with the wrong value or name does not.
    let cookie = |c: &str| Request {
        cookie: Some(c.to_owned()),
        ..get("/api/overview")
    };
    assert_eq!(r(cookie(&format!("tf_session={TOKEN}"))).status, 200);
    assert_eq!(r(cookie(&format!("a=b; tf_session={TOKEN}"))).status, 200);
    assert_eq!(r(cookie("tf_session=nope")).status, 401);
    assert_eq!(r(cookie(&format!("other={TOKEN}"))).status, 401);
    // The home page is the login form until signed in.
    assert!(r(get("/")).body.contains("type=password"));
    assert!(r(signed("/")).body.contains("/app.js"));
    assert_eq!(r(get("/app.js")).status, 401);
    let js = r(signed("/app.js"));
    assert_eq!((js.status, js.content_type), (200, "text/javascript"));
    assert!(js.body.contains("/api/overview"));
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn signing_in_sets_an_http_only_same_site_cookie_and_a_wrong_token_does_not() {
    let s = Source {
        ledger: None,
        store: None,
    };
    let post = |body: &str| Request {
        method: "POST".into(),
        body: body.into(),
        ..get("/login")
    };
    let ok = handle(&s, TOKEN, &post(&format!("token={TOKEN}")));
    assert_eq!(ok.status, 303);
    assert!(ok.extra.contains(&("Location", "/".to_owned())));
    let cookie = &ok.extra.iter().find(|(k, _)| *k == "Set-Cookie").unwrap().1;
    assert_eq!(
        cookie,
        &format!("tf_session={TOKEN}; Path=/; HttpOnly; SameSite=Strict")
    );
    for bad in [
        "token=wrong-horse-battery",
        "token=",
        "",
        "nothing",
        "token=correct-horse-batter",
    ] {
        let r = handle(&s, TOKEN, &post(bad));
        assert_eq!(r.status, 401, "{bad}");
        assert!(r.extra.iter().all(|(k, _)| *k != "Set-Cookie"));
    }
}

#[test]
fn no_request_but_a_get_or_the_login_is_accepted_and_nothing_changes_on_disk() {
    let dir = scratch("readonly");
    let j = account(&dir, true);
    let before = std::fs::read(dir.join("ledger.log")).unwrap();
    let s = src(&dir);
    for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        for path in ["/", "/api/overview", "/api/runs", "/api/orders", "/login/x"] {
            let q = Request {
                method: method.into(),
                authorization: Some(format!("Bearer {TOKEN}")),
                body: "token=x".into(),
                ..get(path)
            };
            let r = handle(&s, TOKEN, &q);
            assert_eq!(r.status, 405, "{method} {path}");
            assert_eq!(r.extra, [("Allow", "GET".to_owned())]);
        }
    }
    assert_eq!(handle(&s, TOKEN, &signed("/api/orders")).status, 404);
    assert_eq!(
        handle(&s, TOKEN, &signed("/api/overview/extra")).status,
        404
    );
    for _ in 0..3 {
        handle(&s, TOKEN, &signed("/api/overview"));
        handle(&s, TOKEN, &signed("/api/runs"));
    }
    assert_eq!(std::fs::read(dir.join("ledger.log")).unwrap(), before);
    drop(j);
    // With no writer, reading takes no lock and leaves none.
    handle(&s, TOKEN, &signed("/api/overview"));
    assert!(!dir.join("ledger.lock").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_strategy_filter_is_decoded_and_a_broken_ledger_is_a_500_that_says_why() {
    let dir = scratch("errors");
    let j = account(&dir, true);
    let s = src(&dir);
    let q = Request {
        query: "x=1&strategy=be%74a".into(),
        ..signed("/api/runs")
    };
    let r = handle(&s, TOKEN, &q);
    assert_eq!(r.body.matches("\"strategy\":").count(), 1, "{}", r.body);
    drop(j);
    let text = std::fs::read_to_string(dir.join("ledger.log")).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines[1].push('x'); // a changed byte in the middle fails its checksum
    std::fs::write(dir.join("ledger.log"), lines.join("\n") + "\n").unwrap();
    let r = handle(&s, TOKEN, &signed("/api/overview"));
    assert_eq!(r.status, 500);
    assert!(r.body.starts_with("{\"error\":\""), "{}", r.body);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tokens_must_be_long_and_plain() {
    assert!(valid_token("0123456789abcdef"));
    assert!(valid_token("Abc-def_ghi-jkl_mno"));
    assert!(!valid_token("short"));
    assert!(!valid_token("0123456789abcde"));
    assert!(!valid_token("0123456789abcdef!"));
    assert!(!valid_token("0123456789 abcdef"));
    assert!(!valid_token(""));
}

fn exchange(addr: std::net::SocketAddr, raw: &[u8]) -> String {
    let mut c = TcpStream::connect(addr).unwrap();
    c.write_all(raw).unwrap();
    let mut out = String::new();
    // The server may close with some of a large request unread, which can reset the connection
    // after it has answered; what was read is the answer.
    let _ = c.read_to_string(&mut out);
    out
}

#[test]
fn over_a_socket_the_server_answers_reads_refuses_writes_and_survives_garbage() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let s = Source {
        ledger: None,
        store: None,
    };
    let t = std::thread::spawn(move || serve(&l, &s, TOKEN, Some(8)));
    let ok = exchange(addr, b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(ok.starts_with("HTTP/1.1 200 OK\r\n"), "{ok}");
    assert!(
        ok.contains("Content-Length: 11\r\n") && ok.ends_with("{\"ok\":true}"),
        "{ok}"
    );
    assert!(ok.contains("Cache-Control: no-store") && ok.contains("nosniff"));
    assert!(
        ok.contains("default-src 'none'; script-src 'self'; connect-src 'self';")
            && !ok.contains("script-src 'unsafe-inline'")
            && ok.contains("frame-ancestors 'none'"),
        "{ok}"
    );
    let denied = exchange(addr, b"GET /api/overview HTTP/1.1\r\n\r\n");
    assert!(denied.starts_with("HTTP/1.1 401 Unauthorized"), "{denied}");
    let signed_in = exchange(
        addr,
        format!("GET /api/overview HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").as_bytes(),
    );
    assert!(signed_in.starts_with("HTTP/1.1 200 OK") && signed_in.contains("\"account\":null"));
    let write = exchange(addr, b"DELETE /api/runs HTTP/1.1\r\n\r\n");
    assert!(
        write.starts_with("HTTP/1.1 405 Method Not Allowed") && write.contains("Allow: GET"),
        "{write}"
    );
    let login = exchange(
        addr,
        format!(
            "POST /login HTTP/1.1\r\nContent-Length: {}\r\n\r\ntoken={TOKEN}",
            6 + TOKEN.len()
        )
        .as_bytes(),
    );
    assert!(
        login.starts_with("HTTP/1.1 303 See Other") && login.contains("HttpOnly"),
        "{login}"
    );
    let big = exchange(
        addr,
        b"POST /login HTTP/1.1\r\nContent-Length: 5000\r\n\r\ntoken=",
    );
    assert!(big.starts_with("HTTP/1.1 413"), "{big}");
    let junk = exchange(addr, b"this is not http\r\n\r\n");
    assert!(junk.starts_with("HTTP/1.1 400 Bad Request"), "{junk}");
    let huge = exchange(
        addr,
        format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(17_000)).as_bytes(),
    );
    assert!(huge.starts_with("HTTP/1.1 413"), "{huge}");
    t.join().unwrap();
}

#[test]
fn query_text_is_decoded_and_json_text_is_escaped() {
    assert_eq!(decode("be%74a"), "beta");
    assert_eq!(decode("a+b"), "a b");
    assert_eq!(decode("100%"), "100%");
    assert_eq!(decode("%zz"), "%zz");
    assert_eq!(decode("%4"), "%4");
    assert_eq!(decode("%e2%82%ac"), "\u{20ac}");
    assert_eq!(js("a\"b\\c\nd\te\u{1}f"), "\"a\\\"b\\\\c\\nd\\te\\u0001f\"");
}

#[test]
fn a_new_day_starts_day_pnl_from_nothing() {
    let dir = scratch("newday");
    let mut j = account(&dir, true);
    assert!(
        overview(&src(&dir))
            .unwrap()
            .contains("\"day_pnl\":\"-100.00\"")
    );
    j.new_day(100 * SEC).unwrap();
    let text = overview(&src(&dir)).unwrap();
    assert!(
        text.contains("\"used\":\"500.00\",\"day_pnl\":\"0.00\"}"),
        "{text}"
    );
    assert!(text.contains("\"name\":\"beta\",\"number\":2,\"group\":\"day\",\"share_bp\":3000,\"budget\":\"9000.00\",\"used\":\"0.00\",\"day_pnl\":\"0.00\""), "{text}");
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_scheduled_change_is_listed_in_words_until_the_rebalance_applies_it() {
    let dir = scratch("scheduled");
    let mut j = account(&dir, true);
    let want = Tree::new(vec![Group {
        id: "day".into(),
        share: 10_000,
        loss: LossLimits {
            soft: 250,
            hard: 600,
        },
        strategies: vec![
            S {
                id: "alpha".into(),
                share: 6_000,
            },
            S {
                id: "beta".into(),
                share: 2_050,
            },
            S {
                id: "gamma".into(),
                share: 1_950,
            },
        ],
    }])
    .unwrap();
    assert!(overview(&src(&dir)).unwrap().contains("\"scheduled\":[]"));
    j.schedule_budgets(Some(want), 80 * SEC).unwrap();
    let text = overview(&src(&dir)).unwrap();
    assert!(text.contains("\"scheduled_change\":true"), "{text}");
    for line in [
        "day loss limits: stop opening 3% → 2.5%, flatten 6% → 6%",
        "alpha: 50% → 60% of day",
        "beta: 30% → 20.5% of day",
        "gamma: 20% → 19.5% of day",
    ] {
        assert!(text.contains(&format!("\"{line}\"")), "{line} in {text}");
    }
    j.schedule_budgets(None, 81 * SEC).unwrap();
    assert!(overview(&src(&dir)).unwrap().contains("\"scheduled\":[]"));
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn percentages_are_written_the_short_way() {
    assert_eq!(crate::pct(0), "0%");
    assert_eq!(crate::pct(10_000), "100%");
    assert_eq!(crate::pct(2_050), "20.5%");
    assert_eq!(crate::pct(3_333), "33.33%");
    assert_eq!(crate::pct(5), "0.05%");
    assert_eq!(crate::pct(250), "2.5%");
}

#[test]
fn the_page_only_reads_fields_the_api_sends_and_never_writes_markup_from_data() {
    let dir = scratch("contract");
    let mut j = account(&dir, true);
    let mut big = intent(2, 70, Side::Buy, Purpose::Open, 5 * P);
    big.qty = 100_000;
    j.decide(&big, big.ts).unwrap();
    let api = overview(&src(&dir)).unwrap();
    let app = include_str!("ui/app.js");
    // Every field the script reads is one the overview sends.
    for key in [
        "kind",
        "records",
        "killed",
        "budgets",
        "balance",
        "scheduled",
        "used",
        "day_pnl",
        "share_bp",
        "budget",
        "loss_soft_bp",
        "loss_hard_bp",
        "loss_soft",
        "loss_hard",
        "state",
        "runs",
        "group",
        "name",
        "id",
    ] {
        assert!(
            app.contains(&format!(".{key}")),
            "the page does not use {key}"
        );
        assert!(
            api.contains(&format!("\"{key}\":")),
            "the API does not send {key}"
        );
    }
    // The same for the run view: the runs list and a run's detail.
    let runs_json = runs(&src(&dir), Some("beta")).unwrap();
    let detail = run_detail(&src(&dir), "beta", "session-1")
        .unwrap()
        .unwrap();
    for key in [
        "started",
        "net_pnl",
        "trades",
        "rules",
        "budget",
        "explorable",
        "id",
        "kind",
    ] {
        assert!(
            app.contains(&format!(".{key}")),
            "the page does not use {key}"
        );
        assert!(
            runs_json.contains(&format!("\"{key}\":")),
            "/api/runs does not send {key}"
        );
    }
    for key in [
        "trades",
        "curve",
        "refused",
        "note",
        "time",
        "side",
        "purpose",
        "qty",
        "instrument",
        "price",
        "pnl",
        "reason",
        "count",
    ] {
        assert!(
            app.contains(&format!(".{key}")),
            "the page does not use {key}"
        );
        assert!(
            detail.contains(&format!("\"{key}\":")),
            "/api/run does not send {key}"
        );
    }
    // Text from the ledger goes in as text, and the page cannot send anything but GETs.
    for banned in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
    ] {
        assert!(!app.contains(banned), "{banned}");
    }
    assert!(
        !app.contains("method:"),
        "the page makes no request but a GET"
    );
    assert!(
        !include_str!("ui/index.html").contains("<script>"),
        "no inline script: the CSP forbids it"
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_change_to_a_groups_share_is_listed_with_the_group_added() {
    let dir = scratch("groupshare");
    let mut j = account(&dir, true);
    let g = |id: &str, share, who: &[(&str, u32)]| Group {
        id: id.into(),
        share,
        loss: LossLimits::default(),
        strategies: who
            .iter()
            .map(|(n, s)| S {
                id: (*n).into(),
                share: *s,
            })
            .collect(),
    };
    let want = Tree::new(vec![
        g(
            "day",
            8_000,
            &[("alpha", 5_000), ("beta", 3_000), ("gamma", 2_000)],
        ),
        g("swing", 2_000, &[("delta", 10_000)]),
    ])
    .unwrap();
    j.schedule_budgets(Some(want), 80 * SEC).unwrap();
    let text = overview(&src(&dir)).unwrap();
    assert!(
        text.contains("\"day: 100% → 80% of the balance\""),
        "{text}"
    );
    assert!(text.contains("\"add group swing\""), "{text}");
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_run_comes_with_its_trades_a_running_profit_and_what_was_refused() {
    let dir = scratch("detail");
    let mut j = account(&dir, true);
    let mut big = intent(1, 70, Side::Buy, Purpose::Open, 5 * P);
    big.qty = 100_000;
    j.decide(&big, big.ts).unwrap();
    let s = src(&dir);
    let beta = run_detail(&s, "beta", "session-1").unwrap().unwrap();
    assert!(beta.contains("\"trades\":[{\"time\":\"00:00:20\",\"instrument\":0,\"side\":\"buy\",\"purpose\":\"open\",\"qty\":100,\"price\":\"5.0000\",\"pnl\":\"0.00\"},{\"time\":\"00:00:30\",\"instrument\":0,\"side\":\"sell\",\"purpose\":\"close\",\"qty\":100,\"price\":\"4.0000\",\"pnl\":\"-100.00\"}]"), "{beta}");
    assert!(beta.contains("\"curve\":[\"0.00\",\"-100.00\"]"), "{beta}");
    assert!(
        beta.contains("\"refused\":[]") && beta.contains("\"note\":null"),
        "{beta}"
    );
    assert!(beta.contains("\"id\":\"session-1\""), "{beta}");
    assert!(
        !beta.contains(dir.to_str().unwrap()),
        "the ledger's path is not given out"
    );
    // alpha only opened, and had an order refused.
    let alpha = run_detail(&s, "alpha", "session-1").unwrap().unwrap();
    assert!(alpha.contains("\"curve\":[\"0.00\"]"), "{alpha}");
    assert!(
        alpha.contains("\"refused\":[{\"reason\":\"max_notional\",\"count\":1}]"),
        "{alpha}"
    );
    assert_eq!(run_detail(&s, "alpha", "session-9").unwrap(), None);
    assert_eq!(run_detail(&s, "nobody", "session-1").unwrap(), None);
    assert_eq!(run_detail(&s, "beta", "../../etc").unwrap(), None);
    // Over HTTP: needs sign-in and both parameters.
    let q = |query: &str, signed_in: bool| {
        let r = Request {
            query: query.into(),
            ..if signed_in {
                signed("/api/run")
            } else {
                get("/api/run")
            }
        };
        handle(&s, TOKEN, &r)
    };
    assert_eq!(q("strategy=beta&id=session-1", false).status, 401);
    assert_eq!(q("strategy=beta&id=session-1", true).status, 200);
    assert_eq!(q("strategy=beta", true).status, 400);
    assert_eq!(q("id=session-1", true).status, 400);
    assert_eq!(q("strategy=beta&id=nope", true).status, 404);
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prices_keep_four_places_and_the_clock_is_the_time_of_day() {
    assert_eq!(crate::price(5 * P), "5.0000");
    assert_eq!(crate::price(1_234_560_000), "1.2346");
    assert_eq!(crate::price(49_999), "0.0000");
    assert_eq!(crate::price(50_000), "0.0001");
    assert_eq!(crate::price(-2 * P), "-2.0000");
    assert_eq!(crate::price(-10_000), "0.0000");
    assert_eq!(crate::clock(0), "00:00:00");
    assert_eq!(crate::clock((13 * 3600 + 5 * 60 + 9) * SEC), "13:05:09");
    assert_eq!(crate::clock((86_400 + 61) * SEC), "00:01:01");
}

#[test]
fn a_stored_backtest_has_no_trades_here_and_points_to_the_explorer() {
    use tf_manifest::{DataRange, DirStore, Manifest, RunResult};
    let store = scratch("storedrun");
    let m = Manifest::new(
        "abc1234",
        "backtest",
        1,
        DataRange {
            source: "synth:universe".into(),
            from: 5 * SEC,
            to: 9 * SEC,
        },
    )
    .unwrap()
    .with_config("strategy", "momentum")
    .unwrap();
    let r = RunResult::new(m, 1, 2).with_metric("trades", 4).unwrap();
    DirStore::new(&store).put(&r).unwrap();
    let s = Source {
        ledger: None,
        store: Some(store.clone()),
    };
    let id = r.key().hex();
    let d = run_detail(&s, "momentum", &id).unwrap().unwrap();
    assert!(
        d.contains("\"trades\":null,\"curve\":null") && d.contains("explorer"),
        "{d}"
    );
    assert!(
        d.contains(&format!("\"id\":\"{id}\"")) && d.contains("\"explorable\":true"),
        "{d}"
    );
    let _ = std::fs::remove_dir_all(&store);
}
