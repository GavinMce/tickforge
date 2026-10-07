//! One set of multi-timeframe bars for every strategy of an engine (E19-S03).
//!
//! A per-symbol `Host` owns its own [`MtfBars`]; twenty cross-sectional strategies over the same
//! thousand symbols must not build twenty copies (about 40 KB a symbol each). [`SharedBars`] wraps one
//! [`MtfBars`] for the union of the symbols that strategies ask for, and keeps a *claim* per strategy and
//! symbol, in the manner of the Tier 1 claims (ADR 0044):
//!
//! - **Counted per strategy.** A symbol is tracked while any strategy claims it. One strategy letting go
//!   does not stop another's bars; the last to let go drops them.
//! - **Bounded, and a refusal is an answer.** The tracked set has a hard limit (memory is about 40 KB a
//!   symbol). A claim for a new symbol past it is refused with [`BarsRefused::Full`], counted against the
//!   strategy that asked, and listed in [`SharedBars::metrics_text`]. A claim for a symbol already tracked
//!   always succeeds, because it costs nothing.
//! - **Read through your own claim.** [`SharedBars::symbol`] gives bars only to a strategy that claimed
//!   the symbol, so what a strategy can see does not depend on which other strategies are running.
//!
//! Bars begin with the first trade after the first claim; nothing is back-filled. A strategy that claims
//! a symbol another already tracks sees the bars from that earlier start (a superset of what it would
//! have built alone, never a different bar: a closed bar is a function of the trades in its interval).

use std::collections::BTreeMap;

use tf_core::{Event, InstrumentId, Nanos};

use crate::claims::Owner;
use crate::mtf::{BarClose, MtfBars, MtfConfig, SymbolBars, TrackError};

/// Why a claim was not granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarsRefused {
    /// The tracked set is at its limit and the symbol is not in it.
    Full,
    /// The id is outside the id space.
    Unknown,
}

/// What a granted claim did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarsGrant {
    /// The symbol was not tracked: bars start with its next trade.
    Started,
    /// Another strategy already tracks the symbol: its bars are shared.
    Joined,
    /// The strategy already had this claim.
    Already,
}

/// Per-strategy counts, for the metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BarsStats {
    pub requests: u64,
    pub started: u64,
    pub joined: u64,
    pub already: u64,
    pub refused_full: u64,
    pub refused_unknown: u64,
    pub released: u64,
}

/// Bars shared by the strategies of one engine.
pub struct SharedBars {
    bars: MtfBars,
    /// Tracked symbols and the strategies claiming them, owners ascending.
    owners: BTreeMap<InstrumentId, Vec<Owner>>,
    stats: BTreeMap<Owner, BarsStats>,
}

impl SharedBars {
    pub fn new(cfg: MtfConfig, id_space: usize, max_tracked: usize) -> SharedBars {
        SharedBars {
            bars: MtfBars::new(cfg, id_space, max_tracked),
            owners: BTreeMap::new(),
            stats: BTreeMap::new(),
        }
    }

    fn stat(&mut self, owner: Owner) -> &mut BarsStats {
        self.stats.entry(owner).or_default()
    }

    /// `owner` asks for bars of `id`.
    pub fn claim(&mut self, owner: Owner, id: InstrumentId) -> Result<BarsGrant, BarsRefused> {
        self.stat(owner).requests += 1;
        if let Some(v) = self.owners.get_mut(&id) {
            return Ok(match v.binary_search(&owner) {
                Ok(_) => {
                    self.stat(owner).already += 1;
                    BarsGrant::Already
                }
                Err(at) => {
                    v.insert(at, owner);
                    self.stat(owner).joined += 1;
                    BarsGrant::Joined
                }
            });
        }
        match self.bars.track(id) {
            Ok(()) => {
                self.owners.insert(id, vec![owner]);
                self.stat(owner).started += 1;
                Ok(BarsGrant::Started)
            }
            Err(TrackError::Full) => {
                self.stat(owner).refused_full += 1;
                Err(BarsRefused::Full)
            }
            Err(TrackError::Unknown) => {
                self.stat(owner).refused_unknown += 1;
                Err(BarsRefused::Unknown)
            }
            // Tracked without an owner cannot happen: every track goes through this method.
            Err(TrackError::AlreadyTracked) => unreachable!("a tracked symbol always has an owner"),
        }
    }

