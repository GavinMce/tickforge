# 0031. Budgets are reserved, hierarchical, and enforced by the gateway

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-119

## Context

The workspace app (E17) splits one account's balance into budgets: groups (for example day
trading, swing, ETF) take a share of the balance and strategies take a share of their group.
Several questions had to be settled before the pieces could be built, and a UI mock was
used to check the answers against how they would feel.

## Decision

- **Reserved, hierarchical shares.** A budget is a share of the node above it. Shares of the
  children never exceed their parent; the remainder is explicitly unassigned. A share cannot
  go below what is in use. Budgets are a percentage of the live balance and change only at a
  rebalance.
- **One account for now.** The tree hangs off one linked broker account. The account is a
  field so more can be added later; nothing is netted across accounts.
- **Rebalance after each session.** Realised gains and losses move into the strategy that
  made them, within a floor and ceiling per node. A change made during the day is scheduled
  and takes effect at the rebalance. A cut never forces an open position to resize.
- **What a position uses.** A position is charged at what it could lose: longs at notional,
  shorts at the worst case under the gap rule (E09-S03). Broker margin is read as a
  constraint, not used as the budget.
- **Loss limits, two tiers, per strategy, of its own budget.** Soft (default 3%): it stops
  opening, exits still pass. Hard (default 6%): its positions are flattened. Only that
  strategy is affected. Pausing a group is a person's decision. Limits are risk settings and
  are never agent-tunable.
- **Agent proposals are asymmetric.** A decrease within bounds (a step size and cooldown per
  node, like the parameter store) is applied and recorded; an increase needs a person's
  approval, is refused for a strategy in drawdown, and carries its evidence. This follows
  the rule-edit review (ADR 0029).
- **No hardcoded regulation.** Rules such as the pattern-day-trader requirement are being
  changed and phased in per broker. The app reads what the broker reports (buying power,
  margin, restrictions) and treats it as a cap on usable budget.
- **Shared symbols need sub-accounts.** The gateway holds one position per instrument.
  Strategies that trade the same symbol need their own lots in the ledger, netted only at the
  broker (E17-S03).
- **Deferred:** lending idle budget to a sibling (E17-S16).

## Consequences

- The gateway and the ledger change before any screen can be honest: per-node limits
  (E17-S02), attribution (E17-S03), budget events (E17-S05).
- Reserved budgets leave cash idle when a strategy has no setups. That is accepted for the
  simplicity of the rule; overflow is the answer if it hurts.
- The screens (overview, run view, explorer view, budget editor, proposals panel) are
  read-only against a service until the enforcement pieces exist, so no screen can place an
  order.
