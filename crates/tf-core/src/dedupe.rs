//! Dropping the event a replay delivers twice.
//!
//! After a reconnect the gateway replays from a time and may send again an event it already sent: same
//! time received, same everything. [`Dedupe`] remembers the events of the current receive time and says
//! whether one is a repeat of one of them. An event that differs in any field is not a repeat, so two
//! different quotes of one instrument at one time both pass.
//!
//! The rule is part of the pipeline, not a patch for reconnects: the live path and the replay of its
//! raw capture both apply it, so they see the same events (the capture holds what the gateway sent,
//! repeats included).

use crate::{Event, Nanos};

/// The most events of one receive time that are remembered.
const MAX_GROUP: usize = 256;

#[derive(Debug, Default)]
pub struct Dedupe {
    at: Nanos,
    group: Vec<Event>,
    dropped: u64,
}

impl Dedupe {
    pub fn new() -> Dedupe {
        Dedupe::default()
    }

    /// Whether to deliver `e`. Events must come in `ts_recv` order, as every provider's do; one that
    /// goes back in time is passed on (it is not a repeat of anything held) and starts a new group.
    pub fn admit(&mut self, e: &Event) -> bool {
        let t = e.ts_recv();
        if t != self.at {
            self.at = t;
            self.group.clear();
        } else if self.group.contains(e) {
            self.dropped += 1;
            return false;
        }
        // A burst that shares one receive time is not allowed to make this quadratic.
        if self.group.len() >= MAX_GROUP {
            self.group.remove(0);
        }
        self.group.push(*e);
        true
    }

    /// Repeats dropped so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Header, ProviderId, Px, Trade, TradeFlags};

    fn trade(instrument: u32, ts_recv: u64, ts_event: u64, seq: u64, cents: i64) -> Event {
        Event::Trade(Trade {
            hdr: Header {
                ts_event,
                ts_recv,
                seq,
                instrument,
                provider: ProviderId::Databento,
            },
            px: Px::from_cents(cents),
            size: 100,
            flags: TradeFlags::NONE,
        })
    }

    #[test]
    fn an_exact_repeat_in_the_same_receive_time_is_dropped_and_anything_different_is_not() {
        let mut d = Dedupe::new();
        let a = trade(1, 10, 9, 5, 100);
        assert!(d.admit(&a));
        assert!(!d.admit(&a));
        assert!(!d.admit(&a));
        for different in [
            trade(2, 10, 9, 5, 100),
            trade(1, 10, 8, 5, 100),
            trade(1, 10, 9, 6, 100),
            trade(1, 10, 9, 5, 101),
        ] {
            assert!(d.admit(&different), "{different:?}");
        }
        // A repeat of one of those, later in the same time, is still found.
        assert!(!d.admit(&trade(1, 10, 9, 6, 100)));
        assert_eq!(d.dropped(), 3);
        // The next receive time forgets: the same event then is new.
        assert!(d.admit(&trade(1, 11, 9, 5, 100)));
        assert!(d.admit(&a), "going back in time starts again");
    }

    #[test]
    fn a_burst_at_one_instant_is_bounded() {
        let mut d = Dedupe::new();
        for i in 0..1_000u64 {
            assert!(d.admit(&trade(1, 5, 5, i, 100)));
        }
        // Only the recent are remembered: a repeat of the latest is caught, of the first is not.
        assert!(!d.admit(&trade(1, 5, 5, 999, 100)));
        assert!(d.admit(&trade(1, 5, 5, 0, 100)));
    }
}
