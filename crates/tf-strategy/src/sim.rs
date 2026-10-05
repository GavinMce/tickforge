//! A simulated broker for backtests: fills, latency, slippage and borrow cost.
//!
//! Orders are matched against the **recorded quotes**, never against a model of
//! the market:
//! - An intent reaches the venue `latency_ns` after the strategy decided it. It
//!   then sees the market *as it was at that instant*: a quote that arrives at
//!   exactly the arrival time is not yet visible, and a better quote that arrived
//!   while the order was in flight is. That is the cost of being slow.
//! - A buy crosses when the ask is at or below its worst price; a sell when the bid
//!   is at or above it. It fills at the quote's price (so it can improve on the
//!   limit), for at most the size shown. Each quote's size can be consumed once,
//!   shared by every order, so many orders cannot all take the same liquidity.
//! - The unfilled rest of an IOC order expires on arrival. A day order rests and
//!   tries again on every later quote for its instrument.
//! - Nothing fills while the instrument is halted.
//! - **Slippage** is recorded on every [`Fill`]: the price paid against the
//!   intent's reference price, per share, positive when worse.
//! - **Borrow cost** accrues on short positions for the time they are held, at
//!   `borrow_bps_per_year` of their value (priced at the last trade, else the
//!   quote midpoint), in integers, rounded up so it is never understated. Time
//!   is counted as elapsed event time, including overnight.
//!
//! Not modelled: queue position, hidden liquidity or depth beyond the best quote,
//! market impact of our own orders, commissions, and the broker-side protective
//! orders on an opening intent (stops and targets). A backtest that depends on
//! those is optimistic; the report should say so.
//!
//! Everything is integer arithmetic on event time, so a run is reproducible.

use tf_core::{Event, InstrumentId, Nanos, Px, StatusKind};

use crate::intent::{Intent, IntentId, Side, Tif};
use crate::lifecycle::{Order, OrderId, OrderState, OrderUpdate, RejectReason};
use crate::strategy::{Host, Strategy};

/// 365 days, in nanoseconds.
const YEAR_NS: u128 = 365 * 86_400 * 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// Delay from a strategy's decision to the order reaching the venue.
    pub latency_ns: Nanos,
    /// Annual borrow fee on short positions, in basis points of their value.
    pub borrow_bps_per_year: u32,
}

/// One execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub order: OrderId,
    pub intent: IntentId,
    pub instrument: InstrumentId,
    pub side: Side,
    pub qty: u32,
    pub px: Px,
    pub ts: Nanos,
    /// Per share, in raw price units, against the intent's reference price:
    /// positive when we did worse than that, negative when we improved on it.
    pub slippage: i64,
}

#[derive(Clone, Copy, Default)]
struct Book {
    bid: i64,
    ask: i64,
    /// Shares still available at the current bid and ask.
    bid_rem: u32,
    ask_rem: u32,
    last_px: i64,
    halted: bool,
}

impl Book {
    /// What a share is worth now, for the borrow fee.
    fn mark(&self) -> i64 {
        if self.last_px > 0 {
            self.last_px
        } else if self.bid > 0 && self.ask > 0 {
            (self.bid + self.ask) / 2
        } else {
            0
        }
    }
}

#[derive(Clone, Copy)]
struct Live {
    order: Order,
    arrival: Nanos,
}

pub struct SimBroker {
    cfg: SimConfig,
    books: Vec<Book>,
    /// Signed shares per instrument (negative = short).
    pos: Vec<i64>,
    /// Sum of shares x price x bps x ns, divided by [`YEAR_NS`] x 10,000 on read.
    borrow_num: Vec<u128>,
    last_accrue: Option<Nanos>,
    /// Orders not yet finished, in the order they were submitted.
    live: Vec<Live>,
    next_order: u64,
    updates: Vec<OrderUpdate>,
    fills: Vec<Fill>,
}

impl SimBroker {
    pub fn new(cfg: SimConfig, instruments: usize) -> SimBroker {
        SimBroker {
            cfg,
            books: vec![Book::default(); instruments],
            pos: vec![0; instruments],
            borrow_num: vec![0; instruments],
            last_accrue: None,
            live: Vec::new(),
            next_order: 0,
            updates: Vec::new(),
            fills: Vec::new(),
        }
    }

