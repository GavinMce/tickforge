//! T03, the premarket VWAP reclaim (docs/research 8a, ADR 0078): a name that gaps up on real volume, dips under its premarket VWAP and
//! closes a one-minute bar back above it is turning up; buy the reclaim, long only.
//!
//! - **Candidates.** At every review until `screen_by_minutes` after 04:00, a member whose premarket dollars traded so far are at least
//!   `min_dollars`, whose last price is at least `gap_bp` basis points over the prior close, and whose price is in the band, becomes a
//!   candidate, and its one-minute bars are claimed (the shared bars are bounded; a claim refused is counted). A name that is not a
//!   candidate by then is not one that day. The premarket VWAP is the one Tier 0 keeps (E19-S02, read through [`MemberView::session`]).
//! - **Dip, then reclaim.** From `start_minutes` after 04:00, a closed one-minute bar whose low is at least `dip_bp` under the VWAP is a
//!   dip; a *later* bar that closes above the VWAP is the reclaim and the entry: `dollars` at the ask with a collar, a day order, no
//!   protective order. Not later than `last_entry_minutes` before the open, at most `names` positions at once, the quoted spread at
//!   most `spread_cap_bp` of the mid. One trade a name a day.
//! - **Exits** are the strategy's own (the broker takes no stop in the premarket): the target is `target_bp` over the VWAP at the entry;
//!   a one-minute bar that closes `exit_below_bp` under the VWAP sends the exit (a decision about a bar close, [`ExitBook::exit_now`]); a
//!   disaster stop `stop_permille` under the fill; and a time exit `flat_minutes` before the open.
//!
//! Integers throughout; nothing reads a clock. The bars are read as they close, at the strategy's review (every five seconds), so the
//! VWAP a bar is compared with is the one at that review and not at the bar's last trade.

use std::collections::{BTreeMap, BTreeSet};

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_engine::{TfBar, Timeframe};

use crate::cross::{CrossStrategy, MemberView};
use crate::exits::{ExitBook, ExitPlan, ExitStats};
use crate::intent::{IntentId, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};
use crate::trace::Trace;

pub use crate::closing_reversal::ParamError;

/// The `reason` of an entry.
pub const REASON_ENTRY: u16 = 1;

/// The exit book's timers are this plus the instrument number.
const BOOK_TIMERS: u32 = 0x1000_0000;
/// Seconds between looks at the candidates' bars.
const REVIEW_SECS: u64 = 5;

/// What a variant of the strategy is: every field is in the text of [`VwapReclaimParams::render`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VwapReclaimParams {
    /// Positions at once.
    pub names: u32,
    /// Dollars bought of each name.
    pub dollars: u32,
    /// Premarket dollars traded, at least, for a candidate.
    pub min_dollars: u32,
    /// The last price over the prior close, in basis points, at least.
    pub gap_bp: u32,
    /// A name becomes a candidate only in the first this many minutes after 04:00.
    pub screen_by_minutes: u32,
    /// No dip counts and no entry is made before this many minutes after 04:00.
    pub start_minutes: u32,
    /// No entry in the last this many minutes before the open.
    pub last_entry_minutes: u32,
    /// A bar's low this many basis points under the VWAP is a dip.
    pub dip_bp: u32,
    /// The target is this many basis points over the VWAP at the entry.
    pub target_bp: u32,
    /// A bar closing this many basis points under the VWAP is the exit.
    pub exit_below_bp: u32,
    /// The quoted spread at the entry, at most this many basis points of the mid; 0 for no cap.
    pub spread_cap_bp: u32,
    /// How far from its reference price an order may fill, in permille, entry and exit.
    pub collar_permille: u32,
    /// The disaster stop, this many permille under the fill.
    pub stop_permille: u32,
    /// Flat this many minutes before the open.
    pub flat_minutes: u32,
    /// The last price, in cents, between these two.
    pub min_cents: u32,
    pub max_cents: u32,
    /// The most names whose bars are claimed.
    pub max_candidates: u32,
}

impl Default for VwapReclaimParams {
    fn default() -> Self {
        VwapReclaimParams {
            names: 3,
            dollars: 1_000,
            min_dollars: 500_000,
            gap_bp: 300,
            screen_by_minutes: 240,
            start_minutes: 180,
            last_entry_minutes: 10,
            dip_bp: 100,
            target_bp: 100,
            exit_below_bp: 50,
            spread_cap_bp: 50,
            collar_permille: 10,
            stop_permille: 30,
            flat_minutes: 5,
            min_cents: 100,
            max_cents: 5_000,
            max_candidates: 200,
        }
    }
}

