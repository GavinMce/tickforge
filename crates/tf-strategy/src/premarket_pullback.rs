//! A premarket volume spike, and the first small pullback in the run it starts (ADR 0076).
//!
//! In the premarket (04:00 to 09:30 New York) a name that trades far more in the last few minutes than it has so far, while its
//! price is running up, is "in play". The strategy waits for the run to give back a small part of itself and buys the first turn
//! up, long only:
//!
//! - **Spike.** Once a minute, for each name that has traded: the shares of the last `window_minutes` against the average
//!   window of the minutes before them (at least `spike_x10` / 10 times it), with at least `min_dollars` and `min_trades` in the
//!   window, a last price between `min_cents` and `max_cents`, and the price up at least `min_thrust_bp` from its low of the
//!   window. The window's low is the base of the run; the spike arms the name.
//! - **Run and pullback.** Every `review_secs`, an armed name's high since the spike is followed. Its depth is how much of the
//!   run (high minus base) has been given back, in permille. Under `min_pullback_permille` it is still running; between that and
//!   `max_pullback_permille` it is in a pullback, whose low is followed; over `max_pullback_permille` it is dropped for the day (the
//!   run is failing, not pausing). A new high before the turn is a new run.
//! - **Turn and entry.** A pullback that is up `turn_bp` from its low is bought, `dollars` at the ask with a collar, no later than
//!   `last_entry_minutes` before the open, at most `names` positions at once, one trade a name a day, when the quoted spread is at
//!   most `spread_cap_bp` of the mid.
//! - **Exits** are held by the strategy (the broker takes no stop in the premarket, ADR 0034): a stop `stop_buffer_permille` under
//!   the pullback low, raised to `trail_permille` under the highest price since the entry, and a time exit `flat_minutes` before
//!   the open. A position is flat before the regular session.
//!
//! Integers throughout; nothing reads a clock (the review is event time). A name's history is one mark a minute for the last
//! `window_minutes`, so it costs memory for the names that traded and not for the universe.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tf_core::{Event, InstrumentId, NANOS_PER_SEC, Nanos, Px};

use crate::cross::{CrossStrategy, MemberView};
use crate::exits::{ExitBook, ExitPlan, ExitStats};
use crate::intent::{IntentId, Pricing, Purpose, Side, StrategyId, Tif};
use crate::lifecycle::OrderUpdate;
use crate::strategy::{Ctx, Request, TimerId};
use crate::trace::Trace;

/// The `reason` of an entry.
pub const REASON_ENTRY: u16 = 1;

/// The exit book's timers are this plus the instrument number.
const BOOK_TIMERS: u32 = 0x1000_0000;

pub use crate::closing_reversal::ParamError;

/// What a variant of the strategy is. Every field is in the text of [`PremarketPullbackParams::render`], so another value is
/// another variant for the certificate and the trial registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PremarketPullbackParams {
    /// Positions at once.
    pub names: u32,
    /// Dollars bought of each name.
    pub dollars: u32,
    /// The minutes compared with the minutes before them.
    pub window_minutes: u32,
    /// No spike before this many minutes of the premarket have passed.
    pub min_history_minutes: u32,
    /// The window's shares must be at least this / 10 times the average window before it.
    pub spike_x10: u32,
    /// Dollars traded in the window, at least.
    pub min_dollars: u32,
    /// Trades in the window, at least.
    pub min_trades: u32,
    /// The last price, in cents, between these two.
    pub min_cents: u32,
    pub max_cents: u32,
    /// The price up at least this many basis points from the window's low.
    pub min_thrust_bp: u32,
    /// A pullback is this much of the run given back, in permille, at least and at most.
    pub min_pullback_permille: u32,
    pub max_pullback_permille: u32,
    /// Buy when the price is this many basis points up from the pullback's low.
    pub turn_bp: u32,
    /// The quoted spread at the entry, at most this many basis points of the mid; 0 for no cap.
    pub spread_cap_bp: u32,
    /// How far from its reference price an order may fill, in permille, entry and exit.
    pub collar_permille: u32,
    /// The stop is this many permille under the pullback's low.
    pub stop_buffer_permille: u32,
    /// The stop trails the highest price since the entry by this many permille.
    pub trail_permille: u32,
    /// No entry in the last this many minutes before the open.
    pub last_entry_minutes: u32,
    /// Flat this many minutes before the open.
    pub flat_minutes: u32,
    /// Seconds between looks at the names that are armed.
    pub review_secs: u32,
}

