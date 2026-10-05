use tf_core::ProviderId::{Alpaca, Databento};

use super::*;

fn d(n: u32) -> Date {
    Date::new(n).unwrap()
}

/// - 0 `FOO`, renamed `BAR` on 2026-03-02 (and split 2-for-1 on 2026-04-15: no record).
/// - 1 `BAZ`, acquired: last trading day 2026-06-30.
/// - 2 a different company that reuses `BAZ` from 2026-07-01.
///
/// Databento reassigns 0's key on 2026-06-01 and reuses the old key for 1.
fn world() -> SecurityMaster {
    let mut b = Builder::new();
    let foo = b.add_security(d(20200102));
    b.add_symbol(foo, "FOO", d(20200102), Some(d(20260301)))
        .unwrap();
    b.add_symbol(foo, "BAR", d(20260302), None).unwrap();
    let old = b.add_security(d(20210101));
    b.delist(old, d(20260630)).unwrap();
    b.add_symbol(old, "BAZ", d(20210101), Some(d(20260630)))
        .unwrap();
    let newco = b.add_security(d(20260701));
    b.add_symbol(newco, "BAZ", d(20260701), None).unwrap();

    b.map_native(Databento, 1111, foo, d(20200102), Some(d(20260531)))
        .unwrap();
    b.map_native(Databento, 2222, foo, d(20260601), None)
        .unwrap();
    b.map_native(Databento, 1111, old, d(20260601), Some(d(20260630)))
        .unwrap();
    b.map_native(Databento, 3333, newco, d(20260701), None)
        .unwrap();
    b.build().unwrap()
}

#[test]
fn an_id_survives_a_ticker_change_and_a_split() {
    let m = world();
    assert_eq!(m.resolve_symbol("FOO", d(20260301)), Some(0));
    assert_eq!(
        m.resolve_symbol("FOO", d(20260302)),
        None,
        "FOO was renamed"
    );
    assert_eq!(
        m.resolve_symbol("BAR", d(20260301)),
        None,
        "BAR did not exist yet"
    );
    assert_eq!(m.resolve_symbol("BAR", d(20260302)), Some(0));
    // Around the split nothing changes: no corporate-action record exists.
    for day in [20260414, 20260415, 20260416] {
        assert_eq!(m.resolve_symbol("BAR", d(day)), Some(0));
    }
    assert_eq!(m.symbol_on(0, d(20250101)), Some("FOO"));
    assert_eq!(m.symbol_on(0, d(20260401)), Some("BAR"));
}

#[test]
fn a_reused_ticker_resolves_to_the_security_that_held_it_that_day() {
    let m = world();
    assert_eq!(m.resolve_symbol("BAZ", d(20260101)), Some(1));
    assert_eq!(
        m.resolve_symbol("BAZ", d(20260630)),
        Some(1),
        "last day is inclusive"
    );
    assert_eq!(m.resolve_symbol("BAZ", d(20260701)), Some(2));
    assert_eq!(m.resolve_symbol("BAZ", d(20260801)), Some(2));
    assert!(m.is_listed(1, d(20260630)));
    assert!(
        !m.is_listed(1, d(20260701)),
        "delisted, but the row and id remain"
    );
    assert!(!m.is_listed(2, d(20260630)));
    assert_eq!(m.len(), 3);
}

#[test]
fn databento_keys_are_dated_alpaca_resolves_by_symbol() {
    let m = world();
    assert_eq!(m.resolve_native(Databento, 1111, d(20260101)), Some(0));
    assert_eq!(
        m.resolve_native(Databento, 1111, d(20260601)),
        Some(1),
        "key reused by another security"
    );
    assert_eq!(m.resolve_native(Databento, 1111, d(20260701)), None);
    assert_eq!(
        m.resolve_native(Databento, 2222, d(20260601)),
        Some(0),
        "reassigned key, same security"
    );
    assert_eq!(m.resolve_native(Databento, 2222, d(20260531)), None);
    assert_eq!(
        m.resolve_native(Alpaca, 1111, d(20260101)),
        None,
        "keys are per provider"
    );

    let s = m.session(d(20260601));
    assert_eq!(
        s.instrument_for_symbol("BAR"),
        Some(0),
        "Alpaca sends tickers"
    );
    assert_eq!(s.instrument_for_symbol("FOO"), None);
}

