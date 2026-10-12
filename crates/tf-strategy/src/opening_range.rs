//! T01, the opening range breakout on stocks in play (docs/research 1c, ADR 0082): trade the direction of the first candle of the session, with a stop order at its
//! high or low, on the names whose volume in that candle is large against their own normal.
//!
//! - **Candidates.** The members of the strategy's universe (the static filters of the paper belong to it: price over $5, average volume over a million
//!   shares, ATR over fifty cents; it must also carry the columns the rule reads: `atr14` and the volume baseline) have their bars claimed in the
//!   premarket, up to `max_tracked` (the shared bars are bounded; a refusal is counted).
//! - **The candle.** The shared bar of `range_minutes` (5 or 15) that starts at the open. At `settle_seconds` after it ends, once, each claimed name
//!   whose bar has closed (a bar closes when a later trade of the name arrives) is judged; the rest are skipped for the day. A doji (close equal to open)
//!   gets no trade; a close over the open is a long, under it a short (only in a name that is easy to borrow).
//! - **Stocks in play.** Relative volume is the candle's volume over the name's average for the same window (`vol_first5` for five minutes, the
//!   profile's first quarter hour for fifteen): at least `min_relvol_pct` percent of it. The `names` highest ratios (ties to the lower instrument number)
//!   are traded.
//! - **Entry.** An [`EntryBook`] trigger at the candle's high (a long) or low (a short), from the end of the candle until `entry_minutes` after the open: the
//!   first trade at or through it buys (or sells short) with a marketable limit a collar around that trade, once. Sized by the risk to the stop: `risk_ppm`
//!   millionths of the strategy's budget over a stop `stop_atr_pct` percent of the 14-day ATR from the level, and no more than `max_position_permille` of the
//!   budget or the budget that is left holds ([`crate::budget::BudgetView::shares_for_risk`]); with no budget in force, `dollars` of the level. One trigger a
//!   name a day, so at most `names` positions at once.
//! - **Exits** are the strategy's own ([`ExitBook`]): the stop at that distance, held by the engine, and a time exit `flat_minutes` before the close. The
//!   opening order also carries the same stop for the broker, as the framework requires of an opening order in the regular session; the simulator does not
//!   model it and the live path has an open question (a standing leg and an engine exit on the same shares: ADR 0082).
//!
//! - **The bias filter** (`bias=1`, the hourly trend of T02, E19-S16, [`crate::hourly_bias`]): a trigger that fires is sent only if the name's last completed
//!   hourly close is on the side of the trade (over the EMA(100) for a long, under it for a short). Before the first hourly bucket ends (10:30) there is no
//!   bias, so a break before it is no trade (the trigger is spent, as for a refused spread); the armed names keep their bars to 15:30 for the closes.
//!
//! Integers throughout; nothing reads a clock.

use std::collections::{BTreeMap, BTreeSet};

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_engine::{SymbolState, TfBar, Timeframe};
use tf_universe::{OPEN_MIN, RefInfo};

use crate::budget::BudgetView;
use crate::cross::{CrossStrategy, MemberView};
use crate::entries::{EntryBook, EntryStats, EntryTrigger};
use crate::exits::{ExitBook, ExitPlan, ExitStats, flat_by};
use crate::hourly_bias::{Close, HourlyBias};
use crate::intent::{IntentId, Protective, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, TimerId};
use crate::trace::Trace;

pub use crate::closing_reversal::ParamError;

/// The `reason` of an entry.
pub const REASON_ENTRY: u16 = 1;

/// The exit book's timers are this plus the instrument number; the entry book's, this plus twice it.
const BOOK_TIMERS: u32 = 0x1000_0000;
const ENTRY_TIMERS: u32 = 0x2000_0000;
const MINUTE: Nanos = 60 * NANOS_PER_SEC;