impl Default for PremarketPullbackParams {
    fn default() -> Self {
        PremarketPullbackParams {
            names: 3,
            dollars: 1_000,
            window_minutes: 5,
            min_history_minutes: 15,
            spike_x10: 30,
            min_dollars: 50_000,
            min_trades: 20,
            min_cents: 100,
            max_cents: 3_000,
            min_thrust_bp: 300,
            min_pullback_permille: 100,
            max_pullback_permille: 300,
            turn_bp: 30,
            spread_cap_bp: 100,
            collar_permille: 10,
            stop_buffer_permille: 5,
            trail_permille: 30,
            last_entry_minutes: 10,
            flat_minutes: 5,
            review_secs: 5,
        }
    }
}

const KEYS: [&str; 20] = [
    "names",
    "dollars",
    "window_minutes",
    "min_history_minutes",
    "spike_x10",
    "min_dollars",
    "min_trades",
    "min_cents",
    "max_cents",
    "min_thrust_bp",
    "min_pullback_permille",
    "max_pullback_permille",
    "turn_bp",
    "spread_cap_bp",
    "collar_permille",
    "stop_buffer_permille",
    "trail_permille",
    "last_entry_minutes",
    "flat_minutes",
    "review_secs",
];

impl PremarketPullbackParams {
    fn values(&self) -> [u32; 20] {
        [
            self.names,
            self.dollars,
            self.window_minutes,
            self.min_history_minutes,
            self.spike_x10,
            self.min_dollars,
            self.min_trades,
            self.min_cents,
            self.max_cents,
            self.min_thrust_bp,
            self.min_pullback_permille,
            self.max_pullback_permille,
            self.turn_bp,
            self.spread_cap_bp,
            self.collar_permille,
            self.stop_buffer_permille,
            self.trail_permille,
            self.last_entry_minutes,
            self.flat_minutes,
            self.review_secs,
        ]
    }

