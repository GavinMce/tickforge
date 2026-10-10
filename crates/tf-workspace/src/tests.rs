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
        ..Source::default()
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
        ..Source::default()
    };
    assert_eq!(
        overview(&none).unwrap(),
        "{\"account\":null,\"groups\":[],\"strategies\":[]}"
    );
    assert_eq!(runs(&none, None).unwrap(), "{\"runs\":[]}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_ledger_directory_with_no_records_yet_is_no_account_and_is_one_as_soon_as_a_day_begins() {
    // The deployment makes the directory before any day has run: nothing in it, then an empty log.
    let dir = scratch("fresh");
    std::fs::create_dir_all(&dir).unwrap();
    let s = src(&dir);
    let none = "{\"account\":null,\"groups\":[],\"strategies\":[]}";
    for step in 0..2 {
        if step == 1 {
            std::fs::write(dir.join("ledger.log"), "").unwrap();
        }
        assert_eq!(overview(&s).unwrap(), none, "step {step}");
        assert_eq!(runs(&s, None).unwrap(), "{\"runs\":[]}", "step {step}");
        for path in ["/api/overview", "/api/runs"] {
            let r = handle(&s, TOKEN, &signed(path));
            assert_eq!(r.status, 200, "{path} step {step}: {}", r.body);
        }
    }
    // The same service, no restart, once the engine has started a ledger there.
    std::fs::remove_file(dir.join("ledger.log")).unwrap();
    let j = account(&dir, true);
    let text = overview(&s).unwrap();
    assert!(text.contains("\"budgets\":true"), "{text}");
    assert!(
        runs(&s, None).unwrap().contains("\"runs\":[{"),
        "a run appears"
    );
    drop(j);
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
        requested_with: None,
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
        ..Source::default()
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
        ..Source::default()
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
        b"POST /login HTTP/1.1\r\nContent-Length: 20000\r\n\r\ntoken=",
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
        "alpha: 50% → 60% of day ($15,000.00 → $18,000.00)",
        "beta: 30% → 20.5% of day ($9,000.00 → $6,150.00)",
        "gamma: 20% → 19.5% of day ($6,000.00 → $5,850.00)",
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
    // And for the editor: what the page reads from /api/budgets, and what it sends back is the text
    // form the server parses.
    let budgets_json = crate::budgets::view(&src(&dir), None).unwrap();
    let waiting = {
        crate::budgets::request(&src(&dir), &tree_text(5_000, 3_500, 1_500, 300, 600), "t")
            .unwrap();
        crate::budgets::view(&src(&dir), None).unwrap()
    };
    for key in [
        "valid",
        "error",
        "text",
        "changes",
        "groups",
        "pending",
        "range",
        "min",
        "max",
        "min_why",
        "max_why",
        "share_bp",
        "loss_soft_bp",
        "loss_hard_bp",
        "budget",
        "used",
        "id",
        "name",
        "by",
    ] {
        assert!(
            app.contains(&format!(".{key}")),
            "the editor does not use {key}"
        );
        assert!(
            budgets_json.contains(&format!("\"{key}\":"))
                || waiting.contains(&format!("\"{key}\":")),
            "/api/budgets does not send {key}"
        );
    }
    assert!(
        app.contains("\"budgets v1\\n\"") && app.contains("\"group \""),
        "the page writes the text form"
    );
    // And for the proposals panel.
    propose(&dir, "growth-agent", 5_500, 3_000, 1_500, 100 * DAY_NS);
    let props_json = crate::proposals::view(&src(&dir)).unwrap();
    for key in [
        "proposals",
        "unreadable",
        "id",
        "by",
        "at",
        "state",
        "reason",
        "evidence",
        "why",
        "changes",
        "decision",
        "call",
        "note",
    ] {
        assert!(
            app.contains(&format!(".{key}")),
            "the panel does not use {key}"
        );
        assert!(
            props_json.contains(&format!("\"{key}\":")) || key == "call" || key == "note",
            "/api/proposals does not send {key}"
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
    // The page writes nothing itself: its one way to send a request that is not a GET is the
    // helper that always carries the header, and it is only pointed at the budget routes.
    assert_eq!(
        app.matches("method:").count(),
        2,
        "two places make non-GET requests: the budget helper and the proposal answer"
    );
    assert_eq!(
        app.matches("\"X-Requested-With\": \"workspace\"").count(),
        2
    );
    assert!(app.contains("fetch(\"/api/proposals/\" + p.id + \"/\" + what, { method: \"POST\""));
    assert!(
        app.contains("what === \"decline\" ? \"declined in the app\" : \"approved in the app\""),
        "the answer is only ever approve or decline"
    );
    let posts: Vec<&str> = app
        .match_indices("post(\"")
        .map(|(i, _)| &app[i + 6..])
        .collect();
    assert_eq!(posts.len(), 3, "preview, schedule and withdraw");
    for p in posts {
        assert!(p.starts_with("/api/budgets/"), "{}", &p[..30]);
    }
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
        text.contains("\"day: 100% → 80% of the balance ($30,000.00 → $24,000.00)\""),
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
        ..Source::default()
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

static ASKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn fake_explorer() -> crate::Explorer {
    use crate::ExplorerError::{NotFound, Refused};
    crate::Explorer::new(|run| {
        ASKED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match run {
            "aaaaaaaa" => Ok("<!doctype html><title>EXPLORER PAGE</title>".to_owned()),
            "bbbbbbbb" => Err(Refused(
                "the code changed <b>behaviour</b> & the run no longer reproduces".into(),
            )),
            _ => Err(NotFound(format!("no stored run starts with {run}"))),
        }
    })
}

#[test]
fn the_explorer_opens_a_stored_run_only_after_sign_in_and_under_its_own_policy() {
    let s = Source {
        explorer: Some(fake_explorer()),
        ..Source::default()
    };
    assert_eq!(handle(&s, TOKEN, &get("/explorer/aaaaaaaa")).status, 401);
    assert!(
        handle(&s, TOKEN, &get("/explorer/aaaaaaaa"))
            .body
            .contains("type=password")
    );
    let ok = handle(&s, TOKEN, &signed("/explorer/aaaaaaaa"));
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("EXPLORER PAGE"));
    // The page may run its own script but can neither load nor send anything.
    assert!(
        ok.csp.contains("script-src 'unsafe-inline'") && ok.csp.contains("connect-src 'none'"),
        "{}",
        ok.csp
    );
    assert!(ok.csp.contains("frame-ancestors 'none'") && ok.csp.contains("form-action 'none'"));
    assert!(
        String::from_utf8(ok.to_bytes())
            .unwrap()
            .contains(&format!("Content-Security-Policy: {}", ok.csp))
    );
    // Every other page keeps the strict policy.
    let home = handle(&s, TOKEN, &signed("/"));
    assert!(
        !home.csp.contains("unsafe-inline'; style") && home.csp.contains("script-src 'self'"),
        "{}",
        home.csp
    );
    // A run that cannot be opened says why, as text and not as markup.
    let refused = handle(&s, TOKEN, &signed("/explorer/bbbbbbbb"));
    assert_eq!(refused.status, 422);
    assert!(
        refused
            .body
            .contains("&lt;b&gt;behaviour&lt;/b&gt; &amp; the run"),
        "{}",
        refused.body
    );
    assert!(!refused.body.contains("<b>behaviour"));
    assert!(refused.body.contains("href=/"));
    assert_eq!(handle(&s, TOKEN, &signed("/explorer/cccccccc")).status, 404);
    // Only hex run names of a sensible length reach the explorer at all.
    let asked = ASKED.load(std::sync::atomic::Ordering::SeqCst);
    for bad in [
        "/explorer/",
        "/explorer/aaaa",
        "/explorer/../x",
        "/explorer/gggggggg",
        &format!("/explorer/{}", "a".repeat(65)),
        "/explorer/aaaaaaaa/x",
    ] {
        let r = handle(&s, TOKEN, &signed(bad));
        assert_eq!(r.status, 404, "{bad}");
        assert!(!r.body.contains("EXPLORER PAGE"), "{bad}");
    }
    assert_eq!(
        ASKED.load(std::sync::atomic::Ordering::SeqCst),
        asked,
        "a name that is not 8 to 64 hex digits is never passed on"
    );
    // Reading only: nothing else is accepted on that path.
    let post = Request {
        method: "POST".into(),
        ..signed("/explorer/aaaaaaaa")
    };
    assert_eq!(handle(&s, TOKEN, &post).status, 405);
    // Without a run store there is nothing to open.
    let none = handle(&Source::default(), TOKEN, &signed("/explorer/aaaaaaaa"));
    assert_eq!(none.status, 404);
    assert!(none.body.contains("run store"));
}

#[test]
fn the_page_links_a_stored_run_to_the_explorer_and_back_to_the_run() {
    let app = include_str!("ui/app.js");
    assert!(app.contains("\"/explorer/\" + encodeURIComponent(run.id) + \"?back=\""));
    assert!(app.contains("Open in trade explorer"));
    // The viewer only accepts a way back that is one of this site's run addresses.
    let viewer = tf_backtest::export::fragment("{}");
    assert!(viewer.contains("id=\"back\"") && viewer.contains("Back to the run"));
    assert!(
        viewer.contains(r"/^\/#\/s\/[\w.~%-]+(\/[\w.~%-]+)?$/.test(to)"),
        "the back link is validated"
    );
}

#[test]
fn the_explorer_page_asks_nobody_else_for_anything() {
    let viewer = tf_backtest::export::fragment("{}");
    for outside in ["https://", "http://", "//fonts", "@import", "src=\"http"] {
        assert!(!viewer.contains(outside), "the viewer refers to {outside}");
    }
}

fn tree_text(alpha: u32, beta: u32, gamma: u32, soft: u32, hard: u32) -> String {
    format!(
        "budgets v1\ngroup day 10000 {soft} {hard}\nstrategy day alpha {alpha}\nstrategy day beta {beta}\nstrategy day gamma {gamma}\n"
    )
}

#[test]
fn the_editor_view_gives_each_share_its_range_and_the_reason() {
    let dir = scratch("editor");
    let j = account(&dir, true);
    let v = crate::budgets::view(&src(&dir), None).unwrap();
    let has = |s: &str| assert!(v.contains(s), "missing {s} in {v}");
    has("\"balance\":\"30000.00\",\"valid\":true,\"error\":null,\"unassigned_bp\":0");
    has("\"changes\":[]");
    has("\"pending\":[]");
    // The group may not go below what alpha's $500 needs (3.34% of $30,000 gives alpha 50% = $501), nor above the whole;
    // alpha itself needs 1.67% of its group.
    has(
        "\"id\":\"day\",\"share_bp\":10000,\"budget\":\"$30,000.00\",\"used\":\"$500.00\",\"loss_soft_bp\":300,\"loss_hard_bp\":600,\"range\":{\"min\":334,\"max\":10000,\"min_why\":\"alpha has $500.00 in use\",\"max_why\":\"what is not yet assigned of the balance\"}",
    );
    // alpha: floor from its use; ceiling is what is unassigned in the group (nothing).
    has(
        "\"id\":\"alpha\",\"share_bp\":5000,\"budget\":\"$15,000.00\",\"used\":\"$500.00\",\"range\":{\"min\":167,\"max\":5000,\"min_why\":\"alpha has $500.00 in use\",\"max_why\":\"what is not yet assigned in day\"}",
    );
    has(
        "\"id\":\"beta\",\"share_bp\":3000,\"budget\":\"$9,000.00\",\"used\":\"$0.00\",\"range\":{\"min\":0,\"max\":3000,\"min_why\":\"nothing is in use, so it can go to zero\"",
    );
    // Lowering gamma frees room, so the others' ceilings rise in the next view.
    let v =
        crate::budgets::view(&src(&dir), Some(&tree_text(5_000, 3_000, 1_000, 300, 600))).unwrap();
    assert!(
        v.contains("\"valid\":true") && v.contains("\"unassigned_bp\":0"),
        "{v}"
    );
    assert!(
        v.contains("\"id\":\"alpha\",\"share_bp\":5000") && v.contains("\"min\":167,\"max\":6000"),
        "{v}"
    );
    assert!(
        v.contains("\"changes\":[\"gamma: 20% → 10% of day ($6,000.00 → $3,000.00)\"]"),
        "{v}"
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_draft_that_is_not_allowed_says_why_and_keeps_the_ranges_of_what_is_in_force() {
    let dir = scratch("editor-bad");
    let j = account(&dir, true);
    let bad = |draft: &str| crate::budgets::view(&src(&dir), Some(draft)).unwrap();
    // alpha cut below its $500 in use.
    let v = bad(&tree_text(100, 3_000, 2_000, 300, 600));
    assert!(
        v.contains("\"valid\":false") && v.contains("\"error\":\"alpha: the edit leaves a budget"),
        "{v}"
    );
    assert!(
        v.contains("\"share_bp\":5000"),
        "the groups shown are the ones in force: {v}"
    );
    assert!(v.contains("\"changes\":[]"));
    // Over 100%, soft above hard, a new strategy, text that is not a tree.
    assert!(bad(&tree_text(6_000, 3_000, 2_000, 300, 600)).contains("add up to 11000"));
    assert!(bad(&tree_text(5_000, 3_000, 2_000, 600, 300)).contains("loss limits need"));
    assert!(
        bad("budgets v1\ngroup day 10000 300 600\nstrategy day alpha 5000\n")
            .contains("same groups and strategies")
    );
    assert!(bad("not a tree").contains("\"valid\":false"));
    // Loss limits can be edited.
    let v = bad(&tree_text(5_000, 3_000, 2_000, 250, 500));
    assert!(
        v.contains("\"valid\":true")
            && v.contains("day loss limits: stop opening 3% → 2.5%, flatten 6% → 5%"),
        "{v}"
    );
    // Nothing to edit without budgets or a ledger.
    let nob = scratch("editor-nob");
    let jn = account(&nob, false);
    assert!(matches!(
        crate::budgets::view(&src(&nob), None),
        Err(crate::budgets::Refusal::NothingToEdit(_))
    ));
    assert!(matches!(
        crate::budgets::view(&Source::default(), None),
        Err(crate::budgets::Refusal::NothingToEdit(_))
    ));
    drop((j, jn));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&nob);
}

fn post(path: &str, body: &str, header: Option<&str>) -> Request {
    Request {
        method: "POST".into(),
        body: body.into(),
        requested_with: header.map(str::to_owned),
        ..signed(path)
    }
}

#[test]
fn a_change_is_requested_through_the_inbox_and_the_ledger_is_not_touched() {
    let dir = scratch("editor-post");
    let j = account(&dir, true);
    let s = src(&dir);
    let log = std::fs::read(dir.join("ledger.log")).unwrap();
    let draft = tree_text(5_000, 3_500, 1_500, 300, 600);
    // Sign-in and the header are both needed.
    let anon = Request {
        method: "POST".into(),
        body: draft.clone(),
        requested_with: Some("workspace".into()),
        ..get("/api/budgets/schedule")
    };
    assert_eq!(handle(&s, TOKEN, &anon).status, 401);
    assert_eq!(
        handle(&s, TOKEN, &post("/api/budgets/schedule", &draft, None)).status,
        403
    );
    assert_eq!(
        handle(
            &s,
            TOKEN,
            &post("/api/budgets/schedule", &draft, Some("other"))
        )
        .status,
        403
    );
    assert!(
        !dir.join("inbox").exists(),
        "nothing was written by the refused requests"
    );
    // Preview writes nothing either.
    let p = handle(
        &s,
        TOKEN,
        &post("/api/budgets/preview", &draft, Some("workspace")),
    );
    assert_eq!(p.status, 200);
    assert!(
        p.body.contains("\"valid\":true")
            && p.body
                .contains("beta: 30% → 35% of day ($9,000.00 → $10,500.00)"),
        "{}",
        p.body
    );
    assert!(!dir.join("inbox").exists());
    // Scheduling puts one file in the inbox.
    let r = handle(
        &s,
        TOKEN,
        &post("/api/budgets/schedule", &draft, Some("workspace")),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        r.body.contains("\"requested\":\"0000000001.req\"")
            && r.body
                .contains("gamma: 20% → 15% of day ($6,000.00 → $4,500.00)"),
        "{}",
        r.body
    );
    let (waiting, _) = tf_ledger::inbox::pending(&dir).unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].by, "the workspace app (shared token)");
    assert_eq!(waiting[0].tree.as_ref().unwrap().render(), draft);
    assert_eq!(
        std::fs::read(dir.join("ledger.log")).unwrap(),
        log,
        "the ledger was not written"
    );
    // The request shows in the editor and in what the overview says is waiting.
    let v = crate::budgets::view(&s, None).unwrap();
    assert!(v.contains("\"pending\":[{\"name\":\"0000000001.req\",\"by\":\"the workspace app (shared token)\",\"changes\":[\"beta:"), "{v}");
    // Refused: not allowed (422), no different (422), and the refusals leave the inbox alone.
    let no = handle(
        &s,
        TOKEN,
        &post(
            "/api/budgets/schedule",
            &tree_text(100, 3_000, 2_000, 300, 600),
            Some("workspace"),
        ),
    );
    assert_eq!(no.status, 422);
    assert!(no.body.contains("in use"), "{}", no.body);
    let same = handle(
        &s,
        TOKEN,
        &post(
            "/api/budgets/schedule",
            &tree_text(5_000, 3_000, 2_000, 300, 600),
            Some("workspace"),
        ),
    );
    assert_eq!(
        (
            same.status,
            same.body.contains("same as the budgets in force")
        ),
        (422, true),
        "{}",
        same.body
    );
    assert_eq!(
        handle(
            &s,
            TOKEN,
            &post("/api/budgets/schedule", "junk", Some("workspace"))
        )
        .status,
        422
    );
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0.len(), 1);
    // Withdrawing is a request too.
    let w = handle(
        &s,
        TOKEN,
        &post("/api/budgets/withdraw", "", Some("workspace")),
    );
    assert_eq!(
        (w.status, w.body.as_str()),
        (200, "{\"requested\":\"0000000002.req\"}")
    );
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0[1].tree, None);
    let v = crate::budgets::view(&s, None).unwrap();
    assert!(
        v.contains("\"changes\":[\"withdraw the scheduled change\"]"),
        "{v}"
    );
    // Only the three routes under /api/budgets/ take a POST.
    assert_eq!(
        handle(&s, TOKEN, &post("/api/budgets", &draft, Some("workspace"))).status,
        405
    );
    // No budgets: nothing to change (409). Unknown route: 404. GET of the write routes: 404.
    let nob = scratch("editor-post-nob");
    let jn = account(&nob, false);
    assert_eq!(
        handle(
            &src(&nob),
            TOKEN,
            &post("/api/budgets/schedule", &draft, Some("workspace"))
        )
        .status,
        409
    );
    assert_eq!(
        handle(
            &s,
            TOKEN,
            &post("/api/budgets/other", "", Some("workspace"))
        )
        .status,
        404
    );
    assert_eq!(
        handle(&s, TOKEN, &signed("/api/budgets/schedule")).status,
        404
    );
    assert_eq!(handle(&s, TOKEN, &signed("/api/budgets")).status, 200);
    assert_eq!(
        handle(&src(&nob), TOKEN, &signed("/api/budgets")).status,
        409
    );
    assert_eq!(handle(&s, TOKEN, &get("/api/budgets")).status, 401);
    drop((j, jn));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&nob);
}

