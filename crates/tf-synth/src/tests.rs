use tf_core::{Event, Fnv1a64, NANOS_PER_SEC, Nanos, Status, StatusKind, Trade};
use tf_provider::{Channels, Poll, Provider, ProviderError, Subscription};

use crate::{
    Account, DEFAULT_SESSION_START, Faults, PullbackKind, Scenario, SymbolSpec, SynthConfig,
    SynthOptions, SynthProvider, SynthStream,
};

fn hash_events(evs: impl IntoIterator<Item = Event>) -> (u64, u64) {
    let mut h = Fnv1a64::new();
    let mut n = 0;
    let mut buf = Vec::new();
    for ev in evs {
        buf.clear();
        ev.encode(&mut buf);
        h.write(&buf);
        n += 1;
    }
    (n, h.finish())
}

fn drain(p: &mut SynthProvider) -> Vec<Event> {
    let mut out = Vec::new();
    loop {
        match p.poll(&mut out, 4096) {
            Poll::End => return out,
            Poll::Events(_) => {}
            other => panic!("unexpected poll result {other:?}"),
        }
    }
}

fn single(scenario: Scenario, base_px_cents: i64, secs: u64) -> SynthConfig {
    SynthConfig {
        seed: 1,
        session_start: DEFAULT_SESSION_START,
        duration: secs * NANOS_PER_SEC,
        symbols: vec![SymbolSpec {
            symbol: "RUN".into(),
            base_px_cents,
            base_interval_ns: NANOS_PER_SEC,
            quote_every: 2,
            scenario,
        }],
    }
}

/// Golden values: any change to the generator, scenarios or encoding changes
/// these, and must be a deliberate decision (regenerate and say why in the PR).
#[test]
fn golden_stream_hash() {
    let cfg = SynthConfig::universe(42, 20, 120 * NANOS_PER_SEC, 500);
    let (n, h) = hash_events(SynthStream::new(&cfg));
    assert_eq!(
        (n, h),
        (GOLDEN_COUNT, GOLDEN_HASH),
        "got count={n} hash={h:#018x}"
    );
}
const GOLDEN_COUNT: u64 = 3166;
const GOLDEN_HASH: u64 = 0xa19b_6423_e169_b302;

#[test]
fn same_seed_same_stream_different_seed_differs() {
    let a = hash_events(SynthStream::new(&SynthConfig::universe(
        7,
        10,
        60 * NANOS_PER_SEC,
        300,
    )));
    let b = hash_events(SynthStream::new(&SynthConfig::universe(
        7,
        10,
        60 * NANOS_PER_SEC,
        300,
    )));
    let c = hash_events(SynthStream::new(&SynthConfig::universe(
        8,
        10,
        60 * NANOS_PER_SEC,
        300,
    )));
    assert_eq!(a, b);
    assert_ne!(a.1, c.1);
}

#[test]
fn stream_is_arrival_ordered_with_dense_seq() {
    let cfg = SynthConfig::universe(3, 50, 120 * NANOS_PER_SEC, 400);
    let end = cfg.session_start + cfg.duration;
    let evs: Vec<Event> = SynthStream::new(&cfg).collect();
    assert!(evs.len() > 1_000);
    let mut last_event_ts = vec![0u64; 50];
    for (i, w) in evs.windows(2).enumerate() {
        assert!(
            w[0].ts_recv() <= w[1].ts_recv(),
            "ts_recv went backwards at {i}"
        );
    }
    for (i, ev) in evs.iter().enumerate() {
        assert_eq!(ev.seq(), i as u64);
        assert!(ev.ts_event() < end);
        assert!(ev.ts_recv() > ev.ts_event());
        let slot = &mut last_event_ts[ev.instrument() as usize];
        assert!(
            ev.ts_event() >= *slot,
            "ts_event regressed within one symbol"
        );
        *slot = ev.ts_event();
    }
}

/// (peak price in the impulse, trough price in the pullback window, base) in cents,
/// plus the best price seen after the pullback.
fn runner_shape(kind: PullbackKind) -> (i64, i64, i64, i64) {
    let lead_in: Nanos = 20 * NANOS_PER_SEC;
    let base = 500;
    let cfg = single(Scenario::runner(kind, lead_in), base, 300);
    let t0 = cfg.session_start;
    let s = NANOS_PER_SEC;
    let (mut peak, mut trough, mut after_peak) = (0, i64::MAX, 0);
    for ev in SynthStream::new(&cfg) {
        if let Event::Trade(t) = ev {
            let dt = t.hdr.ts_event - t0;
            let c = t.px.to_cents();
            if (lead_in..lead_in + 30 * s).contains(&dt) {
                peak = peak.max(c);
            } else if (lead_in + 30 * s..lead_in + 70 * s).contains(&dt) {
                trough = trough.min(c);
            } else if dt >= lead_in + 70 * s {
                after_peak = after_peak.max(c);
            }
        }
    }
    (peak, trough, base, after_peak)
}