    fn from_values(v: [u32; 20]) -> PremarketPullbackParams {
        PremarketPullbackParams {
            names: v[0],
            dollars: v[1],
            window_minutes: v[2],
            min_history_minutes: v[3],
            spike_x10: v[4],
            min_dollars: v[5],
            min_trades: v[6],
            min_cents: v[7],
            max_cents: v[8],
            min_thrust_bp: v[9],
            min_pullback_permille: v[10],
            max_pullback_permille: v[11],
            turn_bp: v[12],
            spread_cap_bp: v[13],
            collar_permille: v[14],
            stop_buffer_permille: v[15],
            trail_permille: v[16],
            last_entry_minutes: v[17],
            flat_minutes: v[18],
            review_secs: v[19],
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
        if self.window_minutes == 0 || self.window_minutes > 30 {
            return bad("window_minutes must be from 1 to 30");
        }
        if self.min_history_minutes <= self.window_minutes || self.min_history_minutes > 300 {
            return bad(
                "min_history_minutes must be more than window_minutes (there must be minutes to compare with) and at most 300",
            );
        }
        if self.spike_x10 < 10 {
            return bad("spike_x10 must be at least 10 (a window at least as busy as the average)");
        }
        if self.min_cents == 0 || self.max_cents < self.min_cents {
            return bad("min_cents must be at least 1 and no more than max_cents");
        }
        if self.min_thrust_bp == 0 {
            return bad("min_thrust_bp must be at least 1: a spike is a price run too");
        }
        if self.min_pullback_permille == 0
            || self.max_pullback_permille <= self.min_pullback_permille
            || self.max_pullback_permille > 900
        {
            return bad(
                "pullback bounds must satisfy 0 < min_pullback_permille < max_pullback_permille <= 900",
            );
        }
        if self.turn_bp == 0 {
            return bad("turn_bp must be at least 1: the turn is a rise from the low");
        }
        if self.collar_permille > 999 {
            return bad("collar_permille must be below 1000");
        }
        if self.stop_buffer_permille > 999 {
            return bad("stop_buffer_permille must be below 1000");
        }
        if self.trail_permille == 0 || self.trail_permille > 999 {
            return bad("trail_permille must be from 1 to 999");
        }
        if self.flat_minutes == 0 || self.last_entry_minutes <= self.flat_minutes {
            return bad("flat_minutes must be at least 1 and less than last_entry_minutes");
        }
        if self.last_entry_minutes > 330 {
            return bad("last_entry_minutes must be at most 330 (the premarket is 330 minutes)");
        }
        if self.review_secs == 0 || self.review_secs > 60 {
            return bad("review_secs must be from 1 to 60");
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

    /// Parameters from their text: every key once and no other, and values that pass [`PremarketPullbackParams::validate`].
    pub fn parse(text: &str) -> Result<PremarketPullbackParams, ParamError> {
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
        let mut vals = [0u32; 20];
        for (i, k) in KEYS.iter().enumerate() {
            vals[i] = *got
                .get(k)
                .ok_or_else(|| ParamError(format!("{k} is missing")))?;
        }
        let p = PremarketPullbackParams::from_values(vals);
        p.validate()?;
        Ok(p)
    }
}

/// Counts, for the strategy's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PremarketPullbackStats {
    /// Names armed by a spike.
    pub spikes: u64,
    /// Names bought.
    pub entries: u64,
    /// Armed names given up on, because the run failed (the pullback went past its bound).
    pub failed_runs: u64,
    /// Turns that were not bought: too late, or no room.
    pub missed: u64,
    /// Entries the strategy's own checks refused.
    pub entries_refused: u64,
    /// The exits held and sent.
    pub exits: ExitStats,
}

/// What a name had traded by a minute boundary, and its last price.
#[derive(Clone, Copy, Debug, Default)]
struct Mark {
    minute: u32,
    shares: u64,
    dollars: u64,
    trades: u32,
    px: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Not armed.
    Quiet,
    /// Armed: the run from `base` to `high`.
    Run { base: i64, high: i64 },
    /// In a pullback from `high` to `low`.
    Pullback { base: i64, high: i64, low: i64 },
    /// Bought, or given up on, for the day.
    Done,
}

#[derive(Debug)]
struct Track {
    /// The marks of the last window and the one before it, oldest first.
    marks: VecDeque<Mark>,
    phase: Phase,
}

/// An entry out: the name, what has filled, and the stop it was bought with.
#[derive(Clone, Copy, Debug)]
struct Entry {
    instrument: InstrumentId,
    seen: u32,
    stop: i64,
}

pub struct PremarketPullback {
    id: StrategyId,
    p: PremarketPullbackParams,
    book: ExitBook,
    /// The open of the day the tracks are for.
    open: Option<Nanos>,
    tracks: BTreeMap<InstrumentId, Track>,
    /// The armed names, in order.
    watch: BTreeSet<InstrumentId>,
    /// The highest price since the entry, for each name held or being bought.
    since_entry: BTreeMap<InstrumentId, i64>,
    entries: BTreeMap<IntentId, Entry>,
    /// The minute last marked.
    last_minute: Option<u32>,
    stats: PremarketPullbackStats,
    tracing: bool,
    traces: Vec<Trace>,
}

impl PremarketPullback {
    /// The strategy numbered `id` with `p` (which must pass [`PremarketPullbackParams::validate`]).
    pub fn new(id: u16, p: PremarketPullbackParams) -> Result<PremarketPullback, ParamError> {
        p.validate()?;
        Ok(PremarketPullback {
            id: StrategyId(id),
            p,
            book: ExitBook::new(BOOK_TIMERS),
            open: None,
            tracks: BTreeMap::new(),
            watch: BTreeSet::new(),
            since_entry: BTreeMap::new(),
            entries: BTreeMap::new(),
            last_minute: None,
            stats: PremarketPullbackStats::default(),
            tracing: false,
            traces: Vec::new(),
        })
    }

    pub fn params(&self) -> &PremarketPullbackParams {
        &self.p
    }

    pub fn stats(&self) -> PremarketPullbackStats {
        PremarketPullbackStats {
            exits: self.book.stats(),
            ..self.stats
        }
    }

    fn start_day(&mut self, open: Nanos) {
        self.open = Some(open);
        self.tracks.clear();
        self.watch.clear();
        self.since_entry.clear();
        self.last_minute = None;
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

    /// A mark for each name that has traded, for the minute `m` of the premarket, and the spike test for each of them.
    fn mark_minute(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>, m: u32) {
        let window = self.p.window_minutes;
        for id in view.ids() {
            let Some(st) = view.state(id) else { continue };
            if st.volume == 0 {
                continue;
            }
            let px = st.last_px.map_or(0, |p| p.raw());
            let mark = Mark {
                minute: m,
                shares: st.volume,
                dollars: u64::try_from(st.notional / 1_000_000_000).unwrap_or(u64::MAX),
                trades: st.trades,
                px,
            };
            let track = self.tracks.entry(id).or_insert_with(|| Track {
                marks: VecDeque::new(),
                phase: Phase::Quiet,
            });
            track.marks.push_back(mark);
            // Keep the marks from the one at or before the start of the window on.
            while track.marks.len() > 1 && track.marks[1].minute + window <= m {
                track.marks.pop_front();
            }
            if track.phase != Phase::Quiet {
                continue;
            }
            if let Some(phase) = self.spike(id, m) {
                let track = self.tracks.get_mut(&id).expect("the track was just made");
                track.phase = phase;
                self.watch.insert(id);
                self.stats.spikes += 1;
                if let Phase::Run { base, high } = phase {
                    let fields = [("base", base.to_string()), ("high", high.to_string())];
                    self.note(ctx.now(), "spike", id, &fields);
                }
            }
        }
    }

    /// Whether name `id` is in a spike at minute `m`, and the run it starts.
    fn spike(&self, id: InstrumentId, m: u32) -> Option<Phase> {
        let p = &self.p;
        let track = self.tracks.get(&id)?;
        let now = *track.marks.back()?;
        if m < p.min_history_minutes || now.px <= 0 {
            return None;
        }
        let cents = now.px / 10_000_000;
        if cents < i64::from(p.min_cents) || cents > i64::from(p.max_cents) {
            return None;
        }
        let from = m.checked_sub(p.window_minutes)?;
        // What had been traded when the window began: the last mark at or before it, or nothing for a name that had not traded.
        let start = track
            .marks
            .iter()
            .rev()
            .find(|k| k.minute <= from)
            .copied()
            .unwrap_or_default();
        let recent_shares = now.shares.saturating_sub(start.shares);
        let recent_dollars = now.dollars.saturating_sub(start.dollars);
        let recent_trades = now.trades.saturating_sub(start.trades);
        if recent_dollars < u64::from(p.min_dollars) || recent_trades < p.min_trades {
            return None;
        }
        // At least spike_x10 / 10 times the average window of the minutes before it: recent / window >= x * start / from, in
        // shares a minute, without dividing.
        let lhs = u128::from(recent_shares) * u128::from(from.max(1)) * 10;
        let rhs = u128::from(p.spike_x10) * u128::from(start.shares) * u128::from(p.window_minutes);
        if lhs < rhs {
            return None;
        }
        // The price run: from the lowest price at a mark in the window to the highest.
        let in_window = track.marks.iter().filter(|k| k.minute >= from && k.px > 0);
        let base = in_window.clone().map(|k| k.px).min()?;
        let high = in_window.map(|k| k.px).max()?;
        let thrust = i128::from(now.px - base) * 10_000 / i128::from(base);
        if thrust < i128::from(p.min_thrust_bp) || high <= base {
            return None;
        }
        Some(Phase::Run { base, high })
    }

    /// Look at the armed names, enter on a turn, and follow the stops of the names held.
    fn review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let ids: Vec<InstrumentId> = self.watch.iter().copied().collect();
        for id in ids {
            let Some(st) = view.state(id) else { continue };
            let Some(last) = st.last_px.map(|p| p.raw()).filter(|&p| p > 0) else {
                continue;
            };
            let Some(track) = self.tracks.get_mut(&id) else {
                continue;
            };
            let next = match track.phase {
                Phase::Run { base, high } => {
                    let high = high.max(last);
                    let run = i128::from(high - base);
                    let depth = i128::from(high - last) * 1_000 / run.max(1);
                    if depth > i128::from(self.p.max_pullback_permille) {
                        Phase::Done
                    } else if depth >= i128::from(self.p.min_pullback_permille) {
                        Phase::Pullback {
                            base,
                            high,
                            low: last,
                        }
                    } else {
                        Phase::Run { base, high }
                    }
                }
                Phase::Pullback { base, high, low } => {
                    if last > high {
                        Phase::Run { base, high: last }
                    } else {
                        let low = low.min(last);
                        let run = i128::from(high - base);
                        let depth = i128::from(high - low) * 1_000 / run.max(1);
                        if depth > i128::from(self.p.max_pullback_permille) {
                            Phase::Done
                        } else {
                            Phase::Pullback { base, high, low }
                        }
                    }
                }
                other => other,
            };
            track.phase = next;
            match next {
                Phase::Done => {
                    self.watch.remove(&id);
                    self.stats.failed_runs += 1;
                    self.note(
                        ctx.now(),
                        "drop",
                        id,
                        &[("reason", "pullback_too_deep".to_owned())],
                    );
                }
                Phase::Pullback { base, high, low } => {
                    let turn = i128::from(last - low) * 10_000 / i128::from(low.max(1));
                    if turn >= i128::from(self.p.turn_bp) {
                        self.try_enter(ctx, view, id, base, high, low);
                    }
                }
                _ => {}
            }
        }
        self.trail(view);
    }

    /// Buy `id` at its ask on the turn of its pullback, if there is room and time and the quote is a fair one.
    fn try_enter(
        &mut self,
        ctx: &mut Ctx<'_>,
        view: &MemberView<'_>,
        id: InstrumentId,
        base: i64,
        high: i64,
        low: i64,
    ) {
        let Some(open) = self.open else { return };
        let now = ctx.now();
        let last_entry =
            open.saturating_sub(u64::from(self.p.last_entry_minutes) * 60 * NANOS_PER_SEC);
        let done = |s: &mut Self, why: &str| {
            if let Some(track) = s.tracks.get_mut(&id) {
                track.phase = Phase::Done;
            }
            s.watch.remove(&id);
            s.stats.missed += 1;
            s.note(now, "drop", id, &[("reason", why.to_owned())]);
        };
        if now > last_entry {
            done(self, "too_late");
            return;
        }
        if self.entries.len() + self.book.len() >= self.p.names as usize {
            done(self, "no_room");
            return;
        }
        let Some(st) = view.state(id) else { return };
        if st.halted {
            return;
        }
        let (Some(bid), Some(ask)) = (st.bid, st.ask) else {
            return;
        };
        let (bid, ask) = (bid.0.raw(), ask.0.raw());
        if bid <= 0 || ask < bid {
            return;
        }
        if self.p.spread_cap_bp > 0 {
            let mid2 = i128::from(bid) + i128::from(ask);
            if i128::from(ask - bid) * 20_000 > i128::from(self.p.spread_cap_bp) * mid2 {
                return;
            }
        }
        let qty = u128::from(self.p.dollars) * 1_000_000_000 / ask.max(1) as u128;
        let Ok(qty) = u32::try_from(qty) else { return };
        if qty == 0 {
            return;
        }
        let stop = i128::from(low) * i128::from(1_000 - self.p.stop_buffer_permille) / 1_000;
        let Ok(stop) = i64::try_from(stop) else {
            return;
        };
        if stop <= 0 {
            return;
        }
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
                self.entries.insert(
                    intent,
                    Entry {
                        instrument: id,
                        seen: 0,
                        stop,
                    },
                );
                self.since_entry.insert(id, ask);
                if let Some(track) = self.tracks.get_mut(&id) {
                    track.phase = Phase::Done;
                }
                self.watch.remove(&id);
                let fields = [
                    ("base", base.to_string()),
                    ("high", high.to_string()),
                    ("low", low.to_string()),
                    ("ask", ask.to_string()),
                    ("stop", stop.to_string()),
                ];
                self.note(now, "entry", id, &fields);
            }
            Err(_) => self.stats.entries_refused += 1,
        }
    }

