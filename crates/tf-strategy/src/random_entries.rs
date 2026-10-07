//! T14, the null strategy (E19-S28): what doing nothing clever earns.
//!
//! Every result of the research harness is compared with this one (`docs/research/data-and-protocol.md`, section 3): entries
//! at random times on random names of the same universe, with the same exits, the same sizing and the same costs as the
//! strategy it stands for, run many times with different seeds. Its distribution is what a result has to beat, and its mean
//! after costs is what the exits and the spread alone do (a market with no signal pays the spread and the fees on every
//! round trip, so that mean is a cost).
//!
//! - **What is random.** At its first review of a day it draws, from a seeded generator, `names` distinct members and, for
//!   each, an entry time uniform to the second between `window_start_minutes` and `window_end_minutes` before the close. The
//!   draw is of the seed and the day (the close), so a seed gives the same day the same draw every time, and another seed or
//!   another day another. With the window one instant (start equal to end) the time is not random: the null of a rule that
//!   buys at 15:30 is a strategy that buys *random names* at 15:30.
//! - **What is not.** A name is bought only if it could be: not halted or paused, not under the short-sale restriction, with a
//!   two-sided quote, a price that buys a share and a stop above nothing: the rule of T04, so the null is not charged for
//!   names nobody could trade. The order is a collar around the ask for `dollars`, with the disaster stop the framework
//!   requires of an opening order ([`crate::closing_reversal`]).
//! - **The same exits.** The [`ExitBook`]'s: a time exit `hold_seconds` after the fill, but never later than 30 seconds before
//!   the close, and optionally a stop and a target that many permille from the average fill price (0 for none), for a
//!   reference strategy that has them. The host flattens what is left.

use std::collections::BTreeMap;

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_stats::rng::SplitMix64;

use crate::closing_reversal::{ClosingReversalParams, ParamError};
use crate::cross::{CrossStrategy, MemberView};
use crate::exits::{ExitBook, ExitPlan, ExitStats};
use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};

/// The `reason` code of this strategy's entries.
pub const REASON_ENTRY: u16 = 1;

/// The entry of the `k`th name drawn is timer `ENTRY_TIMERS + k`.
const ENTRY_TIMERS: u32 = 100;
/// The most names a day may draw: below the exit book's timers.
const MAX_NAMES: u32 = 100_000;
/// The exit book's timers are this plus the instrument number.
const BOOK_TIMERS: u32 = 0x1000_0000;
/// The time exit is never later than this many seconds before the close.
const EXIT_MARGIN_SECS: u64 = 30;

/// What a null strategy is. Every field, the seed included, is in the text of [`RandomEntriesParams::render`]: another seed
/// is another variant for the certificate and the trial registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomEntriesParams {
    /// The seed of the draws.
    pub seed: u64,
    /// Names drawn a day (fewer if the universe has fewer).
    pub names: u32,
    /// Dollars bought of each.
    pub dollars: u32,
    /// How far from the ask an entry may fill, and from its trigger an exit, in permille.
    pub collar_permille: u32,
    /// The disaster stop of an entry, in permille under the ask (the framework needs one).
    pub stop_permille: u32,
    /// The earliest an entry is, in minutes before the close.
    pub window_start_minutes: u32,
    /// The latest an entry is, in minutes before the close; at most `window_start_minutes`.
    pub window_end_minutes: u32,
    /// Hold for this many seconds after the fill (but out 30 seconds before the close at the latest).
    pub hold_seconds: u32,
    /// An exit stop this many permille under the average fill price; 0 for none.
    pub exit_stop_permille: u32,
    /// An exit target this many permille over the average fill price; 0 for none.
    pub target_permille: u32,
}

impl Default for RandomEntriesParams {
    /// The null of T04 at its defaults: twenty random names at 15:30, held until 15:59:30.
    fn default() -> Self {
        RandomEntriesParams {
            seed: 1,
            names: 20,
            dollars: 2_000,
            collar_permille: 5,
            stop_permille: 100,
            window_start_minutes: 30,
            window_end_minutes: 30,
            hold_seconds: 1_770,
            exit_stop_permille: 0,
            target_permille: 0,
        }
    }
}

impl From<&ClosingReversalParams> for RandomEntriesParams {
    /// The null of a closing reversal: its names, dollars, collar and disaster stop, entering at its entry time (the window one
    /// instant) and held until its exit time (to within the broker's latency); seed 1, to be set for each seed. The reversal's
    /// own filters (`extreme_bp`, `spread_cap_bp`) choose which names it buys and have no counterpart here: its null buys random
    /// names among those that can be bought, as many as it does.
    fn from(c: &ClosingReversalParams) -> Self {
        RandomEntriesParams {
            seed: 1,
            names: c.names,
            dollars: c.dollars,
            collar_permille: c.collar_permille,
            stop_permille: c.stop_permille,
            window_start_minutes: c.entry_minutes,
            window_end_minutes: c.entry_minutes,
            hold_seconds: c.entry_minutes * 60 - c.exit_seconds,
            exit_stop_permille: 0,
            target_permille: 0,
        }
    }
}