#[test]
fn healthy_runner_pulls_back_shallowly_then_makes_new_highs() {
    let (peak, trough, base, after) = runner_shape(PullbackKind::Healthy);
    let depth = (peak - trough) as f64 / (peak - base) as f64;
    assert!(peak as f64 > 1.3 * base as f64, "no impulse: peak {peak}");
    assert!(
        (0.05..0.5).contains(&depth),
        "pullback depth {depth:.2} (peak {peak}, trough {trough})"
    );
    assert!(
        after > peak,
        "no continuation: after {after} <= peak {peak}"
    );
}

#[test]
fn dangerous_runner_retraces_deeply_and_never_recovers() {
    let (peak, trough, base, after) = runner_shape(PullbackKind::Dangerous);
    let depth = (peak - trough) as f64 / (peak - base) as f64;
    assert!(peak as f64 > 1.3 * base as f64, "no impulse: peak {peak}");
    assert!(
        depth > 0.6,
        "pullback depth {depth:.2} (peak {peak}, trough {trough})"
    );
    assert!(after < peak, "dangerous scenario recovered to new highs");
}

#[test]
fn runner_volume_spikes_during_impulse() {
    let lead_in = 20 * NANOS_PER_SEC;
    let cfg = single(Scenario::runner(PullbackKind::Healthy, lead_in), 500, 120);
    let t0 = cfg.session_start;
    let (mut quiet, mut hot) = (0u64, 0u64);
    for ev in SynthStream::new(&cfg) {
        if let Event::Trade(t) = ev {
            let dt = t.hdr.ts_event - t0;
            if dt < lead_in {
                quiet += u64::from(t.size);
            } else if dt < lead_in + 30 * NANOS_PER_SEC {
                hot += u64::from(t.size);
            }
        }
    }
    // Per second of wall time: impulse volume must dwarf the lead-in's.
    let quiet_per_s = quiet as f64 / 20.0;
    let hot_per_s = hot as f64 / 30.0;
    assert!(
        hot_per_s > 20.0 * quiet_per_s,
        "quiet {quiet_per_s:.0}/s vs impulse {hot_per_s:.0}/s"
    );
}

#[test]
fn account_enforces_connection_limit_across_providers() {
    let cfg = SynthConfig::universe(1, 3, 10 * NANOS_PER_SEC, 0);
    let account = Account::new(1);
    let mut a = SynthProvider::new(cfg.clone(), SynthOptions::default(), account.clone());
    let mut b = SynthProvider::new(cfg, SynthOptions::default(), account.clone());
    a.connect().unwrap();
    assert_eq!(
        b.connect(),
        Err(ProviderError::ConnectionLimitExceeded { limit: 1 })
    );
    a.disconnect();
    assert_eq!(account.active(), 0);
    b.connect().unwrap();
}

#[test]
fn symbol_limit_is_enforced_like_alpaca_basic() {
    let cfg = SynthConfig::universe(1, 100, 10 * NANOS_PER_SEC, 0);
    let opts = SynthOptions {
        max_symbols: Some(30),
        ..Default::default()
    };
    let mut p = SynthProvider::new(cfg, opts, Account::new(1));
    assert_eq!(
        p.subscribe(&Subscription::all(Channels::ALL)),
        Err(ProviderError::NotConnected)
    );
    p.connect().unwrap();
    let too_many = Subscription::list(Channels::ALL, (0..31).collect());
    assert_eq!(
        p.subscribe(&too_many),
        Err(ProviderError::SymbolLimitExceeded {
            limit: 30,
            requested: 31
        })
    );
    p.subscribe(&Subscription::list(Channels::ALL, (0..30).collect()))
        .unwrap();
}

