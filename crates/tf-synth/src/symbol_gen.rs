//! Per-symbol event generator: a small state machine that walks a scenario
//! and always has its next event pre-generated, so the merge heap can order
//! symbols by `ts_recv`.

use tf_core::{Event, Header, InstrumentId, Nanos, ProviderId, Px, Quote, Trade, TradeFlags};

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
    trade_ts: Nanos,
    last_recv: Nanos,
    phase_idx: usize,
    phase_end: Nanos,
    since_quote: u32,
    pending_quote: Option<Event>,
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
        let mut g = SymbolGen {
            id,
            rng,
            scenario,
            base_interval_ns: spec.base_interval_ns,
            quote_every: spec.quote_every.max(1),
            end,
            px_cents: spec.base_px_cents.max(1),
            trade_ts: session_start,
            last_recv: 0,
            phase_idx: 0,
            phase_end,
            since_quote: 0,
            pending_quote: None,
            next: None,
        };
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

    fn phase_at(&mut self, ts: Nanos) -> Phase {
        while ts >= self.phase_end && self.phase_idx + 1 < self.scenario.phases.len() {
            self.phase_idx += 1;
            self.phase_end = self
                .phase_end
                .saturating_add(self.scenario.phases[self.phase_idx].duration);
        }
        self.scenario.phases[self.phase_idx].clone()
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
        if let Some(q) = self.pending_quote.take() {
            self.next = Some(q);
            return;
        }

        let rate = self.phase_at(self.trade_ts).rate_permille.max(1);
        let mean = u128::from(self.base_interval_ns) * 1000 / u128::from(rate);
        let gap = ((mean * u128::from(500 + self.rng.below(1000)) / 1000) as Nanos).max(MIN_GAP_NS);
        self.trade_ts = self.trade_ts.saturating_add(gap);
        if self.trade_ts >= self.end {
            self.next = None;
            return;
        }

        let phase = self.phase_at(self.trade_ts);
        let step = if self.rng.permille(300) {
            0
        } else {
            1 + self.rng.below(u64::from(phase.max_step_cents.max(1))) as i64
        };
        let up = self.rng.permille(phase.up_permille);
        self.px_cents = (self.px_cents + if up { step } else { -step }).max(1);

        let lots = 1 + self.rng.below(5);
        let size = (lots * 100 * u64::from(phase.size_permille) / 1000)
            .clamp(1, u64::from(u32::MAX)) as u32;
        let ts_recv = self.stamp_recv(self.trade_ts);
        self.next = Some(Event::Trade(Trade {
            hdr: self.hdr(self.trade_ts, ts_recv),
            px: Px::from_cents(self.px_cents),
            size,
            flags: if size < 100 {
                TradeFlags::ODD_LOT
            } else {
                TradeFlags::NONE
            },
        }));

        self.since_quote += 1;
        if self.since_quote >= self.quote_every {
            self.since_quote = 0;
            let q_ts = self.trade_ts + QUOTE_LAG_NS;
            if q_ts < self.end {
                let spread = i64::from(phase.spread_cents.max(1));
                let bid = (self.px_cents - spread / 2).max(1);
                let q_recv = self.stamp_recv(q_ts);
                self.pending_quote = Some(Event::Quote(Quote {
                    hdr: self.hdr(q_ts, q_recv),
                    bid_px: Px::from_cents(bid),
                    ask_px: Px::from_cents(bid + spread),
                    bid_sz: 100 * (1 + self.rng.below(10)) as u32,
                    ask_sz: 100 * (1 + self.rng.below(10)) as u32,
                }));
            }
        }
    }
}