    /// Send an intent. It reaches the venue `latency_ns` after `intent.ts`.
    pub fn submit(&mut self, intent: &Intent) {
        if intent.instrument as usize >= self.books.len() {
            let u = OrderUpdate::rejected(intent.id, RejectReason::Broker, intent.ts);
            self.updates.push(u);
            return;
        }
        let id = OrderId(self.next_order);
        self.next_order += 1;
        self.live.push(Live {
            order: Order::new(id, *intent),
            arrival: intent.ts.saturating_add(self.cfg.latency_ns),
        });
    }

    /// Cancel an order that has not finished. Returns whether there was one.
    pub fn cancel(&mut self, intent: IntentId, ts: Nanos) -> bool {
        self.release(ts);
        self.accrue(ts);
        let Some(i) = self.live.iter().position(|l| l.order.intent.id == intent) else {
            return false;
        };
        // Still in flight when asked to cancel: the cancel wins.
        let mut l = self.live.remove(i);
        let ok = l.order.transition(OrderState::Cancelled).is_ok();
        if ok {
            self.updates.push(l.order.update(ts));
        } else {
            self.live.insert(i, l);
        }
        ok
    }

    /// Feed one market event, in stream order.
    pub fn on_event(&mut self, ev: &Event) {
        let ts = ev.ts_recv();
        self.release(ts);
        self.accrue(ts);
        let i = ev.instrument() as usize;
        let Some(book) = self.books.get_mut(i) else {
            return;
        };
        match ev {
            Event::Quote(q) => {
                book.bid = q.bid_px.raw();
                book.ask = q.ask_px.raw();
                book.bid_rem = if book.bid > 0 { q.bid_sz } else { 0 };
                book.ask_rem = if book.ask > 0 { q.ask_sz } else { 0 };
                self.match_resting(ev.instrument(), ts);
            }
            Event::Trade(t) => book.last_px = t.px.raw(),
            Event::Status(s) => match s.kind {
                StatusKind::TradingHalt => book.halted = true,
                StatusKind::TradingResume => {
                    book.halted = false;
                    self.match_resting(ev.instrument(), ts);
                }
                StatusKind::LuldBand | StatusKind::ShortSaleRestriction => {}
            },
            _ => {}
        }
    }

    /// The end of the session: orders that reached the venue and are still working
    /// expire, and borrow accrues to `ts`.
    pub fn end_of_day(&mut self, ts: Nanos) {
        self.release(ts);
        self.accrue(ts);
        for l in &mut self.live {
            if l.order.state() != OrderState::Pending
                && l.order.transition(OrderState::Expired).is_ok()
            {
                self.updates.push(l.order.update(ts));
            }
        }
        self.live.retain(|l| !l.order.state().is_terminal());
    }

    pub fn drain_updates(&mut self) -> Vec<OrderUpdate> {
        std::mem::take(&mut self.updates)
    }

    /// Every fill so far, in order.
    pub fn fills(&self) -> &[Fill] {
        &self.fills
    }

    /// Signed position in shares (negative = short).
    pub fn position(&self, instrument: InstrumentId) -> i64 {
        self.pos.get(instrument as usize).copied().unwrap_or(0)
    }

    /// Borrow fee accrued on `instrument`, in raw price units (total, not per share).
    pub fn borrow_fee(&self, instrument: InstrumentId) -> u128 {
        let den = YEAR_NS * 10_000;
        self.borrow_num
            .get(instrument as usize)
            .map_or(0, |n| n.div_ceil(den))
    }

    /// Orders still pending or working.
    pub fn open_orders(&self) -> usize {
        self.live.len()
    }

    /// Orders whose arrival time has come reach the venue, in (arrival, id) order.
    fn release(&mut self, upto: Nanos) {
        loop {
            let next = self
                .live
                .iter()
                .enumerate()
                .filter(|(_, l)| l.order.state() == OrderState::Pending && l.arrival <= upto)
                .min_by_key(|(_, l)| (l.arrival, l.order.id))
                .map(|(i, _)| i);
            let Some(i) = next else { break };
            let at = self.live[i].arrival;
            self.accrue(at);
            let mut o = self.live[i].order;
            o.transition(OrderState::Accepted).expect("pending accepts");
            self.updates.push(o.update(at));
            self.try_fill(&mut o, at);
            if o.intent.tif == Tif::Ioc && !o.state().is_terminal() {
                o.transition(OrderState::Expired)
                    .expect("a working order expires");
                self.updates.push(o.update(at));
            }
            self.live[i].order = o;
            self.live.retain(|l| !l.order.state().is_terminal());
        }
    }

