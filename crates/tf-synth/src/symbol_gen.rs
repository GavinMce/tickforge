//! Per-symbol event generator: a small state machine that walks a scenario
//! and always has its next event pre-generated, so the merge heap can order
//! symbols by `ts_recv`.

use std::collections::VecDeque;

use tf_core::{
    Event, Header, InstrumentId, Nanos, ProviderId, Px, Quote, Status, StatusKind, Trade,
    TradeFlags,
};

use crate::config::SymbolSpec;
use crate::rng::SplitMix64;
use crate::scenario::{Phase, Scenario};

/// Smallest gap between two trades of one symbol; keeps a trade's quote
/// (1 us later) strictly before the next trade.
const MIN_GAP_NS: Nanos = 2_000;
const QUOTE_LAG_NS: Nanos = 1_000;
/// Simulated network latency: 50..500 us.
const LATENCY_BASE_NS: Nanos = 50_000;
const LATENCY_JITTER_NS: u64 = 450_000;

pub(crate) struct SymbolGen {
    id: InstrumentId,
    rng: SplitMix64,
    scenario: Scenario,
    base_interval_ns: Nanos,
    quote_every: u32,
    end: Nanos,

    px_cents: i64,
    /// The starting price, standing in for the prior close (the SSR reference).
    ref_px_cents: i64,
    /// The LULD band `(lo, hi)` in cents while one is active.
    band: Option<(i64, i64)>,
    /// SSR is watched for and has not triggered yet.
    ssr_armed: bool,
    trade_ts: Nanos,
    /// Latest `ts_event` queued; keeps a status at a phase boundary from
    /// stepping back behind a quote that trails the previous trade.
    last_event_ts: Nanos,
    last_recv: Nanos,
    phase_idx: usize,
    phase_end: Nanos,
    since_quote: u32,
    /// Events generated ahead of `next`, in delivery order: statuses at a
    /// phase boundary, then the trade, an SSR trigger and the trade's quote.
    queue: VecDeque<Event>,
    next: Option<Event>,
}

impl SymbolGen {
    pub(crate) fn new(
        id: InstrumentId,
        rng: SplitMix64,
        spec: &SymbolSpec,
        session_start: Nanos,
        end: Nanos,
    ) -> Self {
        let scenario = spec.scenario.clone();
        let phase_end = session_start.saturating_add(scenario.phases[0].duration);
        let base_px_cents = spec.base_px_cents.max(1);
        let mut g = SymbolGen {
            id,
            rng,
            ssr_armed: scenario.ssr,
            scenario,
            base_interval_ns: spec.base_interval_ns,
            quote_every: spec.quote_every.max(1),
            end,
            px_cents: base_px_cents,
            ref_px_cents: base_px_cents,
            band: None,
            trade_ts: session_start,
            last_event_ts: session_start,
            last_recv: 0,
            phase_idx: 0,
            phase_end,
            since_quote: 0,
            queue: VecDeque::new(),
            next: None,
        };
        g.enter_phase(session_start, false);
        g.advance();
        g
    }

    pub(crate) fn peek_recv(&self) -> Option<Nanos> {
        self.next.as_ref().map(Event::ts_recv)
    }

    pub(crate) fn pop(&mut self) -> Option<Event> {
        let ev = self.next.take();
        if ev.is_some() {
            self.advance();
        }
        ev
    }

    fn phase(&self) -> Phase {
        self.scenario.phases[self.phase_idx].clone()
    }

    /// Walk into every phase that has started by `ts`, applying each one's
    /// entry effects at its own start time (a candidate trade time can jump
    /// over a short phase, such as a halt, and its effects must still happen).
    fn enter_phases_through(&mut self, ts: Nanos) {
        while ts >= self.phase_end && self.phase_idx + 1 < self.scenario.phases.len() {
            let start = self.phase_end;
            let left_halt = self.scenario.phases[self.phase_idx].halted;
            self.phase_idx += 1;
            self.phase_end = start.saturating_add(self.scenario.phases[self.phase_idx].duration);
            self.enter_phase(start, left_halt);
        }
    }

    /// Entry effects of the current phase, which starts at `start`: resume
    /// from a halt, halt, gap, then a fresh LULD band around the new price.
    fn enter_phase(&mut self, start: Nanos, left_halt: bool) {
        let phase = self.phase();
        if left_halt {
            self.push_status(StatusKind::TradingResume, start, Px::ZERO, Px::ZERO);
        }
        if phase.halted {
            self.push_status(StatusKind::TradingHalt, start, Px::ZERO, Px::ZERO);
            self.band = None;
            return;
        }
        if phase.gap_permille != 0 {
            let jumped = self.px_cents * (1000 + i64::from(phase.gap_permille)) / 1000;
            self.px_cents = jumped.max(1);
        }
        self.band = (phase.luld_permille > 0).then(|| {
            let off = self.px_cents * i64::from(phase.luld_permille) / 1000;
            ((self.px_cents - off).max(1), self.px_cents + off)
        });
        if let Some((lo, hi)) = self.band {
            let (lo, hi) = (Px::from_cents(lo), Px::from_cents(hi));
            self.push_status(StatusKind::LuldBand, start, lo, hi);
        }
    }