#[test]
fn subscription_filters_symbols_and_channels() {
    let cfg = SynthConfig::universe(5, 10, 60 * NANOS_PER_SEC, 0);
    let mut p = SynthProvider::new(cfg, SynthOptions::default(), Account::new(1));
    p.connect().unwrap();
    p.subscribe(&Subscription::list(Channels::TRADES, vec![2, 4]))
        .unwrap();
    let evs = drain(&mut p);
    assert!(!evs.is_empty());
    assert!(
        evs.iter()
            .all(|e| matches!(e, Event::Trade(_)) && [2, 4].contains(&e.instrument()))
    );
}

fn connected(cfg: &SynthConfig, opts: SynthOptions) -> SynthProvider {
    let mut p = SynthProvider::new(cfg.clone(), opts, Account::new(1));
    p.connect().unwrap();
    p.subscribe(&Subscription::all(Channels::ALL)).unwrap();
    p
}

#[test]
fn duplicates_and_reordering_are_injected_but_lose_nothing() {
    let cfg = SynthConfig::universe(9, 20, 60 * NANOS_PER_SEC, 200);
    let clean = drain(&mut connected(&cfg, SynthOptions::default()));
    let faults = Faults {
        dup_permille: 50,
        reorder_permille: 50,
        ..Default::default()
    };
    let noisy = drain(&mut connected(
        &cfg,
        SynthOptions {
            faults,
            ..Default::default()
        },
    ));

    assert!(noisy.len() > clean.len(), "no duplicates injected");
    assert!(
        noisy.windows(2).any(|w| w[0].seq() > w[1].seq()),
        "no reordering injected"
    );

    let mut seqs: Vec<u64> = noisy.iter().map(Event::seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), clean.len(), "events were lost");
}

#[test]
fn disconnect_without_replay_loses_the_outage() {
    let cfg = SynthConfig::universe(11, 20, 60 * NANOS_PER_SEC, 200);
    let clean = drain(&mut connected(&cfg, SynthOptions::default()));
    let faults = Faults {
        disconnect_after_events: Some(500),
        outage_events: 100,
        ..Default::default()
    };
    let mut p = connected(
        &cfg,
        SynthOptions {
            faults,
            ..Default::default()
        },
    );

    let mut got = Vec::new();
    assert_eq!(p.poll(&mut got, 10_000), Poll::Events(500));
    assert_eq!(p.poll(&mut got, 10_000), Poll::Disconnected);
    p.reconnect(Some(got.last().unwrap().ts_recv())).unwrap();
    got.extend(drain(&mut p));

    assert_eq!(got.len(), clean.len() - 100);
    assert_eq!(got[500].seq(), 600, "outage should skip exactly 100 events");
}

#[test]
fn disconnect_with_replay_recovers_everything_with_one_boundary_duplicate() {
    let cfg = SynthConfig::universe(11, 20, 60 * NANOS_PER_SEC, 200);
    let clean = drain(&mut connected(&cfg, SynthOptions::default()));
    let faults = Faults {
        disconnect_after_events: Some(500),
        outage_events: 100,
        ..Default::default()
    };
    let mut p = connected(
        &cfg,
        SynthOptions {
            faults,
            replay: true,
            ..Default::default()
        },
    );

    let mut got = Vec::new();
    assert_eq!(p.poll(&mut got, 10_000), Poll::Events(500));
    assert_eq!(p.poll(&mut got, 10_000), Poll::Disconnected);
    p.reconnect(Some(got.last().unwrap().ts_recv())).unwrap();
    got.extend(drain(&mut p));

    // Replay is inclusive of the boundary: the last pre-drop event comes again.
    assert_eq!(got.len(), clean.len() + 1);
    assert_eq!(got[499], got[500]);
    got.remove(500);
    assert_eq!(got, clean);
}

// ---- scenario library v2: halts, LULD, SSR, squeeze, gap-and-go, multi-spike ----

fn collect(cfg: &SynthConfig) -> Vec<Event> {
    SynthStream::new(cfg).collect()
}

fn statuses(evs: &[Event]) -> Vec<Status> {
    evs.iter()
        .filter_map(|e| match e {
            Event::Status(s) => Some(*s),
            _ => None,
        })
        .collect()
}

fn trades(evs: &[Event]) -> Vec<Trade> {
    evs.iter()
        .filter_map(|e| match e {
            Event::Trade(t) => Some(*t),
            _ => None,
        })
        .collect()
}

fn kinds(sts: &[Status]) -> Vec<StatusKind> {
    sts.iter().map(|s| s.kind).collect()
}

