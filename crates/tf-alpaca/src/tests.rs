use tf_core::Px;
use tf_strategy::intent::{Intent, IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use tf_strategy::lifecycle::{OrderId, OrderState};

use crate::events::*;
use crate::json::{Json, quote};
use crate::wire::*;

const D: i64 = 1_000_000_000;

fn px(dollars_text: &str) -> Px {
    parse_px(dollars_text).unwrap()
}

// ------------------------------------------------------------------------- json

#[test]
fn json_is_read_strictly_and_numbers_stay_text() {
    let v = Json::parse(
        r#" {"a": [1, -2.50, 3e2, true, false, null, "x\n\u00e9\ud83d\ude00"], "b": {"c": "d"}} "#,
    )
    .unwrap();
    let Json::Arr(a) = v.get("a").unwrap() else {
        panic!()
    };
    assert_eq!(a[0], Json::Num("1".into()));
    assert_eq!(
        a[1],
        Json::Num("-2.50".into()),
        "a number keeps the digits it came with"
    );
    assert_eq!(a[2], Json::Num("3e2".into()));
    assert_eq!(
        (a[3].clone(), a[4].clone(), a[5].clone()),
        (Json::Bool(true), Json::Bool(false), Json::Null)
    );
    assert_eq!(a[6], Json::Str("x\né😀".into()));
    assert_eq!(v.obj_at("b").unwrap().str_at("c"), Some("d"));
    assert_eq!(v.str_at("missing"), None);
    assert_eq!(v.get("a").unwrap().text(), None);
    assert_eq!(Json::parse("{}").unwrap(), Json::Obj(Default::default()));
    assert_eq!(Json::parse("[]").unwrap(), Json::Arr(vec![]));
    assert_eq!(Json::parse("12").unwrap().text(), Some("12"));
}

#[test]
fn json_that_is_not_quite_json_is_refused_with_where() {
    let bad = |t: &str| Json::parse(t).unwrap_err();
    for (text, why) in [
        ("", "the text ends"),
        ("{} x", "text after the value"),
        ("{\"a\":1,\"a\":2}", "a key appears twice"),
        ("[1,]", "not a value"),
        ("{\"a\":1,}", "expected a key"),
        ("{\"a\" 1}", "expected `:`"),
        ("[1 2]", "expected `,` or `]`"),
        ("{\"a\":1 \"b\":2}", "expected `,` or `}`"),
        ("01", "a number cannot start with 0"),
        ("-", "a number needs digits"),
        ("1.", "a fraction needs digits"),
        ("1e", "an exponent needs digits"),
        ("\"abc", "a string is not closed"),
        ("\"a\u{1}b\"", "a control character in a string"),
        ("\"\\q\"", "bad escape"),
        ("\"\\ud800\"", "a lone surrogate"),
        ("\"\\udc00\"", "a lone surrogate"),
        ("\"\\ud800\\u0041\"", "a lone surrogate"),
        ("\"\\u12\"", "short \\u escape"),
        ("\"\\u12zz\"", "bad \\u escape"),
        ("nul", "not a value"),
        ("tru", "not a value"),
    ] {
        assert_eq!(bad(text).why, why, "{text:?}");
    }
    assert_eq!(bad("[1, x]").at, 4);
    let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
    assert_eq!(bad(&deep).why, "nested too deeply");
    assert!(bad("[1, x]").to_string().contains("byte 4"));
}

#[test]
fn text_is_quoted_so_it_cannot_break_out() {
    assert_eq!(quote("AAPL"), "\"AAPL\"");
    assert_eq!(
        quote("a\"b\\c\nd\te\r\u{1}"),
        "\"a\\\"b\\\\c\\nd\\te\\r\\u0001\""
    );
    let nasty = "x\",\"side\":\"sell";
    let back = Json::parse(&quote(nasty)).unwrap();
    assert_eq!(back, Json::Str(nasty.into()));
}

// -------------------------------------------------------------------------- wire

#[test]
fn prices_are_read_exactly() {
    assert_eq!(parse_px("105.8988475").unwrap().raw(), 105_898_847_500);
    assert_eq!(parse_px("0.0001").unwrap().raw(), 100_000);
    assert_eq!(parse_px("150").unwrap().raw(), 150 * D);
    assert_eq!(parse_px("150.").unwrap().raw(), 150 * D);
    assert_eq!(parse_px("-1.5").unwrap().raw(), -1_500_000_000);
    assert_eq!(
        parse_px("1.0000000000").unwrap().raw(),
        D,
        "zeros past a billionth are nothing"
    );
    assert_eq!(parse_px("0.000000001").unwrap().raw(), 1);
    for bad in [
        "",
        ".5",
        "1e3",
        "1.0000000001",
        "abc",
        "1,5",
        "--1",
        "+1",
        " 1",
        "1 ",
        "99999999999999999999",
    ] {
        assert_eq!(parse_px(bad), None, "{bad:?}");
    }
}

#[test]
fn prices_go_to_the_tick_in_the_direction_asked_and_never_a_float() {
    let t = |raw: i64, r: Round| price_text(Px::from_raw(raw), r);
    // Two places from a dollar up.
    assert_eq!(t(150_250_000_000, Round::Down).unwrap(), "150.25");
    assert_eq!(t(150_259_999_999, Round::Down).unwrap(), "150.25");
    assert_eq!(t(150_250_000_001, Round::Up).unwrap(), "150.26");
    assert_eq!(
        t(150_250_000_000, Round::Up).unwrap(),
        "150.25",
        "already on the tick"
    );
    assert_eq!(t(D, Round::Down).unwrap(), "1.00");
    assert_eq!(t(D + 1, Round::Up).unwrap(), "1.01");
    assert_eq!(t(D + 9_999_999, Round::Down).unwrap(), "1.00");
    // Four places below a dollar.
    assert_eq!(t(123_450_000, Round::Down).unwrap(), "0.1234");
    assert_eq!(t(123_450_000, Round::Up).unwrap(), "0.1235");
    assert_eq!(t(999_900_000, Round::Down).unwrap(), "0.9999");
    // Up from just under a dollar lands on a dollar and takes two places.
    assert_eq!(t(999_900_001, Round::Up).unwrap(), "1.00");
    assert_eq!(t(999_999_999, Round::Up).unwrap(), "1.00");
    assert_eq!(t(999_999_999, Round::Down).unwrap(), "0.9999");
    // Nothing for what is not a price.
    assert_eq!(t(0, Round::Up), None);
    assert_eq!(t(-5 * D, Round::Down), None);
    assert_eq!(t(99_999, Round::Down), None, "rounds to nothing");
    assert_eq!(t(99_999, Round::Up).unwrap(), "0.0001");
    assert_eq!(t(1_000_000 * D, Round::Up).unwrap(), "1000000.00");
}

fn intent(
    side: Side,
    purpose: Purpose,
    pricing: Pricing,
    protect: Option<Protective>,
    tif: Tif,
) -> Intent {
    Intent {
        id: IntentId {
            strategy: StrategyId(1),
            seq: 1,
        },
        instrument: 0,
        side,
        qty: 100,
        purpose,
        pricing,
        protect,
        tif,
        ts: 0,
        reason: 0,
    }
}

fn limit(p: &str) -> Pricing {
    Pricing::Limit(px(p))
}

fn body(i: &Intent) -> Json {
    let r = order_request(i, OrderId(7), "AAPL", "tf1-").unwrap();
    Json::parse(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.body))
}