#[test]
fn a_session_is_dense_arrays_that_agree_with_the_master() {
    let m = world();
    let s = m.session(d(20260601));
    assert_eq!(s.date(), d(20260601));
    assert_eq!(s.id_space(), 3);
    assert_eq!(s.live(), 2, "id 2 is not listed yet");
    assert_eq!(s.instrument(Databento, 2222), Some(0));
    assert_eq!(s.instrument(Databento, 1111), Some(1));
    assert_eq!(s.instrument(Databento, 3333), None);
    assert_eq!(
        s.instrument(Databento, u32::MAX),
        None,
        "past the table is None, not a panic"
    );
    assert_eq!(
        (s.symbol(0), s.symbol(1), s.symbol(2)),
        (Some("BAR"), Some("BAZ"), None)
    );

    // For every day and key, the session answers what the master does (for live securities).
    for day in [20200102, 20260531, 20260601, 20260630, 20260701, 20261231] {
        let s = m.session(d(day));
        for key in [0, 1111, 2222, 3333, 9999] {
            let want = m
                .resolve_native(Databento, key, d(day))
                .filter(|&id| m.is_listed(id, d(day)) && m.symbol_on(id, d(day)).is_some());
            assert_eq!(s.instrument(Databento, key), want, "key {key} on {day}");
        }
        for id in 0..3 {
            let live = m.is_listed(id, d(day)) && m.symbol_on(id, d(day)).is_some();
            assert_eq!(s.symbol(id).is_some(), live, "id {id} on {day}");
        }
    }
}

#[test]
fn ids_are_append_only_so_new_securities_never_renumber_old_ones() {
    let m = world();
    let mut text = m.to_text();
    text.push_str(
        "security 3 20260801 -\nsymbol 3 NEW 20260801 -\nnative databento 4444 3 20260801 -\n",
    );
    let bigger = SecurityMaster::parse(&text).unwrap();
    assert_eq!(bigger.len(), 4);
    for (sym, day, id) in [
        ("FOO", 20260301, 0),
        ("BAR", 20260401, 0),
        ("BAZ", 20260101, 1),
        ("BAZ", 20260801, 2),
    ] {
        assert_eq!(bigger.resolve_symbol(sym, d(day)), Some(id));
    }
    assert_eq!(bigger.resolve_symbol("NEW", d(20260801)), Some(3));
    assert_eq!(bigger.resolve_native(Databento, 4444, d(20260801)), Some(3));
}

#[test]
fn the_text_form_round_trips_to_the_same_text() {
    let m = world();
    let text = m.to_text();
    assert!(
        text.starts_with("tfsm 1\nsecurity 0 20200102 -\nsymbol 0 FOO 20200102 20260301\n"),
        "{text}"
    );
    let again = SecurityMaster::parse(&text).unwrap();
    assert_eq!(again.to_text(), text);
    assert_eq!(again.resolve_symbol("BAZ", d(20260801)), Some(2));
}

#[test]
fn a_hand_written_file_with_comments_loads() {
    let m = SecurityMaster::parse(
        "# a master\n\ntfsm 1\nsecurity 0 20200102 -   # listed\nsymbol 0 AAA 20200102 -\nnative alpaca 7 0 20200102 -\n",
    )
    .unwrap();
    assert_eq!(m.resolve_symbol("AAA", d(20250101)), Some(0));
    assert_eq!(m.resolve_native(Alpaca, 7, d(20250101)), Some(0));
}

#[test]
fn load_reads_a_file_and_reports_a_missing_one() {
    let path = std::env::temp_dir().join(format!("tf-secmaster-{}.tfsm", std::process::id()));
    std::fs::write(&path, world().to_text()).unwrap();
    let loaded = SecurityMaster::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(loaded.to_text(), world().to_text());
    assert!(matches!(
        SecurityMaster::load(&path),
        Err(Error::Io(m)) if m.contains("tf-secmaster-")
    ));
}

fn one_security() -> (Builder, InstrumentId) {
    let mut b = Builder::new();
    let id = b.add_security(d(20200101));
    (b, id)
}

