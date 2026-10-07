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
//! - **Protective orders** are modelled when asked for ([`SimBroker::with_protective_orders`]; off by default, so
//!   every earlier result is unchanged), as Alpaca runs a bracket or an OTO order. They exist for the shares the
//!   opening order has filled so far (a partial fill carries through) and share one pool of shares, so a fill of one
//!   leg shrinks the other and the legs end together: one cancels the other. The *target* is a resting limit: a long's
//!   sells when the bid is at or above it, at the bid. The *stop* is triggered by a trade at or through its trigger
//!   (and stays triggered), then becomes a market order, which fills at the bid (the ask, for a short) whatever it is,
//!   so a gap through the stop fills at the gapped price; or a limit order at `stop_limit`, which fills only at that
//!   price or better and so can be gapped through. A leg fill is a [`Fill`] with its `leg` set, moves the position,
//!   and is a [`Kind::LegFill`] event on the parent order. Legs are for the day: they end with it. A leg never takes
//!   the position past flat.
//! - **Extended hours.** Through the [`Broker`] interface (and `submit`), an order decided in the premarket or
//!   after-hours that carries protective orders or is immediate-or-cancel is refused with the broker's reason
//!   ([`crate::session_rules`]).
//!
//! Not modelled: queue position, hidden liquidity or depth beyond the best quote,
//! market impact of our own orders, commissions, a stop triggered by a quote (the broker triggers on trades), and
//! another order closing a position whose legs still stand (Alpaca refuses it for want of available shares). A backtest
//! that depends on those is optimistic; the report should say so.
//!
//! Everything is integer arithmetic on event time, so a run is reproducible.

use tf_core::{Event, InstrumentId, Nanos, Px, StatusKind};

use crate::broker::{Broker, BrokerEvent, CancelOutcome, Kind, Leg, Submission};
use crate::intent::{Intent, IntentId, Purpose, Side, Tif};
use crate::lifecycle::{Order, OrderId, OrderState, OrderUpdate, RejectReason};
use crate::session_rules::{ExtendedHoursRefusal, extended_hours_refusal};
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
    /// Per share, in raw price units, against the intent's reference price (for a protective leg: its trigger or
    /// target): positive when we did worse than that, negative when we improved on it.
    pub slippage: i64,
    /// `Some` for the fill of a protective leg of `order`, `None` for the order itself.
    pub leg: Option<Leg>,
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

/// The protective orders of one opening order that has filled, and what they have used.
#[derive(Clone, Copy)]
struct Legs {
    order: OrderId,
    intent: Intent,
    /// Shares the opening order has filled so far, which the legs protect, and shares the legs have since filled.
    entered: u32,
    exited: u32,
    /// A trade has reached the stop's trigger (it stays so).
    triggered: bool,
}

#[derive(Clone, Copy)]
struct Live {
    order: Order,
    arrival: Nanos,
    /// The venue will refuse it when it arrives (a [`FaultPlan`] decided so).
    venue_rejects: bool,
}

/// Failures the simulated broker injects, deterministically, by the count of placements through the
/// [`Broker`] interface (the first placement is number 1; `every: 3` hits numbers 3, 6, 9...). The
/// first rule that matches a placement applies, in the order listed. A rule with `every: 0` is off.
///
/// These exist so a host can be shown to cope with what a real broker does: refuse, ask to slow
/// down, say nothing, or accept and then reject.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaultPlan {
    /// Placements in these instruments are refused.
    pub refuse_instruments: Vec<InstrumentId>,
    /// Every Nth placement is refused outright.
    pub refuse_every: u32,
    /// Every Nth placement is told to wait `rate_limit_retry_ns`; the order does not happen.
    pub rate_limit_every: u32,
    pub rate_limit_retry_ns: Nanos,
    /// Every Nth placement gets no answer ([`Submission::Unknown`]). Alternate ones arrive at the
    /// venue anyway (the first, third, ...); the others never happen.
    pub unknown_every: u32,
    /// Every Nth placement is accepted by the broker and then refused by the venue on arrival.
    pub venue_reject_every: u32,
}