#[test]
fn a_limit_order_becomes_a_limit_order_that_cannot_pay_more() {
    let i = intent(Side::Buy, Purpose::Open, limit("150.259"), None, Tif::Day);
    let r = order_request(&i, OrderId(7), "AAPL", "tf1-").unwrap();
    assert_eq!(r.client_order_id, "tf1-7");
    let b = Json::parse(&r.body).unwrap();
    assert_eq!(b.str_at("symbol"), Some("AAPL"));
    assert_eq!(b.str_at("qty"), Some("100"));
    assert_eq!(b.str_at("side"), Some("buy"));
    assert_eq!(b.str_at("type"), Some("limit"));
    assert_eq!(b.str_at("time_in_force"), Some("day"));
    assert_eq!(b.str_at("limit_price"), Some("150.25"), "a buy rounds down");
    assert_eq!(b.str_at("client_order_id"), Some("tf1-7"));
    assert_eq!(b.get("order_class"), None);
    // A sell limit rounds up, a short sells, a close is just a sell, IOC is carried.
    let sell = intent(Side::Sell, Purpose::Close, limit("150.251"), None, Tif::Ioc);
    let b = body(&sell);
    assert_eq!(
        (
            b.str_at("side"),
            b.str_at("limit_price"),
            b.str_at("time_in_force")
        ),
        (Some("sell"), Some("150.26"), Some("ioc"))
    );
    let short = intent(
        Side::SellShort,
        Purpose::Open,
        limit("20.001"),
        None,
        Tif::Day,
    );
    let b = body(&short);
    assert_eq!(
        (b.str_at("side"), b.str_at("limit_price")),
        (Some("sell"), Some("20.01"))
    );
}

#[test]
fn a_collar_is_a_limit_at_the_worst_price_it_allows() {
    // Reference $50.00, 1% collar: a buy may pay up to $50.50, a sell may take down to $49.50.
    let buy = intent(
        Side::Buy,
        Purpose::Open,
        Pricing::Collar {
            reference: px("50.00"),
            collar_permille: 10,
        },
        None,
        Tif::Ioc,
    );
    assert_eq!(body(&buy).str_at("limit_price"), Some("50.50"));
    let sell = intent(
        Side::Sell,
        Purpose::Close,
        Pricing::Collar {
            reference: px("50.00"),
            collar_permille: 10,
        },
        None,
        Tif::Ioc,
    );
    assert_eq!(body(&sell).str_at("limit_price"), Some("49.50"));
    // Not on a tick: the buy rounds down (never above its bound), the sell up (never below).
    let buy = intent(
        Side::Buy,
        Purpose::Open,
        Pricing::Collar {
            reference: px("50.07"),
            collar_permille: 10,
        },
        None,
        Tif::Ioc,
    );
    assert_eq!(
        body(&buy).str_at("limit_price"),
        Some("50.57"),
        "50.07 * 1.01 = 50.5707"
    );
    let sell = intent(
        Side::Sell,
        Purpose::Close,
        Pricing::Collar {
            reference: px("50.07"),
            collar_permille: 10,
        },
        None,
        Tif::Ioc,
    );
    assert_eq!(
        body(&sell).str_at("limit_price"),
        Some("49.57"),
        "50.07 * 0.99 = 49.5693"
    );
    // Whatever the collar, the request never allows more than the intent's own worst price.
    for (reference, permille) in [("3.33", 7u32), ("0.4567", 25), ("999.99", 999), ("1.00", 0)] {
        for side in [Side::Buy, Side::Sell] {
            let pricing = Pricing::Collar {
                reference: px(reference),
                collar_permille: permille,
            };
            let i = intent(side, Purpose::Open, pricing, None, Tif::Ioc);
            let sent = parse_px(body(&i).str_at("limit_price").unwrap())
                .unwrap()
                .raw();
            let worst = pricing.worst_price(side).raw();
            if side.is_buy() {
                assert!(sent <= worst, "{reference} {permille}: {sent} > {worst}");
            } else {
                assert!(sent >= worst, "{reference} {permille}: {sent} < {worst}");
            }
            assert!((sent - worst).abs() < D / 100, "within a tick");
        }
    }
}

fn protect(stop: &str, stop_limit: Option<&str>, target: Option<&str>) -> Option<Protective> {
    Some(Protective {
        stop_trigger: px(stop),
        stop_limit: stop_limit.map(px),
        take_profit: target.map(px),
    })
}

#[test]
fn a_stop_alone_is_one_order_that_triggers_one_other_and_with_a_target_it_is_a_bracket() {
    let oto = intent(
        Side::Buy,
        Purpose::Open,
        limit("100.00"),
        protect("95.005", None, None),
        Tif::Day,
    );
    let b = body(&oto);
    assert_eq!(b.str_at("order_class"), Some("oto"));
    assert_eq!(b.get("take_profit"), None);
    assert_eq!(
        b.obj_at("stop_loss").unwrap().str_at("stop_price"),
        Some("95.01"),
        "a long's stop rounds up: never later"
    );
    assert_eq!(b.obj_at("stop_loss").unwrap().get("limit_price"), None);
    let bracket = intent(
        Side::Buy,
        Purpose::Open,
        limit("100.00"),
        protect("95.00", Some("94.904"), Some("110.001")),
        Tif::Day,
    );
    let b = body(&bracket);
    assert_eq!(b.str_at("order_class"), Some("bracket"));
    assert_eq!(
        b.obj_at("take_profit").unwrap().str_at("limit_price"),
        Some("110.01"),
        "a long's target rounds up: never at a worse price"
    );
    let sl = b.obj_at("stop_loss").unwrap();
    assert_eq!(
        (sl.str_at("stop_price"), sl.str_at("limit_price")),
        (Some("95.00"), Some("94.91"))
    );
    // A short's exits are buys and round down.
    let short = intent(
        Side::SellShort,
        Purpose::Open,
        limit("100.00"),
        protect("105.009", None, Some("90.009")),
        Tif::Day,
    );
    let b = body(&short);
    assert_eq!(b.str_at("side"), Some("sell"));
    assert_eq!(
        b.obj_at("stop_loss").unwrap().str_at("stop_price"),
        Some("105.00")
    );
    assert_eq!(
        b.obj_at("take_profit").unwrap().str_at("limit_price"),
        Some("90.00")
    );
}

#[test]
fn what_cannot_be_sent_is_refused_before_it_is_sent() {
    let req = |i: &Intent| order_request(i, OrderId(1), "AAPL", "p-");
    let mut i = intent(Side::Buy, Purpose::Open, limit("10.00"), None, Tif::Day);
    i.qty = 0;
    assert_eq!(req(&i), Err(RequestError::ZeroQty));
    let free = intent(Side::Buy, Purpose::Open, limit("0.00001"), None, Tif::Day);
    assert_eq!(req(&free), Err(RequestError::BadPrice));
    let close = intent(
        Side::Sell,
        Purpose::Close,
        limit("10.00"),
        protect("9.00", None, None),
        Tif::Day,
    );
    assert_eq!(req(&close), Err(RequestError::ProtectOnClose));
    let ioc = intent(
        Side::Buy,
        Purpose::Open,
        limit("10.00"),
        protect("9.00", None, None),
        Tif::Ioc,
    );
    assert_eq!(req(&ioc), Err(RequestError::ProtectNeedsDay));
    let bad_stop = intent(
        Side::Buy,
        Purpose::Open,
        limit("10.00"),
        protect("0.00", None, None),
        Tif::Day,
    );
    assert_eq!(req(&bad_stop), Err(RequestError::BadPrice));
    for e in [
        RequestError::ZeroQty,
        RequestError::BadPrice,
        RequestError::ProtectOnClose,
        RequestError::ProtectNeedsDay,
    ] {
        assert!(!e.to_string().is_empty());
    }
    // A symbol or prefix with quotes cannot change the request.
    let ok = intent(Side::Buy, Purpose::Open, limit("10.00"), None, Tif::Day);
    let r = order_request(&ok, OrderId(3), "A\",\"side\":\"sell", "x\"-").unwrap();
    let b = Json::parse(&r.body).unwrap();
    assert_eq!(b.str_at("side"), Some("buy"));
    assert_eq!(b.str_at("symbol"), Some("A\",\"side\":\"sell"));
    assert_eq!(b.str_at("client_order_id"), Some("x\"-3"));
}

