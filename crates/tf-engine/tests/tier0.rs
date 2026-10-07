//! Tier 0 against aggregates recomputed from each symbol's own events.

use tf_core::{
    CancelError, CancelErrorKind, Correction, Event, Header, NANOS_PER_SEC, ProviderId, Px, Quote,
    Status, StatusKind, Trade, TradeFlags,
};
use tf_engine::{SymbolState, Tier0};
use tf_synth::{
    DEFAULT_SESSION_START, PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream,
};

/// A universe plus one symbol per v2 scenario, so statuses, halts, gaps and
/// news all appear.
fn config() -> SynthConfig {
    let mut cfg = SynthConfig::universe(11, 40, 400 * NANOS_PER_SEC, 300);
    let lead = 20 * NANOS_PER_SEC;
    let scenarios = [
        Scenario::halt_up(lead),
        Scenario::luld(lead),
        Scenario::ssr(lead),
        Scenario::squeeze(lead),
        Scenario::gap_and_go(120),
        Scenario::multi_spike(2, lead),
        Scenario::runner(PullbackKind::Dangerous, lead),
    ];
    for (i, scenario) in scenarios.into_iter().enumerate() {
        cfg.symbols.push(SymbolSpec {
            symbol: format!("SCN{i}"),
            base_px_cents: 500,
            base_interval_ns: 300_000_000,
            quote_every: 2,
            scenario,
            news: Vec::new(),
        });
    }
    cfg
}