const KEYS: [&str; 10] = [
    "seed",
    "names",
    "dollars",
    "collar_permille",
    "stop_permille",
    "window_start_minutes",
    "window_end_minutes",
    "hold_seconds",
    "exit_stop_permille",
    "target_permille",
];

impl RandomEntriesParams {
    /// What the strategy can run with.
    pub fn validate(&self) -> Result<(), ParamError> {
        let bad = |m: &str| Err(ParamError(m.to_owned()));
        if self.names == 0 || self.names > MAX_NAMES {
            return bad("names must be from 1 to 100000");
        }
        if self.dollars == 0 {
            return bad("dollars must be at least 1");
        }
        if self.collar_permille > 999 {
            return bad("collar_permille must be below 1000");
        }
        if self.stop_permille == 0 || self.stop_permille > 999 {
            return bad("stop_permille must be from 1 to 999: an entry has to carry a stop");
        }
        if self.window_end_minutes == 0 || self.window_end_minutes > self.window_start_minutes {
            return bad("window_end_minutes must be from 1 to window_start_minutes");
        }
        if self.window_start_minutes > 390 {
            return bad("window_start_minutes must be at most 390 (a session is 390 minutes)");
        }
        if self.hold_seconds == 0 {
            return bad("hold_seconds must be at least 1");
        }
        if self.exit_stop_permille > 999 {
            return bad("exit_stop_permille must be below 1000");
        }
        if self.target_permille > 1_000_000 {
            return bad("target_permille must be at most 1000000");
        }
        Ok(())
    }

    fn values(&self) -> [u64; 10] {
        [
            self.seed,
            u64::from(self.names),
            u64::from(self.dollars),
            u64::from(self.collar_permille),
            u64::from(self.stop_permille),
            u64::from(self.window_start_minutes),
            u64::from(self.window_end_minutes),
            u64::from(self.hold_seconds),
            u64::from(self.exit_stop_permille),
            u64::from(self.target_permille),
        ]
    }

