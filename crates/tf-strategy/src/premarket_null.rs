//! T26, the null of the premarket strategy: what buying active names at random times in the premarket earns (ADR 0077).
//!
//! The null of T04 ([`crate::random_entries`]) draws names from the universe and times from a window before the close. That cannot
//! stand for T25 ([`crate::premarket_pullback`]): in the premarket most of a universe has not traded, so most draws would find no quote.
//! This null keeps what T25 does not decide on its signal and takes away the signal:
//!
//! - **Names that are active.** At each entry time it buys a name drawn at random from the members that have traded at least
//!   `min_dollars` and `min_trades` in the premarket so far, with a last price between `min_cents` and `max_cents`, a quote whose
//!   spread is at most `spread_cap_bp` of the mid, and not halted: the filters of T25 that do not depend on a spike or a pullback.
//! - **Random times.** `names` entry times a day, uniform to the second between `window_start_minutes` and `window_end_minutes`
//!   after 04:00. The draw is of the seed and the day, so a seed repeats a day and another seed or day draws again.
//! - **The same exits and order.** A collar at the ask for `dollars`, a day order, no protective order; a stop `stop_permille` under
//!   the fill, raised to `trail_permille` under the highest price since the entry, and a time exit `flat_minutes` before the open. The
//!   only difference from T25's exits is the first stop: T25's is under the pullback's low, which the null does not have.
//!
//! What T25 earns above this, per trade after costs, is what its spike and pullback add to buying an active name at a random time.

use std::collections::{BTreeMap, BTreeSet};

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_stats::rng::SplitMix64;

use crate::cross::{CrossStrategy, MemberView};
use crate::exits::{ExitBook, ExitPlan, ExitStats};
use crate::intent::{IntentId, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};
use crate::trace::Trace;

pub use crate::closing_reversal::ParamError;

/// The `reason` of an entry.
pub const REASON_ENTRY: u16 = 1;

/// The entry of the `k`th draw is timer `ENTRY_TIMERS + k`.
const ENTRY_TIMERS: u32 = 100;
const MAX_NAMES: u32 = 10_000;
/// The exit book's timers are this plus the instrument number.
const BOOK_TIMERS: u32 = 0x1000_0000;
/// Seconds between looks at the names held, for the trailing stop.
const REVIEW_SECS: u64 = 5;

/// What a null is. Every field, the seed included, is in the text of [`PremarketNullParams::render`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PremarketNullParams {
    pub seed: u64,
    /// Entry times drawn a day.
    pub names: u32,
    pub dollars: u32,
    /// The earliest and latest entry, in minutes after 04:00.
    pub window_start_minutes: u32,
    pub window_end_minutes: u32,
    /// A name must have traded this many dollars and trades in the premarket so far.
    pub min_dollars: u32,
    pub min_trades: u32,
    pub min_cents: u32,
    pub max_cents: u32,
    pub spread_cap_bp: u32,
    pub collar_permille: u32,
    /// The first stop, this many permille under the fill.
    pub stop_permille: u32,
    pub trail_permille: u32,
    pub flat_minutes: u32,
}

impl Default for PremarketNullParams {
    /// The null of T25 at its defaults: three entries a day between 04:20 and 09:15.
    fn default() -> Self {
        PremarketNullParams {
            seed: 1,
            names: 3,
            dollars: 1_000,
            window_start_minutes: 20,
            window_end_minutes: 315,
            min_dollars: 50_000,
            min_trades: 20,
            min_cents: 100,
            max_cents: 3_000,
            spread_cap_bp: 100,
            collar_permille: 10,
            stop_permille: 30,
            trail_permille: 30,
            flat_minutes: 5,
        }
    }
}

const KEYS: [&str; 14] = [
    "seed",
    "names",
    "dollars",
    "window_start_minutes",
    "window_end_minutes",
    "min_dollars",
    "min_trades",
    "min_cents",
    "max_cents",
    "spread_cap_bp",
    "collar_permille",
    "stop_permille",
    "trail_permille",
    "flat_minutes",
];

