//! The strategies of the library as definitions the host takes (E19).
//!
//! A definition is a number, a name, the parameters as text, a universe and a builder; the fingerprint of those is the
//! variant's identity in the trial registry (ADR 0060), so another parameter is another variant. The command that names
//! definitions in a registry and runs them over stored days is E19-S31; until then a strategy is added here.

use tf_strategy::closing_reversal::{ClosingReversalParams, ParamError};
use tf_strategy::premarket_null::PremarketNullParams;
use tf_strategy::premarket_pullback::PremarketPullbackParams;
use tf_strategy::random_entries::RandomEntriesParams;
use tf_strategy::{ClosingReversal, PremarketNull, PremarketPullback, RandomEntries};
use tf_universe::Spec;

use crate::def::{Route, StrategyDef};
use crate::runner::runner;

/// T04, the closing reversal ([`ClosingReversal`], ADR 0061): the day's biggest losers into the close, over `universe`, on
/// the simulated broker. `name` is how the variant is called in results and the registry.
pub fn closing_reversal(
    id: u16,
    name: &str,
    universe: Spec,
    params: ClosingReversalParams,
) -> Result<StrategyDef, ParamError> {
    params.validate()?;
    Ok(StrategyDef {
        id,
        name: name.to_owned(),
        params: params.render(),
        universe,
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(ClosingReversal::new(id, params).expect("the parameters were validated"))
        }),
    })
}

/// T14, the null strategy ([`RandomEntries`], ADR 0062): random names at random times with the same exits, over `universe`,
/// on the simulated broker. The seed is in `params`, so each seed is its own variant; [`crate::research::null::null_defs`]
/// makes one per seed.
pub fn random_entries(
    id: u16,
    name: &str,
    universe: Spec,
    params: RandomEntriesParams,
) -> Result<StrategyDef, ParamError> {
    params.validate()?;
    Ok(StrategyDef {
        id,
        name: name.to_owned(),
        params: params.render(),
        universe,
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(RandomEntries::new(id, params).expect("the parameters were validated"))
        }),
    })
}

/// T25, the premarket volume spike and its first pullback ([`PremarketPullback`], ADR 0076): a long bought on the turn of a small
/// pullback in a premarket run that began with a burst of volume, over `universe`, on the simulated broker. Its exits are held by
/// the strategy, so it is the same on any broker in the premarket.
pub fn premarket_pullback(
    id: u16,
    name: &str,
    universe: Spec,
    params: PremarketPullbackParams,
) -> Result<StrategyDef, ParamError> {
    params.validate()?;
    Ok(StrategyDef {
        id,
        name: name.to_owned(),
        params: params.render(),
        universe,
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(PremarketPullback::new(id, params).expect("the parameters were validated"))
        }),
    })
}

/// T26, the null of the premarket strategy ([`PremarketNull`], ADR 0077): active names at random times in the premarket with the
/// exits of T25, over `universe`, on the simulated broker. The seed is in `params`, so each seed is its own variant.
pub fn premarket_null(
    id: u16,
    name: &str,
    universe: Spec,
    params: PremarketNullParams,
) -> Result<StrategyDef, ParamError> {
    params.validate()?;
    Ok(StrategyDef {
        id,
        name: name.to_owned(),
        params: params.render(),
        universe,
        priority: 1,
        route: Route::Sim,
        build: Box::new(move || {
            runner(PremarketNull::new(id, params).expect("the parameters were validated"))
        }),
    })
}