    /// The text of the parameters: every one, in a fixed order, `key=value` separated by spaces.
    pub fn render(&self) -> String {
        KEYS.iter()
            .zip(self.values())
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Parameters from their text: every key once and no other, and values that pass [`RandomEntriesParams::validate`].
    pub fn parse(text: &str) -> Result<RandomEntriesParams, ParamError> {
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
        let mut vals = [0u64; 10];
        for (i, k) in KEYS.iter().enumerate() {
            vals[i] = *got
                .get(k)
                .ok_or_else(|| ParamError(format!("{k} is missing")))?;
        }
        let small = |i: usize| {
            u32::try_from(vals[i]).map_err(|_| ParamError(format!("{} is too large", KEYS[i])))
        };
        let p = RandomEntriesParams {
            seed: vals[0],
            names: small(1)?,
            dollars: small(2)?,
            collar_permille: small(3)?,
            stop_permille: small(4)?,
            window_start_minutes: small(5)?,
            window_end_minutes: small(6)?,
            hold_seconds: small(7)?,
            exit_stop_permille: small(8)?,
            target_permille: small(9)?,
        };
        p.validate()?;
        Ok(p)
    }
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RandomEntriesStats {
    /// Days drawn for.
    pub days: u64,
    /// Names drawn, and so entry times set.
    pub drawn: u64,
    /// Entries sent.
    pub entries: u64,
    /// Names drawn that could not be bought when their time came (halted, restricted, no quote, no share, no stop).
    pub skipped: u64,
    /// The exits held and sent.
    pub exits: ExitStats,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    instrument: InstrumentId,
    seen: u32,
}

pub struct RandomEntries {
    id: StrategyId,
    p: RandomEntriesParams,
    book: ExitBook,
    /// The regular close of the day the draw was made for.
    close: Option<Nanos>,
    /// The names drawn, in the order of their entry timers.
    picks: Vec<InstrumentId>,
    entries: BTreeMap<IntentId, Entry>,
    stats: RandomEntriesStats,
}

impl RandomEntries {
    /// The null strategy numbered `id` with `p` (which must pass [`RandomEntriesParams::validate`]).
    pub fn new(id: u16, p: RandomEntriesParams) -> Result<RandomEntries, ParamError> {
        p.validate()?;
        Ok(RandomEntries {
            id: StrategyId(id),
            p,
            book: ExitBook::new(BOOK_TIMERS),
            close: None,
            picks: Vec::new(),
            entries: BTreeMap::new(),
            stats: RandomEntriesStats::default(),
        })
    }

    pub fn params(&self) -> &RandomEntriesParams {
        &self.p
    }

    pub fn stats(&self) -> RandomEntriesStats {
        RandomEntriesStats {
            exits: self.book.stats(),
            ..self.stats
        }
    }

    /// The generator of a day: the seed and the close, so that every day is its own draw and a seed repeats it.
    fn day_rng(&self, close: Nanos) -> SplitMix64 {
        SplitMix64::new(
            self.p
                .seed
                .wrapping_add(close.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        )
    }

    /// Draw the day's names and set an entry timer for each.
    fn start_day(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, close: Nanos) {
        self.close = Some(close);
        self.picks.clear();
        self.stats.days += 1;
        let mut ids: Vec<InstrumentId> = view.ids().collect();
        let n = ids.len();
        let k = (self.p.names as usize).min(n);
        let mut rng = self.day_rng(close);
        // The first k of a shuffle: distinct names, each member as likely as any other.
        for i in 0..k {
            let j = i + rng.below(n - i);
            ids.swap(i, j);
        }
        ids.truncate(k);
        let lo = u64::from(self.p.window_end_minutes) * 60;
        let span = u64::from(self.p.window_start_minutes) * 60 - lo + 1;
        for (i, id) in ids.into_iter().enumerate() {
            let secs = lo + rng.below(span as usize) as u64;
            ctx.set_timer(
                TimerId(ENTRY_TIMERS + i as u32),
                close.saturating_sub(secs * NANOS_PER_SEC),
            );
            self.picks.push(id);
            self.stats.drawn += 1;
        }
    }

    /// Buy the name drawn, if it can be bought now.
    fn enter(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, id: InstrumentId) {
        // Not reachable: the names drawn are members. Kept so that a name the view does not know is a skip, not a panic.
        let Some(st) = view.state(id) else {
            self.stats.skipped += 1;
            return;
        };
        let quote = st.bid.zip(st.ask).map(|(b, a)| (b.0.raw(), a.0));
        let Some((_, ask)) = quote.filter(|&(b, a)| !st.halted && !st.ssr && b > 0 && a.raw() >= b)
        else {
            self.stats.skipped += 1;
            return;
        };
        let qty = u128::from(self.p.dollars) * 1_000_000_000 / ask.raw().max(1) as u128;
        let stop = i128::from(ask.raw()) * i128::from(1_000 - self.p.stop_permille) / 1_000;
        let (Ok(qty), Ok(stop)) = (u32::try_from(qty), i64::try_from(stop)) else {
            self.stats.skipped += 1;
            return;
        };
        if qty == 0 || stop <= 0 {
            self.stats.skipped += 1;
            return;
        }
        let req = Request {
            side: Side::Buy,
            qty,
            purpose: Purpose::Open,
            pricing: Pricing::Collar {
                reference: ask,
                collar_permille: self.p.collar_permille,
            },
            protect: Some(Protective {
                stop_trigger: Px::from_raw(stop),
                stop_limit: None,
                take_profit: None,
            }),
            tif: Tif::Day,
            reason: REASON_ENTRY,
        };
        match ctx.submit(id, req) {
            Ok(intent) => {
                self.stats.entries += 1;
                self.entries.insert(
                    intent,
                    Entry {
                        instrument: id,
                        seen: 0,
                    },
                );
            }
            // Not reachable with parameters that pass `validate`, as for the closing reversal; kept for the day the
            // framework's checks change.
            Err(_) => self.stats.skipped += 1,
        }
    }
}

impl CrossStrategy for RandomEntries {
    /// Only for the exit book's stop and target; with none of them the handler returns at once.
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        self.id
    }

    /// A minute: the review only draws the day, once.
    fn period(&self) -> Nanos {
        60 * NANOS_PER_SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let Some(close) = ctx.day().map(|d| d.close) else {
            return;
        };
        if self.close != Some(close) {
            self.start_day(ctx, view, close);
        }
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
        let drawn = timer
            .0
            .checked_sub(ENTRY_TIMERS)
            .and_then(|k| self.picks.get(k as usize).copied());
        match drawn {
            Some(id) => self.enter(ctx, view, id),
            None => {
                self.book.on_timer(ctx, timer);
            }
        }
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
        }
        let (Some(close), Some(avg)) = (self.close, update.avg_px) else {
            return;
        };
        let at = update
            .ts
            .saturating_add(u64::from(self.p.hold_seconds) * NANOS_PER_SEC)
            .min(close.saturating_sub(EXIT_MARGIN_SECS * NANOS_PER_SEC));
        let away = |permille: i128| {
            Px::from_raw(
                i64::try_from(i128::from(avg.raw()) * permille / 1_000).unwrap_or(i64::MAX),
            )
        };
        let plan = ExitPlan {
            stop: (self.p.exit_stop_permille > 0)
                .then(|| away(i128::from(1_000 - self.p.exit_stop_permille))),
            target: (self.p.target_permille > 0)
                .then(|| away(i128::from(1_000 + self.p.target_permille))),
            flat_by: Some(at),
            collar_permille: self.p.collar_permille,
            ..ExitPlan::new()
        };
        // Nothing filled adds nothing: the book ignores a size of 0.
        self.book.arm(ctx, instrument, true, new, plan);
    }
}
