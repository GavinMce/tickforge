//! What a strategy may see of its own budget (E19-S51), and a size from the risk it may take.
//!
//! The gateway holds the budget tree and what each strategy uses of it (each position at the higher of cost and mark, a short charged with the
//! gap rule's assumption, and its working opening orders). A strategy that sizes a trade by "risk one percent of my budget" needs both: the
//! whole and what is left. The host gives each callback that has the market (a review, an event, a timer) a [`BudgetSource`]; it is asked only when a
//! strategy asks, so a strategy that does not read it pays nothing and decides as it did.
//!
//! All amounts are raw units (1e-9 dollars), the same as a price times a number of shares.

use tf_core::Px;

use crate::intent::StrategyId;

/// A strategy's budget as the gateway accounts it, now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetView {
    /// The strategy's share of the balance.
    pub total: u128,
    /// What its positions and working opening orders use.
    pub used: u128,
}

impl BudgetView {
    /// What is left to open with: nothing once the budget is used (or over-used by marks moving against it).
    pub fn left(&self) -> u128 {
        self.total.saturating_sub(self.used)
    }

    /// The number of shares to open so that losing `stop_distance` (raw price units, a positive distance from the entry to the stop) on each costs
    /// `risk_ppm` millionths of the whole budget, and no more than the budget that is left can hold at `price` (the entry price; for a short sale
    /// the price times the gap rule's assumption, since a short is charged more than its notional). Fewer as the budget is used, none when
    /// nothing is left, none for a stop that is not a distance away or a price that is not one. Rounds down.
    pub fn shares_for_risk(&self, risk_ppm: u32, stop_distance: i64, price: Px) -> u32 {
        let (Ok(dist), Ok(px)) = (u128::try_from(stop_distance), u128::try_from(price.raw()))
        else {
            return 0;
        };
        if dist == 0 || px == 0 {
            return 0;
        }
        let by_risk = self.total.saturating_mul(u128::from(risk_ppm)) / 1_000_000 / dist;
        let by_room = self.left() / px;
        u32::try_from(by_risk.min(by_room)).unwrap_or(u32::MAX)
    }
}

/// Who can say what a strategy uses of its budget: the gateway, for the host.
pub trait BudgetSource {
    /// `None` for a strategy that has no budget (budgets are not in force, or it is not in the tree).
    fn budget(&self, strategy: StrategyId) -> Option<BudgetView>;
}