/// Shares traded per second from `from` to `to` seconds after `t0`.
fn shares_per_s(evs: &[Event], t0: Nanos, from: u64, to: u64) -> f64 {
    let (a, b) = (t0 + from * NANOS_PER_SEC, t0 + to * NANOS_PER_SEC);
    let shares: u64 = trades(evs)
        .iter()
        .filter(|t| (a..b).contains(&t.hdr.ts_event))
        .map(|t| u64::from(t.size))
        .sum();
    shares as f64 / (to - from) as f64
}

/// The last trade before a halt and the first after it ends.
fn around_halt(evs: &[Event], halt: Nanos, resume: Nanos) -> (Trade, Trade) {
    let ts = trades(evs);
    let pre = ts.iter().rev().find(|t| t.hdr.ts_event < halt).copied();
    let post = ts.iter().find(|t| t.hdr.ts_event > resume).copied();
    (
        pre.expect("no trade before the halt"),
        post.expect("no trade after the halt"),
    )
}

fn assert_silent_between(evs: &[Event], from: Nanos, to: Nanos) {
    for ev in evs {
        if matches!(ev, Event::Trade(_) | Event::Quote(_)) {
            let ts = ev.ts_event();
            assert!(
                !(from < ts && ts < to),
                "{:?} at {ts} during the halt",
                ev.kind()
            );
        }
    }
}

fn assert_golden(cfg: &SynthConfig, want: (u64, u64)) {
    let got = hash_events(SynthStream::new(cfg));
    assert_eq!(got, want, "got count={} hash={:#018x}", got.0, got.1);
}

#[test]
fn halt_up_spikes_halts_then_reopens_higher() {
    let s = NANOS_PER_SEC;
    let lead_in = 20 * s;
    let cfg = single(Scenario::halt_up(lead_in), 500, 200);
    let t0 = cfg.session_start;
    let evs = collect(&cfg);

    let st = statuses(&evs);
    assert_eq!(
        kinds(&st),
        [StatusKind::TradingHalt, StatusKind::TradingResume]
    );
    let (halt, resume) = (st[0].hdr.ts_event, st[1].hdr.ts_event);
    assert!(
        halt.abs_diff(t0 + lead_in + 15 * s) < 10_000,
        "halt at +{}",
        halt - t0
    );
    assert!(
        resume.abs_diff(halt + 60 * s) < 10_000,
        "halt lasted {}",
        resume - halt
    );
    assert_silent_between(&evs, halt, resume);

    let (pre, post) = around_halt(&evs, halt, resume);
    assert!(
        pre.px.to_cents() > 540,
        "no spike before the halt: {}",
        pre.px
    );
    assert!(
        post.px.to_cents() * 1000 >= pre.px.to_cents() * 1040,
        "reopened at {} after {}",
        post.px,
        pre.px
    );

    let quiet = shares_per_s(&evs, t0, 0, 20);
    let spike = shares_per_s(&evs, t0, 20, 35);
    assert!(
        spike > 10.0 * quiet,
        "quiet {quiet:.0}/s vs spike {spike:.0}/s"
    );
}

#[test]
fn luld_pins_at_the_band_then_halts_and_recentres() {
    let s = NANOS_PER_SEC;
    let cfg = single(Scenario::luld(20 * s), 500, 260);
    let evs = collect(&cfg);

    let st = statuses(&evs);
    assert_eq!(
        kinds(&st),
        [
            StatusKind::LuldBand,
            StatusKind::LuldBand,
            StatusKind::TradingHalt,
            StatusKind::TradingResume,
            StatusKind::LuldBand,
        ]
    );
    let band = |s: &Status| (s.lo.to_cents(), s.hi.to_cents());
    assert_eq!(
        band(&st[0]),
        (450, 550),
        "opening band is +/-10% of the price"
    );

    // Every trade sits inside the band in force when it printed.
    let mut current = None;
    for ev in &evs {
        match ev {
            Event::Status(s) if s.kind == StatusKind::LuldBand => current = Some(band(s)),
            Event::Status(s) if s.kind == StatusKind::TradingHalt => current = None,
            Event::Trade(t) => {
                if let Some((lo, hi)) = current {
                    let c = t.px.to_cents();
                    assert!(
                        (lo..=hi).contains(&c),
                        "trade at {c} outside band {lo}..{hi}"
                    );
                }
            }
            _ => {}
        }
    }

    // The push runs into the upper limit and stays there until the halt.
    let (push_start, halt, resume) = (st[1].hdr.ts_event, st[2].hdr.ts_event, st[3].hdr.ts_event);
    let push_hi = band(&st[1]).1;
    let pinned = trades(&evs)
        .iter()
        .filter(|t| (push_start..halt).contains(&t.hdr.ts_event) && t.px.to_cents() == push_hi)
        .count();
    assert!(pinned >= 20, "only {pinned} prints at the limit {push_hi}");
    assert_silent_between(&evs, halt, resume);

    // The halt starts at the limit (the price still jitters, by at most one
    // 3c step), and the band after it is centred on the price it reopens at.
    let (pre, _) = around_halt(&evs, halt, resume);
    let last = pre.px.to_cents();
    assert!(
        (push_hi - 3..=push_hi).contains(&last),
        "halt began at {last}, limit {push_hi}"
    );
    let (lo, hi) = band(&st[4]);
    assert_eq!(
        (lo + hi) / 2,
        last,
        "new band {lo}..{hi} not centred on {last}"
    );
}

