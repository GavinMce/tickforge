//! The hourly bias of docs/research 1a (E19-S16): a name's last completed regular-session hourly close above or below its EMA(100) of such closes.
//!
//! - **The series.** Hourly buckets from the open: 09:30-10:30, 10:30-11:30, ... 14:30-15:30 and the half hour to the close; the close of a bucket is the last
//!   trade price in it (a quiet name's is its last price before). The snapshot carries the EMA's state at the previous close (`ema100h_state`, `ema100h_count`,
//!   the simple-average seed over the first 100 closes), and this day's closes are folded in as their buckets end, so the live value is the one an offline
//!   pass over the same closes gives ([`Ema::resume`] is exact).
//! - **The bias** is up when the last completed close of the day is over the EMA, down when under it, and none before the first bucket ends (10:30), on a
//!   tie, for a name the snapshot has no state for, and for a name any of whose closes today was missing (it would be a different series).
//! - **The closes** come from the shared 15 minute bars: the close of the bar that ends at the bucket's end, which the caller finds (`close_at`): the
//!   bars fill a quiet quarter hour with a flat bar, so the bar is there whenever the name has traded since its bars began.
//!
//! A filter, not an entry: a strategy asks [`HourlyBias::allows`] for the side it is about to take. Integers throughout; nothing reads a clock.

use std::collections::BTreeMap;

use tf_core::{InstrumentId, NANOS_PER_SEC, Nanos, Px};
use tf_engine::{Ema, Seed};
use tf_universe::RefInfo;

use crate::intent::Side;

/// The period of the EMA the snapshot carries (`tf_reference::EMA_PERIOD`).
const EMA_PERIOD: u32 = 100;
const HOUR: Nanos = 3_600 * NANOS_PER_SEC;
/// A bucket's close is the close of the bar of this length that ends with it.
pub const BAR_SECS: u64 = 900;
/// The buckets that end before the last half hour: 10:30 ... 15:30.
const BOUNDARIES: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    Up,
    Down,
}

/// What the bars say of the close of a bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Close {
    /// The close.
    Is(Px),
    /// The bar that ends with the bucket has not closed yet: ask again later.
    NotYet,
    /// There is no such close and there will be none (the name has no bars): the name's series is broken for the day.
    Missing,
}

#[derive(Clone, Copy, Debug)]
struct Track {
    ema: Ema,
    last: Option<Px>,
    /// Buckets folded today (0 to [`BOUNDARIES`]).
    folded: u32,
    broken: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HourlyBias {
    open: Nanos,
    names: BTreeMap<InstrumentId, Track>,
}

impl HourlyBias {
    pub fn new() -> HourlyBias {
        HourlyBias::default()
    }

    /// A new day: no name is followed until [`HourlyBias::add`].
    pub fn start_day(&mut self, open: Nanos) {
        self.open = open;
        self.names.clear();
    }

    /// Follow a name from what the snapshot says. False (and the name has no bias today) for a snapshot without the EMA's state.
    pub fn add(&mut self, id: InstrumentId, r: &RefInfo) -> bool {
        let (Some(state), Some(count)) = (r.hist.ema100h_state, r.hist.ema100h_count) else {
            return false;
        };
        let Some(ema) = u32::try_from(count)
            .ok()
            .and_then(|c| Ema::resume(EMA_PERIOD, Seed::Sma, state, c))
        else {
            return false;
        };
        self.names.insert(
            id,
            Track {
                ema,
                last: None,
                folded: 0,
                broken: false,
            },
        );
        true
    }

    pub fn forget(&mut self, id: InstrumentId) {
        self.names.remove(&id);
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Fold the buckets that have ended by `now` less `settle` nanoseconds into each name; `close_at(id, start_sec)` says what the 15 minute bar of `id`
    /// that starts at `start_sec` closed at. A name whose bar has not closed yet waits for the next call (and has the bias of the last bucket
    /// until then); a name with no such close is broken for the day.
    pub fn advance(
        &mut self,
        now: Nanos,
        settle: Nanos,
        close_at: impl Fn(InstrumentId, u64) -> Close,
    ) {
        let open = self.open;
        for (id, t) in self.names.iter_mut() {
            while t.folded < BOUNDARIES {
                let b = open + HOUR * u64::from(t.folded + 1);
                if now < b.saturating_add(settle) {
                    break;
                }
                let start = (b / NANOS_PER_SEC).saturating_sub(BAR_SECS);
                match close_at(*id, start) {
                    Close::NotYet => break,
                    Close::Is(c) if !t.broken => {
                        t.ema.update(c.raw());
                        t.last = Some(c);
                    }
                    _ => t.broken = true,
                }
                t.folded += 1;
            }
        }
    }

    /// The bias of a name now.
    pub fn bias(&self, id: InstrumentId) -> Option<Bias> {
        let t = self.names.get(&id).filter(|t| !t.broken)?;
        let (last, avg) = (t.last?.raw(), t.ema.value()?);
        match last.cmp(&avg) {
            std::cmp::Ordering::Greater => Some(Bias::Up),
            std::cmp::Ordering::Less => Some(Bias::Down),
            std::cmp::Ordering::Equal => None,
        }
    }

    /// The EMA of a followed name as it stands, in raw price units.
    pub fn average(&self, id: InstrumentId) -> Option<i64> {
        self.names.get(&id)?.ema.value()
    }

    /// Whether a trade of `side` in `id` goes with the bias: a buy with up, a sell with down. With no bias, no.
    pub fn allows(&self, id: InstrumentId, side: Side) -> bool {
        matches!(
            (self.bias(id), side),
            (Some(Bias::Up), Side::Buy) | (Some(Bias::Down), Side::Sell | Side::SellShort)
        )
    }
}
