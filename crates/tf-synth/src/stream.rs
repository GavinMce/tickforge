use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use tf_core::{Event, InstrumentId, Nanos};

use crate::config::SynthConfig;
use crate::rng::SplitMix64;
use crate::symbol_gen::SymbolGen;

/// Merged, `ts_recv`-ordered stream of every symbol's events, with the
/// provider-level `seq` assigned in arrival order. Fully determined by the
/// config: rebuilding it from the same config yields the same events with the
/// same `seq`s, which is how the provider implements replay.
pub struct SynthStream {
    gens: Vec<SymbolGen>,
    heap: BinaryHeap<Reverse<(Nanos, InstrumentId)>>,
    /// Scripted news in arrival order, merged in with the symbols' events.
    news: VecDeque<Event>,
    seq: u64,
}

impl SynthStream {
    pub fn new(cfg: &SynthConfig) -> Self {
        let end = cfg.session_start.saturating_add(cfg.duration);
        let mut gens = Vec::with_capacity(cfg.symbols.len());
        let mut heap = BinaryHeap::with_capacity(cfg.symbols.len());
        for (i, spec) in cfg.symbols.iter().enumerate() {
            let id = InstrumentId::try_from(i).expect("more than u32::MAX symbols");
            let rng = SplitMix64::fork(cfg.seed, u64::from(id));
            let g = SymbolGen::new(id, rng, spec, cfg.session_start, end);
            if let Some(t) = g.peek_recv() {
                heap.push(Reverse((t, id)));
            }
            gens.push(g);
        }
        let news = cfg.news_schedule().into_iter().map(|s| s.event).collect();
        SynthStream {
            gens,
            heap,
            news,
            seq: 0,
        }
    }

    pub fn next_event(&mut self) -> Option<Event> {
        // News goes first only if it strictly beats every symbol's next event.
        let news_first = match (self.heap.peek(), self.news.front()) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(Reverse((t, _))), Some(n)) => n.ts_recv() < *t,
        };
        let mut ev = if news_first {
            self.news.pop_front()?
        } else {
            let Reverse((_, id)) = self.heap.pop()?;
            let g = &mut self.gens[id as usize];
            let ev = g.pop()?;
            if let Some(t) = g.peek_recv() {
                self.heap.push(Reverse((t, id)));
            }
            ev
        };
        ev.hdr_mut().seq = self.seq;
        self.seq += 1;
        Some(ev)
    }
}

impl Iterator for SynthStream {
    type Item = Event;
    fn next(&mut self) -> Option<Event> {
        self.next_event()
    }
}