#[test]
fn ssr_fires_once_on_the_first_print_ten_percent_below_the_close() {
    let cfg = single(Scenario::ssr(20 * NANOS_PER_SEC), 500, 120);
    let evs = collect(&cfg);

    let st = statuses(&evs);
    assert_eq!(kinds(&st), [StatusKind::ShortSaleRestriction]);
    assert_eq!(
        (st[0].lo.to_cents(), st[0].hi.to_cents()),
        (450, 500),
        "trigger and prior close"
    );

    let first = trades(&evs)
        .into_iter()
        .find(|t| t.px.to_cents() <= 450)
        .expect("the sell-off never reached the trigger");
    assert_eq!(
        st[0].hdr.ts_event, first.hdr.ts_event,
        "SSR is stamped at the tripping print"
    );
    let trade_at = evs.iter().position(|e| *e == Event::Trade(first)).unwrap();
    let status_at = evs
        .iter()
        .position(|e| matches!(e, Event::Status(_)))
        .unwrap();
    assert!(
        trade_at < status_at,
        "SSR arrived before the print that tripped it"
    );

    // No 10% drop, no SSR.
    let calm = single(
        Scenario {
            ssr: true,
            ..Scenario::quiet()
        },
        500,
        120,
    );
    assert!(statuses(&collect(&calm)).is_empty());
}

#[test]
fn squeeze_gaps_through_a_naive_stop() {
    let s = NANOS_PER_SEC;
    let lead_in = 20 * s;
    let cfg = single(Scenario::squeeze(lead_in), 500, 300);
    let t0 = cfg.session_start;
    let evs = collect(&cfg);

    let st = statuses(&evs);
    assert_eq!(
        kinds(&st),
        [StatusKind::TradingHalt, StatusKind::TradingResume]
    );
    let (halt, resume) = (st[0].hdr.ts_event, st[1].hdr.ts_event);
    assert_silent_between(&evs, halt, resume);

    // A short taken at the last pre-halt print with a stop 10% above it.
    let (pre, post) = around_halt(&evs, halt, resume);
    let entry = pre.px.to_cents();
    let stop = entry * 110 / 100;
    let fill = post.px.to_cents();
    assert!(entry > 600, "no ramp before the halt: {entry}");
    assert!(
        fill > stop,
        "stop {stop} was not gapped, reopened at {fill}"
    );
    assert!(
        (fill - stop) * 100 >= stop * 10,
        "stop {stop} filled at {fill}: slipped less than 10% past its trigger"
    );

    // Blow-off, then fade.
    let blowoff_end = resume + 20 * s;
    let peak = trades(&evs)
        .iter()
        .filter(|t| (resume..blowoff_end).contains(&t.hdr.ts_event))
        .map(|t| t.px.to_cents())
        .max()
        .unwrap();
    let trough = trades(&evs)
        .iter()
        .filter(|t| (blowoff_end..blowoff_end + 60 * s).contains(&t.hdr.ts_event))
        .map(|t| t.px.to_cents())
        .min()
        .unwrap();
    assert!(
        trough * 10 < peak * 9,
        "no fade: peak {peak}, trough {trough}"
    );

    let quiet = shares_per_s(&evs, t0, 0, 20);
    let ramp = shares_per_s(&evs, t0, 20, 45);
    assert!(
        ramp > 10.0 * quiet,
        "quiet {quiet:.0}/s vs ramp {ramp:.0}/s"
    );
}