#[test]
fn contradictory_data_is_rejected_not_resolved_by_guessing() {
    // Two symbols on one security on the same day.
    let (mut b, id) = one_security();
    b.add_symbol(id, "AAA", d(20200101), Some(d(20240101)))
        .unwrap();
    b.add_symbol(id, "BBB", d(20240101), None).unwrap();
    assert!(matches!(b.build(), Err(Error::Overlap(_))));

    // One symbol on two securities on the same day.
    let (mut b, a) = one_security();
    let c = b.add_security(d(20200101));
    b.add_symbol(a, "AAA", d(20200101), None).unwrap();
    b.add_symbol(c, "AAA", d(20230101), None).unwrap();
    assert!(matches!(b.build(), Err(Error::Overlap(_))));

    // A symbol outside the listing, either side.
    let (mut b, id) = one_security();
    b.add_symbol(id, "AAA", d(20190101), None).unwrap();
    assert!(matches!(b.build(), Err(Error::BadSpan(_))));
    let (mut b, id) = one_security();
    b.delist(id, d(20210101)).unwrap();
    b.add_symbol(id, "AAA", d(20200101), None).unwrap();
    assert!(
        matches!(b.build(), Err(Error::BadSpan(_))),
        "open-ended symbol on a delisted security"
    );

    // One native key to two securities, or two keys to one security, on the same day.
    let (mut b, a) = one_security();
    let c = b.add_security(d(20200101));
    b.map_native(Databento, 5, a, d(20200101), None).unwrap();
    b.map_native(Databento, 5, c, d(20210101), None).unwrap();
    assert!(matches!(b.build(), Err(Error::Overlap(_))));
    let (mut b, a) = one_security();
    b.map_native(Databento, 5, a, d(20200101), None).unwrap();
    b.map_native(Databento, 6, a, d(20210101), None).unwrap();
    assert!(matches!(b.build(), Err(Error::Overlap(_))));

    // Delisted before listed.
    let (mut b, id) = one_security();
    b.delist(id, d(20190101)).unwrap();
    assert!(matches!(b.build(), Err(Error::BadSpan(_))));
}

#[test]
fn bad_inputs_are_errors() {
    let (mut b, id) = one_security();
    assert_eq!(
        b.add_symbol(99, "AAA", d(20200101), None),
        Err(Error::UnknownInstrument(99))
    );
    assert_eq!(
        b.map_native(Databento, 1, 99, d(20200101), None),
        Err(Error::UnknownInstrument(99))
    );
    assert!(matches!(
        b.add_symbol(id, "", d(20200101), None),
        Err(Error::BadSymbol(_))
    ));
    assert!(matches!(
        b.add_symbol(id, "A B", d(20200101), None),
        Err(Error::BadSymbol(_))
    ));
    assert!(matches!(
        b.add_symbol(id, "A#", d(20200101), None),
        Err(Error::BadSymbol(_))
    ));
    assert!(matches!(
        b.add_symbol(id, "AAA", d(20210101), Some(d(20200101))),
        Err(Error::BadSpan(_))
    ));
    assert_eq!(
        b.map_native(Databento, MAX_NATIVE_KEY, id, d(20200101), None),
        Err(Error::NativeKeyTooLarge(MAX_NATIVE_KEY))
    );
    assert!(
        b.map_native(Databento, MAX_NATIVE_KEY - 1, id, d(20200101), None)
            .is_ok()
    );
    for bad in [0, 20261301, 20260132, 18991231, 20260100] {
        assert_eq!(Date::new(bad), Err(Error::BadDate(bad)));
    }
}

#[test]
fn parse_errors_name_the_line() {
    let line_of = |text: &str| match SecurityMaster::parse(text) {
        Err(Error::Parse { line, .. }) => line,
        other => panic!("expected a parse error, got {other:?}"),
    };
    assert_eq!(line_of(""), 0, "no header");
    assert_eq!(line_of("tfsm 2\n"), 1, "unknown version");
    assert_eq!(line_of("security 0 20200101 -\n"), 1, "header missing");
    assert_eq!(
        line_of("tfsm 1\nsecurity 1 20200101 -\n"),
        2,
        "ids must start at 0 and stay dense"
    );
    assert_eq!(
        line_of("tfsm 1\nsecurity 0 20200101 -\nsecurity 2 20200101 -\n"),
        3
    );
    assert_eq!(
        line_of("tfsm 1\nsecurity 0 2020-01-01 -\n"),
        2,
        "dates are YYYYMMDD"
    );
    assert_eq!(
        line_of("tfsm 1\nsymbol 0 AAA 20200101 -\n"),
        2,
        "symbol before its security"
    );
    assert_eq!(
        line_of("tfsm 1\nsecurity 0 20200101 -\nnative nasdaq 1 0 20200101 -\n"),
        3,
        "unknown provider"
    );
    assert_eq!(line_of("tfsm 1\nbogus line\n"), 2);
    // Cross-checks run at the end and still report as errors.
    let overlap = "tfsm 1\nsecurity 0 20200101 -\nsymbol 0 A 20200101 -\nsymbol 0 B 20210101 -\n";
    assert!(matches!(
        SecurityMaster::parse(overlap),
        Err(Error::Overlap(_))
    ));
}