const KEYS: [&str; 17] = [
    "names",
    "dollars",
    "min_dollars",
    "gap_bp",
    "screen_by_minutes",
    "start_minutes",
    "last_entry_minutes",
    "dip_bp",
    "target_bp",
    "exit_below_bp",
    "spread_cap_bp",
    "collar_permille",
    "stop_permille",
    "flat_minutes",
    "min_cents",
    "max_cents",
    "max_candidates",
];

impl VwapReclaimParams {
    fn values(&self) -> [u32; 17] {
        [
            self.names,
            self.dollars,
            self.min_dollars,
            self.gap_bp,
            self.screen_by_minutes,
            self.start_minutes,
            self.last_entry_minutes,
            self.dip_bp,
            self.target_bp,
            self.exit_below_bp,
            self.spread_cap_bp,
            self.collar_permille,
            self.stop_permille,
            self.flat_minutes,
            self.min_cents,
            self.max_cents,
            self.max_candidates,
        ]
    }

    fn from_values(v: [u32; 17]) -> VwapReclaimParams {
        VwapReclaimParams {
            names: v[0],
            dollars: v[1],
            min_dollars: v[2],
            gap_bp: v[3],
            screen_by_minutes: v[4],
            start_minutes: v[5],
            last_entry_minutes: v[6],
            dip_bp: v[7],
            target_bp: v[8],
            exit_below_bp: v[9],
            spread_cap_bp: v[10],
            collar_permille: v[11],
            stop_permille: v[12],
            flat_minutes: v[13],
            min_cents: v[14],
            max_cents: v[15],
            max_candidates: v[16],
        }
    }