#[test]
fn gap_and_go_opens_above_the_close_and_keeps_going() {
    let s = NANOS_PER_SEC;
    let cfg = single(Scenario::gap_and_go(120), 500, 300);
    let t0 = cfg.session_start;
    let evs = collect(&cfg);

    assert!(
        statuses(&evs).is_empty(),
        "a plain gap has no halts or bands"
    );
    let ts = trades(&evs);
    let open = ts[0].px.to_cents();
    assert!(
        (550..=570).contains(&open),
        "opened at {open}, expected about 560 (12% over 500)"
    );
    let lowest = ts.iter().map(|t| t.px.to_cents()).min().unwrap();
    assert!(lowest > 500, "the gap filled: traded down to {lowest}");
    let high = ts
        .iter()
        .filter(|t| t.hdr.ts_event < t0 + 60 * s)
        .map(|t| t.px.to_cents())
        .max()
        .unwrap();
    assert!(
        high * 100 >= open * 105,
        "no follow-through: open {open}, high {high}"
    );

    let hot = shares_per_s(&evs, t0, 0, 60);
    let calm = shares_per_s(&evs, t0, 200, 260);
    assert!(hot > 10.0 * calm, "open {hot:.0}/s vs late {calm:.0}/s");
}

#[test]
fn multi_spike_repeats_with_cooldowns_between() {
    let s = NANOS_PER_SEC;
    let cfg = single(Scenario::multi_spike(3, 20 * s), 500, 520);
    let t0 = cfg.session_start;
    let evs = collect(&cfg);

    let baseline = shares_per_s(&evs, t0, 0, 20);
    let cycle = 135; // 20 s spike + 25 s pullback + 90 s cool-down
    for k in 0..3 {
        let start = 20 + k * cycle;
        let spike = shares_per_s(&evs, t0, start, start + 20);
        let cooled = shares_per_s(&evs, t0, start + 45, start + cycle);
        assert!(
            spike > 20.0 * baseline,
            "spike {k}: {spike:.0}/s vs baseline {baseline:.0}/s"
        );
        assert!(
            cooled < 2.0 * baseline,
            "cool-down {k}: {cooled:.0}/s vs baseline {baseline:.0}/s"
        );
    }
    let after = shares_per_s(&evs, t0, 20 + 3 * cycle + 10, 20 + 3 * cycle + 70);
    assert!(
        after < 2.0 * baseline,
        "still hot after the last cycle: {after:.0}/s"
    );

    // Exactly three spikes in the whole session. Count runs of hot 10 s
    // buckets: a spike can straddle two or three of them.
    let hot: Vec<bool> = (0..52)
        .map(|b| shares_per_s(&evs, t0, b * 10, b * 10 + 10) > 20.0 * baseline)
        .collect();
    let runs = usize::from(hot[0]) + hot.windows(2).filter(|w| !w[0] && w[1]).count();
    assert_eq!(runs, 3, "hot buckets: {hot:?}");
}

/// Per-scenario goldens, like `golden_stream_hash`: any change to a scenario
/// or to how the generator walks it changes these, and must be deliberate.
#[test]
fn golden_stream_hash_halt_up() {
    assert_golden(
        &single(Scenario::halt_up(20 * NANOS_PER_SEC), 500, 300),
        (876, 0xf54a_e23a_af55_54d5),
    );
}

#[test]
fn golden_stream_hash_luld() {
    assert_golden(
        &single(Scenario::luld(20 * NANOS_PER_SEC), 500, 300),
        (1202, 0x3f68_7768_52a2_4889),
    );
}

#[test]
fn golden_stream_hash_ssr() {
    assert_golden(
        &single(Scenario::ssr(20 * NANOS_PER_SEC), 500, 300),
        (865, 0x228d_dbd0_71a5_8119),
    );
}

#[test]
fn golden_stream_hash_squeeze() {
    assert_golden(
        &single(Scenario::squeeze(20 * NANOS_PER_SEC), 500, 300),
        (1662, 0xa908_c2bd_0363_bc91),
    );
}

#[test]
fn golden_stream_hash_gap_and_go() {
    assert_golden(
        &single(Scenario::gap_and_go(120), 500, 300),
        (2025, 0x0619_03c6_8e50_9beb),
    );
}

#[test]
fn golden_stream_hash_multi_spike() {
    assert_golden(
        &single(Scenario::multi_spike(3, 20 * NANOS_PER_SEC), 500, 520),
        (2094, 0x74bf_a259_3313_bce9),
    );
}