impl PremarketNullParams {
    fn values(&self) -> [u64; 14] {
        [
            self.seed,
            u64::from(self.names),
            u64::from(self.dollars),
            u64::from(self.window_start_minutes),
            u64::from(self.window_end_minutes),
            u64::from(self.min_dollars),
            u64::from(self.min_trades),
            u64::from(self.min_cents),
            u64::from(self.max_cents),
            u64::from(self.spread_cap_bp),
            u64::from(self.collar_permille),
            u64::from(self.stop_permille),
            u64::from(self.trail_permille),
            u64::from(self.flat_minutes),
        ]
    }

    /// What the null can run with.
    pub fn validate(&self) -> Result<(), ParamError> {
        let bad = |m: &str| Err(ParamError(m.to_owned()));
        if self.names == 0 || self.names > MAX_NAMES {
            return bad("names must be from 1 to 10000");
        }
        if self.dollars == 0 {
            return bad("dollars must be at least 1");
        }
        if self.window_start_minutes == 0 || self.window_end_minutes < self.window_start_minutes {
            return bad("the window must satisfy 1 <= window_start_minutes <= window_end_minutes");
        }
        if self.flat_minutes == 0 || self.window_end_minutes + self.flat_minutes >= 330 {
            return bad(
                "window_end_minutes plus flat_minutes must be under 330 (the premarket is 330 minutes)",
            );
        }
        if self.min_cents == 0 || self.max_cents < self.min_cents {
            return bad("min_cents must be at least 1 and no more than max_cents");
        }
        if self.collar_permille > 999 {
            return bad("collar_permille must be below 1000");
        }
        if self.stop_permille == 0 || self.stop_permille > 999 {
            return bad("stop_permille must be from 1 to 999");
        }
        if self.trail_permille == 0 || self.trail_permille > 999 {
            return bad("trail_permille must be from 1 to 999");
        }
        Ok(())
    }

