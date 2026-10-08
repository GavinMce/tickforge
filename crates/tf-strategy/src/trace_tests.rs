use crate::trace::{Trace, TraceError, parse_all, render_all};

fn sample() -> Vec<(u16, Trace)> {
    let mut a = Trace::new(1_777_663_800_000_000_000, "rank")
        .with("close", 1_777_665_600_000_000_000u64)
        .with("names", 3)
        .with_columns(&["rank", "instrument", "status"]);
    a.push_row(vec!["1".into(), "4".into(), "entered".into()]);
    a.push_row(vec!["".into(), "9".into(), "skipped:halted".into()]);
    let b = Trace::new(7, "note").with("only", "a head");
    let mut c = Trace::new(8, "draw").with_columns(&["k"]);
    c.push_row(vec!["0".into()]);
    vec![(1, a), (2, b), (1, c)]
}

#[test]
fn traces_read_back_exactly_in_the_order_they_were_given() {
    let all = sample();
    let text = render_all(&all);
    assert!(text.starts_with(
        "traces v1\ntrace\t1\t1777663800000000000\trank\nhead\tclose\t1777665600000000000\n"
    ));
    assert!(text.contains(
        "cols\trank\tinstrument\tstatus\nrow\t1\t4\tentered\nrow\t\t9\tskipped:halted\n"
    ));
    assert_eq!(parse_all(&text).unwrap(), all);
    // Nothing at all is a day with nothing to trace.
    assert_eq!(parse_all(&render_all(&[])).unwrap(), vec![]);
    // The same traces always give the same text.
    assert_eq!(render_all(&all), text);
}

#[test]
fn tabs_line_breaks_and_backslashes_survive() {
    let mut t = Trace::new(1, "odd kind")
        .with("a\tb", "x\ny\r\\z\\")
        .with_columns(&["c\\d", "e\tf"]);
    t.push_row(vec!["1\t2".into(), "3\n4".into()]);
    t.push_row(vec!["\\t".into(), "\\".into()]);
    let text = render_all(&[(5, t.clone())]);
    // The file has the lines it should: no value broke one.
    assert_eq!(text.lines().count(), 1 + 1 + 1 + 1 + 2 + 1);
    assert_eq!(parse_all(&text).unwrap(), vec![(5, t)]);
}

#[test]
fn every_kind_of_damage_is_noticed() {
    let text = render_all(&sample());
    let seal = |body: &str| {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in body.as_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{body}end {h:016x}\n")
    };
    let body = &text[..text.rfind("end ").unwrap()];
    for (bad, what) in [
        (text.replacen("entered", "entereD", 1), "checksum"),
        (text.replace("end ", "ent "), "cut short"),
        (text[..text.len() - 1].to_owned(), "after `end`"),
        (format!("{text}row\tx\n"), "after `end`"),
        (text.replace("end ", "end zz"), "hexadecimal"),
        (seal(&body.replacen("v1", "v2", 1)), "not `traces v1`"),
        (seal(&format!("{body}surprise\n")), "does not know"),
        (
            seal(&body.replacen("trace\t1\t", "trace\tx\t", 1)),
            "strategy number",
        ),
        (
            seal(&body.replacen("\t1777663800000000000\t", "\tlater\t", 1)),
            "not a time",
        ),
        (seal("traces v1\nhead\tk\tv\n"), "before any `trace`"),
        (seal("traces v1\ncols\ta\n"), "before any `trace`"),
        (seal("traces v1\nrow\ta\n"), "before any `trace`"),
        (seal(&format!("{body}row\ta\tb\tc\n")), "cells under"),
        (seal(&format!("{body}cols\ta\n")), "twice, or after a row"),
        (
            seal("traces v1\ntrace\t1\t1\tk\ncols\ta\ncols\tb\n"),
            "twice, or after a row",
        ),
        (seal("traces v1\ntrace\t1\t1\tk\nhead\tx\t\\q\n"), "escape"),
        (seal("traces v1\ntrace\t1\t1\tk\nhead\tx\t\\\n"), "escape"),
        (seal("traces v1\ntrace\t1\t1\tk\nrow\t1\n"), "cells under 0"),
    ] {
        let TraceError(e) = parse_all(&bad).unwrap_err();
        assert!(e.contains(what), "{what}: {e}");
    }
}

#[test]
fn instrument_numbers_become_symbols_and_nothing_else_changes() {
    let mut t = Trace::new(1, "rank")
        .with("n", 2)
        .with_columns(&["rank", "instrument", "status"]);
    t.push_row(vec!["1".into(), "4".into(), "entered".into()]);
    t.push_row(vec!["".into(), "x".into(), "odd".into()]);
    t.resolve_symbols(|id| format!("S{id:02}"));
    assert_eq!(t.columns, ["rank", "symbol", "status"]);
    assert_eq!(t.column("symbol").unwrap(), ["S04", "x"]);
    assert_eq!(t.value("n"), Some("2"));
    assert_eq!(t.value("missing"), None);
    assert_eq!(t.column("instrument"), None);
    // A trace with no such column is left alone.
    let mut u = Trace::new(1, "k").with_columns(&["a"]);
    u.push_row(vec!["7".into()]);
    let before = u.clone();
    u.resolve_symbols(|_| "never".into());
    assert_eq!(u, before);
}

#[test]
fn a_trace_error_says_what_it_is() {
    let e = parse_all("").unwrap_err();
    assert_eq!(e.to_string(), "traces: cut short: no `end`");
}

#[test]
fn a_trailer_that_is_gone_is_cut_short_not_misread_from_a_value_that_says_end() {
    let t = Trace::new(1, "k")
        .with("note", "the end of it")
        .with("x", "y");
    let text = render_all(&[(1, t)]);
    let cut = &text[..text.rfind("end ").unwrap()];
    assert!(cut.contains("the end of it"));
    let TraceError(e) = parse_all(cut).unwrap_err();
    assert!(e.contains("cut short"), "{e}");
}
