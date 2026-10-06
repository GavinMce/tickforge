//! Promotion and demotion between Tier 0 and Tier 1, with hysteresis.
//!
//! Tier 0 holds a few numbers for every symbol; Tier 1 holds full history and
//! features for the few worth watching closely (about fifty). The [`Promoter`] moves
//! symbols between them on the [`Scanner`]'s hits, and every move is a
//! [`TierChange`] event for the tape, so a replay follows exactly the membership the
//! session had.
//!
//! **Hysteresis**, so the membership does not flap:
//! - the bar to get in (the scanner's z-score, 8 standard deviations) is much higher
//!   than the bar to stay (`demote_z_milli`, 3 by default): a promoted symbol is
//!   "hot" while its 10 s volume stays above the lower bar, and is demoted only
//!   after `cooldown_secs` without being hot;
//! - a promoted symbol stays at least `min_dwell_secs`;
//! - a demoted symbol cannot be promoted again for `repromote_after_secs`;
//! - optionally `confirm_hits` hits within `confirm_window_secs` are needed to
//!   promote;
//! - a **pinned** symbol (one a strategy holds a position in) is never demoted.
//!
//! **Bounded**: at most `max_tier1` symbols are promoted. A hit that finds it full is
//! counted ([`Promoter::refused_full`]) and not promoted; membership frees up only by
//! demotion.
//!
//! **Where the tape puts a change.** A decision is caused by an event and takes effect for
//! that same event: the strategy that sees the event sees the symbol already promoted, and
//! Tier 1 records the event. So the tape must hold each `TierChange` *immediately before*
//! the event that caused it (same timestamp); then a follower that applies the change and
//! then the event reproduces exactly what the live run saw.
//!
//! **Two modes, one rule.** A deciding promoter ([`Promoter::new`]) turns hits into
//! events and applies them to itself through [`Promoter::apply`]. A following one
//! ([`Promoter::follower`]) has no scanner and applies the `TierChange` events it
//! sees, which is what replaying a tape does. Since `apply` is the only thing that
//! changes membership, both end in the same state.
//!
//! Integers only; time from events.

use tf_core::{
    Event, Header, InstrumentId, NANOS_PER_SEC, Nanos, ProviderId, TierAction, TierChange,
};

use crate::claims::{Claims, Denied, Grant, Owner, OwnerStats};
use crate::{
    BASE_SECS, Hit, PromoteError, Scanner, ScannerConfig, ScannerError, Tier0, Tier1, Tier1Symbol,
};

