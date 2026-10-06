//! The dynamic layer: among the static members, the top few by a live measurement, with hysteresis.

use tf_core::{InstrumentId, NANOS_PER_SEC, Nanos};
use tf_engine::Tier0;

use crate::feature::LiveFeature;
use crate::spec::{Cmp, Dynamic, Spec};

/// Today's measurements of a symbol.
pub trait LiveView {
    /// `None` when the symbol has not traded, or the measurement needs reference data it lacks.
    fn value(&self, id: InstrumentId, f: LiveFeature) -> Option<i64>;
}

/// What a reference snapshot says about a symbol, by instrument id, for measurements against it.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefInfo {
    /// Prior close, raw.
    pub price: Option<i64>,
    pub adv_shares: Option<i64>,
}

/// Measurements read from the engine's Tier 0 state.
pub struct Tier0View<'a> {
    pub tier0: &'a Tier0,
    /// By instrument id.
    pub refs: &'a [RefInfo],
}

fn ratio(num: i128, den: i128) -> Option<i64> {
    if den <= 0 {
        return None;
    }
    i64::try_from(num * 1000 / den).ok()
}

impl LiveView for Tier0View<'_> {
    fn value(&self, id: InstrumentId, f: LiveFeature) -> Option<i64> {
        let s = self.tier0.symbol(id)?;
        let r = self.refs.get(id as usize).copied().unwrap_or_default();
        match f {
            LiveFeature::Trades => Some(i64::from(s.trades)),
            LiveFeature::DollarVolume => i64::try_from(s.notional / 1_000_000_000)
                .ok()
                .filter(|_| s.trades > 0),
            LiveFeature::GapPermille => {
                let (last, prior) = (s.last_px?.raw(), r.price?);
                ratio(i128::from(last - prior), i128::from(prior))
            }
            LiveFeature::VolumeRatioPermille => {
                if s.trades == 0 {
                    return None;
                }
                ratio(i128::from(s.volume), i128::from(r.adv_shares?))
            }
            LiveFeature::RangePermille => ratio(
                i128::from(s.day_high?.raw() - s.day_low?.raw()),
                i128::from(s.last_px?.raw()),
            ),
        }
    }
}

/// Who joined and who left at an evaluation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub entered: Vec<InstrumentId>,
    pub left: Vec<InstrumentId>,
}

pub struct Selector {
    top: usize,
    keep: usize,
    by: LiveFeature,
    descending: bool,
    every: Nanos,
    filters: Vec<(LiveFeature, Cmp, i64)>,
    members: Vec<InstrumentId>,
    next_at: Option<Nanos>,
}

impl Selector {
    /// `None` when the spec has no dynamic layer or a filter names a parameter it never declared.
    pub fn new(spec: &Spec) -> Option<Selector> {
        let d: &Dynamic = spec.dynamic.as_ref()?;
        let filters = d
            .filters
            .iter()
            .map(|c| Some((c.feature, c.cmp, spec.value(&c.operand)?)))
            .collect::<Option<Vec<_>>>()?;
        Some(Selector {
            top: d.top as usize,
            keep: d.keep as usize,
            by: d.by,
            descending: d.descending,
            every: Nanos::from(d.every_secs) * NANOS_PER_SEC,
            filters,
            members: Vec::new(),
            next_at: None,
        })
    }

    /// Sorted by instrument id.
    pub fn members(&self) -> &[InstrumentId] {
        &self.members
    }

    /// Re-rank if `every` has passed since the last time (the first call always does). `candidates` is
    /// the static selection as instrument ids; `now` is event time, so a replay re-ranks at the same
    /// moments the live run did.
    pub fn update(
        &mut self,
        now: Nanos,
        view: &dyn LiveView,
        candidates: &[InstrumentId],
    ) -> Option<Change> {
        if self.next_at.is_some_and(|t| now < t) {
            return None;
        }
        self.next_at = Some(now.saturating_add(self.every));
        let mut ranked: Vec<(i64, InstrumentId)> = candidates
            .iter()
            .filter(|id| {
                self.filters
                    .iter()
                    .all(|(f, c, t)| view.value(**id, *f).is_some_and(|v| c.holds(v, *t)))
            })
            .filter_map(|id| Some((view.value(*id, self.by)?, *id)))
            .collect();
        if self.descending {
            ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        } else {
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        }
        let mut next: Vec<InstrumentId> = ranked
            .iter()
            .enumerate()
            .filter(|(i, (_, id))| {
                *i < self.top || (*i < self.keep && self.members.binary_search(id).is_ok())
            })
            .map(|(_, (_, id))| *id)
            .collect();
        next.sort_unstable();
        let entered: Vec<InstrumentId> = next
            .iter()
            .filter(|i| self.members.binary_search(i).is_err())
            .copied()
            .collect();
        let left: Vec<InstrumentId> = self
            .members
            .iter()
            .filter(|i| next.binary_search(i).is_err())
            .copied()
            .collect();
        self.members = next;
        Some(Change { entered, left })
    }
}
