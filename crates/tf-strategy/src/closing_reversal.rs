//! T04, the closing reversal (E19-S18; the rule is `docs/research/08-extended-hours.md`, 8c).
//!
//! Across stocks the return from the previous close to 15:00 *negatively* predicts the return from 15:30 to the close,
//! through the day's losers. So: at 15:30 rank the universe by the return from the previous close to the last trade at or
//! before 15:00, buy the most negative few, and sell at 15:59:30. One decision a day, long only, no stops (thirty minutes
//! held; the risk is the size).
//!
//! All the times come from the day's calendar ([`Ctx::day`]): they are minutes and seconds **before the regular close**,
//! so an early-close day (13:00) moves all of them with it. The strategy sets its timers at its first review of a day, when
//! the host has told Tier 0 the day.
//!
//! - **The 15:00 price.** A due timer fires after the engine has applied the event that made it due, so at the 15:00 timer
//!   one symbol (the event's) may already have a later trade in Tier 0. The strategy takes a first snapshot of every member's
//!   last price five minutes before, and at 15:00 reads the last trade from Tier 0 when it is at or before 15:00 and from
//!   that snapshot when it is not. The rule is exact except for that one symbol, and only when its last trade before 15:00
//!   was more than five minutes earlier.
//! - **Skips.** A name that is halted or paused (`halted`) or under the short-sale restriction (`ssr`) is not bought, nor one
//!   with no two-sided quote, no prior close or no price at 15:00. A restricted name is a stock down ten percent on the day:
//!   the rule as written skips the most extreme losers.
//! - **Entry.** At the decision time, a marketable order a collar around the ask, an equal dollar amount in each name; the
//!   most negative `names`, ties to the lower instrument number. **Exit.** The [`ExitBook`]'s time exit, `exit_seconds` before
//!   the close, a sell a collar under the last trade, which the simulator fills at the bid; the host's end-of-day flatten
//!   takes what is left.
//! - **A stop that is not part of the rule.** The framework refuses an opening order in the regular session that has no
//!   protective stop ([`crate::intent::IntentError::MissingProtection`]), and the rule has none: the holding period is thirty
//!   minutes and the risk is the size. So every entry carries a *disaster stop* `stop_permille` under the ask (10% by
//!   default), far enough that it is not touched in thirty minutes on a liquid name. R, which is net over the money between
//!   the entry and the stop, is therefore measured against that distance and is not comparable with a strategy whose stop is
//!   part of its rule: judge this one in basis points.
//! - **Variants** are parameters: `names` (10, 20 or 40), `extreme_bp` (only a return of at most minus that many basis
//!   points), `spread_cap_bp` (only a quoted spread of at most that many basis points of the mid) and `entry_minutes` equal
//!   to `ref_minutes` (buy at 15:00 instead, to see whether the skipped half hour matters).

use std::collections::BTreeMap;

use tf_core::{InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_engine::SymbolState;

use crate::cross::{CrossStrategy, MemberView};
use crate::exits::{ExitBook, ExitPlan, ExitStats};
use crate::intent::{IntentId, Pricing, Protective, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};

/// The `reason` code of this strategy's entries.
pub const REASON_ENTRY: u16 = 1;

const SNAPSHOT: TimerId = TimerId(1);
const REFERENCE: TimerId = TimerId(2);
const DECISION: TimerId = TimerId(3);
/// The exit book's timers are this plus the instrument number.
const BOOK_TIMERS: u32 = 0x1000_0000;

/// How long before the price at the reference time the first snapshot of last prices is taken.
const SNAPSHOT_MINUTES_BEFORE: u32 = 5;

/// What a variant of the strategy is. Every field is in the text of [`ClosingReversalParams::render`], so another value is
/// another variant for the certificate and the trial registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosingReversalParams {
    /// Buy this many: the most negative.
    pub names: u32,
    /// Only names whose return is at most minus this many basis points; 0 for no such floor.
    pub extreme_bp: u32,
    /// Only names whose quoted spread at the decision is at most this many basis points of the mid; 0 for no cap.
    pub spread_cap_bp: u32,
    /// Dollars bought of each name.
    pub dollars: u32,
    /// How far from its reference price an order may fill, in permille, entry and exit.
    pub collar_permille: u32,
    /// The disaster stop: how far under the ask, in permille, the protective stop of an entry is. Not part of the rule (see
    /// the module documentation); the framework needs one.
    pub stop_permille: u32,
    /// The price used is the last trade at or before this many minutes before the close (60 is 15:00).
    pub ref_minutes: u32,
    /// Buy this many minutes before the close (30 is 15:30); at most `ref_minutes`.
    pub entry_minutes: u32,
    /// Sell this many seconds before the close (30 is 15:59:30).
    pub exit_seconds: u32,
}

impl Default for ClosingReversalParams {
    fn default() -> Self {
        ClosingReversalParams {
            names: 20,
            extreme_bp: 0,
            spread_cap_bp: 0,
            dollars: 2_000,
            collar_permille: 5,
            stop_permille: 100,
            ref_minutes: 60,
            entry_minutes: 30,
            exit_seconds: 30,
        }
    }
}