/// Reason codes on [`TierChange`] events.
pub mod reason {
    /// Promoted on a scanner hit.
    pub const SCANNER_HIT: u8 = 1;
    /// Demoted after `cooldown_secs` without being hot.
    pub const COOLED_OFF: u8 = 2;
    /// Promoted because a strategy asked for it.
    pub const STRATEGY_REQUEST: u8 = 3;
    /// Demoted to make room for a request from a strategy of higher priority.
    pub const EVICTED: u8 = 4;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromoterConfig {
    /// Most symbols in Tier 1 at once.
    pub max_tier1: usize,
    /// A promoted symbol is hot while its 10 s volume z-score is at least this (times
    /// 1000). Must be below the scanner's promotion bar.
    pub demote_z_milli: i64,
    /// Demote after this many seconds without being hot.
    pub cooldown_secs: u64,
    /// A promoted symbol stays at least this long.
    pub min_dwell_secs: u64,
    /// A demoted symbol cannot be promoted again for this long.
    pub repromote_after_secs: u64,
    /// Hits needed within `confirm_window_secs` to promote (1 = promote on the first).
    pub confirm_hits: u32,
    pub confirm_window_secs: u64,
}

impl Default for PromoterConfig {
    fn default() -> Self {
        PromoterConfig {
            max_tier1: 50,
            demote_z_milli: 3_000,
            cooldown_secs: 120,
            min_dwell_secs: 60,
            repromote_after_secs: 120,
            confirm_hits: 1,
            confirm_window_secs: 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromoterError(pub &'static str);

impl From<ScannerError> for PromoterError {
    fn from(e: ScannerError) -> Self {
        PromoterError(e.0)
    }
}

/// Why a `TierChange` could not be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierError {
    Promote(PromoteError),
    /// A demotion of a symbol that is not promoted.
    NotPromoted,
}

#[derive(Clone, Copy, Default)]
struct State {
    promoted: bool,
    promoted_sec: u64,
    last_hot_sec: u64,
    demoted_sec: Option<u64>,
    first_hit_sec: u64,
    hits: u32,
}

enum Mode {
    Decide(Box<Scanner>),
    Follow,
}

pub struct Promoter {
    cfg: PromoterConfig,
    mode: Mode,
    tier1: Tier1,
    state: Vec<State>,
    /// Promoted ids in ascending order, so sweeps and events are deterministic.
    promoted: Vec<InstrumentId>,
    next_seq: u64,
    last_sweep_sec: u64,
    refused_full: u64,
    claims: Claims,
}

impl Promoter {
    /// A promoter that decides: it runs a scanner and turns its hits into events.
    pub fn new(
        cfg: PromoterConfig,
        scanner: ScannerConfig,
        id_space: usize,
    ) -> Result<Promoter, PromoterError> {
        if cfg.max_tier1 == 0
            || cfg.cooldown_secs == 0
            || cfg.confirm_hits == 0
            || cfg.confirm_window_secs == 0
        {
            return Err(PromoterError(
                "max_tier1, cooldown_secs, confirm_hits and confirm_window_secs must be positive",
            ));
        }
        if cfg.demote_z_milli <= 0 || cfg.demote_z_milli >= scanner.min_z_milli {
            return Err(PromoterError(
                "demote_z_milli must be positive and below the scanner's min_z_milli (that gap is the hysteresis)",
            ));
        }
        let scanner = Scanner::new(scanner, id_space)?;
        Ok(Promoter::build(
            cfg,
            Mode::Decide(Box::new(scanner)),
            id_space,
        ))
    }

    /// A promoter that follows the tape: it decides nothing and applies the
    /// `TierChange` events in the stream it is fed.
    pub fn follower(max_tier1: usize, id_space: usize) -> Promoter {
        let cfg = PromoterConfig {
            max_tier1,
            ..PromoterConfig::default()
        };
        Promoter::build(cfg, Mode::Follow, id_space)
    }

    fn build(cfg: PromoterConfig, mode: Mode, id_space: usize) -> Promoter {
        Promoter {
            tier1: Tier1::new(id_space, cfg.max_tier1),
            cfg,
            mode,
            state: vec![State::default(); id_space],
            promoted: Vec::new(),
            next_seq: 0,
            last_sweep_sec: 0,
            refused_full: 0,
            claims: Claims::default(),
        }
    }

    pub fn config(&self) -> &PromoterConfig {
        &self.cfg
    }

    /// Record the float of `id` for the scanner's float filter.
    pub fn set_float(&mut self, id: InstrumentId, shares: u64) {
        if let Mode::Decide(sc) = &mut self.mode {
            sc.set_float(id, shares);
        }
    }

    /// `owner` holds `id` (it has a position or a working order there): the symbol is never demoted or
    /// evicted while anyone holds it. Holds are counted per owner.
    pub fn pin(&mut self, owner: Owner, id: InstrumentId) {
        if (id as usize) < self.state.len() {
            self.claims.set_hold(owner, id, true);
        }
    }

    pub fn unpin(&mut self, owner: Owner, id: InstrumentId) {
        self.claims.set_hold(owner, id, false);
    }

    /// Whether anyone holds `id`.
    pub fn is_pinned(&self, id: InstrumentId) -> bool {
        self.claims.has_hold(id)
    }

    pub fn is_pinned_by(&self, owner: Owner, id: InstrumentId) -> bool {
        self.claims.is_held_by(owner, id)
    }

    /// Whether `owner` is interested in `id`.
    pub fn is_wanted_by(&self, owner: Owner, id: InstrumentId) -> bool {
        self.claims.has_interest_of(owner, id)
    }

    /// Set a strategy's priority for evictions (see the `claims` module for the rule).
    pub fn set_priority(&mut self, owner: Owner, priority: u8) {
        self.claims.set_priority(owner, priority);
    }

    /// `owner` asks for `id` to be in Tier 1. A promotion (and an eviction, if Tier 1 is full) is
    /// applied now and appended to `out` for the tape. A denial is counted for `owner`.
    pub fn request(
        &mut self,
        owner: Owner,
        id: InstrumentId,
        ts: Nanos,
        out: &mut Vec<TierChange>,
    ) -> Grant {
        self.claims.stat(owner).requests += 1;
        let Some(st) = self.state.get(id as usize) else {
            self.claims.stat(owner).denied_other += 1;
            return Grant::Denied(Denied::Unknown);
        };
        if st.promoted {
            self.claims.set_interest(owner, id, true);
            self.claims.stat(owner).already += 1;
            return Grant::Already;
        }
        if matches!(self.mode, Mode::Follow) {
            self.claims.stat(owner).denied_other += 1;
            return Grant::Denied(Denied::Following);
        }
        let sec = ts / NANOS_PER_SEC;
        let mut evicted = None;
        if self.promoted.len() >= self.cfg.max_tier1 {
            let Some(victim) = self.victim(owner, sec) else {
                self.claims.stat(owner).denied_full += 1;
                return Grant::Denied(Denied::Full);
            };
            let ev = self.event(victim, ts, TierAction::Demote, reason::EVICTED, 0);
            if self.apply(&ev).is_err() {
                self.claims.stat(owner).denied_other += 1;
                return Grant::Denied(Denied::Full);
            }
            out.push(ev);
            evicted = Some(victim);
        }
        let ev = self.event(id, ts, TierAction::Promote, reason::STRATEGY_REQUEST, 0);
        if self.apply(&ev).is_err() {
            self.claims.stat(owner).denied_other += 1;
            return Grant::Denied(Denied::Full);
        }
        out.push(ev);
        self.claims.set_interest(owner, id, true);
        let s = self.claims.stat(owner);
        s.promoted += 1;
        match evicted {
            Some(evicted) => {
                s.evicted_for += 1;
                Grant::PromotedByEviction { evicted }
            }
            None => Grant::Promoted,
        }
    }

    /// `owner` no longer wants `id`. The symbol stays until nobody does and it has cooled off.
    pub fn release(&mut self, owner: Owner, id: InstrumentId) {
        self.claims.set_interest(owner, id, false);
    }

    /// Interests lost since the last call because their symbol left Tier 1, as `(owner, symbol)`.
    pub fn drain_revoked(&mut self) -> Vec<(Owner, InstrumentId)> {
        self.claims.drain_revoked()
    }

    /// Per strategy counts of requests and their outcomes.
    pub fn owner_stats(&self) -> Vec<(Owner, OwnerStats)> {
        self.claims.stats()
    }

    /// The counts as text, one `name{strategy="N"} value` line each (the shape a metrics scrape reads),
    /// and the totals for the promoter as a whole.
    pub fn metrics_text(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "tier1_symbols {}", self.promoted.len());
        let _ = writeln!(s, "tier1_capacity {}", self.cfg.max_tier1);
        let _ = writeln!(s, "tier1_scanner_refused_full {}", self.refused_full);
        for (o, st) in self.claims.stats() {
            for (name, v) in [
                ("requests", st.requests),
                ("already", st.already),
                ("promoted", st.promoted),
                ("evicted_for", st.evicted_for),
                ("denied_full", st.denied_full),
                ("denied_other", st.denied_other),
                ("lost", st.lost),
            ] {
                let _ = writeln!(s, "tier1_{name}{{strategy=\"{o}\"}} {v}");
            }
        }
        s
    }

    /// The symbol to demote for a request from `owner`, by the rule in the `claims` module.
    fn victim(&self, owner: Owner, sec: u64) -> Option<InstrumentId> {
        let mine = self.claims.priority(owner);
        self.promoted
            .iter()
            .copied()
            .filter(|&id| {
                let st = &self.state[id as usize];
                !self.claims.has_hold(id)
                    && sec >= st.promoted_sec + self.cfg.min_dwell_secs
                    && self.claims.level(id).0 < mine
            })
            .min_by_key(|&id| {
                let (level, n) = self.claims.level(id);
                (level, n, self.state[id as usize].last_hot_sec, id)
            })
    }

    pub fn is_promoted(&self, id: InstrumentId) -> bool {
        self.state.get(id as usize).is_some_and(|s| s.promoted)
    }

    /// The promoted symbols, in id order.
    pub fn promoted(&self) -> &[InstrumentId] {
        &self.promoted
    }

    /// Tier 1 state of a promoted symbol.
    pub fn symbol(&self, id: InstrumentId) -> Option<&Tier1Symbol> {
        self.tier1.symbol(id)
    }

    /// Hits that found Tier 1 full.
    pub fn refused_full(&self) -> u64 {
        self.refused_full
    }

    /// Apply a tier change. The only thing that changes membership, live and in replay.
    /// Nothing changes on error.
    pub fn apply(&mut self, c: &TierChange) -> Result<(), TierError> {
        let id = c.hdr.instrument;
        let sec = c.hdr.ts_recv / NANOS_PER_SEC;
        match c.action {
            TierAction::Promote => {
                self.tier1.promote(id).map_err(TierError::Promote)?;
                let s = &mut self.state[id as usize];
                s.promoted = true;
                s.promoted_sec = sec;
                s.last_hot_sec = sec;
                s.hits = 0;
                let at = self.promoted.partition_point(|&p| p < id);
                self.promoted.insert(at, id);
            }
            TierAction::Demote => {
                if !self.tier1.demote(id) {
                    return Err(TierError::NotPromoted);
                }
                let s = &mut self.state[id as usize];
                s.promoted = false;
                s.demoted_sec = Some(sec);
                s.hits = 0;
                self.promoted.retain(|&p| p != id);
                self.claims.revoke_interests(id);
            }
        }
        self.next_seq = self.next_seq.max(c.hdr.seq.saturating_add(1));
        Ok(())
    }

    fn event(
        &mut self,
        id: InstrumentId,
        ts: Nanos,
        action: TierAction,
        reason: u8,
        score: i64,
    ) -> TierChange {
        let seq = self.next_seq;
        TierChange {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq,
                instrument: id,
                provider: ProviderId::Internal,
            },
            action,
            reason,
            score,
        }
    }

    /// Feed an event that Tier 0 has already absorbed. A deciding promoter appends the
    /// tier changes it makes to `out` (and has already applied them); a following one
    /// applies `TierChange` events from the stream and appends nothing. Either way,
    /// Tier 1 receives the trades and quotes of promoted symbols.
    pub fn on_event(&mut self, tier0: &Tier0, ev: &Event, out: &mut Vec<TierChange>) {
        match (&self.mode, ev) {
            (Mode::Follow, Event::TierChange(c)) => {
                let _ = self.apply(c);
                return;
            }
            (_, Event::TierChange(_)) => return,
            _ => {}
        }
        let Mode::Decide(_) = self.mode else {
            self.tier1.on_event(ev);
            return;
        };
        let ts = ev.ts_recv();
        let sec = ts / NANOS_PER_SEC;
        let mut hits = Vec::new();
        if let Mode::Decide(sc) = &mut self.mode {
            sc.on_event(tier0, ev, &mut hits);
        }
        for h in hits {
            self.on_hit(&h, sec, ts, out);
        }
        if sec > self.last_sweep_sec {
            self.last_sweep_sec = sec;
            self.sweep(tier0, sec, ts, out);
        }
        // Tier 1 sees the event after the decisions it caused, so a follower that applies
        // the change first and then the event ends up with exactly the same history.
        self.tier1.on_event(ev);
    }

    fn on_hit(&mut self, h: &Hit, sec: u64, ts: Nanos, out: &mut Vec<TierChange>) {
        let cfg = self.cfg;
        let st = &mut self.state[h.instrument as usize];
        if st.promoted {
            return; // the once-a-second sweep keeps it hot: a hit is a z-score above the demotion bar
        }
        if st
            .demoted_sec
            .is_some_and(|d| sec < d + cfg.repromote_after_secs)
        {
            return;
        }
        if st.hits == 0 || sec > st.first_hit_sec + cfg.confirm_window_secs {
            st.first_hit_sec = sec;
            st.hits = 0;
        }
        st.hits += 1;
        if st.hits < cfg.confirm_hits {
            return;
        }
        if self.promoted.len() >= cfg.max_tier1 {
            self.refused_full += 1;
            return;
        }
        let ev = self.event(
            h.instrument,
            ts,
            TierAction::Promote,
            reason::SCANNER_HIT,
            h.z_milli,
        );
        if self.apply(&ev).is_ok() {
            out.push(ev);
        }
    }

    /// Once a second: refresh how hot each promoted symbol is, and demote the cold ones.
    fn sweep(&mut self, tier0: &Tier0, sec: u64, ts: Nanos, out: &mut Vec<TierChange>) {
        let ids = self.promoted.clone();
        for id in ids {
            let z = match (&self.mode, tier0.windows(id)) {
                (Mode::Decide(sc), Some(w)) => sc.z_milli(id, w.volume_asof(sec, BASE_SECS)),
                _ => None,
            };
            let st = &mut self.state[id as usize];
            if z.is_some_and(|z| z >= self.cfg.demote_z_milli) {
                st.last_hot_sec = sec;
            }
            let cold = sec >= st.last_hot_sec + self.cfg.cooldown_secs;
            let dwelt = sec >= st.promoted_sec + self.cfg.min_dwell_secs;
            if cold && dwelt && !self.claims.is_claimed(id) {
                let ev = self.event(
                    id,
                    ts,
                    TierAction::Demote,
                    reason::COOLED_OFF,
                    z.unwrap_or(0),
                );
                if self.apply(&ev).is_ok() {
                    out.push(ev);
                }
            }
        }
    }
}
