//! A strategy runner without its type, so a host can hold twenty different strategies in one list.

use tf_core::{Event, InstrumentId, Nanos, TierChange};
use tf_engine::{Promoter, Tier0};
use tf_strategy::lifecycle::OrderUpdate;
use tf_strategy::{CrossRunner, CrossStrategy, Intent, Market, Members};

pub trait DynRunner: Send {
    fn on_event(&mut self, market: Market<'_>, promoter: Option<&mut Promoter>, ev: &Event);
    fn advance_to(&mut self, market: Market<'_>, promoter: Option<&mut Promoter>, ts: Nanos);
    fn on_order_update(&mut self, tier0: &Tier0, promoter: Option<&mut Promoter>, u: &OrderUpdate);
    fn on_tier1_revoked(
        &mut self,
        market: Market<'_>,
        promoter: Option<&mut Promoter>,
        id: InstrumentId,
    );
    fn drain_intents(&mut self) -> Vec<Intent>;
    fn drain_tier_events(&mut self) -> Vec<TierChange>;
    fn members(&self) -> &Members;
    fn members_mut(&mut self) -> &mut Members;
    fn reviews(&self) -> u64;
    fn invalid_intents(&self) -> u64;
}

impl<S: CrossStrategy> DynRunner for CrossRunner<S> {
    fn on_event(&mut self, market: Market<'_>, promoter: Option<&mut Promoter>, ev: &Event) {
        CrossRunner::on_event(self, market, promoter, ev);
    }

    fn advance_to(&mut self, market: Market<'_>, promoter: Option<&mut Promoter>, ts: Nanos) {
        CrossRunner::advance_to(self, market, promoter, ts);
    }

    fn on_order_update(&mut self, tier0: &Tier0, promoter: Option<&mut Promoter>, u: &OrderUpdate) {
        CrossRunner::on_order_update(self, tier0, promoter, u);
    }

    fn on_tier1_revoked(
        &mut self,
        market: Market<'_>,
        promoter: Option<&mut Promoter>,
        id: InstrumentId,
    ) {
        CrossRunner::on_tier1_revoked(self, market, promoter, id);
    }

    fn drain_intents(&mut self) -> Vec<Intent> {
        CrossRunner::drain_intents(self)
    }

    fn drain_tier_events(&mut self) -> Vec<TierChange> {
        CrossRunner::drain_tier_events(self)
    }

    fn members(&self) -> &Members {
        CrossRunner::members(self)
    }

    fn members_mut(&mut self) -> &mut Members {
        CrossRunner::members_mut(self)
    }

    fn reviews(&self) -> u64 {
        CrossRunner::reviews(self)
    }

    fn invalid_intents(&self) -> u64 {
        CrossRunner::invalid_intents(self)
    }
}

/// Wrap a strategy for the host.
pub fn runner<S: CrossStrategy + 'static>(strategy: S) -> Box<dyn DynRunner> {
    Box::new(CrossRunner::new(strategy, Members::new()))
}