    /// The text of the parameters: every one, in a fixed order, `key=value` separated by spaces.
    pub fn render(&self) -> String {
        KEYS.iter()
            .zip(self.values())
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Parameters from their text: every key once and no other, and values that pass [`PremarketNullParams::validate`].
    pub fn parse(text: &str) -> Result<PremarketNullParams, ParamError> {
        let mut got: BTreeMap<&str, u64> = BTreeMap::new();
        for w in text.split_whitespace() {
            let Some((k, v)) = w.split_once('=') else {
                return Err(ParamError(format!("`{w}` is not key=value")));
            };
            if !KEYS.contains(&k) {
                return Err(ParamError(format!(
                    "`{k}` is not a parameter of this strategy"
                )));
            }
            let v: u64 = v
                .parse()
                .map_err(|_| ParamError(format!("`{v}` is not a whole number for {k}")))?;
            if got.insert(k, v).is_some() {
                return Err(ParamError(format!("{k} is given twice")));
            }
        }
        let mut vals = [0u64; 14];
        for (i, k) in KEYS.iter().enumerate() {
            vals[i] = *got
                .get(k)
                .ok_or_else(|| ParamError(format!("{k} is missing")))?;
        }
        let small = |i: usize| {
            u32::try_from(vals[i]).map_err(|_| ParamError(format!("{} is too large", KEYS[i])))
        };
        let p = PremarketNullParams {
            seed: vals[0],
            names: small(1)?,
            dollars: small(2)?,
            window_start_minutes: small(3)?,
            window_end_minutes: small(4)?,
            min_dollars: small(5)?,
            min_trades: small(6)?,
            min_cents: small(7)?,
            max_cents: small(8)?,
            spread_cap_bp: small(9)?,
            collar_permille: small(10)?,
            stop_permille: small(11)?,
            trail_permille: small(12)?,
            flat_minutes: small(13)?,
        };
        p.validate()?;
        Ok(p)
    }
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PremarketNullStats {
    pub days: u64,
    /// Entry times drawn.
    pub drawn: u64,
    pub entries: u64,
    /// Times that came with no name to buy: none active, or none with a fair quote.
    pub skipped: u64,
    pub exits: ExitStats,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    instrument: InstrumentId,
    seen: u32,
}

pub struct PremarketNull {
    id: StrategyId,
    p: PremarketNullParams,
    book: ExitBook,
    open: Option<Nanos>,
    rng: SplitMix64,
    /// Names bought today: one trade a name, as T25.
    bought: BTreeSet<InstrumentId>,
    /// The highest price since the entry, for the names held or being bought.
    since_entry: BTreeMap<InstrumentId, i64>,
    entries: BTreeMap<IntentId, Entry>,
    stats: PremarketNullStats,
    tracing: bool,
    traces: Vec<Trace>,
}

impl PremarketNull {
    /// The null numbered `id` with `p` (which must pass [`PremarketNullParams::validate`]).
    pub fn new(id: u16, p: PremarketNullParams) -> Result<PremarketNull, ParamError> {
        p.validate()?;
        Ok(PremarketNull {
            id: StrategyId(id),
            p,
            book: ExitBook::new(BOOK_TIMERS),
            open: None,
            rng: SplitMix64::new(p.seed),
            bought: BTreeSet::new(),
            since_entry: BTreeMap::new(),
            entries: BTreeMap::new(),
            stats: PremarketNullStats::default(),
            tracing: false,
            traces: Vec::new(),
        })
    }

    pub fn params(&self) -> &PremarketNullParams {
        &self.p
    }

    pub fn stats(&self) -> PremarketNullStats {
        PremarketNullStats {
            exits: self.book.stats(),
            ..self.stats
        }
    }

    /// The day's draw: the seed and the open, so that every day is its own draw and a seed repeats it.
    fn start_day(&mut self, ctx: &mut Ctx<'_>, premarket: Nanos, open: Nanos) {
        self.open = Some(open);
        self.bought.clear();
        self.since_entry.clear();
        self.stats.days += 1;
        self.rng = SplitMix64::new(
            self.p
                .seed
                .wrapping_add(open.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        );
        let lo = u64::from(self.p.window_start_minutes) * 60;
        let span = u64::from(self.p.window_end_minutes) * 60 - lo + 1;
        let mut secs = Vec::new();
        for k in 0..self.p.names {
            let s = lo + self.rng.below(span as usize) as u64;
            ctx.set_timer(
                TimerId(ENTRY_TIMERS + k),
                premarket.saturating_add(s * NANOS_PER_SEC),
            );
            secs.push(s);
            self.stats.drawn += 1;
        }
        if self.tracing {
            let mut t = Trace::new(ctx.now(), "draw")
                .with("seed", self.p.seed)
                .with("open", open)
                .with("names", self.p.names)
                .with_columns(&["k", "secs_after_premarket"]);
            for (k, s) in secs.iter().enumerate() {
                t.push_row(vec![k.to_string(), s.to_string()]);
            }
            self.traces.push(t);
        }
    }

    /// Whether `id` may be bought now: active, in the price band, with a fair quote.
    fn eligible(&self, view: &MemberView<'_>, id: InstrumentId) -> Option<(i64, i64)> {
        let p = &self.p;
        if self.bought.contains(&id) {
            return None;
        }
        let st = view.state(id)?;
        let dollars = u64::try_from(st.notional / 1_000_000_000).unwrap_or(u64::MAX);
        if st.halted || dollars < u64::from(p.min_dollars) || st.trades < p.min_trades {
            return None;
        }
        let cents = st.last_px.map_or(0, |x| x.raw() / 10_000_000);
        if cents < i64::from(p.min_cents) || cents > i64::from(p.max_cents) {
            return None;
        }
        let (bid, ask) = (st.bid?.0.raw(), st.ask?.0.raw());
        if bid <= 0 || ask < bid {
            return None;
        }
        if p.spread_cap_bp > 0 {
            let mid2 = i128::from(bid) + i128::from(ask);
            if i128::from(ask - bid) * 20_000 > i128::from(p.spread_cap_bp) * mid2 {
                return None;
            }
        }
        Some((bid, ask))
    }

    /// An entry time has come: buy a name drawn at random from those that can be bought now.
    fn enter(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, k: u32) {
        let pool: Vec<(InstrumentId, i64)> = view
            .ids()
            .filter_map(|id| self.eligible(view, id).map(|(_, ask)| (id, ask)))
            .collect();
        let mut result = "no_name";
        let mut chosen = None;
        if !pool.is_empty() {
            let (id, ask) = pool[self.rng.below(pool.len())];
            chosen = Some(id);
            let qty = u128::from(self.p.dollars) * 1_000_000_000 / ask.max(1) as u128;
            result = match u32::try_from(qty).ok().filter(|&q| q > 0) {
                None => "no_share",
                Some(qty) => {
                    let req = Request {
                        side: Side::Buy,
                        qty,
                        purpose: Purpose::Open,
                        pricing: Pricing::Collar {
                            reference: Px::from_raw(ask),
                            collar_permille: self.p.collar_permille,
                        },
                        protect: None,
                        tif: Tif::Day,
                        reason: REASON_ENTRY,
                    };
                    match ctx.submit(id, req) {
                        Ok(intent) => {
                            self.stats.entries += 1;
                            self.bought.insert(id);
                            self.since_entry.insert(id, ask);
                            self.entries.insert(
                                intent,
                                Entry {
                                    instrument: id,
                                    seen: 0,
                                },
                            );
                            "entered"
                        }
                        Err(_) => "refused",
                    }
                }
            };
        }
        if result != "entered" {
            self.stats.skipped += 1;
        }
        if self.tracing {
            let mut t = Trace::new(ctx.now(), "entry")
                .with("k", k)
                .with("pool", pool.len())
                .with("result", result);
            if let Some(id) = chosen {
                t = t.with("instrument", id);
            }
            self.traces.push(t);
        }
    }

    /// Raise the stop of each name held to the trail under its highest price since the entry.
    fn trail(&mut self, view: &MemberView<'_>) {
        let keep = i128::from(1_000 - self.p.trail_permille);
        for (&id, high) in &mut self.since_entry {
            let Some(last) = view
                .state(id)
                .and_then(|s| s.last_px)
                .map(|p| p.raw())
                .filter(|&p| p > 0)
            else {
                continue;
            };
            *high = (*high).max(last);
            if let Ok(stop) = i64::try_from(i128::from(*high) * keep / 1_000) {
                self.book.raise_stop(id, Px::from_raw(stop));
            }
        }
    }
}

impl CrossStrategy for PremarketNull {
    /// Only for the exit book's stop.
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        self.id
    }

    fn period(&self) -> Nanos {
        REVIEW_SECS * NANOS_PER_SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let Some((premarket, open)) = ctx.day().map(|d| (d.premarket, d.open)) else {
            return;
        };
        if self.open != Some(open) {
            self.start_day(ctx, premarket, open);
        }
        self.trail(view);
    }

    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>, ev: &Event) {
        if self.book.is_empty() {
            return;
        }
        if let Event::Trade(t) = ev {
            self.book.on_trade(ctx, t.hdr.instrument, t.px);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, timer: TimerId) {
        match timer.0.checked_sub(ENTRY_TIMERS) {
            Some(k) if k < self.p.names => self.enter(ctx, view, k),
            _ => {
                self.book.on_timer(ctx, timer);
            }
        }
    }

    fn set_tracing(&mut self, on: bool) {
        self.tracing = on;
        if !on {
            self.traces.clear();
        }
    }

    fn take_traces(&mut self) -> Vec<Trace> {
        std::mem::take(&mut self.traces)
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, update: &OrderUpdate) {
        let Some(e) = self.entries.get_mut(&update.intent) else {
            self.book.on_order_update(ctx, update);
            return;
        };
        let new = update.filled_qty.saturating_sub(e.seen);
        e.seen = update.filled_qty;
        let instrument = e.instrument;
        if update.state.is_terminal() {
            self.entries.remove(&update.intent);
            if update.filled_qty == 0 {
                self.since_entry.remove(&instrument);
            }
        }
        let (Some(open), Some(avg)) = (self.open, update.avg_px) else {
            return;
        };
        let stop = i128::from(avg.raw()) * i128::from(1_000 - self.p.stop_permille) / 1_000;
        let plan = ExitPlan {
            stop: i64::try_from(stop).ok().map(Px::from_raw),
            flat_by: Some(open.saturating_sub(u64::from(self.p.flat_minutes) * 60 * NANOS_PER_SEC)),
            collar_permille: self.p.collar_permille,
            ..ExitPlan::new()
        };
        // Nothing filled adds nothing: the book ignores a size of 0.
        self.book.arm(ctx, instrument, true, new, plan);
    }
}