    /// Raise the stop of each name held to the trail under its highest price since the entry.
    fn trail(&mut self, view: &MemberView<'_>) {
        let trail = i128::from(1_000 - self.p.trail_permille);
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
            if let Ok(stop) = i64::try_from(i128::from(*high) * trail / 1_000) {
                self.book.raise_stop(id, Px::from_raw(stop));
            }
        }
    }

    /// The minute of the premarket `now` is in, if it is in it.
    fn minute(&self, ctx: &Ctx<'_>) -> Option<u32> {
        let day = ctx.day()?;
        let now = ctx.now();
        if now < day.premarket || now >= day.open {
            return None;
        }
        u32::try_from((now - day.premarket) / (60 * NANOS_PER_SEC)).ok()
    }
}

impl CrossStrategy for PremarketPullback {
    /// Only for the exit book's stop: a trade of a name held.
    const WANTS_MEMBER_EVENTS: bool = true;

    fn id(&self) -> StrategyId {
        self.id
    }

    fn period(&self) -> Nanos {
        u64::from(self.p.review_secs) * NANOS_PER_SEC
    }

    fn on_review(&mut self, ctx: &mut Ctx<'_>, view: &MemberView<'_>) {
        let Some(open) = ctx.day().map(|d| d.open) else {
            return;
        };
        if self.open != Some(open) {
            self.start_day(open);
        }
        let Some(m) = self.minute(ctx) else { return };
        if self.last_minute != Some(m) {
            self.last_minute = Some(m);
            self.mark_minute(ctx, view, m);
        }
        self.review(ctx, view);
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
        let (instrument, stop) = (e.instrument, e.stop);
        if update.state.is_terminal() {
            self.entries.remove(&update.intent);
            if update.filled_qty == 0 {
                self.since_entry.remove(&instrument);
            }
        }
        let Some(open) = self.open else { return };
        let plan = ExitPlan {
            stop: Some(Px::from_raw(stop)),
            flat_by: Some(open.saturating_sub(u64::from(self.p.flat_minutes) * 60 * NANOS_PER_SEC)),
            collar_permille: self.p.collar_permille,
            ..ExitPlan::new()
        };
        // Nothing filled adds nothing: the book ignores a size of 0.
        self.book.arm(ctx, instrument, true, new, plan);
    }
}