    /// The parameters the rule can run with.
    pub fn validate(&self) -> Result<(), ParamError> {
        let bad = |m: &str| Err(ParamError(m.to_owned()));
        if self.names == 0 {
            return bad("names must be at least 1");
        }
        if self.dollars == 0 {
            return bad("dollars must be at least 1");
        }
        if self.min_dollars == 0 {
            return bad("min_dollars must be at least 1");
        }
        if self.gap_bp == 0 || self.gap_bp > 100_000 {
            return bad("gap_bp must be from 1 to 100000");
        }
        if self.screen_by_minutes == 0 || self.screen_by_minutes > 329 {
            return bad("screen_by_minutes must be from 1 to 329 (the premarket is 330 minutes)");
        }
        if self.flat_minutes == 0 || self.last_entry_minutes <= self.flat_minutes {
            return bad("flat_minutes must be at least 1 and less than last_entry_minutes");
        }
        if self.start_minutes + self.last_entry_minutes >= 330 {
            return bad(
                "start_minutes plus last_entry_minutes must be under 330 (the premarket is 330 minutes)",
            );
        }
        if self.dip_bp == 0 || self.dip_bp > 5_000 {
            return bad("dip_bp must be from 1 to 5000");
        }
        if self.target_bp == 0 || self.target_bp > 10_000 {
            return bad("target_bp must be from 1 to 10000");
        }
        if self.exit_below_bp == 0 || self.exit_below_bp > 5_000 {
            return bad("exit_below_bp must be from 1 to 5000");
        }
        if self.collar_permille > 999 {
            return bad("collar_permille must be below 1000");
        }
        if self.stop_permille == 0 || self.stop_permille > 999 {
            return bad("stop_permille must be from 1 to 999");
        }
        if self.min_cents == 0 || self.max_cents < self.min_cents {
            return bad("min_cents must be at least 1 and no more than max_cents");
        }
        if self.max_candidates == 0 || self.max_candidates > 10_000 {
            return bad("max_candidates must be from 1 to 10000");
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

    /// Parameters from their text: every key once and no other, and values that pass [`VwapReclaimParams::validate`].
    pub fn parse(text: &str) -> Result<VwapReclaimParams, ParamError> {
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
        let mut vals = [0u32; 17];
        for (i, k) in KEYS.iter().enumerate() {
            vals[i] = *got
                .get(k)
                .ok_or_else(|| ParamError(format!("{k} is missing")))?;
        }
        let p = VwapReclaimParams::from_values(vals);
        p.validate()?;
        Ok(p)
    }
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VwapReclaimStats {
    /// Names whose bars were claimed.
    pub candidates: u64,
    /// Candidates the shared bars refused (full) or the host has no bars for.
    pub refused_bars: u64,
    /// Candidates past `max_candidates`.
    pub refused_cap: u64,
    pub dips: u64,
    pub entries: u64,
    /// Reclaims that were not bought: too late or no room.
    pub missed: u64,
    pub entries_refused: u64,
    pub exits: ExitStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for a dip.
    Watching,
    /// A bar went under the VWAP: waiting for a later one to close above it.
    Dipped,
    /// Bought, or given up on, for the day.
    Done,
}

#[derive(Clone, Copy, Debug)]
struct Cand {
    phase: Phase,
    /// Closed one-minute bars of the name already looked at.
    seen: u64,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    instrument: InstrumentId,
    seen: u32,
    /// The VWAP when the entry was decided, for the target.
    vwap: i64,
}

/// What came of a reclaim.
enum Outcome {
    Entered,
    /// Not now (a halt, no quote, a wide spread): the name stays dipped for the next bar.
    Wait,
    Drop(&'static str),
}

pub struct VwapReclaim {
    id: StrategyId,
    p: VwapReclaimParams,
    book: ExitBook,
    open: Option<Nanos>,
    cands: BTreeMap<InstrumentId, Cand>,
    entries: BTreeMap<IntentId, Entry>,
    /// The names held or being bought, with the one-minute bars of each already looked at for the exit.
    holds: BTreeMap<InstrumentId, u64>,
    /// Names bought today.
    bought: BTreeSet<InstrumentId>,
    stats: VwapReclaimStats,
    tracing: bool,
    traces: Vec<Trace>,
}

impl VwapReclaim {
    /// The strategy numbered `id` with `p` (which must pass [`VwapReclaimParams::validate`]).
    pub fn new(id: u16, p: VwapReclaimParams) -> Result<VwapReclaim, ParamError> {
        p.validate()?;
        Ok(VwapReclaim {
            id: StrategyId(id),
            p,
            book: ExitBook::new(BOOK_TIMERS),
            open: None,
            cands: BTreeMap::new(),
            entries: BTreeMap::new(),
            holds: BTreeMap::new(),
            bought: BTreeSet::new(),
            stats: VwapReclaimStats::default(),
            tracing: false,
            traces: Vec::new(),
        })
    }

    pub fn params(&self) -> &VwapReclaimParams {
        &self.p
    }

    pub fn stats(&self) -> VwapReclaimStats {
        VwapReclaimStats {
            exits: self.book.stats(),
            ..self.stats
        }
    }

    fn start_day(&mut self, ctx: &mut Ctx<'_>, open: Nanos) {
        // The last day's claims on bars are let go: a name that is a candidate again claims them again.
        for id in self.cands.keys().copied().chain(self.holds.keys().copied()) {
            ctx.untrack_bars(id);
        }
        self.open = Some(open);
        self.cands.clear();
        self.holds.clear();
        self.bought.clear();
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

    /// The minute of the premarket `now` is in, if it is in it.
    fn minute(ctx: &Ctx<'_>) -> Option<u32> {
        let day = ctx.day()?;
        let now = ctx.now();
        if now < day.premarket || now >= day.open {
            return None;
        }
        u32::try_from((now - day.premarket) / (60 * NANOS_PER_SEC)).ok()
    }

    /// The names that qualify now become candidates and their bars are claimed.
    fn screen(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, m: u32) {
        if m > self.p.screen_by_minutes {
            return;
        }
        let p = self.p;
        for id in view.ids() {
            if self.cands.contains_key(&id) {
                continue;
            }
            let Some(s) = view.session(id) else { continue };
            let dollars = u64::try_from(s.premarket.notional / 1_000_000_000).unwrap_or(u64::MAX);
            if dollars < u64::from(p.min_dollars) {
                continue;
            }
            let Some(last) = view
                .state(id)
                .and_then(|st| st.last_px)
                .map(|x| x.raw())
                .filter(|&x| x > 0)
            else {
                continue;
            };
            let cents = last / 10_000_000;
            if cents < i64::from(p.min_cents) || cents > i64::from(p.max_cents) {
                continue;
            }
            let Some(prior) = view.reference(id).and_then(|r| r.price).filter(|&x| x > 0) else {
                continue;
            };
            let gap = i128::from(last - prior) * 10_000 / i128::from(prior);
            if gap < i128::from(p.gap_bp) {
                continue;
            }
            if self.cands.len() >= p.max_candidates as usize {
                self.stats.refused_cap += 1;
                continue;
            }
            if ctx.track_bars(id).is_err() {
                self.stats.refused_bars += 1;
                continue;
            }
            // Bars of the name that an earlier claim of another strategy began are not looked at: from here on.
            let seen = ctx.bars(id).map_or(0, |b| b.closed_total(Timeframe::M1));
            self.cands.insert(
                id,
                Cand {
                    phase: Phase::Watching,
                    seen,
                },
            );
            self.stats.candidates += 1;
            let fields = [
                ("gap_bp", gap.to_string()),
                ("dollars", dollars.to_string()),
            ];
            self.note(ctx.now(), "candidate", id, &fields);
        }
    }

    /// The one-minute bars of `id` closed since `seen`, oldest first, and the new total.
    fn new_bars(ctx: &Ctx<'_>, id: InstrumentId, seen: u64) -> (Vec<TfBar>, u64) {
        let Some(b) = ctx.bars(id) else {
            return (Vec::new(), seen);
        };
        let total = b.closed_total(Timeframe::M1);
        let n = usize::try_from(total.saturating_sub(seen))
            .unwrap_or(usize::MAX)
            .min(b.closed_len(Timeframe::M1));
        let bars = (0..n)
            .rev()
            .filter_map(|i| b.closed(Timeframe::M1, i).copied())
            .collect();
        (bars, total)
    }

    /// The candidates' new bars: a dip, then a reclaim.
    fn follow(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let Some(day) = ctx.day().copied() else {
            return;
        };
        let start = day.premarket + u64::from(self.p.start_minutes) * 60 * NANOS_PER_SEC;
        let ids: Vec<InstrumentId> = self.cands.keys().copied().collect();
        for id in ids {
            let Some(c) = self.cands.get(&id).copied() else {
                continue;
            };
            if c.phase == Phase::Done {
                continue;
            }
            let (bars, total) = Self::new_bars(ctx, id, c.seen);
            let mut phase = c.phase;
            let vwap = view
                .session(id)
                .and_then(|s| s.premarket.vwap())
                .map(|x| x.raw());
            for bar in &bars {
                let (Some(vwap), true) = (vwap, phase != Phase::Done) else {
                    continue;
                };
                if vwap <= 0 || bar.start_sec * NANOS_PER_SEC < start {
                    continue;
                }
                match phase {
                    Phase::Watching => {
                        let under = i128::from(vwap) * i128::from(10_000 - self.p.dip_bp);
                        if i128::from(bar.low.raw()) * 10_000 <= under {
                            phase = Phase::Dipped;
                            self.stats.dips += 1;
                            let fields = [
                                ("low", bar.low.raw().to_string()),
                                ("vwap", vwap.to_string()),
                            ];
                            self.note(ctx.now(), "dip", id, &fields);
                        }
                    }
                    Phase::Dipped => {
                        if bar.close.raw() > vwap {
                            match self.try_enter(ctx, view, id, vwap, bar.close.raw()) {
                                Outcome::Entered => phase = Phase::Done,
                                Outcome::Wait => {}
                                Outcome::Drop(why) => {
                                    phase = Phase::Done;
                                    self.stats.missed += 1;
                                    self.note(ctx.now(), "drop", id, &[("reason", why.to_owned())]);
                                }
                            }
                        }
                    }
                    Phase::Done => {}
                }
            }
            if let Some(c) = self.cands.get_mut(&id) {
                c.phase = phase;
                c.seen = total;
            }
            // A name that is finished with and not held does not keep its bars.
            if phase == Phase::Done && !self.holds.contains_key(&id) {
                ctx.untrack_bars(id);
            }
        }
    }

    /// Buy `id` at its ask on the reclaim, if there is room and time and the quote is a fair one.
    fn try_enter(
        &mut self,
        ctx: &mut Ctx<'_>,
        view: &MemberView<'_>,
        id: InstrumentId,
        vwap: i64,
        close: i64,
    ) -> Outcome {
        let Some(open) = self.open else {
            return Outcome::Wait;
        };
        let now = ctx.now();
        if now > open.saturating_sub(u64::from(self.p.last_entry_minutes) * 60 * NANOS_PER_SEC) {
            return Outcome::Drop("too_late");
        }
        if self.bought.contains(&id) {
            return Outcome::Drop("bought");
        }
        if self.entries.len() + self.book.len() >= self.p.names as usize {
            return Outcome::Drop("no_room");
        }
        let Some(st) = view.state(id) else {
            return Outcome::Wait;
        };
        if st.halted {
            return Outcome::Wait;
        }
        let (Some(bid), Some(ask)) = (st.bid, st.ask) else {
            return Outcome::Wait;
        };
        let (bid, ask) = (bid.0.raw(), ask.0.raw());
        if bid <= 0 || ask < bid {
            return Outcome::Wait;
        }
        if self.p.spread_cap_bp > 0 {
            let mid2 = i128::from(bid) + i128::from(ask);
            if i128::from(ask - bid) * 20_000 > i128::from(self.p.spread_cap_bp) * mid2 {
                return Outcome::Wait;
            }
        }
        let qty = u128::from(self.p.dollars) * 1_000_000_000 / ask.max(1) as u128;
        let Some(qty) = u32::try_from(qty).ok().filter(|&q| q > 0) else {
            return Outcome::Wait;
        };
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
                self.entries.insert(
                    intent,
                    Entry {
                        instrument: id,
                        seen: 0,
                        vwap,
                    },
                );
                let seen = ctx.bars(id).map_or(0, |b| b.closed_total(Timeframe::M1));
                self.holds.insert(id, seen);
                let fields = [
                    ("vwap", vwap.to_string()),
                    ("close", close.to_string()),
                    ("ask", ask.to_string()),
                    ("qty", qty.to_string()),
                ];
                self.note(now, "entry", id, &fields);
                Outcome::Entered
            }
            Err(_) => {
                self.stats.entries_refused += 1;
                Outcome::Wait
            }
        }
    }

    /// A one-minute bar of a name held that closed under the VWAP by the exit margin sends the exit.
    fn watch_exits(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let ids: Vec<InstrumentId> = self.holds.keys().copied().collect();
        for id in ids {
            let seen = self.holds.get(&id).copied().unwrap_or(0);
            let (bars, total) = Self::new_bars(ctx, id, seen);
            self.holds.insert(id, total);
            let Some(vwap) = view
                .session(id)
                .and_then(|s| s.premarket.vwap())
                .map(|x| x.raw())
            else {
                continue;
            };
            for bar in &bars {
                let under = i128::from(vwap) * i128::from(10_000 - self.p.exit_below_bp);
                if i128::from(bar.close.raw()) * 10_000 < under && self.book.is_held(id) {
                    let Some(last) = view.state(id).and_then(|s| s.last_px) else {
                        continue;
                    };
                    if self.book.exit_now(ctx, id, last) {
                        let fields = [
                            ("close", bar.close.raw().to_string()),
                            ("vwap", vwap.to_string()),
                        ];
                        self.note(ctx.now(), "signal_exit", id, &fields);
                        break;
                    }
                }
            }
            // A name no longer held or being bought stops being looked at, and its bars are let go.
            let pending = self.entries.values().any(|e| e.instrument == id);
            if !self.book.is_held(id) && !pending {
                self.holds.remove(&id);
                if self.cands.get(&id).is_none_or(|c| c.phase == Phase::Done) {
                    ctx.untrack_bars(id);
                }
            }
        }
    }
}

impl CrossStrategy for VwapReclaim {
    /// Only for the exit book's stop and target: a trade of a name held.
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        self.id
    }

    fn period(&self) -> Nanos {
        REVIEW_SECS * NANOS_PER_SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let Some(open) = ctx.day().map(|d| d.open) else {
            return;
        };
        if self.open != Some(open) {
            self.start_day(ctx, open);
        }
        let Some(m) = Self::minute(ctx) else { return };
        self.screen(ctx, view, m);
        self.follow(ctx, view);
        self.watch_exits(ctx, view);
    }

    fn on_member_event(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>, ev: &Event) {
        if self.book.is_empty() {
            return;
        }
        if let Event::Trade(t) = ev {
            self.book.on_trade(ctx, t.hdr.instrument, t.px);
        }
    }

    fn on_timer(&mut self, ctx: &mut Ctx<'_>, _view: &MemberView<'_>, timer: TimerId) {
        self.book.on_timer(ctx, timer);
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
        let (instrument, vwap) = (e.instrument, e.vwap);
        if update.state.is_terminal() {
            self.entries.remove(&update.intent);
        }
        let (Some(open), Some(avg)) = (self.open, update.avg_px) else {
            return;
        };
        let away = |base: i64, permille: i128| {
            Px::from_raw(i64::try_from(i128::from(base) * permille / 1_000).unwrap_or(i64::MAX))
        };
        let plan = ExitPlan {
            stop: Some(away(avg.raw(), i128::from(1_000 - self.p.stop_permille))),
            target: Some(Px::from_raw(
                i64::try_from(i128::from(vwap) * i128::from(10_000 + self.p.target_bp) / 10_000)
                    .unwrap_or(i64::MAX),
            )),
            flat_by: Some(open.saturating_sub(u64::from(self.p.flat_minutes) * 60 * NANOS_PER_SEC)),
            collar_permille: self.p.collar_permille,
            ..ExitPlan::new()
        };
        // Nothing filled adds nothing: the book ignores a size of 0.
        self.book.arm(ctx, instrument, true, new, plan);
    }
}
