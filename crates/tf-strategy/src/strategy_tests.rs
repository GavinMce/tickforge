use tf_core::{Event, Header, NANOS_PER_SEC, Nanos, ProviderId, Px, Trade, TradeFlags};
use tf_synth::{PullbackKind, Scenario, SymbolSpec, SynthConfig, SynthStream};

use crate::intent::{IntentError, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::{OrderId, OrderState, OrderUpdate};
use crate::strategy::{Ctx, Host, MAX_TIMER_FIRES_PER_STEP, Request, Strategy, TimerId};
use crate::{Intent, IntentId};

const SEC: Nanos = NANOS_PER_SEC;

fn trade(instrument: u32, ts: Nanos, cents: i64, size: u32) -> Event {
    Event::Trade(Trade {
        hdr: Header {
            ts_event: ts,
            ts_recv: ts,
            seq: ts,
            instrument,
            provider: ProviderId::Synthetic,
        },
        px: Px::from_cents(cents),
        size,
        flags: TradeFlags::NONE,
    })
}

fn buy(px: Px, stop_pct: i64) -> Request {
    Request {
        side: Side::Buy,
        qty: 100,
        purpose: Purpose::Open,
        pricing: Pricing::Collar {
            reference: px,
            collar_permille: 20,
        },
        protect: Some(Protective {
            stop_trigger: Px::from_raw(px.raw() * (100 - stop_pct) / 100),
            stop_limit: None,
            take_profit: None,
        }),
        tif: Tif::Day,
        reason: 1,
    }
}

fn sell_to_close(px: Px) -> Request {
    Request {
        side: Side::Sell,
        qty: 100,
        purpose: Purpose::Close,
        pricing: Pricing::Collar {
            reference: px,
            collar_permille: 20,
        },
        protect: None,
        tif: Tif::Ioc,
        reason: 2,
    }
}

// ---- a small momentum strategy, the kind the real ones will be ----

/// Buy 100 shares of anything up 3% over 5 s on 3,000+ shares; sell it 30 s later if it filled.
struct Momentum {
    pending: Vec<bool>,
    held: Vec<bool>,
    entries: Vec<(IntentId, u32)>,
}

impl Momentum {
    fn new(n: usize) -> Self {
        Momentum {
            pending: vec![false; n],
            held: vec![false; n],
            entries: Vec::new(),
        }
    }
}

impl Strategy for Momentum {
    fn id(&self) -> StrategyId {
        StrategyId(7)
    }

    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
        let Event::Trade(t) = ev else { return };
        let i = t.hdr.instrument;
        if self.pending[i as usize] || self.held[i as usize] {
            return;
        }
        let Some(w) = ctx.windows(i) else { return };
        let hot = w.price_change_permille(5).is_some_and(|c| c >= 30) && w.volume(5) >= 3000;
        if !hot {
            return;
        }
        if let Ok(id) = ctx.submit(i, buy(t.px, 1)) {
            self.pending[i as usize] = true;
            self.entries.push((id, i));
            ctx.set_timer_in(TimerId(i), 30 * SEC);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, timer: TimerId) {
        let i = timer.0;
        if self.held[i as usize] {
            self.held[i as usize] = false;
            let last = ctx.state(i).and_then(|s| s.last_px).expect("it traded");
            ctx.submit(i, sell_to_close(last))
                .expect("a close is valid");
        }
        self.pending[i as usize] = false;
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, u: &OrderUpdate) {
        let Some(&(_, i)) = self.entries.iter().find(|(id, _)| *id == u.intent) else {
            return;
        };
        match u.state {
            OrderState::Filled => self.held[i as usize] = true,
            OrderState::Rejected | OrderState::Cancelled | OrderState::Expired => {
                self.pending[i as usize] = false;
                ctx.cancel_timer(TimerId(i));
            }
            _ => {}
        }
    }
}

/// One runner and one quiet symbol (a 5 s move of 3% is impossible for it: at most
/// ~10 trades of 1 cent on 10.00).
fn session() -> Vec<Event> {
    let spec = |symbol: &str, scenario| SymbolSpec {
        symbol: symbol.into(),
        base_px_cents: if symbol == "RUN" { 500 } else { 1000 },
        base_interval_ns: 300_000_000,
        quote_every: 2,
        scenario,
        news: Vec::new(),
    };
    let cfg = SynthConfig {
        seed: 5,
        session_start: tf_synth::DEFAULT_SESSION_START,
        duration: 200 * SEC,
        symbols: vec![
            spec("RUN", Scenario::runner(PullbackKind::Healthy, 20 * SEC)),
            SymbolSpec {
                base_interval_ns: SEC,
                ..spec("CALM", Scenario::quiet())
            },
        ],
    };
    SynthStream::new(&cfg).collect()
}

fn fills(host: &mut Host<Momentum>, intents: &[Intent]) {
    for i in intents.iter().filter(|i| i.purpose == Purpose::Open) {
        let u = OrderUpdate {
            intent: i.id,
            order: Some(OrderId(i.id.seq)),
            state: OrderState::Filled,
            filled_qty: i.qty,
            avg_px: Some(i.pricing.worst_price(i.side)),
            reject: None,
            ts: i.ts,
        };
        host.on_order_update(&u);
    }
}

#[test]
fn a_strategy_enters_on_the_runner_and_stamps_its_intents_with_event_time() {
    let events = session();
    let t0 = events[0].ts_recv();
    let mut host = Host::new(Momentum::new(2), 2);
    let intents = host.run(events.iter().copied());

    assert!(
        !intents.is_empty(),
        "the runner's impulse should trigger an entry"
    );
    for (n, i) in intents.iter().enumerate() {
        assert_eq!(
            (i.instrument, i.side, i.purpose),
            (0, Side::Buy, Purpose::Open),
            "only the runner, only entries (no fills were reported)"
        );
        assert_eq!(
            i.id,
            IntentId {
                strategy: StrategyId(7),
                seq: n as u64
            },
            "dense per-strategy sequence"
        );
        // The first entry comes during the impulse; an unfilled one clears after
        // 30 s and may re-arm, never sooner.
        if n == 0 {
            assert!(
                i.ts >= t0 + 20 * SEC && i.ts < t0 + 55 * SEC,
                "entered during the impulse, at +{} s",
                (i.ts - t0) / SEC
            );
        } else {
            assert!(i.ts >= intents[n - 1].ts + 30 * SEC, "re-armed too early");
        }
        assert_eq!(i.validate(), Ok(()));
        // The intent's time is the arrival time of a real trade in the stream.
        assert!(events.iter().any(
            |e| matches!(e, Event::Trade(t) if t.hdr.ts_recv == i.ts && t.hdr.instrument == 0)
        ));
    }
    assert_eq!(host.invalid_intents(), 0);
}

#[test]
fn a_filled_entry_is_closed_by_its_timer_exactly_thirty_seconds_later() {
    let events = session();
    let mut host = Host::new(Momentum::new(2), 2);
    let mut all = Vec::new();
    for ev in &events {
        host.on_event(ev);
        let new = host.drain_intents();
        fills(&mut host, &new); // a gateway that fills everything at once
        all.extend(new);
    }
    let entry = all
        .iter()
        .find(|i| i.purpose == Purpose::Open)
        .expect("an entry");
    let close = all
        .iter()
        .find(|i| i.purpose == Purpose::Close)
        .expect("a close");
    assert_eq!(
        close.ts,
        entry.ts + 30 * SEC,
        "the timer fired at its own time, not at the next event's"
    );
    assert_eq!(
        (close.instrument, close.side, close.protect, close.tif),
        (entry.instrument, Side::Sell, None, Tif::Ioc)
    );
    assert_eq!(close.validate(), Ok(()));
    assert!(close.id.seq > entry.id.seq);
}

#[test]
fn an_unfilled_entry_is_never_closed() {
    let events = session();
    let mut host = Host::new(Momentum::new(2), 2);
    let all = host.run(events.iter().copied());
    assert!(
        all.iter().all(|i| i.purpose == Purpose::Open),
        "nothing filled, so there is nothing to close"
    );
    assert!(
        all.len() >= 2,
        "the entry re-arms once its timer clears it: {} intents",
        all.len()
    );
}

#[test]
fn the_same_events_give_the_same_intents() {
    let events = session();
    let run = || {
        let mut host = Host::new(Momentum::new(2), 2);
        let mut all = Vec::new();
        for ev in &events {
            host.on_event(ev);
            let new = host.drain_intents();
            fills(&mut host, &new);
            all.extend(new);
        }
        all
    };
    let (a, b) = (run(), run());
    assert!(a.len() >= 2);
    assert_eq!(a, b);
}

// ---- timer and context semantics, with a probe ----

#[derive(Debug, PartialEq, Eq)]
enum Call {
    Event(Nanos),
    Timer(Nanos, u32),
    Update(Nanos),
}

struct Probe {
    log: Vec<Call>,
    /// Timers to set when an event at this time arrives: (event ts, timer, delay).
    on_event_sets: Vec<(Nanos, u32, Nanos)>,
    /// When this timer fires, set another: (timer, new timer, delay).
    chain: Option<(u32, u32, Nanos)>,
    rearm_every_ns: Option<u32>,
    state_seen: Vec<Option<Px>>,
}

impl Probe {
    fn new() -> Self {
        Probe {
            log: Vec::new(),
            on_event_sets: Vec::new(),
            chain: None,
            rearm_every_ns: None,
            state_seen: Vec::new(),
        }
    }
}

impl Strategy for Probe {
    fn id(&self) -> StrategyId {
        StrategyId(1)
    }

    fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
        self.log.push(Call::Event(ctx.now()));
        assert_eq!(ctx.now(), ev.ts_recv(), "now is the event's arrival time");
        self.state_seen
            .push(ctx.state(ev.instrument()).and_then(|s| s.last_px));
        for &(at, id, delay) in &self.on_event_sets {
            if at == ev.ts_recv() {
                ctx.set_timer_in(TimerId(id), delay);
            }
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, timer: TimerId) {
        self.log.push(Call::Timer(ctx.now(), timer.0));
        if let Some((from, to, delay)) = self.chain {
            if from == timer.0 {
                ctx.set_timer_in(TimerId(to), delay);
            }
        }
        if self.rearm_every_ns == Some(timer.0) {
            ctx.set_timer_in(timer, 1);
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, _: &OrderUpdate) {
        self.log.push(Call::Update(ctx.now()));
    }
}

fn host(p: Probe) -> Host<Probe> {
    Host::new(p, 4)
}

#[test]
fn timers_fire_in_time_then_id_order_at_their_own_time_before_the_event() {
    let mut p = Probe::new();
    p.on_event_sets = vec![(100, 5, 50), (100, 3, 50), (100, 9, 20)];
    let mut h = host(p);
    h.on_event(&trade(0, 100, 500, 10));
    assert_eq!(h.pending_timers(), 3);
    h.on_event(&trade(0, 400, 501, 10)); // all three are due before this
    assert_eq!(
        h.strategy().log,
        [
            Call::Event(100),
            Call::Timer(120, 9), // earliest
            Call::Timer(150, 3), // tie on time: lower id first
            Call::Timer(150, 5),
            Call::Event(400), // after the timers, and now == the event's time
        ]
    );
    assert_eq!(h.pending_timers(), 0);
}

#[test]
fn a_timer_set_for_the_events_own_instant_fires_before_that_event() {
    let mut p = Probe::new();
    p.on_event_sets = vec![(100, 1, 100)]; // due at 200
    let mut h = host(p);
    h.on_event(&trade(0, 100, 500, 1));
    h.on_event(&trade(0, 200, 500, 1));
    assert_eq!(
        h.strategy().log,
        [Call::Event(100), Call::Timer(200, 1), Call::Event(200)]
    );
}

#[test]
fn rescheduling_replaces_and_cancelling_prevents() {
    struct Re;
    impl Strategy for Re {
        fn id(&self) -> StrategyId {
            StrategyId(2)
        }
        fn on_event(&mut self, ctx: &mut Ctx<'_>, _: &Event) {
            ctx.set_timer(TimerId(1), 1_000);
            ctx.set_timer(TimerId(1), 2_000); // replaces the first
            ctx.set_timer(TimerId(2), 1_500);
            assert!(ctx.cancel_timer(TimerId(2)));
            assert!(!ctx.cancel_timer(TimerId(2)), "already gone");
        }
        fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {
            panic!("only timer 1 at 2000 should be left, and the test checks that below");
        }
    }
    let mut h = Host::new(Re, 1);
    h.on_event(&trade(0, 100, 500, 1));
    assert_eq!(h.pending_timers(), 1);
    // Nothing fires before 2,000.
    h.advance_to(1_999);
    assert_eq!(h.pending_timers(), 1);
}

#[test]
fn a_timer_cannot_fire_in_the_instant_that_set_it() {
    struct Now;
    impl Strategy for Now {
        fn id(&self) -> StrategyId {
            StrategyId(2)
        }
        fn on_event(&mut self, ctx: &mut Ctx<'_>, _: &Event) {
            ctx.set_timer(TimerId(1), 0); // in the past
            ctx.set_timer_in(TimerId(2), 0); // now
        }
        fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {}
    }
    let mut h = Host::new(Now, 1);
    h.on_event(&trade(0, 100, 500, 1));
    assert_eq!(
        h.pending_timers(),
        2,
        "neither fired during the event that set it"
    );
    h.advance_to(101);
    assert_eq!(h.pending_timers(), 0, "both fire at the next nanosecond");
}

#[test]
fn a_timer_set_by_a_timer_fires_in_the_same_step_if_it_is_due() {
    let mut p = Probe::new();
    p.on_event_sets = vec![(100, 1, 10)];
    p.chain = Some((1, 2, 10));
    let mut h = host(p);
    h.on_event(&trade(0, 100, 500, 1));
    h.on_event(&trade(0, 1_000, 500, 1));
    assert_eq!(
        h.strategy().log,
        [
            Call::Event(100),
            Call::Timer(110, 1),
            Call::Timer(120, 2),
            Call::Event(1_000)
        ]
    );
}

#[test]
fn advance_to_fires_timers_with_no_event_and_never_runs_time_backwards() {
    let mut p = Probe::new();
    p.on_event_sets = vec![(100, 1, 50)];
    let mut h = host(p);
    h.on_event(&trade(0, 100, 500, 1));
    h.advance_to(10_000);
    assert_eq!(h.strategy().log, [Call::Event(100), Call::Timer(150, 1)]);
    assert_eq!(h.now(), 10_000);
    h.advance_to(5); // earlier: no effect
    assert_eq!(h.now(), 10_000);
}

#[test]
fn a_strategy_that_rearms_every_nanosecond_cannot_hang_the_host() {
    let mut p = Probe::new();
    p.on_event_sets = vec![(100, 1, 1)];
    p.rearm_every_ns = Some(1);
    let mut h = host(p);
    h.on_event(&trade(0, 100, 500, 1));
    h.advance_to(100 + 5 * u64::from(MAX_TIMER_FIRES_PER_STEP));
    assert!(h.timer_storms() >= 1);
    let fires = h
        .strategy()
        .log
        .iter()
        .filter(|c| matches!(c, Call::Timer(..)))
        .count();
    assert_eq!(
        fires as u32, MAX_TIMER_FIRES_PER_STEP,
        "the step stopped at the cap"
    );
    assert_eq!(h.pending_timers(), 1, "the timer was put back, not lost");
    // A timer cut off by the cap fires late, at the current time, on the next step.
    h.advance_to(100 + 10 * u64::from(MAX_TIMER_FIRES_PER_STEP));
    let fires = h
        .strategy()
        .log
        .iter()
        .filter(|c| matches!(c, Call::Timer(..)))
        .count();
    assert_eq!(fires as u32, 2 * MAX_TIMER_FIRES_PER_STEP);
}

#[test]
fn the_strategy_sees_the_event_already_in_tier_0() {
    let mut h = host(Probe::new());
    h.on_event(&trade(2, 100, 777, 10));
    assert_eq!(h.strategy().state_seen, [Some(Px::from_cents(777))]);
    assert_eq!(h.tier0().symbol(2).unwrap().volume, 10);
}

#[test]
fn a_timer_never_sees_an_event_that_arrives_after_it() {
    struct Peek(Vec<Option<Px>>);
    impl Strategy for Peek {
        fn id(&self) -> StrategyId {
            StrategyId(3)
        }
        fn on_event(&mut self, ctx: &mut Ctx<'_>, _: &Event) {
            ctx.set_timer(TimerId(1), 200);
        }
        fn on_timer(&mut self, ctx: &mut Ctx<'_>, _: TimerId) {
            self.0.push(ctx.state(0).and_then(|s| s.last_px));
        }
    }
    let mut h = Host::new(Peek(Vec::new()), 1);
    h.on_event(&trade(0, 100, 500, 1));
    h.on_event(&trade(0, 300, 900, 1)); // after the timer at 200
    assert_eq!(
        h.strategy().0,
        [Some(Px::from_cents(500))],
        "the 9.00 print is in the future"
    );
}

#[test]
fn order_updates_reach_the_strategy_at_the_current_event_time() {
    let mut h = host(Probe::new());
    h.on_event(&trade(0, 500, 500, 1));
    let u = OrderUpdate::rejected(
        IntentId {
            strategy: StrategyId(1),
            seq: 0,
        },
        crate::RejectReason::KillSwitch,
        400,
    );
    h.on_order_update(&u);
    assert_eq!(h.strategy().log, [Call::Event(500), Call::Update(500)]);
}

#[test]
fn a_malformed_intent_is_refused_counted_and_does_not_use_a_sequence_number() {
    struct Sloppy;
    impl Strategy for Sloppy {
        fn id(&self) -> StrategyId {
            StrategyId(4)
        }
        fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
            let px = Px::from_cents(1000);
            let mut bad = buy(px, 1);
            bad.protect = None; // an open without a stop
            assert_eq!(
                ctx.submit(ev.instrument(), bad),
                Err(IntentError::MissingProtection)
            );
            let id = ctx.submit(ev.instrument(), buy(px, 1)).unwrap();
            assert_eq!(id.seq, 0, "the refused one did not consume sequence 0");
        }
        fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {}
    }
    let mut h = Host::new(Sloppy, 1);
    h.on_event(&trade(0, 100, 1000, 1));
    assert_eq!(h.invalid_intents(), 1);
    let out = h.drain_intents();
    assert_eq!(
        (out.len(), out[0].ts, out[0].id.strategy),
        (1, 100, StrategyId(4))
    );
    assert!(h.drain_intents().is_empty(), "draining empties the queue");
}

/// The ban list is what keeps wall clocks and I/O out of strategy code (CI runs
/// clippy with -D warnings). Deleting or hollowing it out must not go unnoticed.
#[test]
fn the_clippy_ban_list_still_covers_the_clock_io_and_hash_maps() {
    let cfg = include_str!("../clippy.toml");
    for banned in [
        "std::time::SystemTime::now",
        "std::time::Instant::now",
        "tf_core::clock::SystemClock",
        "std::thread::sleep",
        "std::fs::File",
        "std::fs::read_to_string",
        "std::net::TcpStream",
        "std::process::Command",
        "std::env::var",
        "std::collections::HashMap",
        "std::collections::HashSet",
    ] {
        assert!(cfg.contains(banned), "clippy.toml no longer bans {banned}");
    }
}

// ---- multi-timeframe bars ----

mod bars {
    use super::*;
    use crate::strategy::BarsError;
    use crate::{MtfBars, MtfConfig, TfBar, Timeframe};
    use tf_core::InstrumentId;
    use tf_engine::TrackError;

    const T0: Nanos = 1_767_571_200 * SEC; // a day boundary

    /// Tracks every instrument it sees on its first trade and records the order of
    /// bar closes against events.
    #[derive(Default)]
    struct Watcher {
        log: Vec<String>,
        closed: Vec<(Nanos, InstrumentId, Timeframe, TfBar)>,
        track_result: Vec<Result<(), BarsError>>,
        forming_seen: Vec<Option<u64>>,
    }

    impl Strategy for Watcher {
        fn id(&self) -> StrategyId {
            StrategyId(5)
        }
        fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
            let i = ev.instrument();
            if ctx.bars(i).is_none() {
                self.track_result.push(ctx.track_bars(i));
            }
            self.forming_seen.push(
                ctx.bars(i)
                    .and_then(|b| b.forming(Timeframe::M1))
                    .map(|f| f.start_sec),
            );
            self.log
                .push(format!("event@{}", ev.ts_recv() / SEC - T0 / SEC));
        }
        fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {}
        fn on_bar(&mut self, ctx: &mut Ctx<'_>, i: InstrumentId, tf: Timeframe) {
            let bar = *ctx.bars(i).unwrap().closed(tf, 0).unwrap();
            self.closed.push((ctx.now(), i, tf, bar));
            self.log.push(format!("bar {tf:?}"));
        }
    }

    fn host(max: usize) -> Host<Watcher> {
        Host::new(Watcher::default(), 3).with_bars(MtfBars::new(MtfConfig::default(), 3, max))
    }

    #[test]
    fn a_closed_bar_is_delivered_before_the_event_that_closed_it() {
        let mut h = host(3);
        h.on_event(&trade(0, T0 + 10 * SEC, 1000, 5));
        h.on_event(&trade(0, T0 + 30 * SEC, 1010, 7));
        h.on_event(&trade(0, T0 + 61 * SEC, 1020, 1));
        let w = h.strategy();
        assert_eq!(w.log, ["event@10", "event@30", "bar M1", "event@61"]);
        let (now, id, tf, bar) = w.closed[0];
        assert_eq!(
            (now, id, tf),
            (T0 + 61 * SEC, 0, Timeframe::M1),
            "now is the closing trade's time"
        );
        assert_eq!(
            (bar.start_sec, bar.volume, bar.trades, bar.close),
            (T0 / SEC, 7, 1, Px::from_cents(1010))
        );
        // The strategy asked for tracking in its first on_event, so the 10 s trade is not in the bar.
        assert_eq!(w.track_result, [Ok(())]);
    }

    #[test]
    fn tracking_starts_with_the_next_trade_and_forming_bars_are_visible() {
        let mut h = host(3);
        h.on_event(&trade(0, T0 + 10 * SEC, 1000, 5));
        h.on_event(&trade(0, T0 + 20 * SEC, 1000, 5));
        assert_eq!(h.strategy().forming_seen, [None, Some(T0 / SEC)]);
    }

    #[test]
    fn advance_to_closes_a_quiet_symbols_bar_and_tells_the_strategy() {
        let mut h = host(3);
        h.on_event(&trade(0, T0 + 10 * SEC, 1000, 5));
        h.on_event(&trade(0, T0 + 20 * SEC, 1000, 5));
        h.advance_to(T0 + 60 * SEC);
        let w = h.strategy();
        assert_eq!(w.closed.len(), 1);
        assert_eq!(w.closed[0].0, T0 + 60 * SEC);
        h.advance_to(T0 + 61 * SEC);
        assert_eq!(h.strategy().closed.len(), 1, "once");
    }

    #[test]
    fn a_strategy_can_stop_tracking_and_gets_nothing_more() {
        struct Once(Vec<bool>);
        impl Strategy for Once {
            fn id(&self) -> StrategyId {
                StrategyId(6)
            }
            fn on_event(&mut self, ctx: &mut Ctx<'_>, ev: &Event) {
                let i = ev.instrument();
                if ctx.bars(i).is_none() {
                    ctx.track_bars(i).unwrap();
                } else {
                    self.0.push(ctx.untrack_bars(i));
                    self.0.push(ctx.untrack_bars(i));
                    self.0.push(ctx.bars(i).is_some());
                }
            }
            fn on_timer(&mut self, _: &mut Ctx<'_>, _: TimerId) {}
            fn on_bar(&mut self, _: &mut Ctx<'_>, _: InstrumentId, _: Timeframe) {
                self.0.push(true);
            }
        }
        let mut h =
            Host::new(Once(Vec::new()), 1).with_bars(MtfBars::new(MtfConfig::default(), 1, 1));
        h.on_event(&trade(0, T0, 1000, 1));
        h.on_event(&trade(0, T0 + SEC, 1000, 1));
        h.on_event(&trade(0, T0 + 120 * SEC, 1000, 1));
        assert_eq!(
            h.strategy().0,
            [true, false, false],
            "untracked once, then nothing, and no bar close"
        );
    }

    #[test]
    fn asking_for_bars_without_an_aggregator_or_past_the_bound_is_an_error() {
        let mut plain = Host::new(Watcher::default(), 3);
        plain.on_event(&trade(0, T0, 1000, 1));
        assert_eq!(
            plain.strategy().track_result,
            [Err(BarsError::NotConfigured)]
        );

        let mut tiny = host(1);
        tiny.on_event(&trade(0, T0, 1000, 1));
        tiny.on_event(&trade(1, T0 + SEC, 1000, 1));
        assert_eq!(
            tiny.strategy().track_result,
            [Ok(()), Err(BarsError::Track(TrackError::Full))]
        );
    }
}