#[test]
fn money_is_grouped_in_thousands() {
    assert_eq!(crate::money(0), "$0.00");
    assert_eq!(crate::money(999 * P as i128), "$999.00");
    assert_eq!(crate::money(1_000 * P as i128), "$1,000.00");
    assert_eq!(
        crate::money(1_234_567 * P as i128 + 5 * P as i128 / 10),
        "$1,234,567.50"
    );
    assert_eq!(crate::money(-12_345 * P as i128), "-$12,345.00");
    assert_eq!(crate::money(100_000 * P as i128), "$100,000.00");
}

#[test]
fn a_strategy_already_over_its_budget_is_told_it_cannot_be_cut() {
    let dir = scratch("editor-over");
    let mut j = account(&dir, false);
    // alpha holds $500; give the account a balance so small that alpha's half is $400.
    let ids = [(1, "alpha"), (2, "beta"), (3, "gamma")].map(|(n, id)| (n, id.to_owned()));
    j.set_budgets(
        Some(tf_risk::Budgets::new(budgets().tree().clone(), 800 * P as u128, ids).unwrap()),
        90 * SEC,
    )
    .unwrap();
    let v = crate::budgets::view(&src(&dir), None).unwrap();
    assert!(v.contains("\"id\":\"alpha\",\"share_bp\":5000,\"budget\":\"$400.00\",\"used\":\"$500.00\",\"range\":{\"min\":5000,\"max\":5000,\"min_why\":\"it is already over its budget, so it cannot be cut\""), "{v}");
    // Cutting it is refused; raising it is not.
    let cut = tree_text(4_000, 4_000, 2_000, 300, 600);
    let v = crate::budgets::view(&src(&dir), Some(&cut)).unwrap();
    assert!(
        v.contains("\"valid\":false") && v.contains("alpha: the edit leaves a budget"),
        "{v}"
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- proposals

fn propose(
    dir: &Path,
    by: &str,
    alpha: u32,
    beta: u32,
    gamma: u32,
    at: Nanos,
) -> tf_proposals::flow::Submitted {
    tf_proposals::flow::submit(
        dir,
        &tf_proposals::Policy::default(),
        by,
        "lost three sessions in a row",
        "runs abc123 and def456, review 7",
        &tree_text(alpha, beta, gamma, 300, 600),
        at,
    )
    .unwrap()
}

const DAY_NS: Nanos = 86_400 * SEC;

#[test]
fn the_panel_lists_proposals_newest_first_with_what_changed_why_and_the_evidence() {
    let dir = scratch("panel");
    let j = account(&dir, true);
    let s = src(&dir);
    assert_eq!(
        crate::proposals::view(&s).unwrap(),
        "{\"proposals\":[],\"unreadable\":0}"
    );
    let cut = propose(&dir, "risk-agent", 4_500, 3_000, 2_000, 100 * DAY_NS);
    let up = propose(&dir, "growth-agent", 5_500, 3_000, 1_500, 101 * DAY_NS);
    let same = propose(&dir, "noisy-agent", 5_000, 3_000, 2_000, 102 * DAY_NS);
    assert_eq!((cut.id, up.id, same.id), (1, 2, 3));
    let v = crate::proposals::view(&s).unwrap();
    // Newest first.
    let (p3, p2, p1) = (
        v.find("\"id\":3").unwrap(),
        v.find("\"id\":2").unwrap(),
        v.find("\"id\":1").unwrap(),
    );
    assert!(p3 < p2 && p2 < p1, "{v}");
    assert!(v.contains("\"id\":1,\"by\":\"risk-agent\",\"at\":\"1970-04-11 00:00\",\"state\":\"scheduled\",\"reason\":\"lost three sessions in a row\",\"evidence\":\"runs abc123 and def456, review 7\""), "{v}");
    assert!(
        v.contains("\"changes\":[\"alpha: 50% → 45% of day ($15,000.00 → $13,500.00)\"]"),
        "{v}"
    );
    assert!(
        v.contains("\"state\":\"waiting\"")
            && v.contains("alpha: 50% → 55% of day ($15,000.00 → $16,500.00)")
            && v.contains("gamma: 20% → 15% of day ($6,000.00 → $4,500.00)"),
        "{v}"
    );
    assert!(v.contains("alpha: an increase needs a person"), "{v}");
    assert!(
        v.contains("\"state\":\"refused\"") && v.contains("it changes nothing"),
        "{v}"
    );
    assert!(v.contains("\"decision\":null"), "{v}");
    assert!(v.contains("queued as 0000000001.req"), "{v}");
    // Over HTTP: needs sign-in, and without a ledger there is nothing to show.
    assert_eq!(handle(&s, TOKEN, &get("/api/proposals")).status, 401);
    assert_eq!(handle(&s, TOKEN, &signed("/api/proposals")).status, 200);
    assert_eq!(
        handle(&Source::default(), TOKEN, &signed("/api/proposals")).status,
        409
    );
    drop(j);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_person_approves_or_declines_from_the_app_and_the_ledger_is_never_written() {
    let dir = scratch("panel-answer");
    let mut j = account(&dir, true);
    let s = src(&dir);
    let log = std::fs::read(dir.join("ledger.log")).unwrap();
    let a = propose(&dir, "growth-agent", 5_500, 3_000, 1_500, 100 * DAY_NS);
    let b = propose(&dir, "growth-agent", 4_500, 3_000, 2_500, 101 * DAY_NS);
    let c = propose(&dir, "growth-agent", 5_100, 3_000, 1_900, 102 * DAY_NS);
    let answer = |what: &str, id: u64, note: &str, header: Option<&str>| {
        handle(
            &s,
            TOKEN,
            &post(&format!("/api/proposals/{id}/{what}"), note, header),
        )
    };
    // Sign-in and the header are both needed; nothing is decided by the refused calls.
    let anon = Request {
        method: "POST".into(),
        requested_with: Some("workspace".into()),
        ..get("/api/proposals/1/approve")
    };
    assert_eq!(handle(&s, TOKEN, &anon).status, 401);
    assert_eq!(answer("approve", a.id, "", None).status, 403);
    assert_eq!(answer("approve", a.id, "", Some("x")).status, 403);
    let before = crate::proposals::view(&s).unwrap();
    assert_eq!(
        before.matches("\"state\":\"waiting\"").count(),
        3,
        "{before}"
    );
    // Approve one: queued like any edit, and shown as decided with the note.
    let ok = answer("approve", a.id, "agreed", Some("workspace"));
    assert_eq!(
        (ok.status, ok.body.as_str()),
        (200, "{\"requested\":\"0000000001.req\"}"),
        "{}",
        ok.body
    );
    let (waiting, _) = tf_ledger::inbox::pending(&dir).unwrap();
    assert_eq!(waiting.len(), 1);
    assert!(
        waiting[0]
            .by
            .contains("approving a proposal by growth-agent"),
        "{}",
        waiting[0].by
    );
    let v = crate::proposals::view(&s).unwrap();
    assert!(v.contains("\"state\":\"approved\"") && v.contains("\"decision\":{\"by\":\"the workspace app (shared token)\",\"call\":\"approved\",\"note\":\"agreed\""), "{v}");
    // Decide once.
    assert_eq!(answer("approve", a.id, "", Some("workspace")).status, 409);
    assert_eq!(answer("decline", a.id, "", Some("workspace")).status, 409);
    // Decline another: nothing queued.
    let no = answer("decline", b.id, "not now", Some("workspace"));
    assert_eq!((no.status, no.body.as_str()), (200, r#"{"declined":true}"#));
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0.len(), 1);
    assert!(
        crate::proposals::view(&s)
            .unwrap()
            .contains("\"call\":\"declined\",\"note\":\"not now\"")
    );
    // Unknown, malformed and wrong routes.
    assert_eq!(answer("approve", 99, "", Some("workspace")).status, 404);
    assert_eq!(
        handle(
            &s,
            TOKEN,
            &post("/api/proposals/x/approve", "", Some("workspace"))
        )
        .status,
        404
    );
    assert_eq!(
        handle(&s, TOKEN, &post("/api/proposals/1", "", Some("workspace"))).status,
        404
    );
    assert_eq!(
        handle(
            &s,
            TOKEN,
            &post("/api/proposals/1/delete", "", Some("workspace"))
        )
        .status,
        404
    );
    assert_eq!(
        handle(&s, TOKEN, &post("/api/proposals", "", Some("workspace"))).status,
        405
    );
    // A proposal approved late, after its strategy has run into a drawdown, is refused and stays waiting.
    trip_loss(&mut j);
    let late = answer("approve", c.id, "", Some("workspace"));
    assert_eq!(late.status, 422, "{}", late.body);
    assert!(late.body.contains("drawdown"), "{}", late.body);
    assert!(crate::proposals::view(&s).unwrap().contains("\"id\":3,"));
    assert_eq!(tf_ledger::inbox::pending(&dir).unwrap().0.len(), 1);
    drop(j);
    // The ledger itself was written only by the engine side of this test (the loss), never by the app.
    assert!(
        std::fs::read(dir.join("ledger.log"))
            .unwrap()
            .starts_with(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// alpha (the strategy a proposal raised) sells the 100 shares it holds at $5 for 40 cents: it loses
/// $460, past its soft limit of $450.
fn trip_loss(j: &mut Journal<FileStore>) {
    let s = intent(1, 1_010, Side::Sell, Purpose::Close, 4 * P / 10);
    fill(j, &s, 4 * P / 10);
    assert!(!j.check_loss_limits(1_500 * SEC).unwrap().is_empty());
}

// ---- the backtest view (E19-S34, E19-S35) ----

/// Research results with one replayed day, `alpha` on 2026-05-04, whose ledger is `ledger` (if any): what the program that hosts
/// the service reads from its results directory.
struct FakeResearch {
    ledger: Option<PathBuf>,
}

impl crate::ResearchView for FakeResearch {
    fn runs(&self) -> Result<Vec<tf_catalog::Run>, String> {
        let Some(dir) = &self.ledger else {
            return Ok(vec![]);
        };
        Ok(tf_catalog::sessions(
            tf_ledger::ReadOnlyStore::open(dir),
            "alpha/2026-05-04",
            Kind::Backtest,
        )
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| tf_catalog::as_replay(r, "alpha", "2026-05-04"))
        .collect())
    }

    fn replay_ledger(&self, scenario: &str, day: &str) -> Result<PathBuf, crate::ResearchError> {
        use crate::ResearchError::{NotFound, Refused};
        match (scenario, day, &self.ledger) {
            ("alpha", "2026-05-04", Some(dir)) => Ok(dir.clone()),
            ("damaged", _, _) => Err(Refused("the ledger is <b>cut</b> & damaged".into())),
            _ => Err(NotFound("no such day".into())),
        }
    }

    fn scenarios(&self) -> Result<String, String> {
        Ok("{\"scenarios\":[]}".to_owned())
    }

    fn trades(
        &self,
        scenario: &str,
        day: &str,
        strategy: u16,
    ) -> Result<String, crate::ResearchError> {
        use crate::ResearchError::{NotFound, Refused};
        match (scenario, day, strategy) {
            ("alpha", "2026-05-04", 1) => Ok("{\"trades\":[]}".to_owned()),
            ("damaged", _, _) => Err(Refused("the trace file <b>does not match</b>".into())),
            _ => Err(NotFound("no such day".into())),
        }
    }

    fn trade_page(
        &self,
        scenario: &str,
        day: &str,
        strategy: u16,
        n: usize,
    ) -> Result<String, crate::ResearchError> {
        use crate::ResearchError::{NotFound, Refused};
        match (scenario, day, strategy, n) {
            ("alpha", "2026-05-04", 1, 0) => Ok("<!doctype html><title>TRADE PAGE</title>".into()),
            ("damaged", _, _, _) => Err(Refused("a <b>damaged</b> log & trace".into())),
            _ => Err(NotFound("no such trade".into())),
        }
    }
}

fn with_research() -> Source {
    Source {
        research: Some(crate::Research::new(FakeResearch { ledger: None })),
        ..Source::default()
    }
}

/// Research results whose replayed day has the ledger `account` makes in `dir` (the writer closed, as a finished day's is).
fn with_replayed_day(dir: &Path) -> Source {
    drop(account(dir, true));
    Source {
        research: Some(crate::Research::new(FakeResearch {
            ledger: Some(dir.to_owned()),
        })),
        ..Source::default()
    }
}

#[test]
fn the_backtest_routes_answer_only_signed_in_gets_and_map_what_the_view_says() {
    let s = with_research();
    let api = "/api/research/trades";
    let with = |path: &str, q: &str| Request {
        query: q.into(),
        ..signed(path)
    };
    assert_eq!(handle(&s, TOKEN, &get("/api/research")).status, 401);
    assert_eq!(handle(&s, TOKEN, &get("/research/trade")).status, 401);
    let list = handle(&s, TOKEN, &signed("/api/research"));
    assert_eq!(
        (list.status, list.body.as_str()),
        (200, "{\"scenarios\":[]}")
    );
    let q = "scenario=alpha&day=2026-05-04&strategy=1";
    assert_eq!(handle(&s, TOKEN, &with(api, q)).status, 200);
    assert_eq!(
        handle(
            &s,
            TOKEN,
            &with(api, "scenario=alpha&day=2026-05-05&strategy=1")
        )
        .status,
        404
    );
    // The view's refusal is a 422 and its text is JSON, not markup.
    let refused = handle(&s, TOKEN, &with(api, "scenario=damaged&day=d&strategy=1"));
    assert_eq!(refused.status, 422);
    assert!(refused.body.contains("does not match"), "{}", refused.body);
    // A request that does not say which day of what is a 400 and reaches nobody.
    for bad in [
        "",
        "scenario=alpha",
        "scenario=alpha&day=2026-05-04",
        "scenario=alpha&day=2026-05-04&strategy=x",
        "scenario=alpha&day=2026-05-04&strategy=70000",
    ] {
        assert_eq!(handle(&s, TOKEN, &with(api, bad)).status, 400, "{bad}");
    }
    // Nothing is changed through them.
    for path in ["/api/research", api, "/research/trade"] {
        let post = Request {
            method: "POST".into(),
            ..signed(path)
        };
        assert_eq!(handle(&s, TOKEN, &post).status, 405, "{path}");
    }
    // Without results there is nothing to show.
    let none = Source::default();
    assert_eq!(handle(&none, TOKEN, &signed("/api/research")).status, 404);
    assert_eq!(handle(&none, TOKEN, &with(api, q)).status, 404);
    assert_eq!(
        handle(&none, TOKEN, &with("/research/trade", &format!("{q}&n=0"))).status,
        404
    );
}

#[test]
fn a_trade_page_is_the_explorers_kind_of_page_and_says_why_it_is_not_given_as_text() {
    let s = with_research();
    let page = |q: &str| {
        handle(
            &s,
            TOKEN,
            &Request {
                query: q.into(),
                ..signed("/research/trade")
            },
        )
    };
    let ok = page("scenario=alpha&day=2026-05-04&strategy=1&n=0");
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("TRADE PAGE"));
    assert!(
        ok.csp.contains("script-src 'unsafe-inline'") && ok.csp.contains("connect-src 'none'"),
        "{}",
        ok.csp
    );
    assert!(ok.csp.contains("frame-ancestors 'none'") && ok.csp.contains("form-action 'none'"));
    assert_eq!(
        page("scenario=alpha&day=2026-05-04&strategy=1&n=9").status,
        404
    );
    let refused = page("scenario=damaged&day=2026-05-04&strategy=1&n=0");
    assert_eq!(refused.status, 422);
    assert!(
        refused
            .body
            .contains("a &lt;b&gt;damaged&lt;/b&gt; log &amp; trace"),
        "{}",
        refused.body
    );
    assert!(!refused.body.contains("<b>damaged"));
    for bad in [
        "",
        "scenario=alpha&day=2026-05-04&strategy=1",
        "scenario=alpha&day=2026-05-04&strategy=1&n=-1",
        "scenario=alpha&day=2026-05-04&strategy=1&n=x",
    ] {
        let r = page(bad);
        assert_eq!(r.status, 400, "{bad}");
        assert!(!r.body.contains("TRADE PAGE"));
    }
}

#[test]
fn a_replayed_day_is_in_the_runs_and_opens_as_a_live_session_does() {
    let dir = scratch("replayed-runs");
    let s = with_replayed_day(&dir);
    // Each strategy that acted is a run of the day, a backtest kept apart by its source, naming its scenario and day.
    let runs = crate::runs(&s, Some("beta")).unwrap();
    assert!(
        runs.contains("\"kind\":\"backtest\"") && runs.contains("\"source\":\"replay\""),
        "{runs}"
    );
    assert!(
        runs.contains("\"replayed\":true") && runs.contains("\"explorable\":false"),
        "{runs}"
    );
    assert!(
        runs.contains("\"scenario\":\"alpha\",\"day\":\"2026-05-04\""),
        "{runs}"
    );
    assert!(
        runs.contains("\"id\":\"replay:alpha:2026-05-04\""),
        "{runs}"
    );
    assert_eq!(
        crate::runs(&s, None)
            .unwrap()
            .matches("\"replayed\":true")
            .count(),
        2
    );
    // Opened, it has what the same session of a live ledger has: the fills, the profit after each, the refusals.
    let d = crate::run_detail(&s, "beta", "replay:alpha:2026-05-04")
        .unwrap()
        .unwrap();
    let live = crate::run_detail(&src(&dir), "beta", "session-1")
        .unwrap()
        .unwrap();
    let body = |t: &str| t[t.find("\"trades\":[").unwrap()..].to_owned();
    assert_eq!(body(&d), body(&live), "the same trades, curve and refusals");
    assert!(
        d.contains("\"curve\":[\"-0.00\"") || d.contains("-100.00"),
        "{d}"
    );
    // A run that is not there, and a live session's id, are not found.
    assert_eq!(
        crate::run_detail(&s, "beta", "replay:alpha:2026-05-05").unwrap(),
        None
    );
    assert_eq!(crate::run_detail(&s, "beta", "session-1").unwrap(), None);
    // Without a replayed day, or without results at all, nothing is added, and no run says it is a replay.
    assert!(
        !crate::runs(&with_research(), None)
            .unwrap()
            .contains("replayed\":true")
    );
    assert!(
        !crate::runs(&Source::default(), None)
            .unwrap()
            .contains("replayed\":true")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_replayed_days_overview_is_the_overview_of_its_ledger_and_is_refused_with_the_reason() {
    let dir = scratch("replayed-overview");
    let s = with_replayed_day(&dir);
    let get_ov = |q: &str| {
        handle(
            &s,
            TOKEN,
            &Request {
                query: q.into(),
                ..signed("/api/overview")
            },
        )
    };
    let ok = get_ov("scenario=alpha&day=2026-05-04");
    assert_eq!(ok.status, 200);
    assert!(
        ok.body.contains("\"kind\":\"backtest\"") && ok.body.contains("\"budgets\":true"),
        "{}",
        ok.body
    );
    // The same figures a live view of that ledger shows, as to the groups and strategies.
    let live = overview(&src(&dir)).unwrap();
    for part in [
        "\"balance\":\"30000.00\"",
        "\"name\":\"alpha\"",
        "\"name\":\"beta\"",
        "\"day_pnl\":\"-100.00\"",
    ] {
        assert!(
            ok.body.contains(part) && live.contains(part),
            "{part} in {}",
            ok.body
        );
    }
    // Each strategy's latest run is the replayed day.
    assert!(
        ok.body.contains("\"latest_run\":{") && ok.body.contains("\"source\":\"replay\""),
        "{}",
        ok.body
    );
    // Not found, refused (its reason as text, not markup), and asked for badly.
    assert_eq!(get_ov("scenario=alpha&day=2026-05-05").status, 404);
    let bad = get_ov("scenario=damaged&day=2026-05-04");
    assert_eq!(bad.status, 422);
    assert!(bad.body.contains("cut"), "{}", bad.body);
    assert_eq!(get_ov("scenario=alpha").status, 400);
    // Signed in only, and GET only.
    let unsigned = Request {
        query: "scenario=alpha&day=2026-05-04".into(),
        ..get("/api/overview")
    };
    assert_eq!(handle(&s, TOKEN, &unsigned).status, 401);
    // Without results, nothing to show; and without those parameters it is the ledger's own overview, as before.
    let none = handle(
        &Source::default(),
        TOKEN,
        &Request {
            query: "scenario=alpha&day=2026-05-04".into(),
            ..signed("/api/overview")
        },
    );
    assert_eq!(none.status, 404);
    assert!(
        handle(&s, TOKEN, &signed("/api/overview"))
            .body
            .contains("\"account\":null")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_backtest_screens_read_only_what_the_view_sends_and_stay_get_only() {
    let app = include_str!("ui/app.js");
    // The keys the screens read; the host's own tests read every one of them out of what it writes.
    for key in [
        "scenarios",
        "error",
        "days",
        "trades",
        "net",
        "cost",
        "latency_ms",
        "borrow_bps_per_year",
        "sec_through",
        "taf_through",
        "strategies",
        "id",
        "name",
        "params",
        "universe",
        "mean_bp",
        "budget",
        "budgets",
        "balance",
        "unassigned_bp",
        "groups",
        "share_bp",
        "soft_bp",
        "hard_bp",
        "number",
        "strategy",
        "accepted",
        "rejected",
        "refused",
        "rejections",
        "reason",
        "count",
        "symbol",
        "side",
        "qty",
        "entry",
        "entry_px",
        "exit",
        "exit_px",
        "bp",
        "exit_reason",
        "open_at_end",
        "replayed",
        "ledgers",
        "ok",
        "number",
    ] {
        assert!(
            app.contains(&format!(".{key}")),
            "the screens do not use {key}"
        );
    }
    // Only the pages of the service itself and the one trade page, and the trade page is addressed by encoded values.
    assert!(
        app.contains("\"/api/research\"")
            && app.contains("/api/research/trades?scenario=\" + encodeURIComponent")
    );
    assert!(
        app.contains("\"/research/trade?\" + q") && app.contains("&n=\" + encodeURIComponent(x.n)")
    );
    // Still no request but the two writing ones.
    assert_eq!(app.matches("method:").count(), 2);
    assert!(app.contains("#/backtests"));
}

/// No Rust test runs the app's script, so a syntax error in it would show only in a browser: have node read it, if there is a node
/// (CI has one; a machine without skips this).
#[test]
fn the_apps_script_is_valid_javascript() {
    let dir = scratch("app-js");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("app.js");
    std::fs::write(&file, include_str!("ui/app.js")).unwrap();
    // No node on this machine: not checked here.
    if let Ok(out) = std::process::Command::new("node")
        .arg("--check")
        .arg(&file)
        .output()
    {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