/// What a variant of the strategy is: every field is in the text of [`OpeningRangeParams::render`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpeningRangeParams {
    /// The most names traded a day, which is the most held at once.
    pub names: u32,
    /// The length of the opening candle in minutes: 5 or 15.
    pub range_minutes: u32,
    /// The candle's volume as a percentage of the name's average for the window, at least.
    pub min_relvol_pct: u32,
    /// The stop is this percentage of the 14-day ATR from the level.
    pub stop_atr_pct: u32,
    /// The risk of a trade, in millionths of the strategy's budget.
    pub risk_ppm: u32,
    /// A position holds at most this many thousandths of the budget.
    pub max_position_permille: u32,
    /// Dollars of a name bought when the strategy has no budget to read.
    pub dollars: u32,
    /// Judge the candles this many seconds after they end.
    pub settle_seconds: u32,
    /// A trigger lapses this many minutes after the open.
    pub entry_minutes: u32,
    /// Flat this many minutes before the close.
    pub flat_minutes: u32,
    /// How far from the trade that fires it, or from the trade that stops it, an order may fill, in permille.
    pub collar_permille: u32,
    /// The quoted spread at the entry, at most this many basis points of the mid; 0 for no cap.
    pub spread_cap_bp: u32,
    /// The most names whose bars are claimed.
    pub max_tracked: u32,
    /// 1 to trade only with the hourly bias of T02 ([`crate::hourly_bias`]): a long when the last completed hourly close is over the EMA(100), a short
    /// when under; 0 for no filter. The bias does not exist before the first hourly bucket ends (10:30), so a break before it is no trade.
    pub bias: u32,
}

impl Default for OpeningRangeParams {
    fn default() -> Self {
        OpeningRangeParams {
            names: 20,
            range_minutes: 5,
            min_relvol_pct: 100,
            stop_atr_pct: 10,
            risk_ppm: 10_000,
            max_position_permille: 50,
            dollars: 5_000,
            settle_seconds: 5,
            entry_minutes: 360,
            flat_minutes: 2,
            collar_permille: 10,
            spread_cap_bp: 0,
            max_tracked: 2_000,
            bias: 0,
        }
    }
}

const KEYS: [&str; 14] = [
    "names",
    "range_minutes",
    "min_relvol_pct",
    "stop_atr_pct",
    "risk_ppm",
    "max_position_permille",
    "dollars",
    "settle_seconds",
    "entry_minutes",
    "flat_minutes",
    "collar_permille",
    "spread_cap_bp",
    "max_tracked",
    "bias",
];

impl OpeningRangeParams {
    fn values(&self) -> [u32; 14] {
        [
            self.names,
            self.range_minutes,
            self.min_relvol_pct,
            self.stop_atr_pct,
            self.risk_ppm,
            self.max_position_permille,
            self.dollars,
            self.settle_seconds,
            self.entry_minutes,
            self.flat_minutes,
            self.collar_permille,
            self.spread_cap_bp,
            self.max_tracked,
            self.bias,
        ]
    }

    fn from_values(v: [u32; 14]) -> OpeningRangeParams {
        OpeningRangeParams {
            names: v[0],
            range_minutes: v[1],
            min_relvol_pct: v[2],
            stop_atr_pct: v[3],
            risk_ppm: v[4],
            max_position_permille: v[5],
            dollars: v[6],
            settle_seconds: v[7],
            entry_minutes: v[8],
            flat_minutes: v[9],
            collar_permille: v[10],
            spread_cap_bp: v[11],
            max_tracked: v[12],
            bias: v[13],
        }
    }

