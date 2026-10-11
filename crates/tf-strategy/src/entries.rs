//! Entries a strategy holds itself (E19-S50): the engine's trade watch for a break.
//!
//! A stop-entry ("buy when the price trades through the high of the range") is not an order the broker takes before the open, and a stop
//! resting at the broker is a different order from the one a simulation models. An [`EntryBook`] holds the trigger in the strategy instead,
//! as the [`ExitBook`](crate::exits::ExitBook) holds an exit: the strategy arms it with a level, a size, a collar and the time it
//! lapses, forwards trades and timers to it, and at the **first trade at or through the level** (at or above it for a buy, at or below it for
//! a short sale) it sends a marketable limit a collar away from that trade's price, **once**.
//!
//! - A trigger is for one instrument and one side; a name can have a buy and a short sale armed together (the two sides of a range). The
//!   strategy disarms the other when one fires.
//! - It does not fire before `not_before` (armed from then) or at or after `lapse_at` (given up, counted, and a timer cleans it up if no trade
//!   comes). Arming again for the same instrument and side replaces the trigger.
//! - Once it has fired it is gone, whether the order was accepted or the strategy's own check refused it: a name that is held already is refused
//!   by that check ([`EntryBook::on_trade_checked`]) and counted, and nothing is sent.
//! - The order carries the trigger's reason code and protective orders (a broker-side stop is required in the regular session, and fixed when the
//!   trigger is armed).
//!
//! Deterministic: it reads no clock (the strategy's `ctx.now()` is event time), keeps its triggers in order, and a replay of the same events
//! sends the same orders.

use std::collections::BTreeMap;

use tf_core::{InstrumentId, Nanos, Px};

use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, Tif};
use crate::strategy::{Ctx, Request, TimerId};

/// One trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryTrigger {
    /// `Buy` fires at a trade at or above `level`; `SellShort` at or below it. Nothing else is an entry.
    pub side: Side,
    pub level: Px,
    pub qty: u32,
    /// Not armed before this event time (it fires on the first trade through the level from then).
    pub not_before: Option<Nanos>,
    /// Given up at this event time: a trade at or after it fires nothing.
    pub lapse_at: Option<Nanos>,
    /// How far the order's limit may be from the price of the trade that fired it, in permille (0 to 999).
    pub collar_permille: u32,
    pub tif: Tif,
    /// The broker-side stop (and target) sent with the order, fixed now; required in the regular session.
    pub protect: Option<Protective>,
    /// The `reason` code of the order.
    pub reason: u16,
}

/// Why a trigger was not armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmError {
    /// The side is not `Buy` or `SellShort`.
    Side,
    Qty,
    /// A level that is not a price.
    Level,
    /// A collar above 999 permille.
    Collar,
    /// It would lapse at or before now (or before it is armed).
    Lapsed,
    /// The instrument number leaves no room for the book's timers.
    Timer,
}

/// A trigger that fired and sent an order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fired {
    pub id: InstrumentId,
    pub side: Side,
    pub intent: IntentId,
    pub qty: u32,
    pub level: Px,
    /// The trade that fired it.
    pub px: Px,
    pub reason: u16,
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryStats {
    pub armed: u64,
    pub fired: u64,
    /// Fired but not sent: the strategy's own check or the intent's validation refused it.
    pub refused: u64,
    /// Given up at `lapse_at`.
    pub lapsed: u64,
    pub disarmed: u64,
}

/// The triggers of one strategy, by instrument and side.
pub struct EntryBook {
    timer_base: u32,
    pending: BTreeMap<(InstrumentId, bool), EntryTrigger>,
    stats: EntryStats,
}

impl EntryBook {
    /// `timer_base`: the book uses timers `timer_base + 2 * instrument id` (and one more for a short sale) to clean up a lapsed trigger;
    /// the strategy keeps those clear of its own.
    pub fn new(timer_base: u32) -> EntryBook {
        EntryBook {
            timer_base,
            pending: BTreeMap::new(),
            stats: EntryStats::default(),
        }
    }

    fn timer(&self, id: InstrumentId, long: bool) -> Option<TimerId> {
        id.checked_mul(2)?
            .checked_add(u32::from(!long))?
            .checked_add(self.timer_base)
            .map(TimerId)
    }