    fn push_status(&mut self, kind: StatusKind, ts_event: Nanos, lo: Px, hi: Px) {
        let ts_event = ts_event.max(self.last_event_ts);
        if ts_event >= self.end {
            return;
        }
        self.last_event_ts = ts_event;
        let ts_recv = self.stamp_recv(ts_event);
        self.queue.push_back(Event::Status(Status {
            hdr: self.hdr(ts_event, ts_recv),
            kind,
            lo,
            hi,
        }));
    }

    fn stamp_recv(&mut self, ts_event: Nanos) -> Nanos {
        let r = (ts_event + LATENCY_BASE_NS + self.rng.below(LATENCY_JITTER_NS))
            .max(self.last_recv + 1);
        self.last_recv = r;
        r
    }

    fn hdr(&self, ts_event: Nanos, ts_recv: Nanos) -> Header {
        Header {
            ts_event,
            ts_recv,
            seq: 0,
            instrument: self.id,
            provider: ProviderId::Synthetic,
        }
    }

    fn advance(&mut self) {
        if let Some(ev) = self.queue.pop_front() {
            self.next = Some(ev);
            return;
        }

        self.enter_phases_through(self.trade_ts);
        let rate = self.phase().rate_permille.max(1);
        let mean = u128::from(self.base_interval_ns) * 1000 / u128::from(rate);
        let gap = ((mean * u128::from(500 + self.rng.below(1000)) / 1000) as Nanos).max(MIN_GAP_NS);
        let mut ts = self.trade_ts.saturating_add(gap);
        loop {
            self.enter_phases_through(ts);
            if !self.phase().halted {
                break;
            }
            if self.phase_idx + 1 >= self.scenario.phases.len() {
                // Halted for the rest of the session.
                ts = Nanos::MAX;
                break;
            }
            // Nothing trades until the halt ends.
            ts = self.phase_end.saturating_add(MIN_GAP_NS);
        }
        self.trade_ts = ts;
        if self.trade_ts >= self.end {
            // Statuses queued at boundaries before the end still go out.
            self.next = self.queue.pop_front();
            return;
        }

        let phase = self.phase();
        let step = if self.rng.permille(300) {
            0
        } else {
            1 + self.rng.below(u64::from(phase.max_step_cents.max(1))) as i64
        };
        let up = self.rng.permille(phase.up_permille);
        self.px_cents = (self.px_cents + if up { step } else { -step }).max(1);
        if let Some((lo, hi)) = self.band {
            self.px_cents = self.px_cents.clamp(lo, hi);
        }

        let lots = 1 + self.rng.below(5);
        let size = (lots * 100 * u64::from(phase.size_permille) / 1000)
            .clamp(1, u64::from(u32::MAX)) as u32;
        let ts_recv = self.stamp_recv(self.trade_ts);
        self.last_event_ts = self.trade_ts;
        self.queue.push_back(Event::Trade(Trade {
            hdr: self.hdr(self.trade_ts, ts_recv),
            px: Px::from_cents(self.px_cents),
            size,
            flags: if size < 100 {
                TradeFlags::ODD_LOT
            } else {
                TradeFlags::NONE
            },
        }));

        // SSR triggers on a print 10% or more below the prior close.
        if self.ssr_armed && self.px_cents * 10 <= self.ref_px_cents * 9 {
            self.ssr_armed = false;
            let trigger = Px::from_cents(self.ref_px_cents * 9 / 10);
            let prior_close = Px::from_cents(self.ref_px_cents);
            self.push_status(
                StatusKind::ShortSaleRestriction,
                self.trade_ts,
                trigger,
                prior_close,
            );
        }

        self.since_quote += 1;
        if self.since_quote >= self.quote_every {
            self.since_quote = 0;
            let q_ts = self.trade_ts + QUOTE_LAG_NS;
            if q_ts < self.end {
                let spread = i64::from(phase.spread_cents.max(1));
                let bid = (self.px_cents - spread / 2).max(1);
                let q_recv = self.stamp_recv(q_ts);
                self.last_event_ts = q_ts;
                self.queue.push_back(Event::Quote(Quote {
                    hdr: self.hdr(q_ts, q_recv),
                    bid_px: Px::from_cents(bid),
                    ask_px: Px::from_cents(bid + spread),
                    bid_sz: 100 * (1 + self.rng.below(10)) as u32,
                    ask_sz: 100 * (1 + self.rng.below(10)) as u32,
                }));
            }
        }
        self.next = self.queue.pop_front();
    }
}