    /// The parameters the rule can run with.
    pub fn validate(&self) -> Result<(), ParamError> {
        let bad = |m: &str| Err(ParamError(m.to_owned()));
        if self.names == 0 || self.names > 1_000 {
            return bad("names must be from 1 to 1000");
        }
        if self.range_minutes != 5 && self.range_minutes != 15 {
            return bad(
                "range_minutes must be 5 or 15 (the volume baselines are for those windows)",
            );
        }
        if self.min_relvol_pct == 0 || self.min_relvol_pct > 1_000_000 {
            return bad("min_relvol_pct must be from 1 to 1000000");
        }
        if self.stop_atr_pct == 0 || self.stop_atr_pct > 1_000 {
            return bad("stop_atr_pct must be from 1 to 1000");
        }
        if self.risk_ppm == 0 || self.risk_ppm > 1_000_000 {
            return bad("risk_ppm must be from 1 to 1000000");
        }
        if self.max_position_permille == 0 || self.max_position_permille > 1_000 {
            return bad("max_position_permille must be from 1 to 1000");
        }
        if self.dollars == 0 {
            return bad("dollars must be at least 1");
        }
        if self.settle_seconds > 120 {
            return bad("settle_seconds must be at most 120");
        }
        if self.flat_minutes == 0 || self.flat_minutes > 60 {
            return bad("flat_minutes must be from 1 to 60");
        }
        if self.entry_minutes <= self.range_minutes + 2
            || self.entry_minutes + self.flat_minutes > 390
        {
            return bad(
                "entry_minutes must be over range_minutes by more than 2 and end before the time exit (entry_minutes plus flat_minutes at most 390)",
            );
        }
        if self.collar_permille > 999 {
            return bad("collar_permille must be below 1000");
        }
        if self.spread_cap_bp > 10_000 {
            return bad("spread_cap_bp must be at most 10000");
        }
        if self.max_tracked == 0 || self.max_tracked > 10_000 {
            return bad("max_tracked must be from 1 to 10000");
        }
        if self.bias > 1 {
            return bad("bias must be 0 (no filter) or 1 (the hourly bias)");
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

    /// Parameters from their text: every key once and no other, and values that pass [`OpeningRangeParams::validate`].
    pub fn parse(text: &str) -> Result<OpeningRangeParams, ParamError> {
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
        let mut vals = [0u32; 14];
        for (i, k) in KEYS.iter().enumerate() {
            vals[i] = *got
                .get(k)
                .ok_or_else(|| ParamError(format!("{k} is missing")))?;
        }
        let p = OpeningRangeParams::from_values(vals);
        p.validate()?;
        Ok(p)
    }
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpeningRangeStats {
    /// Names whose bars were claimed.
    pub claimed: u64,
    /// Members the shared bars refused (full) or the host has no bars for, and members past `max_tracked`.
    pub refused_bars: u64,
    /// Members with no ATR or no volume baseline in the snapshot.
    pub no_reference: u64,
    /// Judged names whose candle had not closed.
    pub no_bar: u64,
    pub doji: u64,
    pub low_relvol: u64,
    /// Ranked in, but the name is not easy to borrow.
    pub not_shortable: u64,
    /// A stop that is not a price, or a size of nothing.
    pub no_stop: u64,
    pub no_size: u64,
    /// Names with a trigger armed.
    pub armed: u64,
    /// Ranked in beyond `names`.
    pub cut: u64,
    /// Triggers that fired and sent an order.
    pub entries: u64,
    pub entries_refused: u64,
    /// Triggers that fired against the hourly bias, or before there was one, and sent nothing.
    pub biased_out: u64,
    pub triggers: EntryStats,
    pub exits: ExitStats,
}

#[derive(Clone, Copy, Debug)]
struct Armed {
    long: bool,
    stop: Px,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    instrument: InstrumentId,
    long: bool,
    stop: Px,
    seen: u32,
}

/// A judged name: ratio of the candle's volume to the baseline in permille, the direction, and the candle.
#[derive(Clone, Copy, Debug)]
struct Judged {
    id: InstrumentId,
    ratio: u128,
    long: bool,
    bar: TfBar,
    atr: i64,
}

pub struct OpeningRange {
    id: StrategyId,
    p: OpeningRangeParams,
    book: ExitBook,
    triggers: EntryBook,
    /// The open of the day being traded.
    open: Option<Nanos>,
    claimed: BTreeSet<InstrumentId>,
    claimed_done: bool,
    decided: bool,
    /// The triggers armed, by name, for the plan of the exit when the entry fills.
    armed: BTreeMap<InstrumentId, Armed>,
    entries: BTreeMap<IntentId, Entry>,
    /// The hourly bias of the claimed names, followed when the parameters ask for it.
    bias: HourlyBias,
    stats: OpeningRangeStats,
    tracing: bool,
    traces: Vec<Trace>,
}

impl OpeningRange {
    /// The strategy numbered `id` with `p` (which must pass [`OpeningRangeParams::validate`]).
    pub fn new(id: u16, p: OpeningRangeParams) -> Result<OpeningRange, ParamError> {
        p.validate()?;
        Ok(OpeningRange {
            id: StrategyId(id),
            p,
            book: ExitBook::new(BOOK_TIMERS),
            triggers: EntryBook::new(ENTRY_TIMERS),
            open: None,
            claimed: BTreeSet::new(),
            claimed_done: false,
            decided: false,
            armed: BTreeMap::new(),
            entries: BTreeMap::new(),
            bias: HourlyBias::new(),
            stats: OpeningRangeStats::default(),
            tracing: false,
            traces: Vec::new(),
        })
    }

    pub fn params(&self) -> &OpeningRangeParams {
        &self.p
    }

    pub fn stats(&self) -> OpeningRangeStats {
        OpeningRangeStats {
            triggers: self.triggers.stats(),
            exits: self.book.stats(),
            ..self.stats
        }
    }

    fn start_day(&mut self, ctx: &mut Ctx<'_>, open: Nanos) {
        for id in self.claimed.iter().copied() {
            ctx.untrack_bars(id);
        }
        self.open = Some(open);
        self.bias.start_day(open);
        self.claimed.clear();
        self.claimed_done = false;
        self.decided = false;
        // Yesterday's levels and orders mean nothing today (the host flattened what was held; its fills close the exit book).
        self.triggers.clear(ctx);
        self.armed.clear();
        self.entries.clear();
    }

    fn note(&mut self, ts: Nanos, kind: &str, id: InstrumentId, fields: &[(&str, String)]) {
        if !self.tracing {
            return;
        }
        let mut t = Trace::new(ts, kind).with("instrument", id);
        for (k, v) in fields {
            t = t.with(k, v);
        }
        self.traces.push(t);
    }

    /// The candle's average volume over the sessions: the baseline the ratio is against.
    fn baseline(&self, r: &RefInfo) -> Option<i64> {
        let b = if self.p.range_minutes == 5 {
            r.hist.vol_first5
        } else {
            r.hist
                .profile
                .volume_between(OPEN_MIN, OPEN_MIN + self.p.range_minutes)
        };
        b.filter(|&x| x > 0)
    }

    /// The members with what the rule reads have their bars claimed, in the premarket.
    fn claim(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        for id in view.ids() {
            let Some(r) = view.reference(id) else {
                continue;
            };
            if r.hist.atr14.is_none_or(|a| a <= 0) || self.baseline(r).is_none() {
                self.stats.no_reference += 1;
                continue;
            }
            // The filter needs the EMA's state: a name without it could never be traded.
            if self.p.bias == 1 && !self.bias.add(id, r) {
                self.stats.no_reference += 1;
                continue;
            }
            if self.claimed.len() >= self.p.max_tracked as usize || ctx.track_bars(id).is_err() {
                self.bias.forget(id);
                self.stats.refused_bars += 1;
                continue;
            }
            self.claimed.insert(id);
            self.stats.claimed += 1;
        }
    }

    /// The closed bar of the candle: the one of `range_minutes` that starts at the open.
    fn candle(&self, ctx: &Ctx<'_>, id: InstrumentId, open: Nanos) -> Option<TfBar> {
        let tf = if self.p.range_minutes == 5 {
            Timeframe::M5
        } else {
            Timeframe::M15
        };
        let b = ctx.bars(id)?;
        let start = open / NANOS_PER_SEC;
        (0..b.closed_len(tf))
            .filter_map(|i| b.closed(tf, i))
            .find(|bar| bar.start_sec == start)
            .copied()
    }

    /// Judge every claimed name once, rank the ones in play, arm the triggers of the best, and let the claims go.
    fn decide(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, open: Nanos) {
        let now = ctx.now();
        let mut judged: Vec<Judged> = Vec::new();
        let ids: Vec<InstrumentId> = self.claimed.iter().copied().collect();
        for id in ids {
            let (Some(r), Some(bar)) = (view.reference(id), self.candle(ctx, id, open)) else {
                self.stats.no_bar += 1;
                continue;
            };
            let (Some(atr), Some(base)) = (r.hist.atr14, self.baseline(r)) else {
                continue;
            };
            let long = match bar.close.raw().cmp(&bar.open.raw()) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    self.stats.doji += 1;
                    self.note(now, "doji", id, &[]);
                    continue;
                }
            };
            let ratio = u128::from(bar.volume) * 1_000 / u128::try_from(base).unwrap_or(1);
            if ratio < u128::from(self.p.min_relvol_pct) * 10 {
                self.stats.low_relvol += 1;
                continue;
            }
            judged.push(Judged {
                id,
                ratio,
                long,
                bar,
                atr,
            });
        }
        judged.sort_by(|a, b| b.ratio.cmp(&a.ratio).then(a.id.cmp(&b.id)));
        let mut armed = 0;
        for j in judged {
            if armed >= self.p.names {
                self.stats.cut += 1;
                continue;
            }
            if self.arm(ctx, view, open, j) {
                armed += 1;
            }
        }
        // With the filter the armed names keep their bars: the hourly closes come from them until the last bucket of the day.
        let keep = |me: &Self, id: InstrumentId| me.p.bias == 1 && me.armed.contains_key(&id);
        for id in std::mem::take(&mut self.claimed) {
            if keep(self, id) {
                self.claimed.insert(id);
            } else {
                self.bias.forget(id);
                ctx.untrack_bars(id);
            }
        }
    }

    /// Fold the hourly buckets that have ended into the biases of the followed names. Before a trade is checked as well as at a review: a
    /// quiet name's bar of the last quarter hour is filled in by the trade that follows it, which may be the one that fires.
    fn fold_bias(&mut self, ctx: &Ctx<'_>, now: Nanos) {
        if self.bias.is_empty() {
            return;
        }
        let settle = u64::from(self.p.settle_seconds) * NANOS_PER_SEC;
        self.bias
            .advance(now, settle, |id, s| Self::quarter_close(ctx, id, s));
    }

    /// What the 15 minute bars of `id` say of the close at the end of the quarter hour that starts at `start_sec`: the close of its bar, or of the last
    /// before it when the name did not trade in it; not yet while a bar that ends by then is still forming (bar time has not reached it).
    fn quarter_close(ctx: &Ctx<'_>, id: InstrumentId, start_sec: u64) -> Close {
        let Some(b) = ctx.bars(id) else {
            return Close::Missing;
        };
        if b.forming(Timeframe::M15)
            .is_some_and(|f| f.start_sec <= start_sec)
        {
            return Close::NotYet;
        }
        (0..b.closed_len(Timeframe::M15))
            .filter_map(|i| b.closed(Timeframe::M15, i))
            .find(|bar| bar.start_sec <= start_sec)
            .map_or(Close::Missing, |bar| Close::Is(bar.close))
    }

    /// Arm the trigger of one judged name. True if it is armed.
    fn arm(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, open: Nanos, j: Judged) -> bool {
        let now = ctx.now();
        if !j.long && view.reference(j.id).and_then(|r| r.easy_to_borrow) != Some(true) {
            self.stats.not_shortable += 1;
            self.note(now, "not_shortable", j.id, &[]);
            return false;
        }
        let dist =
            i64::try_from(i128::from(j.atr) * i128::from(self.p.stop_atr_pct) / 100).unwrap_or(0);
        let level = if j.long { j.bar.high } else { j.bar.low };
        let stop = if j.long {
            level.raw() - dist
        } else {
            level.raw() + dist
        };
        if dist <= 0 || stop <= 0 || level.raw() <= 0 {
            self.stats.no_stop += 1;
            self.note(now, "no_stop", j.id, &[("atr", j.atr.to_string())]);
            return false;
        }
        let Some(qty) = self.size(ctx.budget(), level, dist) else {
            self.stats.no_size += 1;
            self.note(now, "no_size", j.id, &[]);
            return false;
        };
        let trigger = EntryTrigger {
            side: if j.long { Side::Buy } else { Side::SellShort },
            level,
            qty,
            not_before: None,
            lapse_at: Some(open + u64::from(self.p.entry_minutes) * MINUTE),
            collar_permille: self.p.collar_permille,
            tif: Tif::Day,
            protect: Some(Protective {
                stop_trigger: Px::from_raw(stop),
                stop_limit: None,
                take_profit: None,
            }),
            reason: REASON_ENTRY,
        };
        if self.triggers.arm(ctx, j.id, trigger).is_err() {
            self.stats.no_stop += 1;
            return false;
        }
        self.armed.insert(
            j.id,
            Armed {
                long: j.long,
                stop: Px::from_raw(stop),
            },
        );
        self.stats.armed += 1;
        let fields = [
            ("long", u8::from(j.long).to_string()),
            ("level", level.raw().to_string()),
            ("stop", stop.to_string()),
            ("qty", qty.to_string()),
            ("ratio_permille", j.ratio.to_string()),
        ];
        self.note(now, "armed", j.id, &fields);
        true
    }

    /// Shares to open at `level` with a stop `dist` away: by the risk to the budget, capped by its room and by the most one position holds; with
    /// no budget, `dollars` of the level.
    fn size(&self, budget: Option<BudgetView>, level: Px, dist: i64) -> Option<u32> {
        let px = u128::try_from(level.raw()).ok().filter(|&x| x > 0)?;
        let qty = match budget {
            Some(b) => {
                let by_risk = b.shares_for_risk(self.p.risk_ppm, dist, level);
                let cap = b.total * u128::from(self.p.max_position_permille) / 1_000 / px;
                u32::try_from(cap).unwrap_or(u32::MAX).min(by_risk)
            }
            None => {
                u32::try_from(u128::from(self.p.dollars) * 1_000_000_000 / px).unwrap_or(u32::MAX)
            }
        };
        (qty > 0).then_some(qty)
    }
}

/// Whether the quote at the entry is tight enough: at most `cap_bp` basis points of the mid between bid and ask. A cap of 0 is none.
fn spread_ok(s: Option<&SymbolState>, cap_bp: u32) -> bool {
    if cap_bp == 0 {
        return true;
    }
    let Some(s) = s else { return false };
    let (Some(bid), Some(ask)) = (s.bid, s.ask) else {
        return false;
    };
    let (b, a) = (i128::from(bid.0.raw()), i128::from(ask.0.raw()));
    if b <= 0 || a < b {
        return false;
    }
    (a - b) * 10_000 * 2 <= (a + b) * i128::from(cap_bp)
}

impl CrossStrategy for OpeningRange {
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        self.id
    }

