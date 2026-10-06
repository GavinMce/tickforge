# 0032. The gateway keeps a sub-account per strategy

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-122

## Context

Budgets, loss limits and run P&L are per strategy (ADR 0031), but a broker account holds one
net position per symbol. Two strategies that trade the same name would either be
indistinguishable or, worse, one could close what the other opened.

## Decision

The risk gateway keeps each strategy's own position, average cost and realised profit in each
instrument. What the broker holds (`position(instrument)`) is the sum of them, and the
gateway's profit and loss is the sum of the strategies', which equals the account's: cash flows
plus the broker's shares at the marks (checked over 1,500 random steps with three strategies,
allowing for average-cost rounding).

- **Closing** is against the strategy's own holding and its own working closes. A strategy
  cannot close what another holds.
- **Opposing positions** are per strategy. One strategy may be long a name while another is
  short it; a strategy may not be on both sides itself.
- **The cap on a symbol** is over what every strategy holds or has working on that side, not
  over the broker's net: two strategies on one side are that much exposure. Opposite sides are
  not added together.
- **The gap rule and gross exposure** count every strategy's position, so shorts held by one
  strategy still count when another strategy's long makes the broker flat. This is
  deliberately conservative: the account is only netted at the broker; the risk is each
  strategy's.
- **The ledger needs no new records.** A decision already names its strategy (in the intent),
  and fills name their order, so replay rebuilds the split; the snapshot carries it and the
  ledger tests compare it.
- A single-strategy run behaves exactly as before (72 backtest reports are byte-identical to
  the previous commit's).

## Consequences

- The snapshot's positions and working orders carry the strategy; `tf ledger verify` shows
  positions and realised profit per strategy.
- Per-node budgets (E17-S02) can read each strategy's exposure from the gateway
  (`strategy_positions`, `strategy_working_open_notional`).
- Lots are not tracked, only an average cost per strategy and instrument; tax lots or FIFO
  matching, if ever needed, are a different layer.