    /// Working orders on `instrument` try the current quote, oldest first.
    fn match_resting(&mut self, instrument: InstrumentId, ts: Nanos) {
        for i in 0..self.live.len() {
            let mut o = self.live[i].order;
            if o.intent.instrument == instrument
                && matches!(
                    o.state(),
                    OrderState::Accepted | OrderState::PartiallyFilled
                )
            {
                self.try_fill(&mut o, ts);
                self.live[i].order = o;
            }
        }
        self.live.retain(|l| !l.order.state().is_terminal());
    }

    fn try_fill(&mut self, o: &mut Order, ts: Nanos) {
        let intent = o.intent;
        let book = &mut self.books[intent.instrument as usize];
        if book.halted {
            return;
        }
        let worst = intent.limit_price().raw();
        let (px, avail) = if intent.side.is_buy() {
            if book.ask <= 0 || book.ask > worst {
                return;
            }
            (book.ask, &mut book.ask_rem)
        } else {
            if book.bid <= 0 || book.bid < worst {
                return;
            }
            (book.bid, &mut book.bid_rem)
        };
        let qty = o.remaining().min(*avail);
        if qty == 0 {
            return;
        }
        *avail -= qty;
        let px = Px::from_raw(px);
        o.fill(qty, px)
            .expect("a working order takes a fill within its size");
        let reference = intent.pricing.reference_price().raw();
        let slippage = if intent.side.is_buy() {
            px.raw() - reference
        } else {
            reference - px.raw()
        };
        self.accrue(ts);
        self.pos[intent.instrument as usize] += if intent.side.is_buy() {
            i64::from(qty)
        } else {
            -i64::from(qty)
        };
        self.fills.push(Fill {
            order: o.id,
            intent: intent.id,
            instrument: intent.instrument,
            side: intent.side,
            qty,
            px,
            ts,
            slippage,
        });
        self.updates.push(o.update(ts));
    }

    /// Charge borrow on short positions for the time since the last call.
    fn accrue(&mut self, ts: Nanos) {
        let Some(last) = self.last_accrue else {
            self.last_accrue = Some(ts);
            return;
        };
        if ts <= last {
            return;
        }
        let dt = u128::from(ts - last);
        let bps = u128::from(self.cfg.borrow_bps_per_year);
        for (i, &p) in self.pos.iter().enumerate() {
            if p < 0 {
                let mark = u128::try_from(self.books[i].mark()).unwrap_or(0);
                self.borrow_num[i] += u128::from(p.unsigned_abs()) * mark * bps * dt;
            }
        }
        self.last_accrue = Some(ts);
    }
}

/// Run a strategy against a simulated broker over a recorded stream and return
/// every intent it emitted. Per event: the broker sees it first (so orders that
/// have reached the venue meet the market as it was), the strategy is told what
/// became of its orders, then it sees the event, and what it asks for is sent.
pub fn run_backtest<S: Strategy>(
    host: &mut Host<S>,
    broker: &mut SimBroker,
    events: impl IntoIterator<Item = Event>,
) -> Vec<Intent> {
    run_backtest_observed(host, broker, events, |_, _| {})
}

/// [`run_backtest`], calling `observe(event, new_fills)` for each event with the
/// fills the broker made while processing it (before the event itself took
/// effect), so a report can follow the run without a second pass.
pub fn run_backtest_observed<S: Strategy>(
    host: &mut Host<S>,
    broker: &mut SimBroker,
    events: impl IntoIterator<Item = Event>,
    mut observe: impl FnMut(&Event, &[Fill]),
) -> Vec<Intent> {
    let mut all = Vec::new();
    let mut seen = 0;
    for ev in events {
        broker.on_event(&ev);
        observe(&ev, &broker.fills()[seen..]);
        seen = broker.fills().len();
        for u in broker.drain_updates() {
            host.on_order_update(&u);
        }
        host.on_event(&ev);
        for i in host.drain_intents() {
            broker.submit(&i);
            all.push(i);
        }
    }
    all
}