    fn period(&self) -> Nanos {
        NANOS_PER_SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let Some(day) = ctx.day().copied() else {
            return;
        };
        if self.open != Some(day.open) {
            self.start_day(ctx, day.open);
        }
        let now = ctx.now();
        if !self.claimed_done {
            self.claimed_done = true;
            // Bars claimed after the open miss the candle: only the premarket claims.
            if now < day.open {
                self.claim(ctx, view);
            }
        }
        self.fold_bias(ctx, now);
        let judge_at = day.open
            + u64::from(self.p.range_minutes) * MINUTE
            + u64::from(self.p.settle_seconds) * NANOS_PER_SEC;
        if !self.decided && now >= judge_at {
            self.decided = true;
            self.decide(ctx, view, day.open);
        }
    }

    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, ev: &Event) {
        let Event::Trade(t) = ev else {
            return;
        };
        if self.triggers.is_empty() && self.book.is_empty() {
            return;
        }
        let id = t.hdr.instrument;
        self.fold_bias(ctx, ctx.now());
        self.book.on_trade(ctx, id, t.px);
        if self.triggers.is_empty() {
            return;
        }
        // Each name has one trigger a day and at most `names` are armed, so the names limit and "not held already" hold by construction; the
        // check at the fire is the quote.
        let (state, cap) = (view.state(id), self.p.spread_cap_bp);
        let armed = self.armed.get(&id).copied();
        // A trigger that fires is spent: the bias is asked at the fire, and a break before there is a bias is no trade.
        let with_bias = self.p.bias == 0
            || armed.is_some_and(|a| {
                self.bias
                    .allows(id, if a.long { Side::Buy } else { Side::SellShort })
            });
        let refused_before = self.triggers.stats().refused;
        let sent = self
            .triggers
            .on_trade_checked(ctx, id, t.px, |_| with_bias && spread_ok(state, cap));
        if !with_bias {
            self.stats.biased_out += self.triggers.stats().refused - refused_before;
        }
        for f in sent {
            self.stats.entries += 1;
            if let Some(a) = armed {
                self.entries.insert(
                    f.intent,
                    Entry {
                        instrument: id,
                        long: a.long,
                        stop: a.stop,
                        seen: 0,
                    },
                );
            }
            let fields = [
                ("level", f.level.raw().to_string()),
                ("px", f.px.raw().to_string()),
                ("qty", f.qty.to_string()),
            ];
            self.note(ctx.now(), "fired", id, &fields);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>, timer: TimerId) {
        if self.triggers.on_timer(ctx, timer).is_none() {
            self.book.on_timer(ctx, timer);
        }
    }

    fn on_order_update(&mut self, ctx: &mut Ctx<'_>, update: &OrderUpdate) {
        let Some(e) = self.entries.get_mut(&update.intent) else {
            self.book.on_order_update(ctx, update);
            return;
        };
        let new = update.filled_qty.saturating_sub(e.seen);
        e.seen = update.filled_qty;
        let e = *e;
        if update.state.is_terminal() {
            self.entries.remove(&update.intent);
            if update.filled_qty == 0 {
                self.stats.entries_refused += 1;
            }
        }
        // Nothing filled adds nothing: the book ignores a size of 0.
        let plan = ExitPlan {
            stop: Some(e.stop),
            flat_by: flat_by(ctx.now(), self.p.flat_minutes),
            collar_permille: self.p.collar_permille,
            ..ExitPlan::new()
        };
        self.book.arm(ctx, e.instrument, e.long, new, plan);
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
}