impl FaultPlan {
    fn hits(every: u32, n: u64) -> bool {
        every != 0 && n % u64::from(every) == 0
    }
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
    /// What the [`Broker`] interface reports, in order.
    events: Vec<BrokerEvent>,
    /// Set by the first placement through the [`Broker`] interface; a run that never uses it keeps no
    /// event log.
    events_on: bool,
    faults: FaultPlan,
    placements: u64,
    unknowns: u64,
    protective: bool,
    legs: Vec<Legs>,
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
            events: Vec::new(),
            events_on: false,
            faults: FaultPlan::default(),
            placements: 0,
            unknowns: 0,
            protective: false,
            legs: Vec::new(),
        }
    }

    /// Run the stops and targets of opening intents (see the module docs). Off by default.
    pub fn with_protective_orders(mut self) -> SimBroker {
        self.protective = true;
        self
    }

    fn note(&mut self, e: BrokerEvent) {
        if self.events_on {
            self.events.push(e);
        }
    }

    /// Inject failures into placements made through the [`Broker`] interface.
    pub fn with_faults(mut self, faults: FaultPlan) -> SimBroker {
        self.faults = faults;
        self
    }

    /// Send an intent. It reaches the venue `latency_ns` after `intent.ts`.
    pub fn submit(&mut self, intent: &Intent) {
        if intent.instrument as usize >= self.books.len()
            || extended_hours_refusal(intent).is_some()
        {
            let u = OrderUpdate::rejected(intent.id, RejectReason::Broker, intent.ts);
            self.updates.push(u);
            return;
        }
        let id = OrderId(self.next_order);
        self.next_order += 1;
        self.live_as(intent, id, false);
    }

    fn live_as(&mut self, intent: &Intent, id: OrderId, venue_rejects: bool) {
        self.live.push(Live {
            order: Order::new(id, *intent),
            arrival: intent.ts.saturating_add(self.cfg.latency_ns),
            venue_rejects,
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
            self.note(BrokerEvent {
                order: l.order.id,
                ts,
                kind: Kind::Close(OrderState::Cancelled),
            });
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
                self.match_legs(ev.instrument(), ts);
            }
            Event::Trade(t) => {
                book.last_px = t.px.raw();
                self.trigger_stops(ev.instrument(), t.px.raw());
                self.match_legs(ev.instrument(), ts);
            }
            Event::Status(s) => match s.kind {
                StatusKind::TradingHalt => book.halted = true,
                StatusKind::TradingResume => {
                    book.halted = false;
                    self.match_resting(ev.instrument(), ts);
                    self.match_legs(ev.instrument(), ts);
                }
                StatusKind::LuldBand | StatusKind::ShortSaleRestriction => {}
            },
            _ => {}
        }
    }

    /// The end of the session: orders that reached the venue and are still working
    /// expire (good-til-cancelled ones do not), the protective orders end, and borrow accrues to `ts`.
    pub fn end_of_day(&mut self, ts: Nanos) {
        self.release(ts);
        self.accrue(ts);
        let mut expired = Vec::new();
        self.legs.clear();
        for l in &mut self.live {
            if l.order.state() != OrderState::Pending
                && l.order.intent.tif != Tif::Gtc
                && l.order.transition(OrderState::Expired).is_ok()
            {
                self.updates.push(l.order.update(ts));
                expired.push(l.order.id);
            }
        }
        for order in expired {
            self.note(BrokerEvent {
                order,
                ts,
                kind: Kind::Close(OrderState::Expired),
            });
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
            if self.live[i].venue_rejects {
                // Refused on arrival, never acknowledged.
                o.transition(OrderState::Rejected).expect("pending rejects");
                let mut u = o.update(at);
                u.reject = Some(RejectReason::Broker);
                self.updates.push(u);
                self.note(BrokerEvent {
                    order: o.id,
                    ts: at,
                    kind: Kind::Close(OrderState::Rejected),
                });
                self.live[i].order = o;
                self.live.retain(|l| !l.order.state().is_terminal());
                continue;
            }
            o.transition(OrderState::Accepted).expect("pending accepts");
            self.updates.push(o.update(at));
            self.note(BrokerEvent {
                order: o.id,
                ts: at,
                kind: Kind::Ack,
            });
            self.try_fill(&mut o, at);
            if o.intent.tif == Tif::Ioc && !o.state().is_terminal() {
                o.transition(OrderState::Expired)
                    .expect("a working order expires");
                self.updates.push(o.update(at));
                self.note(BrokerEvent {
                    order: o.id,
                    ts: at,
                    kind: Kind::Close(OrderState::Expired),
                });
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
            leg: None,
        });
        self.updates.push(o.update(ts));
        self.note(BrokerEvent {
            order: o.id,
            ts,
            kind: Kind::Fill { qty, px },
        });
        if self.protective && intent.protect.is_some() && intent.purpose == Purpose::Open {
            match self.legs.iter_mut().find(|l| l.order == o.id) {
                Some(l) => l.entered += qty,
                None => self.legs.push(Legs {
                    order: o.id,
                    intent,
                    entered: qty,
                    exited: 0,
                    triggered: false,
                }),
            }
        }
    }

    /// A trade at or through a stop's trigger arms it, for good.
    fn trigger_stops(&mut self, instrument: InstrumentId, px: i64) {
        for l in &mut self.legs {
            if l.intent.instrument != instrument || l.triggered {
                continue;
            }
            let Some(p) = l.intent.protect else { continue };
            let long = l.intent.side.is_buy();
            if (long && px <= p.stop_trigger.raw()) || (!long && px >= p.stop_trigger.raw()) {
                l.triggered = true;
            }
        }
    }

    /// The protective orders of `instrument` try the current quote: the target first (a resting limit), then a
    /// triggered stop. Each fill uses the shares both share.
    fn match_legs(&mut self, instrument: InstrumentId, ts: Nanos) {
        for i in 0..self.legs.len() {
            if self.legs[i].intent.instrument != instrument {
                continue;
            }
            for leg in [Leg::Target, Leg::Stop] {
                self.try_leg(i, leg, ts);
            }
        }
        // Done: nothing left to protect and the opening order is no longer filling.
        let live = &self.live;
        let pos = &self.pos;
        self.legs.retain(|l| {
            let left = l.entered - l.exited > 0 && leg_cap(l, pos) > 0;
            left || live.iter().any(|x| x.order.id == l.order)
        });
    }

    fn try_leg(&mut self, i: usize, leg: Leg, ts: Nanos) {
        let l = self.legs[i];
        let Some(p) = l.intent.protect else { return };
        let long = l.intent.side.is_buy();
        let book = &mut self.books[l.intent.instrument as usize];
        if book.halted {
            return;
        }
        let left = (l.entered - l.exited).min(leg_cap(&l, &self.pos));
        if left == 0 {
            return;
        }
        // A long's legs sell at the bid, a short's buy at the ask.
        let (best, avail) = if long {
            (book.bid, &mut book.bid_rem)
        } else {
            (book.ask, &mut book.ask_rem)
        };
        if best <= 0 {
            return;
        }
        let (reference, ok) = match leg {
            Leg::Target => {
                let Some(t) = p.take_profit else { return };
                (
                    t.raw(),
                    if long {
                        best >= t.raw()
                    } else {
                        best <= t.raw()
                    },
                )
            }
            Leg::Stop => {
                if !l.triggered {
                    return;
                }
                match p.stop_limit {
                    None => (p.stop_trigger.raw(), true),
                    Some(lim) => (
                        p.stop_trigger.raw(),
                        if long {
                            best >= lim.raw()
                        } else {
                            best <= lim.raw()
                        },
                    ),
                }
            }
        };
        let qty = left.min(*avail);
        if !ok || qty == 0 {
            return;
        }
        *avail -= qty;
        let px = Px::from_raw(best);
        self.accrue(ts);
        let side = if long { Side::Sell } else { Side::Buy };
        self.pos[l.intent.instrument as usize] += if long {
            -i64::from(qty)
        } else {
            i64::from(qty)
        };
        self.legs[i].exited += qty;
        self.fills.push(Fill {
            order: l.order,
            intent: l.intent.id,
            instrument: l.intent.instrument,
            side,
            qty,
            px,
            ts,
            slippage: if long {
                reference - best
            } else {
                best - reference
            },
            leg: Some(leg),
        });
        self.note(BrokerEvent {
            order: l.order,
            ts,
            kind: Kind::LegFill { leg, qty, px },
        });
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

/// The most the legs may still take: the position in the direction they protect.
fn leg_cap(l: &Legs, pos: &[i64]) -> u32 {
    let p = pos[l.intent.instrument as usize];
    let held = if l.intent.side.is_buy() { p } else { -p };
    u32::try_from(held.max(0)).unwrap_or(u32::MAX)
}

impl Broker for SimBroker {
    fn place(&mut self, intent: &Intent, order: OrderId) -> Submission {
        self.events_on = true;
        self.placements += 1;
        let n = self.placements;
        let f = &self.faults;
        if intent.instrument as usize >= self.books.len() {
            return Submission::Refused {
                code: 400,
                message: "unknown instrument".to_owned(),
            };
        }
        if let Some(r) = extended_hours_refusal(intent) {
            return Submission::Refused {
                code: ExtendedHoursRefusal::CODE,
                message: r.message().to_owned(),
            };
        }
        if f.refuse_instruments.contains(&intent.instrument) || FaultPlan::hits(f.refuse_every, n) {
            return Submission::Refused {
                code: 422,
                message: "simulated refusal".to_owned(),
            };
        }
        if FaultPlan::hits(f.rate_limit_every, n) {
            return Submission::RateLimited {
                retry_after: f.rate_limit_retry_ns,
            };
        }
        if FaultPlan::hits(f.unknown_every, n) {
            self.unknowns += 1;
            if self.unknowns % 2 == 1 {
                self.live_as(intent, order, false);
            }
            return Submission::Unknown;
        }
        let venue_rejects = FaultPlan::hits(f.venue_reject_every, n);
        self.live_as(intent, order, venue_rejects);
        Submission::Accepted
    }

    fn cancel_order(&mut self, order: OrderId, ts: Nanos) -> CancelOutcome {
        let Some(intent) = self
            .live
            .iter()
            .find(|l| l.order.id == order)
            .map(|l| l.order.intent.id)
        else {
            return CancelOutcome::Finished;
        };
        if self.cancel(intent, ts) {
            CancelOutcome::Requested
        } else {
            CancelOutcome::Finished
        }
    }

    fn observe(&mut self, ev: &Event) {
        self.on_event(ev);
    }

    fn close_day(&mut self, ts: Nanos) {
        self.end_of_day(ts);
    }

    fn take_events(&mut self) -> Vec<BrokerEvent> {
        std::mem::take(&mut self.events)
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