/// Recompute a symbol's state from its events, without any running totals.
fn expected(evs: &[Event]) -> SymbolState {
    let trades: Vec<&Trade> = evs
        .iter()
        .filter_map(|e| {
            if let Event::Trade(t) = e {
                Some(t)
            } else {
                None
            }
        })
        .collect();
    let quote = evs.iter().rev().find_map(|e| {
        if let Event::Quote(q) = e {
            Some(*q)
        } else {
            None
        }
    });
    let statuses: Vec<&Status> = evs
        .iter()
        .filter_map(|e| {
            if let Event::Status(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect();

    let volume: u64 = trades.iter().map(|t| u64::from(t.size)).sum();
    let notional: u128 = trades
        .iter()
        .map(|t| t.px.raw() as u128 * u128::from(t.size))
        .sum();
    let last_halt = statuses
        .iter()
        .rposition(|s| s.kind == StatusKind::TradingHalt);
    let last_resume = statuses
        .iter()
        .rposition(|s| s.kind == StatusKind::TradingResume);
    let band = statuses
        .iter()
        .enumerate()
        .rfind(|(_, s)| s.kind == StatusKind::LuldBand)
        .filter(|(i, _)| last_halt.is_none_or(|h| *i > h))
        .map(|(_, s)| (s.lo, s.hi));

    SymbolState {
        last_px: trades.last().map(|t| t.px),
        last_size: trades.last().map_or(0, |t| t.size),
        last_ts: trades.last().map_or(0, |t| t.hdr.ts_recv),
        bid: quote.map(|q| (q.bid_px, q.bid_sz)),
        ask: quote.map(|q| (q.ask_px, q.ask_sz)),
        day_high: trades.iter().map(|t| t.px).max(),
        day_low: trades.iter().map(|t| t.px).min(),
        volume,
        notional,
        trades: trades.len() as u32,
        halted: match (last_halt, last_resume) {
            (Some(h), Some(r)) => h > r,
            (Some(_), None) => true,
            _ => false,
        },
        luld: band,
        ssr: statuses
            .iter()
            .any(|s| s.kind == StatusKind::ShortSaleRestriction),
    }
}

#[test]
fn every_symbol_matches_its_own_events_across_a_mixed_session() {
    let cfg = config();
    let evs: Vec<Event> = SynthStream::new(&cfg).collect();
    let n = cfg.symbols.len();
    let mut per: Vec<Vec<Event>> = vec![Vec::new(); n];
    let mut t0 = Tier0::new(n);
    for ev in &evs {
        per[ev.instrument() as usize].push(*ev);
        t0.on_event(ev);
    }
    assert_eq!(t0.unknown_events(), 0);
    assert!(evs.len() > 20_000);

    let (mut halts, mut bands, mut ssrs, mut vwaps) = (0, 0, 0, 0);
    for (id, own) in per.iter().enumerate() {
        let want = expected(own);
        let got = *t0.symbol(id as u32).unwrap();
        assert_eq!(got, want, "symbol {id} ({})", cfg.symbols[id].symbol);

        halts += usize::from(got.halted);
        bands += usize::from(got.luld.is_some());
        ssrs += usize::from(got.ssr);
        if let Some(v) = got.vwap() {
            vwaps += 1;
            let (lo, hi) = (got.day_low.unwrap(), got.day_high.unwrap());
            assert!(lo <= v && v <= hi, "vwap {v} outside {lo}..{hi}");
            let direct = want.notional / u128::from(want.volume);
            assert_eq!(v.raw() as u128, direct);
        }
    }
    assert!(vwaps >= n - 5, "most symbols traded");
    assert!(
        bands >= 1 && ssrs >= 1,
        "the scenarios' statuses reached the state (bands {bands}, ssr {ssrs}, halted {halts})"
    );
}

#[test]
fn windows_follow_the_same_trades() {
    let cfg = config();
    let n = cfg.symbols.len();
    let mut per: Vec<Vec<Trade>> = vec![Vec::new(); n];
    let mut t0 = Tier0::new(n);
    for ev in SynthStream::new(&cfg) {
        if let Event::Trade(t) = ev {
            per[t.hdr.instrument as usize].push(t);
        }
        t0.on_event(&ev);
    }
    for (id, own) in per.iter().enumerate() {
        let Some(last) = own.last() else { continue };
        let now = last.hdr.ts_recv / NANOS_PER_SEC;
        for secs in [1u64, 5, 60] {
            let want: u64 = own
                .iter()
                .filter(|t| t.hdr.ts_recv / NANOS_PER_SEC + secs > now)
                .map(|t| u64::from(t.size))
                .sum();
            assert_eq!(
                t0.windows(id as u32).unwrap().volume(secs as usize),
                want,
                "symbol {id}, {secs}s"
            );
        }
        assert_eq!(
            t0.windows(id as u32).unwrap().last_price(),
            t0.symbol(id as u32).unwrap().last_px
        );
    }
}

fn hdr(instrument: u32, ts: u64) -> Header {
    Header {
        ts_event: ts,
        ts_recv: ts,
        seq: ts,
        instrument,
        provider: ProviderId::Alpaca,
    }
}

fn trade(id: u32, ts: u64, cents: i64, size: u32) -> Event {
    Event::Trade(Trade {
        hdr: hdr(id, ts),
        px: Px::from_cents(cents),
        size,
        flags: TradeFlags::NONE,
    })
}

#[test]
fn corrections_and_cancels_adjust_volume_and_vwap_but_not_the_extremes() {
    let mut t = Tier0::new(1);
    let s = DEFAULT_SESSION_START;
    t.on_event(&trade(0, s, 1000, 100));
    t.on_event(&trade(0, s + 1, 2000, 100));
    assert_eq!(t.symbol(0).unwrap().vwap(), Some(Px::from_cents(1500)));

    // The second print is corrected to 30.00.
    t.on_event(&Event::Correction(Correction {
        hdr: hdr(0, s + 1),
        orig_px: Px::from_cents(2000),
        orig_size: 100,
        px: Px::from_cents(3000),
        size: 100,
    }));
    let st = *t.symbol(0).unwrap();
    assert_eq!((st.volume, st.vwap()), (200, Some(Px::from_cents(2000))));
    assert_eq!(
        st.day_high,
        Some(Px::from_cents(2000)),
        "extremes are not rewound"
    );

    // The first print is cancelled.
    t.on_event(&Event::CancelError(CancelError {
        hdr: hdr(0, s),
        kind: CancelErrorKind::Cancel,
        px: Px::from_cents(1000),
        size: 100,
    }));
    let st = *t.symbol(0).unwrap();
    assert_eq!(
        (st.volume, st.trades, st.vwap()),
        (100, 1, Some(Px::from_cents(3000)))
    );

    // Cancelling a print it never saw cannot underflow.
    t.on_event(&Event::CancelError(CancelError {
        hdr: hdr(0, s + 2),
        kind: CancelErrorKind::Error,
        px: Px::from_cents(9999),
        size: 1_000_000,
    }));
    let st = *t.symbol(0).unwrap();
    assert_eq!((st.volume, st.trades), (0, 0));
    assert_eq!(st.vwap(), None);
}

#[test]
fn quotes_statuses_and_spread() {
    let mut t = Tier0::new(2);
    let s = DEFAULT_SESSION_START;
    t.on_event(&Event::Quote(Quote {
        hdr: hdr(1, s),
        bid_px: Px::from_cents(999),
        ask_px: Px::from_cents(1001),
        bid_sz: 300,
        ask_sz: 400,
    }));
    let st = *t.symbol(1).unwrap();
    assert_eq!(
        (st.bid, st.ask),
        (
            Some((Px::from_cents(999), 300)),
            Some((Px::from_cents(1001), 400))
        )
    );
    assert_eq!(st.spread(), Some(Px::from_cents(2).raw()));
    assert_eq!(
        *t.symbol(0).unwrap(),
        SymbolState::default(),
        "other symbols untouched"
    );

    let status = |kind, lo, hi| {
        Event::Status(Status {
            hdr: hdr(1, s),
            kind,
            lo: Px::from_cents(lo),
            hi: Px::from_cents(hi),
        })
    };
    t.on_event(&status(StatusKind::LuldBand, 900, 1100));
    assert_eq!(
        t.symbol(1).unwrap().luld,
        Some((Px::from_cents(900), Px::from_cents(1100)))
    );
    t.on_event(&status(StatusKind::TradingHalt, 0, 0));
    let st = *t.symbol(1).unwrap();
    assert!(st.halted && st.luld.is_none(), "a halt voids the band");
    t.on_event(&status(StatusKind::TradingResume, 0, 0));
    t.on_event(&status(StatusKind::ShortSaleRestriction, 0, 0));
    let st = *t.symbol(1).unwrap();
    assert!(!st.halted && st.ssr);
}

#[test]
fn unknown_instruments_are_counted_and_a_new_day_clears_state() {
    let mut t = Tier0::new(3);
    let s = DEFAULT_SESSION_START;
    t.on_event(&trade(3, s, 500, 100)); // one past the end
    t.on_event(&trade(u32::MAX, s, 500, 100));
    assert_eq!(t.unknown_events(), 2);
    assert!(t.symbol(3).is_none() && t.windows(3).is_none());

    t.on_event(&trade(2, s, 500, 100));
    assert_eq!(t.symbol(2).unwrap().volume, 100);
    assert_eq!(t.windows(2).unwrap().volume(60), 100);
    t.reset_day();
    assert_eq!(*t.symbol(2).unwrap(), SymbolState::default());
    assert_eq!(t.windows(2).unwrap().volume(60), 0);
    assert_eq!(t.len(), 3, "the table keeps its size");
}

#[test]
fn the_short_sale_restriction_is_set_by_the_feed_and_cleared_by_it() {
    use tf_core::{Header, ProviderId, Status, StatusKind};
    let status = |ts: u64, instrument: u32, kind: StatusKind| {
        Event::Status(Status {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument,
                provider: ProviderId::Synthetic,
            },
            kind,
            lo: tf_core::Px::ZERO,
            hi: tf_core::Px::ZERO,
        })
    };
    let mut t = tf_engine::Tier0::new(2);
    assert!(!t.symbol(0).unwrap().ssr);
    t.on_event(&status(1, 0, StatusKind::ShortSaleRestriction));
    assert!(t.symbol(0).unwrap().ssr);
    assert!(!t.symbol(1).unwrap().ssr, "per instrument");
    t.on_event(&status(2, 0, StatusKind::TradingHalt));
    assert!(t.symbol(0).unwrap().ssr, "a halt does not lift it");
    t.on_event(&status(3, 0, StatusKind::ShortSaleRestrictionLifted));
    assert!(!t.symbol(0).unwrap().ssr);
    t.on_event(&status(4, 0, StatusKind::ShortSaleRestriction));
    assert!(t.symbol(0).unwrap().ssr);
}