// ------------------------------------------------------------------------ events

const NEW: &str = include_str!("../fixtures/stream_new.json");
const PARTIAL: &str = include_str!("../fixtures/stream_partial_fill.json");
const FILL: &str = include_str!("../fixtures/stream_fill.json");
const CANCELED: &str = include_str!("../fixtures/stream_canceled.json");
const REJECTED: &str = include_str!("../fixtures/stream_rejected.json");
const BRACKET: &str = include_str!("../fixtures/order_bracket_response.json");
const LEG_FILL: &str = include_str!("../fixtures/leg_stop_fill.json");
const BUYING_POWER: &str = include_str!("../fixtures/error_buying_power.json");
const VALIDATION: &str = include_str!("../fixtures/error_validation.json");

fn update(text: &str) -> Update {
    match parse_frame(text).unwrap() {
        Frame::Update(u) => *u,
        other => panic!("{other:?}"),
    }
}

#[test]
fn times_are_read_to_the_nanosecond_with_their_offset() {
    let t = |s: &str| parse_time(s);
    assert_eq!(t("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(t("1970-01-01T00:00:01Z"), Some(1_000_000_000));
    assert_eq!(
        t("2026-10-05T00:00:00Z"),
        Some(1_791_158_400 * 1_000_000_000)
    );
    assert_eq!(
        t("2022-04-19T17:45:05.024916716Z"),
        Some(1_650_390_305_024_916_716)
    );
    assert_eq!(
        t("2022-04-19T13:45:05.024916716-04:00"),
        t("2022-04-19T17:45:05.024916716Z")
    );
    assert_eq!(t("2022-04-19T19:45:05+02:00"), t("2022-04-19T17:45:05Z"));
    assert_eq!(
        t("2022-04-19T17:45:05.5Z"),
        Some(1_650_390_305_500_000_000),
        "a short fraction is a fraction"
    );
    assert_eq!(
        t("2024-02-29T00:00:00Z"),
        Some(1_709_164_800 * 1_000_000_000),
        "a leap day"
    );
    assert_eq!(t("2000-03-01T00:00:00Z"), Some(951_868_800 * 1_000_000_000));
    assert_eq!(
        t("2023-12-31T23:59:59Z"),
        Some(1_704_067_199 * 1_000_000_000)
    );
    for bad in [
        "",
        "2022-04-19",
        "2022-04-19T17:45:05",
        "2022-13-19T17:45:05Z",
        "2022-04-32T17:45:05Z",
        "2022-04-19T24:45:05Z",
        "2022-04-19T17:60:05Z",
        "2022-04-19T17:45:05.Z",
        "2022-04-19T17:45:05.1234567891Z",
        "2022-04-19 17:45:05Z",
        "2022-04-19T17:45:05+0200",
        "2022-04-19T17:45:05+25:00",
        "1969-12-31T23:59:59Z",
        "20x2-04-19T17:45:05Z",
        "2022-04-19T17:45:05ZZ",
    ] {
        assert_eq!(t(bad), None, "{bad:?}");
    }
}

#[test]
fn shares_are_whole() {
    assert_eq!(whole_shares("100"), Some(100));
    assert_eq!(whole_shares("100.0"), Some(100));
    assert_eq!(whole_shares("100.000000000"), Some(100));
    assert_eq!(whole_shares("0"), Some(0));
    for bad in ["", "1.5", "100.01", "-1", "abc", ".5", "4294967296", "1e2"] {
        assert_eq!(whole_shares(bad), None, "{bad:?}");
    }
}

#[test]
fn the_documented_messages_are_read() {
    assert_eq!(
        parse_frame(include_str!("../fixtures/stream_authorized.json")).unwrap(),
        Frame::Authorized
    );
    assert_eq!(
        parse_frame(include_str!("../fixtures/stream_unauthorized.json")).unwrap(),
        Frame::Unauthorized
    );
    assert_eq!(
        parse_frame(include_str!("../fixtures/stream_listening.json")).unwrap(),
        Frame::Listening(vec!["trade_updates".into()])
    );
    assert_eq!(
        parse_frame(r#"{"stream":"account_updates","data":{}}"#).unwrap(),
        Frame::Other("account_updates".into())
    );
    let n = update(NEW);
    assert_eq!(n.event, "new");
    assert_eq!(
        (
            n.order.client_order_id.as_str(),
            n.order.symbol.as_str(),
            n.order.status.as_str(),
            n.order.kind.as_str()
        ),
        ("tf1-7", "AAPL", "new", "limit")
    );
    assert_eq!(
        (n.order.qty, n.order.filled_qty, n.order.filled_avg_price),
        (Some(100), 0, None)
    );
    assert_eq!(n.at, parse_time("2026-10-06T13:30:00.123456789Z"));
    let p = update(PARTIAL);
    assert_eq!(
        (p.qty, p.price, p.execution_id.as_deref()),
        (
            Some(40),
            Some(px("150.20")),
            Some("22222222-0000-4000-8000-000000000002")
        )
    );
    assert_eq!(
        (p.order.filled_qty, p.order.filled_avg_price),
        (40, Some(px("150.20")))
    );
    let f = update(FILL);
    assert_eq!(
        f.order.filled_avg_price,
        Some(px("150.208")),
        "an average can have more places than a tick"
    );
}

#[test]
fn messages_that_are_not_what_they_claim_are_errors_not_guesses() {
    let e = |t: &str| parse_frame(t).unwrap_err();
    assert!(matches!(e("not json"), ParseError::Json(_)));
    assert_eq!(e("{}"), ParseError::Field("stream"));
    assert_eq!(
        e(r#"{"stream":"trade_updates"}"#),
        ParseError::Field("data")
    );
    assert_eq!(
        e(r#"{"stream":"trade_updates","data":{}}"#),
        ParseError::Field("event")
    );
    assert_eq!(
        e(r#"{"stream":"trade_updates","data":{"event":"fill"}}"#),
        ParseError::Field("order")
    );
    let with =
        |field: &str| NEW.replacen("\"timestamp\":\"2026-10-06T13:30:00.123456789Z\"", field, 1);
    assert_eq!(
        e(&with("\"timestamp\":\"yesterday\"")),
        ParseError::Field("timestamp")
    );
    assert_eq!(
        e(&PARTIAL.replacen("\"price\":\"150.20\"", "\"price\":\"1e2\"", 1)),
        ParseError::Field("price")
    );
    assert_eq!(
        e(&PARTIAL.replacen(
            "\"qty\":\"40\",\"position_qty\"",
            "\"qty\":\"40.5\",\"position_qty\"",
            1
        )),
        ParseError::Field("qty")
    );
    assert_eq!(
        e(&NEW.replacen("\"symbol\":\"AAPL\",", "", 1)),
        ParseError::Field("symbol")
    );
    assert_eq!(
        e(&NEW.replacen("\"filled_qty\":\"0\"", "\"filled_qty\":\"0.5\"", 1)),
        ParseError::Field("filled_qty")
    );
    assert!(!e("{}").to_string().is_empty() && !e("x").to_string().is_empty());
}

fn tracker() -> Tracker {
    Tracker::new("tf1-")
}

fn events(out: &[Outcome]) -> Vec<(u64, Kind)> {
    out.iter()
        .filter_map(|o| match o {
            Outcome::Event(e) => Some((e.order.0, e.kind)),
            _ => None,
        })
        .collect()
}

#[test]
fn an_order_is_acknowledged_once_filled_in_pieces_and_the_pieces_add_up() {
    let mut t = tracker();
    let o = t.translate(&update(NEW));
    assert_eq!(events(&o), [(7, Kind::Ack)]);
    assert_eq!(t.ours("tf1-7"), Some(OrderId(7)));
    assert_eq!(t.ours("other-7"), None);
    assert_eq!(t.ours("tf1-x"), None);
    // The same acknowledgement again is a note.
    let again = t.translate(&update(NEW));
    assert!(
        events(&again).is_empty()
            && matches!(&again[0], Outcome::Note(n) if n.contains("already acknowledged")),
        "{again:?}"
    );
    // 40, then 60: the totals agree with what Alpaca says at each step.
    let p = t.translate(&update(PARTIAL));
    assert_eq!(
        events(&p),
        [(
            7,
            Kind::Fill {
                qty: 40,
                px: px("150.20")
            }
        )]
    );
    let f = t.translate(&update(FILL));
    assert_eq!(
        events(&f),
        [(
            7,
            Kind::Fill {
                qty: 60,
                px: px("150.22")
            }
        )]
    );
    if let Outcome::Event(e) = &f[0] {
        assert_eq!(e.ts, parse_time("2026-10-06T13:30:02.250000000Z").unwrap());
    }
}

#[test]
fn a_fill_delivered_twice_is_applied_once() {
    let mut t = tracker();
    t.translate(&update(NEW));
    assert_eq!(events(&t.translate(&update(PARTIAL))).len(), 1);
    let dup = t.translate(&update(PARTIAL));
    assert!(events(&dup).is_empty());
    assert!(
        matches!(&dup[0], Outcome::Note(n) if n.contains("already applied")),
        "{dup:?}"
    );
    // And the rest still goes through afterwards.
    assert_eq!(events(&t.translate(&update(FILL))).len(), 1);
}

#[test]
fn a_fill_that_does_not_add_up_is_reported_not_applied_and_can_be_retried() {
    let mut t = tracker();
    t.translate(&update(NEW));
    // The 60-share fill arrives first: Alpaca says 100 filled, we have 0 + 60.
    let early = t.translate(&update(FILL));
    assert!(events(&early).is_empty());
    assert!(
        matches!(&early[0], Outcome::Anomaly(m) if m.contains("filled 100 shares") && m.contains("not applied")),
        "{early:?}"
    );
    // The missing one arrives, then the one set aside, redelivered, now fits.
    assert_eq!(events(&t.translate(&update(PARTIAL))).len(), 1);
    assert_eq!(
        events(&t.translate(&update(FILL))).len(),
        1,
        "a fill set aside is not remembered as applied"
    );
    // More than the order asked for.
    let mut t = tracker();
    t.translate(&update(NEW));
    let over = PARTIAL.replace("\"qty\":\"100\",\"side\"", "\"qty\":\"30\",\"side\"");
    let o = t.translate(&update(&over));
    assert!(
        events(&o).is_empty()
            && matches!(&o[0], Outcome::Anomaly(m) if m.contains("more than it asked")),
        "{o:?}"
    );
    // A fill with no size, or none.
    let nosize = PARTIAL.replace("\"qty\":\"40\",\"position_qty\"", "\"position_qty\"");
    assert!(
        matches!(&tracker().translate(&update(&nosize))[0], Outcome::Anomaly(m) if m.contains("no size or price"))
    );
    let zero = PARTIAL.replace(
        "\"qty\":\"40\",\"position_qty\"",
        "\"qty\":\"0\",\"position_qty\"",
    );
    assert!(
        matches!(&tracker().translate(&update(&zero))[0], Outcome::Anomaly(m) if m.contains("no shares"))
    );
}

#[test]
fn a_fill_before_any_acknowledgement_acknowledges_first() {
    let mut t = tracker();
    let o = t.translate(&update(PARTIAL));
    assert_eq!(
        events(&o),
        [
            (7, Kind::Ack),
            (
                7,
                Kind::Fill {
                    qty: 40,
                    px: px("150.20")
                }
            )
        ]
    );
    assert!(
        events(&t.translate(&update(NEW))).is_empty(),
        "a late `new` adds nothing"
    );
}

#[test]
fn an_order_ends_cancelled_expired_or_rejected_and_only_a_rejection_needs_no_ack_first() {
    let mut t = tracker();
    t.translate(&update(NEW));
    assert_eq!(
        events(&t.translate(&update(CANCELED))),
        [(7, Kind::Close(OrderState::Cancelled))]
    );
    let expired = CANCELED.replace("\"event\":\"canceled\"", "\"event\":\"expired\"");
    assert_eq!(
        events(&tracker().translate(&update(&expired))),
        [(7, Kind::Ack), (7, Kind::Close(OrderState::Expired))]
    );
    assert_eq!(
        events(&tracker().translate(&update(REJECTED))),
        [(8, Kind::Close(OrderState::Rejected))]
    );
}

#[test]
fn events_that_change_nothing_are_noted_and_ones_nobody_knows_are_anomalies() {
    for name in [
        "done_for_day",
        "replaced",
        "stopped",
        "suspended",
        "calculated",
        "pending_cancel",
        "pending_replace",
        "order_cancel_rejected",
        "order_replace_rejected",
    ] {
        let text = NEW.replace("\"event\":\"new\"", &format!("\"event\":\"{name}\""));
        let o = tracker().translate(&update(&text));
        assert!(
            events(&o).is_empty() && matches!(&o[0], Outcome::Note(n) if n.contains(name)),
            "{name}: {o:?}"
        );
    }
    let o = tracker().translate(&update(
        &NEW.replace("\"event\":\"new\"", "\"event\":\"teleported\""),
    ));
    assert!(
        matches!(&o[0], Outcome::Anomaly(m) if m.contains("teleported")),
        "{o:?}"
    );
    // An order we did not place (another client id, or none).
    let foreign = NEW.replace("tf1-7", "someone-else-1");
    let o = tracker().translate(&update(&foreign));
    assert!(
        events(&o).is_empty()
            && matches!(&o[0], Outcome::Anomaly(m) if m.contains("did not place") && m.contains("someone-else-1")),
        "{o:?}"
    );
}

#[test]
fn the_protective_legs_of_a_bracket_are_known_from_its_response_and_reported_apart() {
    let submitted = parse_order(&Json::parse(BRACKET).unwrap()).unwrap();
    assert_eq!(
        (
            submitted.id.as_str(),
            submitted.status.as_str(),
            submitted.legs.len()
        ),
        ("904837e3-3b76-47ec-b432-046db621571b", "accepted", 2)
    );
    assert_eq!(submitted.legs[0].kind, "limit");
    assert_eq!(submitted.legs[1].kind, "stop");
    assert_eq!(
        submitted.legs[1].parent.as_deref(),
        Some("904837e3-3b76-47ec-b432-046db621571b")
    );
    let mut t = tracker();
    t.register(OrderId(9), &submitted);
    let o = t.translate(&update(LEG_FILL));
    assert_eq!(
        events(&o),
        [(
            9,
            Kind::LegFill {
                leg: Leg::Stop,
                qty: 50,
                px: px("294.90")
            }
        )]
    );
    // Delivered again: once only.
    let dup = t.translate(&update(LEG_FILL));
    assert!(
        events(&dup).is_empty()
            && matches!(&dup[0], Outcome::Note(n) if n.contains("already applied"))
    );
    // The other leg being cancelled is a note.
    let cancel = LEG_FILL
        .replace("\"event\":\"fill\"", "\"event\":\"canceled\"")
        .replace("leg-stop-0002", "leg-target-0001");
    let o = t.translate(&update(&cancel));
    assert!(
        events(&o).is_empty()
            && matches!(&o[0], Outcome::Note(n) if n.contains("Target leg of order 9")),
        "{o:?}"
    );
    // A leg that names its parent is recognised even if its own id was not in the response.
    let mut t = tracker();
    t.register(OrderId(9), &submitted);
    let stranger = LEG_FILL.replace("leg-stop-0002", "leg-new-9999");
    assert_eq!(
        events(&t.translate(&update(&stranger))),
        [(
            9,
            Kind::LegFill {
                leg: Leg::Stop,
                qty: 50,
                px: px("294.90")
            }
        )]
    );
    // Without the registration it is an order we did not place.
    assert!(matches!(
        &tracker().translate(&update(LEG_FILL))[0],
        Outcome::Anomaly(_)
    ));
    // A leg fill with no size.
    let nosize = LEG_FILL.replace("\"qty\":\"50\",\"position_qty\"", "\"position_qty\"");
    let mut t = tracker();
    t.register(OrderId(9), &submitted);
    assert!(matches!(
        &t.translate(&update(&nosize))[0],
        Outcome::Anomaly(_)
    ));
}

#[test]
fn after_a_restart_what_the_ledger_already_has_is_not_applied_again() {
    let mut t = tracker();
    t.resume(OrderId(7), true, 40);
    // The 40-share fill is delivered again by the stream's replay: it no longer fits.
    let o = t.translate(&update(PARTIAL));
    assert!(
        events(&o).is_empty() && matches!(&o[0], Outcome::Anomaly(_)),
        "{o:?}"
    );
    // The 60 that is new fits, and there is no second acknowledgement.
    assert_eq!(
        events(&t.translate(&update(FILL))),
        [(
            7,
            Kind::Fill {
                qty: 60,
                px: px("150.22")
            }
        )]
    );
}

// ------------------------------------------------------------------------ client

use std::collections::VecDeque;

use tf_ledger::{Journal, MemStore};
use tf_risk::Limits;
use tf_strategy::lifecycle::Decision;

use crate::client::*;
use crate::drive::{Applied, apply};

struct Script {
    replies: VecDeque<Result<HttpResponse, TransportError>>,
    sent: Vec<HttpRequest>,
}

impl Transport for Script {
    fn send(&mut self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.sent.push(req.clone());
        self.replies
            .pop_front()
            .expect("the script ran out of replies")
    }
}

fn reply(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        body: body.to_owned(),
        retry_after: None,
    })
}

fn alpaca(replies: Vec<Result<HttpResponse, TransportError>>) -> Alpaca<Script> {
    Alpaca::new(
        Config::new("KEYID123", "SECRET456", "tf1-"),
        Script {
            replies: replies.into(),
            sent: vec![],
        },
    )
}

fn buy_100() -> Intent {
    intent(Side::Buy, Purpose::Open, limit("150.25"), None, Tif::Day)
}

fn order_json(client_id: &str, status: &str) -> String {
    format!(
        r#"{{"id":"broker-{client_id}","client_order_id":"{client_id}","symbol":"AAPL","qty":"100","status":"{status}","filled_qty":"0","filled_avg_price":null,"type":"limit"}}"#
    )
}

#[test]
fn an_accepted_order_is_returned_and_sent_with_the_keys_in_headers_not_the_body() {
    let mut a = alpaca(vec![reply(200, &order_json("tf1-7", "accepted"))]);
    let r = a.submit(&buy_100(), OrderId(7), "AAPL").unwrap();
    let Submission::Accepted(o) = r else {
        panic!("{r:?}")
    };
    assert_eq!(
        (o.id.as_str(), o.client_order_id.as_str(), o.status.as_str()),
        ("broker-tf1-7", "tf1-7", "accepted")
    );
    let sent = &a.transport_sent()[0];
    assert_eq!(
        (sent.method, sent.path.as_str()),
        (Method::Post, "/v2/orders")
    );
    assert!(
        sent.headers
            .contains(&("APCA-API-KEY-ID".to_owned(), "KEYID123".to_owned()))
    );
    assert!(
        sent.headers
            .contains(&("APCA-API-SECRET-KEY".to_owned(), "SECRET456".to_owned()))
    );
    assert!(
        !sent.body.as_deref().unwrap().contains("SECRET456")
            && !sent.body.as_deref().unwrap().contains("KEYID123")
    );
    assert_eq!(
        Json::parse(sent.body.as_deref().unwrap())
            .unwrap()
            .str_at("client_order_id"),
        Some("tf1-7")
    );
    // The keys do not show up when the request or the configuration is printed.
    let shown = format!("{sent:?} {:?}", a.config());
    assert!(
        !shown.contains("SECRET456") && !shown.contains("KEYID123"),
        "{shown}"
    );
    assert!(
        shown.contains("APCA-API-SECRET-KEY") && shown.contains("tf1-"),
        "names, not values: {shown}"
    );
}

impl Alpaca<Script> {
    fn transport_sent(&self) -> &[HttpRequest] {
        &self.transport().sent
    }
}

#[test]
fn what_alpaca_refuses_is_a_refusal_with_its_own_words_and_a_busy_signal_is_not_a_refusal() {
    let mut a = alpaca(vec![
        reply(403, BUYING_POWER),
        reply(422, VALIDATION),
        reply(400, "<html>bad gateway</html>"),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
            retry_after: Some(3),
        }),
        reply(429, ""),
    ]);
    assert_eq!(
        a.submit(&buy_100(), OrderId(1), "AAPL").unwrap(),
        Submission::Refused {
            status: 403,
            message: "insufficient buying power".into()
        }
    );
    assert_eq!(
        a.submit(&buy_100(), OrderId(2), "AAPL").unwrap(),
        Submission::Refused {
            status: 422,
            message: "limit price must be a multiple of 0.01".into()
        }
    );
    let r = a.submit(&buy_100(), OrderId(3), "AAPL").unwrap();
    assert!(
        matches!(&r, Submission::Refused { status: 400, message } if message.starts_with("<html>")),
        "{r:?}"
    );
    assert_eq!(
        a.submit(&buy_100(), OrderId(4), "AAPL").unwrap(),
        Submission::RateLimited { retry_after: 3 }
    );
    assert_eq!(
        a.submit(&buy_100(), OrderId(5), "AAPL").unwrap(),
        Submission::RateLimited { retry_after: 1 }
    );
    assert_eq!(a.transport_sent().len(), 5, "none of these was sent twice");
}

#[test]
fn an_intent_that_cannot_be_sent_never_reaches_the_transport() {
    let mut a = alpaca(vec![]);
    let mut bad = buy_100();
    bad.qty = 0;
    assert_eq!(
        a.submit(&bad, OrderId(1), "AAPL"),
        Err(SubmitError::Request(RequestError::ZeroQty))
    );
    assert!(a.transport_sent().is_empty());
    let mut a = alpaca(vec![reply(200, "{\"id\":1}")]);
    let e = a.submit(&buy_100(), OrderId(2), "AAPL").unwrap_err();
    assert!(
        matches!(e, SubmitError::Answer(_)) && e.to_string().contains("not an order"),
        "{e}"
    );
    let mut a = alpaca(vec![reply(200, "not json")]);
    assert!(matches!(
        a.submit(&buy_100(), OrderId(3), "AAPL"),
        Err(SubmitError::Answer(_))
    ));
    assert!(
        SubmitError::Request(RequestError::BadPrice)
            .to_string()
            .contains("price")
    );
}

#[test]
fn an_order_with_no_answer_is_looked_up_by_its_name_before_anything_else_is_done() {
    // The first request times out; the order is there.
    let mut a = alpaca(vec![
        Err(TransportError::Timeout),
        reply(200, &order_json("tf1-7", "new")),
    ]);
    let r = a.submit(&buy_100(), OrderId(7), "AAPL").unwrap();
    assert!(
        matches!(r, Submission::Accepted(ref o) if o.status == "new"),
        "{r:?}"
    );
    let sent = a.transport_sent();
    assert_eq!(sent.len(), 2, "no second order was sent");
    assert_eq!(
        (sent[1].method, sent[1].path.as_str()),
        (
            Method::Get,
            "/v2/orders:by_client_order_id?client_order_id=tf1-7"
        )
    );
    // A server error, any of them, is the same as no answer.
    for status in [500, 502, 503, 504, 599] {
        let mut a = alpaca(vec![
            reply(status, "unavailable"),
            reply(200, &order_json("tf1-7", "new")),
        ]);
        let r = a.submit(&buy_100(), OrderId(7), "AAPL").unwrap();
        assert!(matches!(r, Submission::Accepted(_)), "{status}");
        assert_eq!(a.transport_sent().len(), 2, "{status}");
    }
    // A client error is an answer.
    let mut a = alpaca(vec![reply(499, "")]);
    let r = a.submit(&buy_100(), OrderId(7), "AAPL").unwrap();
    assert!(matches!(r, Submission::Refused { status: 499, .. }));
    // An order found after a timeout has its protective legs learned all the same.
    let mut a = alpaca(vec![Err(TransportError::Timeout), reply(200, BRACKET)]);
    let r = a.submit(&buy_100(), OrderId(9), "MSFT").unwrap();
    assert!(matches!(r, Submission::Accepted(_)), "{r:?}");
    let Frame::Update(u) = parse_frame(LEG_FILL).unwrap() else {
        panic!()
    };
    assert_eq!(
        events(&a.tracker.translate(&u)),
        [(
            9,
            Kind::LegFill {
                leg: Leg::Stop,
                qty: 50,
                px: px("294.90")
            }
        )]
    );
}

#[test]
fn an_order_that_is_not_there_is_sent_again_under_the_same_name_and_never_a_new_one() {
    let mut a = alpaca(vec![
        Err(TransportError::Connection("reset".into())),
        reply(404, "{}"),
        reply(200, &order_json("tf1-7", "accepted")),
    ]);
    let r = a.submit(&buy_100(), OrderId(7), "AAPL").unwrap();
    assert!(matches!(r, Submission::Accepted(_)), "{r:?}");
    let ids: Vec<String> = a
        .transport_sent()
        .iter()
        .filter(|r| r.method == Method::Post)
        .map(|r| {
            Json::parse(r.body.as_deref().unwrap())
                .unwrap()
                .str_at("client_order_id")
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(ids, ["tf1-7", "tf1-7"]);
    // The second try is refused as a duplicate: the first one had arrived after all.
    let mut a = alpaca(vec![
        Err(TransportError::Timeout),
        reply(404, "{}"),
        reply(422, r#"{"message":"client_order_id must be unique"}"#),
        reply(200, &order_json("tf1-7", "new")),
    ]);
    let r = a.submit(&buy_100(), OrderId(7), "AAPL").unwrap();
    assert!(
        matches!(r, Submission::Accepted(ref o) if o.id == "broker-tf1-7"),
        "{r:?}"
    );
}

#[test]
fn an_order_that_stays_unknown_is_reported_unknown_and_not_closed() {
    let mut a = alpaca(vec![
        Err(TransportError::Timeout),
        Err(TransportError::Timeout),
    ]);
    assert_eq!(
        a.submit(&buy_100(), OrderId(7), "AAPL").unwrap(),
        Submission::Unknown,
        "the lookup itself got no answer"
    );
    assert_eq!(a.transport_sent().len(), 2);
    let mut a = alpaca(vec![
        reply(502, ""),
        reply(404, "{}"),
        Err(TransportError::Timeout),
        reply(404, "{}"),
    ]);
    assert_eq!(
        a.submit(&buy_100(), OrderId(7), "AAPL").unwrap(),
        Submission::Unknown,
        "twice sent, never found"
    );
    assert_eq!(a.transport_sent().len(), 4);
    // A lookup that answers with something odd also leaves it unknown.
    let mut a = alpaca(vec![Err(TransportError::Timeout), reply(500, "")]);
    assert_eq!(
        a.submit(&buy_100(), OrderId(7), "AAPL").unwrap(),
        Submission::Unknown
    );
}

#[test]
fn looking_an_order_up_and_cancelling_one() {
    let mut a = alpaca(vec![
        reply(404, ""),
        reply(500, ""),
        Err(TransportError::Timeout),
        reply(200, "oops"),
        reply(200, &order_json("tf1-7", "filled")),
    ]);
    assert_eq!(a.find(OrderId(7)), Ok(None));
    assert_eq!(a.find(OrderId(7)), Err(FindError::Status(500)));
    assert_eq!(a.find(OrderId(7)), Err(FindError::NoAnswer));
    assert!(matches!(a.find(OrderId(7)), Err(FindError::Unreadable(_))));
    assert_eq!(a.find(OrderId(7)).unwrap().unwrap().status, "filled");
    let mut a = alpaca(vec![
        reply(204, ""),
        reply(200, ""),
        reply(422, ""),
        reply(404, ""),
        reply(500, ""),
        Err(TransportError::Timeout),
    ]);
    assert_eq!(
        [
            a.cancel("x"),
            a.cancel("x"),
            a.cancel("x"),
            a.cancel("x"),
            a.cancel("x"),
            a.cancel("x")
        ],
        [
            Cancel::Requested,
            Cancel::Requested,
            Cancel::Finished,
            Cancel::Finished,
            Cancel::Unknown,
            Cancel::Unknown
        ]
    );
    let s = a.transport_sent();
    assert_eq!(
        (s[0].method, s[0].path.as_str()),
        (Method::Delete, "/v2/orders/x")
    );
}

#[test]
fn the_stream_is_opened_with_the_documented_messages() {
    let c = Config::new("KEY\"ID", "SEC\\RET", "p-");
    let auth = Json::parse(&c.auth_message()).unwrap();
    assert_eq!(
        (
            auth.str_at("action"),
            auth.str_at("key"),
            auth.str_at("secret")
        ),
        (Some("auth"), Some("KEY\"ID"), Some("SEC\\RET"))
    );
    let listen = Json::parse(LISTEN_MESSAGE).unwrap();
    assert_eq!(listen.str_at("action"), Some("listen"));
    let Some(Json::Arr(streams)) = listen.obj_at("data").unwrap().get("streams") else {
        panic!()
    };
    assert_eq!(streams, &[Json::Str("trade_updates".into())]);
    assert!(PAPER_URL.ends_with("paper-api.alpaca.markets") && PAPER_STREAM.ends_with("/stream"));
}

// ------------------------------------------------------------------- the ledger

/// An order the gateway will take: $4,900 of notional, with a stop.
fn small() -> Intent {
    intent(
        Side::Buy,
        Purpose::Open,
        limit("49.00"),
        protect("45.00", None, None),
        Tif::Day,
    )
}

fn journal() -> Journal<MemStore> {
    let limits = Limits::new(
        5_000 * D as u128,
        1_000,
        20_000 * D as u128,
        400 * D as u128,
        6,
        10_000_000_000,
    )
    .unwrap();
    Journal::open(MemStore::from_records(vec![]), limits, 1)
        .unwrap()
        .0
}

#[test]
fn from_the_gateway_through_alpaca_and_the_stream_into_the_ledger_and_back_from_it() {
    let mut j = journal();
    let i = small();
    let d = j.decide(&i, 1).unwrap();
    let Decision::Accepted(order) = d else {
        panic!("{d:?}")
    };
    let id = format!("tf1-{}", order.0);
    let mut a = alpaca(vec![reply(200, &order_json(&id, "accepted"))]);
    let Submission::Accepted(_) = a.submit(&i, order, "AAPL").unwrap() else {
        panic!()
    };
    // Alpaca's stream says: new, 40 filled, 60 filled (client id rewritten to ours).
    let frames = [NEW, PARTIAL, FILL].map(|f| f.replace("tf1-7", &id));
    for f in &frames {
        let Frame::Update(u) = parse_frame(f).unwrap() else {
            panic!()
        };
        for outcome in a.tracker.translate(&u) {
            let Outcome::Event(e) = outcome else { continue };
            assert_eq!(apply(&mut j, &e).unwrap(), Applied::Recorded, "{e:?}");
        }
    }
    let o = j.order(order).unwrap();
    assert_eq!((o.state(), o.filled_qty()), (OrderState::Filled, 100));
    let snap = j.snapshot();
    assert_eq!(snap.positions.len(), 1);
    assert_eq!(
        (snap.positions[0].2, snap.positions[0].0),
        (100, 1),
        "100 shares held by strategy 1"
    );
    // A restart finds the same thing, and the tracker resumed from it does not re-apply a replay.
    let (back, _) =
        Journal::open_recorded(MemStore::from_records(j.store().records().to_vec())).unwrap();
    assert_eq!(back.order(order).unwrap().filled_qty(), 100);
    let mut t = Tracker::new("tf1-");
    t.resume(order, true, back.order(order).unwrap().filled_qty());
    let Frame::Update(replayed) = parse_frame(&frames[1]).unwrap() else {
        panic!()
    };
    assert!(
        t.translate(&replayed)
            .iter()
            .all(|o| !matches!(o, Outcome::Event(_)))
    );
}

#[test]
fn a_rejection_by_alpaca_closes_the_order_in_the_ledger() {
    // A rejection arrives before any acknowledgement. Acknowledging first would leave the order
    // accepted, and the ledger (rightly) cannot turn an accepted order into a rejected one.
    let mut j = journal();
    let Decision::Accepted(order) = j.decide(&small(), 1).unwrap() else {
        panic!()
    };
    let id = format!("tf1-{}", order.0);
    let mut a = alpaca(vec![reply(200, &order_json(&id, "accepted"))]);
    a.submit(&small(), order, "AAPL").unwrap();
    let Frame::Update(u) = parse_frame(&REJECTED.replace("tf1-8", &id)).unwrap() else {
        panic!()
    };
    let mut seen = 0;
    for outcome in a.tracker.translate(&u) {
        let Outcome::Event(e) = outcome else { continue };
        assert_eq!(apply(&mut j, &e).unwrap(), Applied::Recorded, "{e:?}");
        seen += 1;
    }
    assert_eq!(seen, 1);
    assert_eq!(j.order(order).unwrap().state(), OrderState::Rejected);
    assert!(j.open_orders().is_empty());
}

#[test]
fn what_the_ledger_cannot_take_is_said_not_forced() {
    let mut j = journal();
    let ghost = BrokerEvent {
        order: OrderId(99),
        ts: 1,
        kind: Kind::Ack,
    };
    assert!(
        matches!(apply(&mut j, &ghost).unwrap(), Applied::Refused(m) if m.contains("no such order 99")),
        "an order the gateway never saw"
    );
    let Decision::Accepted(order) = j.decide(&small(), 1).unwrap() else {
        panic!()
    };
    let before = j.records();
    // Filling before it was acknowledged, closing as filled, acknowledging twice.
    let early = BrokerEvent {
        order,
        ts: 2,
        kind: Kind::Fill {
            qty: 10,
            px: px("150.00"),
        },
    };
    assert!(matches!(
        apply(&mut j, &early).unwrap(),
        Applied::Refused(_)
    ));
    let bad_close = BrokerEvent {
        order,
        ts: 2,
        kind: Kind::Close(OrderState::Filled),
    };
    assert!(
        matches!(apply(&mut j, &bad_close).unwrap(), Applied::Refused(m) if m.contains("cannot be closed"))
    );
    assert_eq!(j.records(), before, "nothing was written by any of them");
    let ack = BrokerEvent {
        order,
        ts: 2,
        kind: Kind::Ack,
    };
    assert_eq!(apply(&mut j, &ack).unwrap(), Applied::Recorded);
    assert!(matches!(apply(&mut j, &ack).unwrap(), Applied::Refused(_)));
    // A protective leg's fill has nowhere to go yet.
    let leg = BrokerEvent {
        order,
        ts: 3,
        kind: Kind::LegFill {
            leg: Leg::Stop,
            qty: 10,
            px: px("1.00"),
        },
    };
    let n = j.records();
    assert!(
        matches!(apply(&mut j, &leg).unwrap(), Applied::NotRecorded(m) if m.contains("protective leg"))
    );
    assert_eq!(j.records(), n);
}

// ---- the Broker interface ----

use crate::broker::AlpacaBroker;
use tf_strategy::broker::{Broker, CancelOutcome, Submission as Placed, check_events};

fn broker_over(replies: Vec<Result<HttpResponse, TransportError>>) -> AlpacaBroker<Script> {
    AlpacaBroker::new(alpaca(replies), vec!["AAPL".to_owned(), String::new()])
}

#[test]
fn placements_end_the_way_the_broker_interface_says() {
    let mut b = broker_over(vec![
        reply(200, &order_json("tf1-1", "accepted")),
        reply(422, r#"{"message":"insufficient buying power"}"#),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
            retry_after: Some(3),
        }),
        Err(TransportError::Timeout),
        Err(TransportError::Timeout),
        reply(200, "not an order"),
    ]);
    assert_eq!(b.place(&buy_100(), OrderId(1)), Placed::Accepted);
    assert_eq!(
        b.place(&buy_100(), OrderId(2)),
        Placed::Refused {
            code: 422,
            message: "insufficient buying power".into()
        }
    );
    assert_eq!(
        b.place(&buy_100(), OrderId(3)),
        Placed::RateLimited {
            retry_after: 3_000_000_000
        }
    );
    assert_eq!(b.place(&buy_100(), OrderId(4)), Placed::Unknown);
    assert_eq!(b.unsure(), [OrderId(4)]);
    // An answer that is not an order might still be an order: unknown, not refused.
    assert_eq!(b.place(&buy_100(), OrderId(5)), Placed::Unknown);
    // No symbol for the instrument (id 1 has none, 9 is outside the table): refused without a request.
    let sent = b.alpaca().transport_sent().len();
    let mut i = buy_100();
    i.instrument = 1;
    assert!(
        matches!(b.place(&i, OrderId(6)), Placed::Refused { code: 0, message } if message.contains("no symbol"))
    );
    i.instrument = 9;
    assert!(matches!(
        b.place(&i, OrderId(7)),
        Placed::Refused { code: 0, .. }
    ));
    assert_eq!(b.alpaca().transport_sent().len(), sent);
    // An intent that cannot be written as an Alpaca order is refused with the reason.
    let mut z = buy_100();
    z.qty = 0;
    assert!(matches!(
        b.place(&z, OrderId(8)),
        Placed::Refused { code: 0, .. }
    ));
}

#[test]
fn stream_frames_become_the_events_the_broker_interface_returns() {
    let mut b = broker_over(vec![reply(200, &order_json("tf1-7", "accepted"))]);
    assert_eq!(b.place(&buy_100(), OrderId(7)), Placed::Accepted);
    for f in [NEW, PARTIAL, FILL] {
        b.on_stream_frame(f).unwrap();
    }
    let ev = b.take_events();
    assert_eq!(ev.iter().map(|e| e.kind).collect::<Vec<_>>().len(), 3);
    assert!(
        matches!(ev[0].kind, Kind::Ack)
            && matches!(ev[1].kind, Kind::Fill { qty: 40, .. })
            && matches!(ev[2].kind, Kind::Fill { qty: 60, .. })
    );
    check_events(&ev, |o| (o == OrderId(7)).then_some(100)).unwrap();
    assert!(b.take_events().is_empty());
    // Frames that are not updates come back to the caller; a replay of one applied is a note; an
    // order we did not place is an anomaly; none of these makes an event.
    assert!(matches!(
        b.on_stream_frame(include_str!("../fixtures/stream_authorized.json"))
            .unwrap(),
        Frame::Authorized
    ));
    b.on_stream_frame(PARTIAL).unwrap();
    b.on_stream_frame(&NEW.replace("tf1-7", "someone-else-1"))
        .unwrap();
    assert!(b.take_events().is_empty());
    assert_eq!((b.notes().len(), b.anomalies().len()), (1, 1));
    assert!(b.on_stream_frame("{not json").is_err());
}

#[test]
fn a_placement_that_got_no_answer_is_settled_by_the_stream() {
    let mut b = broker_over(vec![
        Err(TransportError::Timeout),
        Err(TransportError::Timeout),
    ]);
    assert_eq!(b.place(&buy_100(), OrderId(7)), Placed::Unknown);
    assert_eq!(
        b.cancel_order(OrderId(7), 0),
        CancelOutcome::Unknown,
        "it may exist, and has no id we know"
    );
    assert_eq!(b.unsure(), [OrderId(7)]);
    // The order did arrive: its first stream message settles the doubt.
    b.on_stream_frame(NEW).unwrap();
    assert!(b.unsure().is_empty());
    assert_eq!(b.take_events().len(), 1);
}

#[test]
fn cancelling_goes_to_alpaca_by_its_own_id() {
    let mut b = broker_over(vec![
        reply(200, &order_json("tf1-1", "accepted")),
        reply(204, ""),
        reply(404, ""),
        reply(500, ""),
    ]);
    b.place(&buy_100(), OrderId(1));
    assert_eq!(b.cancel_order(OrderId(1), 0), CancelOutcome::Requested);
    assert_eq!(b.cancel_order(OrderId(1), 0), CancelOutcome::Finished);
    assert_eq!(b.cancel_order(OrderId(1), 0), CancelOutcome::Unknown);
    let s = b.alpaca().transport_sent();
    assert_eq!(
        (s[1].method, s[1].path.as_str()),
        (Method::Delete, "/v2/orders/broker-tf1-1")
    );
    // Never placed: nothing to cancel, and nothing is sent.
    assert_eq!(b.cancel_order(OrderId(99), 0), CancelOutcome::Finished);
    assert_eq!(b.alpaca().transport_sent().len(), 4);
}

#[test]
fn a_rejection_on_the_stream_ends_the_order_without_an_acknowledgement_and_passes_the_checker() {
    let mut b = broker_over(vec![reply(200, &order_json("tf1-8", "accepted"))]);
    b.place(&buy_100(), OrderId(8));
    b.on_stream_frame(REJECTED).unwrap();
    let ev = b.take_events();
    assert_eq!(ev.len(), 1);
    check_events(&ev, |_| Some(100)).unwrap();
    // The market side of the interface is not Alpaca's business.
    b.observe(&tf_core::Event::Quote(tf_core::Quote {
        hdr: tf_core::Header {
            ts_event: 0,
            ts_recv: 0,
            seq: 0,
            instrument: 0,
            provider: tf_core::ProviderId::Synthetic,
        },
        bid_px: tf_core::Px::from_cents(1),
        ask_px: tf_core::Px::from_cents(2),
        bid_sz: 1,
        ask_sz: 1,
    }));
    b.close_day(0);
    assert!(b.take_events().is_empty());
}

#[test]
fn the_simulated_broker_drives_the_ledger_through_every_outcome() {
    use tf_core::{Event, Header, ProviderId, Quote};
    use tf_strategy::sim::{FaultPlan, SimBroker, SimConfig};

    const SEC: u64 = 1_000_000_000;
    let mut j = journal();
    let mut sim = SimBroker::new(
        SimConfig {
            latency_ns: 0,
            borrow_bps_per_year: 0,
        },
        1,
    )
    .with_faults(FaultPlan {
        refuse_every: 9,
        rate_limit_every: 7,
        rate_limit_retry_ns: SEC,
        unknown_every: 5,
        venue_reject_every: 4,
        ..FaultPlan::default()
    });
    let quote = |ts: u64| {
        Event::Quote(Quote {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument: 0,
                provider: ProviderId::Synthetic,
            },
            bid_px: Px::from_cents(4_890),
            ask_px: Px::from_cents(4_900),
            bid_sz: 1_000,
            ask_sz: 1_000,
        })
    };
    let (mut unknown, mut heard, mut closed_by_us, mut recorded) =
        (Vec::new(), std::collections::BTreeSet::new(), 0, 0);
    let feed = |j: &mut Journal<MemStore>,
                sim: &mut SimBroker,
                heard: &mut std::collections::BTreeSet<OrderId>,
                recorded: &mut u32| {
        for e in sim.take_events() {
            heard.insert(e.order);
            assert_eq!(apply(j, &e).unwrap(), Applied::Recorded, "{e:?}");
            *recorded += 1;
        }
    };
    for n in 0..40u64 {
        let ts = SEC + n * 11 * SEC;
        sim.observe(&quote(ts));
        let mut i = small();
        i.qty = 10;
        i.id.seq = n;
        i.ts = ts;
        let Decision::Accepted(order) = j.decide(&i, ts).unwrap() else {
            panic!("gateway refused order {n}")
        };
        match sim.place(&i, order) {
            Placed::Accepted => {}
            Placed::Refused { .. } | Placed::RateLimited { .. } => {
                // It did not happen: the ledger is told so, which frees what the gateway held for it.
                j.close(order, OrderState::Rejected, ts).unwrap();
                closed_by_us += 1;
            }
            Placed::Unknown => unknown.push(order),
        }
        sim.observe(&quote(ts + SEC));
        feed(&mut j, &mut sim, &mut heard, &mut recorded);
    }
    sim.close_day(500 * SEC);
    feed(&mut j, &mut sim, &mut heard, &mut recorded);
    assert!(
        closed_by_us > 8 && unknown.len() > 6 && recorded > 40,
        "{closed_by_us} {} {recorded}",
        unknown.len()
    );
    // Every order the gateway still has working is one whose placement got no answer and that was
    // never heard of again: it stays working until something settles it, and is never closed on a guess.
    let silent: std::collections::BTreeSet<OrderId> = unknown
        .iter()
        .copied()
        .filter(|o| !heard.contains(o))
        .collect();
    assert!(!silent.is_empty() && silent.len() < unknown.len());
    let open: std::collections::BTreeSet<OrderId> = j.open_orders().iter().map(|o| o.id).collect();
    assert_eq!(open, silent);
    // What the gateway holds is what the broker holds.
    let snap = j.snapshot();
    let held: i64 = snap.positions.iter().map(|p| p.2).sum();
    assert_eq!(held, sim.position(0));
    assert!(held > 0);
    // Some orders were rejected by the venue after being accepted: the ledger has them as rejected.
    assert!(
        j.orders()
            .filter(|o| o.state() == OrderState::Rejected)
            .count() as u32
            > closed_by_us as u32
    );
}