    /// `owner` no longer needs `id`. True if it had a claim. The bars are dropped with the last claim.
    pub fn release(&mut self, owner: Owner, id: InstrumentId) -> bool {
        let Some(v) = self.owners.get_mut(&id) else {
            return false;
        };
        let Ok(at) = v.binary_search(&owner) else {
            return false;
        };
        v.remove(at);
        if v.is_empty() {
            self.owners.remove(&id);
            self.bars.untrack(id);
        }
        self.stat(owner).released += 1;
        true
    }

    /// Release everything `owner` claimed (it stopped). How many claims it had.
    pub fn release_all(&mut self, owner: Owner) -> usize {
        let mine: Vec<InstrumentId> = self
            .owners
            .iter()
            .filter(|(_, v)| v.binary_search(&owner).is_ok())
            .map(|(&id, _)| id)
            .collect();
        for &id in &mine {
            self.release(owner, id);
        }
        mine.len()
    }

    pub fn is_claimed_by(&self, owner: Owner, id: InstrumentId) -> bool {
        self.owners
            .get(&id)
            .is_some_and(|v| v.binary_search(&owner).is_ok())
    }

    /// The strategies claiming `id`, ascending.
    pub fn owners_of(&self, id: InstrumentId) -> &[Owner] {
        self.owners.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The bars of `id`, for a strategy that claimed it; `None` for any other.
    pub fn symbol(&self, owner: Owner, id: InstrumentId) -> Option<&SymbolBars> {
        if self.is_claimed_by(owner, id) {
            self.bars.symbol(id)
        } else {
            None
        }
    }

    /// Feed a market event; closes are appended to `out` (see [`MtfBars::on_event`]).
    pub fn on_event(&mut self, ev: &Event, out: &mut Vec<BarClose>) {
        self.bars.on_event(ev, out);
    }

    /// Time has reached `ts` (see [`MtfBars::advance_to`]).
    pub fn advance_to(&mut self, ts: Nanos, out: &mut Vec<BarClose>) {
        self.bars.advance_to(ts, out);
    }

    /// Symbols being tracked.
    pub fn tracked(&self) -> usize {
        self.bars.tracked()
    }

    pub fn capacity(&self) -> usize {
        self.bars.max_tracked()
    }

    /// Trades session alignment put in no bar.
    pub fn unplaced(&self) -> u64 {
        self.bars.unplaced()
    }

    pub fn config(&self) -> &MtfConfig {
        self.bars.config()
    }

    pub fn stats(&self) -> Vec<(Owner, BarsStats)> {
        self.stats.iter().map(|(o, s)| (*o, *s)).collect()
    }

    /// The counts as text, one `name{strategy="N"} value` line each, and the totals.
    pub fn metrics_text(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "bars_symbols {}", self.tracked());
        let _ = writeln!(s, "bars_capacity {}", self.capacity());
        let _ = writeln!(s, "bars_unplaced_trades {}", self.unplaced());
        for (o, st) in &self.stats {
            for (name, v) in [
                ("requests", st.requests),
                ("started", st.started),
                ("joined", st.joined),
                ("already", st.already),
                ("refused_full", st.refused_full),
                ("refused_unknown", st.refused_unknown),
                ("released", st.released),
            ] {
                let _ = writeln!(s, "bars_{name}{{strategy=\"{o}\"}} {v}");
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mtf::Timeframe;
    use tf_core::{Header, NANOS_PER_SEC, ProviderId, Px, Trade, TradeFlags};

    fn trade(inst: u32, sec: u64, cents: i64, size: u32) -> Event {
        let ts = sec * NANOS_PER_SEC;
        Event::Trade(Trade {
            hdr: Header {
                ts_event: ts,
                ts_recv: ts,
                seq: ts,
                instrument: inst,
                provider: ProviderId::Synthetic,
            },
            px: Px::from_cents(cents),
            size,
            flags: TradeFlags::NONE,
        })
    }

    fn shared(max: usize) -> SharedBars {
        SharedBars::new(MtfConfig::default(), 8, max)
    }

    #[test]
    fn a_symbol_is_tracked_while_any_strategy_claims_it() {
        let mut b = shared(4);
        assert_eq!(b.claim(1, 3), Ok(BarsGrant::Started));
        assert_eq!(b.claim(2, 3), Ok(BarsGrant::Joined));
        assert_eq!(b.claim(2, 3), Ok(BarsGrant::Already));
        assert_eq!(b.owners_of(3), [1, 2]);
        let st: BTreeMap<_, _> = b.stats().into_iter().collect();
        assert_eq!(
            (st[&2].joined, st[&2].already, st[&2].started),
            (1, 1, 0),
            "a repeat is counted as a repeat"
        );
        assert_eq!(b.tracked(), 1, "one set of bars for two strategies");
        let mut out = Vec::new();
        for k in 0..130 {
            b.on_event(&trade(3, 1_000_000 + k, 1000 + k as i64, 1), &mut out);
        }
        let before = *b.symbol(1, 3).unwrap();
        // Strategy 1 lets go: strategy 2's bars carry on, unbroken.
        assert!(b.release(1, 3));
        assert!(!b.release(1, 3), "a second release is nothing");
        assert!(b.symbol(1, 3).is_none());
        assert_eq!(b.tracked(), 1);
        b.on_event(&trade(3, 1_000_200, 5000, 1), &mut out);
        let s2 = b.symbol(2, 3).unwrap();
        assert_eq!(
            s2.closed_total(Timeframe::M1),
            before.closed_total(Timeframe::M1) + 1
        );
        assert_eq!(
            s2.closed(Timeframe::M1, 1),
            before.closed(Timeframe::M1, 0),
            "the bars it had are still there"
        );
        // The last to let go drops them.
        assert!(b.release(2, 3));
        assert_eq!(b.tracked(), 0);
        assert!(b.symbol(2, 3).is_none());
        assert_eq!(b.claim(1, 3), Ok(BarsGrant::Started));
        assert_eq!(
            b.symbol(1, 3).unwrap().closed_total(Timeframe::M1),
            0,
            "a symbol dropped and asked for again starts empty"
        );
    }

    #[test]
    fn a_strategy_reads_only_what_it_claimed() {
        let mut b = shared(4);
        b.claim(1, 0).unwrap();
        b.claim(2, 1).unwrap();
        let mut out = Vec::new();
        b.on_event(&trade(0, 100, 1000, 1), &mut out);
        b.on_event(&trade(1, 100, 1000, 1), &mut out);
        assert!(b.symbol(1, 0).is_some());
        assert!(b.symbol(1, 1).is_none(), "tracked for another strategy");
        assert!(b.symbol(2, 0).is_none());
        assert!(b.symbol(3, 0).is_none(), "a strategy that never asked");
    }

    #[test]
    fn the_tracked_set_is_bounded_and_a_refusal_is_counted_for_who_asked() {
        let mut b = shared(2);
        assert_eq!(b.claim(1, 0), Ok(BarsGrant::Started));
        assert_eq!(b.claim(1, 1), Ok(BarsGrant::Started));
        assert_eq!(b.claim(2, 2), Err(BarsRefused::Full));
        assert_eq!(b.claim(2, 7), Err(BarsRefused::Full));
        assert_eq!(b.claim(2, 99), Err(BarsRefused::Unknown));
        // A symbol already tracked costs nothing, so it is granted when full.
        assert_eq!(b.claim(2, 1), Ok(BarsGrant::Joined));
        assert_eq!(b.tracked(), 2);
        // Room is made by the last claim on a symbol going.
        assert!(b.release(1, 0));
        assert_eq!(b.claim(2, 2), Ok(BarsGrant::Started));
        let st: BTreeMap<_, _> = b.stats().into_iter().collect();
        assert_eq!(
            st[&2],
            BarsStats {
                requests: 5,
                started: 1,
                joined: 1,
                already: 0,
                refused_full: 2,
                refused_unknown: 1,
                released: 0,
            }
        );
        let text = b.metrics_text();
        for line in [
            "bars_symbols 2",
            "bars_capacity 2",
            "bars_refused_full{strategy=\"2\"} 2",
            "bars_refused_unknown{strategy=\"2\"} 1",
            "bars_started{strategy=\"1\"} 2",
            "bars_released{strategy=\"1\"} 1",
        ] {
            assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
        }
    }

    #[test]
    fn a_strategy_that_stops_gives_up_all_its_claims_and_no_one_elses() {
        let mut b = shared(8);
        for id in 0..4 {
            b.claim(1, id).unwrap();
        }
        b.claim(2, 1).unwrap();
        b.claim(2, 5).unwrap();
        assert_eq!(b.release_all(1), 4);
        assert_eq!(b.tracked(), 2);
        assert_eq!(b.owners_of(1), [2]);
        assert_eq!(b.owners_of(5), [2]);
        assert_eq!(b.release_all(1), 0);
    }

    #[test]
    fn shared_bars_are_exactly_the_bars_of_an_aggregator_built_alone() {
        // Two strategies on overlapping symbols against one aggregator per strategy.
        let mut shared = shared(8);
        let mut a = MtfBars::new(MtfConfig::default(), 8, 8);
        let mut c = MtfBars::new(MtfConfig::default(), 8, 8);
        for id in [0, 1, 2] {
            shared.claim(1, id).unwrap();
            a.track(id).unwrap();
        }
        for id in [1, 2, 3] {
            shared.claim(2, id).unwrap();
            c.track(id).unwrap();
        }
        let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let (mut out, mut out_a, mut out_c) = (Vec::new(), Vec::new(), Vec::new());
        let mut sec = 2_000_000;
        for _ in 0..4000 {
            sec += next() % 7;
            let ev = trade(
                (next() % 5) as u32,
                sec,
                1000 + (next() % 200) as i64,
                1 + (next() % 50) as u32,
            );
            shared.on_event(&ev, &mut out);
            a.on_event(&ev, &mut out_a);
            c.on_event(&ev, &mut out_c);
        }
        for (owner, alone) in [(1u16, &a), (2, &c)] {
            for id in 0..5 {
                match (shared.symbol(owner, id), alone.symbol(id)) {
                    (None, None) => {}
                    (Some(s), Some(l)) => {
                        for tf in Timeframe::ALL {
                            assert_eq!(
                                s.closed_total(tf),
                                l.closed_total(tf),
                                "{owner} {id} {tf:?}"
                            );
                            assert_eq!(s.forming(tf), l.forming(tf));
                            for i in 0..s.closed_len(tf) {
                                assert_eq!(s.closed(tf, i), l.closed(tf, i));
                            }
                        }
                    }
                    (s, l) => panic!("{owner} {id}: shared {} alone {}", s.is_some(), l.is_some()),
                }
            }
        }
        // And the closes reported are the union of what the two alone report (each symbol once).
        let mut union: Vec<_> = out_a.iter().chain(out_c.iter()).copied().collect();
        union.sort_by_key(|c| (c.instrument, c.timeframe, c.bar.start_sec));
        union.dedup();
        let mut got = out.clone();
        got.sort_by_key(|c| (c.instrument, c.timeframe, c.bar.start_sec));
        assert_eq!(got, union);
    }

    #[test]
    fn owners_are_kept_in_order_whoever_asks_first_and_the_metrics_name_their_numbers() {
        let mut b = shared(5);
        b.claim(7, 2).unwrap();
        b.claim(3, 2).unwrap();
        b.claim(5, 2).unwrap();
        assert_eq!(b.owners_of(2), [3, 5, 7]);
        b.claim(3, 4).unwrap();
        let text = b.metrics_text();
        assert!(text.lines().any(|l| l == "bars_symbols 2"), "{text}");
        assert!(text.lines().any(|l| l == "bars_capacity 5"), "{text}");
        assert!(
            text.lines().any(|l| l == "bars_unplaced_trades 0"),
            "{text}"
        );
    }
}