/// Why a set of parameters or the text of one was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamError(pub String);

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParamError {}

const KEYS: [&str; 9] = [
    "names",
    "extreme_bp",
    "spread_cap_bp",
    "dollars",
    "collar_permille",
    "stop_permille",
    "ref_minutes",
    "entry_minutes",
    "exit_seconds",
];

impl ClosingReversalParams {
    /// The parameters the rule can run with: something to buy and to spend, an entry no earlier than the price it is based on,
    /// an exit after the entry, and all of it inside one session.
    pub fn validate(&self) -> Result<(), ParamError> {
        let bad = |m: &str| Err(ParamError(m.to_owned()));
        if self.names == 0 {
            return bad("names must be at least 1");
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
        if self.ref_minutes == 0 || self.ref_minutes > 390 {
            return bad("ref_minutes must be from 1 to 390 (a session is 390 minutes)");
        }
        if self.entry_minutes == 0 || self.entry_minutes > self.ref_minutes {
            return bad(
                "entry_minutes must be from 1 to ref_minutes: no buying on a price from after the entry",
            );
        }
        if self.exit_seconds == 0
            || u64::from(self.exit_seconds) >= u64::from(self.entry_minutes) * 60
        {
            return bad(
                "exit_seconds must be from 1 to less than the seconds between the entry and the close",
            );
        }
        Ok(())
    }

    fn values(&self) -> [u32; 9] {
        [
            self.names,
            self.extreme_bp,
            self.spread_cap_bp,
            self.dollars,
            self.collar_permille,
            self.stop_permille,
            self.ref_minutes,
            self.entry_minutes,
            self.exit_seconds,
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

    /// Parameters from their text: every key once and no other, and values that pass [`ClosingReversalParams::validate`].
    pub fn parse(text: &str) -> Result<ClosingReversalParams, ParamError> {
        let mut got: BTreeMap<&str, u32> = BTreeMap::new();
        for w in text.split_whitespace() {
            let Some((k, v)) = w.split_once('=') else {
                return Err(ParamError(format!("`{w}` is not key=value")));
            };
            if !KEYS.contains(&k) {
                return Err(ParamError(format!(
                    "`{k}` is not a parameter of this strategy"
                )));
            }
            let v: u32 = v
                .parse()
                .map_err(|_| ParamError(format!("`{v}` is not a whole number for {k}")))?;
            if got.insert(k, v).is_some() {
                return Err(ParamError(format!("{k} is given twice")));
            }
        }
        let mut vals = [0u32; 9];
        for (i, k) in KEYS.iter().enumerate() {
            vals[i] = *got
                .get(k)
                .ok_or_else(|| ParamError(format!("{k} is missing")))?;
        }
        let p = ClosingReversalParams {
            names: vals[0],
            extreme_bp: vals[1],
            spread_cap_bp: vals[2],
            dollars: vals[3],
            collar_permille: vals[4],
            stop_permille: vals[5],
            ref_minutes: vals[6],
            entry_minutes: vals[7],
            exit_seconds: vals[8],
        };
        p.validate()?;
        Ok(p)
    }
}

/// The return from `prior` to `px` in parts per million, truncated toward zero; `None` unless both are positive.
pub fn return_ppm(prior: i64, px: i64) -> Option<i64> {
    if prior <= 0 || px <= 0 {
        return None;
    }
    i64::try_from((i128::from(px) - i128::from(prior)) * 1_000_000 / i128::from(prior)).ok()
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClosingReversalStats {
    /// Decisions taken (one a day).
    pub decisions: u64,
    /// Names bought across them.
    pub entries: u64,
    /// Entries the strategy's own checks refused.
    pub entries_refused: u64,
    /// The exits held and sent.
    pub exits: ExitStats,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    instrument: InstrumentId,
    /// Shares of it filled so far.
    seen: u32,
}

pub struct ClosingReversal {
    id: StrategyId,
    p: ClosingReversalParams,
    book: ExitBook,
    /// The regular close of the day the timers are set for.
    close: Option<Nanos>,
    /// By instrument number, raw: the last price at the snapshot, and at the reference time; 0 for none.
    snapshot: Vec<i64>,
    reference: Vec<i64>,
    entries: BTreeMap<IntentId, Entry>,
    stats: ClosingReversalStats,
}

impl ClosingReversal {
    /// The strategy numbered `id` with `p` (which must pass [`ClosingReversalParams::validate`]).
    pub fn new(id: u16, p: ClosingReversalParams) -> Result<ClosingReversal, ParamError> {
        p.validate()?;
        Ok(ClosingReversal {
            id: StrategyId(id),
            p,
            book: ExitBook::new(BOOK_TIMERS),
            close: None,
            snapshot: Vec::new(),
            reference: Vec::new(),
            entries: BTreeMap::new(),
            stats: ClosingReversalStats::default(),
        })
    }

    pub fn params(&self) -> &ClosingReversalParams {
        &self.p
    }

    pub fn stats(&self) -> ClosingReversalStats {
        ClosingReversalStats {
            exits: self.book.stats(),
            ..self.stats
        }
    }

    fn before_close(close: Nanos, secs: u64) -> Nanos {
        close.saturating_sub(secs * NANOS_PER_SEC)
    }

    fn ref_at(&self, close: Nanos) -> Nanos {
        Self::before_close(close, u64::from(self.p.ref_minutes) * 60)
    }

    /// Set the timers for the day whose regular session ends at `close`.
    fn start_day(&mut self, ctx: &mut Ctx<'_>, close: Nanos) {
        self.close = Some(close);
        self.snapshot.clear();
        self.reference.clear();
        let minutes = u64::from(self.p.ref_minutes + SNAPSHOT_MINUTES_BEFORE);
        ctx.set_timer(SNAPSHOT, Self::before_close(close, minutes * 60));
        ctx.set_timer(REFERENCE, self.ref_at(close));
        ctx.set_timer(
            DECISION,
            Self::before_close(close, u64::from(self.p.entry_minutes) * 60),
        );
    }

    fn slot(v: &mut Vec<i64>, id: InstrumentId) -> &mut i64 {
        let i = id as usize;
        if v.len() <= i {
            v.resize(i + 1, 0);
        }
        &mut v[i]
    }

    fn take_snapshot(&mut self, view: &MemberView<'_>) {
        for id in view.ids() {
            if let Some(px) = view.state(id).and_then(|s| s.last_px) {
                *Self::slot(&mut self.snapshot, id) = px.raw();
            }
        }
    }

    /// The price at the reference time: the last trade at or before it.
    fn take_reference(&mut self, view: &MemberView<'_>, at: Nanos) {
        for id in view.ids() {
            let from_tier0 = view
                .state(id)
                .filter(|s| s.last_ts <= at)
                .and_then(|s| s.last_px)
                .map(|p| p.raw());
            // 0 is no price: `return_ppm` refuses it.
            let px = from_tier0
                .or_else(|| self.snapshot.get(id as usize).copied())
                .unwrap_or(0);
            *Self::slot(&mut self.reference, id) = px;
        }
    }

    /// The names to buy now, most negative first: the return to the reference price, for names that may be bought.
    fn picks(&self, view: &MemberView<'_>) -> Vec<(i64, InstrumentId)> {
        let p = &self.p;
        let floor_ppm = i64::from(p.extreme_bp) * 100;
        let key = |id: InstrumentId, st: &SymbolState| -> Option<i64> {
            if st.halted || st.ssr {
                return None;
            }
            let prior = view.reference(id)?.price?;
            let px = self.reference.get(id as usize).copied()?;
            let ret = return_ppm(prior, px)?;
            if floor_ppm > 0 && ret > -floor_ppm {
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
            Some(ret)
        };
        view.top_k(p.names as usize, false, key)
    }

    fn decide(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        self.stats.decisions += 1;
        for (_, id) in self.picks(view) {
            let Some(ask) = view.state(id).and_then(|s| s.ask).map(|l| l.0) else {
                continue;
            };
            let qty = u128::from(self.p.dollars) * 1_000_000_000 / ask.raw().max(1) as u128;
            let Ok(qty) = u32::try_from(qty) else {
                continue;
            };
            if qty == 0 {
                continue;
            }
            let stop = i128::from(ask.raw()) * i128::from(1_000 - self.p.stop_permille) / 1_000;
            let Ok(stop) = i64::try_from(stop) else {
                continue;
            };
            if stop <= 0 {
                continue;
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
                // Not reachable with parameters that pass `validate`: an entry the framework's checks accept is the only kind
                // this builds (a price and a stop above nothing, a size above nothing). Kept for the day the checks change.
                Err(_) => self.stats.entries_refused += 1,
            }
        }
    }
}

impl CrossStrategy for ClosingReversal {
    fn id(&self) -> StrategyId {
        self.id
    }

    /// A minute: the review only sets the day's timers, once.
    fn period(&self) -> Nanos {
        60 * NANOS_PER_SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>) {
        let Some(close) = ctx.day().map(|d| d.close) else {
            return;
        };
        if self.close != Some(close) {
            self.start_day(ctx, close);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, timer: TimerId) {
        match timer {
            SNAPSHOT => self.take_snapshot(view),
            REFERENCE => {
                if let Some(close) = self.close {
                    self.take_reference(view, self.ref_at(close));
                }
            }
            DECISION => self.decide(ctx, view),
            t => {
                self.book.on_timer(ctx, t);
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
        // Nothing filled adds nothing: the book ignores a size of 0.
        if let Some(close) = self.close {
            let plan = ExitPlan {
                flat_by: Some(Self::before_close(close, u64::from(self.p.exit_seconds))),
                collar_permille: self.p.collar_permille,
                ..ExitPlan::new()
            };
            self.book.arm(ctx, instrument, true, new, plan);
        }
    }
}