    /// Hold `trigger` for `id`, replacing the one held for the same side.
    pub fn arm(
        &mut self,
        ctx: &mut Ctx<'_>,
        id: InstrumentId,
        trigger: EntryTrigger,
    ) -> Result<(), ArmError> {
        let long = match trigger.side {
            Side::Buy => true,
            Side::SellShort => false,
            Side::Sell => return Err(ArmError::Side),
        };
        if trigger.qty == 0 {
            return Err(ArmError::Qty);
        }
        if trigger.level.raw() <= 0 {
            return Err(ArmError::Level);
        }
        if trigger.collar_permille > 999 {
            return Err(ArmError::Collar);
        }
        let from = trigger.not_before.unwrap_or(0).max(ctx.now());
        if trigger.lapse_at.is_some_and(|t| t <= from) {
            return Err(ArmError::Lapsed);
        }
        let timer = self.timer(id, long).ok_or(ArmError::Timer)?;
        match trigger.lapse_at {
            Some(t) => ctx.set_timer(timer, t),
            None => {
                ctx.cancel_timer(timer);
            }
        }
        self.pending.insert((id, long), trigger);
        self.stats.armed += 1;
        Ok(())
    }

    /// Stop holding the trigger of `side` for `id`. True if there was one.
    pub fn disarm(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId, side: Side) -> bool {
        let long = side.is_buy();
        if self.pending.remove(&(id, long)).is_none() {
            return false;
        }
        if let Some(t) = self.timer(id, long) {
            ctx.cancel_timer(t);
        }
        self.stats.disarmed += 1;
        true
    }

    /// Stop holding both triggers of `id`; how many there were.
    pub fn disarm_all(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId) -> u32 {
        u32::from(self.disarm(ctx, id, Side::Buy))
            + u32::from(self.disarm(ctx, id, Side::SellShort))
    }

    pub fn is_armed(&self, id: InstrumentId, side: Side) -> bool {
        self.pending.contains_key(&(id, side.is_buy()))
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn stats(&self) -> EntryStats {
        self.stats
    }

    /// A trade of `id` at `px`: fire what it goes through. See [`EntryBook::on_trade_checked`].
    pub fn on_trade(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId, px: Px) -> Vec<Fired> {
        self.on_trade_checked(ctx, id, px, |_| true)
    }

    /// A trade of `id` at `px`. Each trigger armed for it that is in force (not before its `not_before`, not at or after its `lapse_at`) and
    /// that the price is at or through fires once: `allow` is asked first (the strategy's own check: it refuses a name already held, a name
    /// with no room), and if it says no, or the intent is refused, the trigger is gone and counted as refused and nothing is sent. A trigger
    /// that has lapsed is dropped. The buy is considered before the short sale; the orders sent are returned.
    pub fn on_trade_checked(
        &mut self,
        ctx: &mut Ctx<'_>,
        id: InstrumentId,
        px: Px,
        mut allow: impl FnMut(&EntryTrigger) -> bool,
    ) -> Vec<Fired> {
        let mut sent = Vec::new();
        if self.pending.is_empty() {
            return sent;
        }
        let now = ctx.now();
        for long in [true, false] {
            let Some(&t) = self.pending.get(&(id, long)) else {
                continue;
            };
            if t.lapse_at.is_some_and(|at| now >= at) {
                self.forget(ctx, id, long);
                self.stats.lapsed += 1;
                continue;
            }
            if t.not_before.is_some_and(|at| now < at) {
                continue;
            }
            if !(if long { px >= t.level } else { px <= t.level }) {
                continue;
            }
            self.forget(ctx, id, long);
            self.stats.fired += 1;
            if !allow(&t) {
                self.stats.refused += 1;
                continue;
            }
            let req = Request {
                side: t.side,
                qty: t.qty,
                purpose: Purpose::Open,
                pricing: Pricing::Collar {
                    reference: px,
                    collar_permille: t.collar_permille,
                },
                protect: t.protect,
                tif: t.tif,
                reason: t.reason,
            };
            match ctx.submit(id, req) {
                Ok(intent) => sent.push(Fired {
                    id,
                    side: t.side,
                    intent,
                    qty: t.qty,
                    level: t.level,
                    px,
                    reason: t.reason,
                }),
                Err(_) => self.stats.refused += 1,
            }
        }
        sent
    }

    /// A timer fired. If it is the lapse of a trigger that is due, the trigger is dropped and `(instrument, side)` returned. `None` for a timer
    /// that is not the book's, or whose trigger is gone or not yet due.
    pub fn on_timer(&mut self, ctx: &mut Ctx<'_>, timer: TimerId) -> Option<(InstrumentId, Side)> {
        let slot = timer.0.checked_sub(self.timer_base)?;
        let (id, long) = (slot / 2, slot % 2 == 0);
        let t = self.pending.get(&(id, long))?;
        if !t.lapse_at.is_some_and(|at| ctx.now() >= at) {
            return None;
        }
        self.pending.remove(&(id, long));
        self.stats.lapsed += 1;
        Some((id, if long { Side::Buy } else { Side::SellShort }))
    }

    fn forget(&mut self, ctx: &mut Ctx<'_>, id: InstrumentId, long: bool) {
        self.pending.remove(&(id, long));
        if let Some(t) = self.timer(id, long) {
            ctx.cancel_timer(t);
        }
    }
}
