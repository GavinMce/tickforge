# 0013. Backtest reports are integer, comparable, and state their blind spots

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-69

## Context

A backtest number is only useful if two runs can be compared exactly, stored as
run results (ADR 0009) and argued about without redefining the terms.

## Decision

`tf-strategy::report` builds a `Report` from the fills and market events of a
run (`run_backtest_observed` feeds it in one pass):

- All money is integer raw price units; hit rate is permille. Nothing in a stored
  metric is a float, so reports are reproducible and go straight into `RunResult`
  metrics (`Report::metrics`, with `group.<label>.<name>` for the breakdown).
- A **trade** is a flat-to-flat round trip per instrument, valued at average cost.
  A flip through zero ends one trade and starts another. Break-even is not a win.
- **Net P&L** = realised + marked open positions - borrow fees. **Max drawdown**
  is the largest fall of the equity curve from its peak, sampled at every event,
  without borrow fees (known only at the end).
- **Slippage** is as the simulator records it per fill: total cost and per-share
  average, plus the worst fill.
- The **breakdown** is by a caller-chosen label per instrument (the scenario it was
  generated from, or later a regime tag); labels are restricted to metric-name
  characters.
- Every rendering ends with what the simulator does not model (ADR 0011).

Average cost is held to one raw unit, so P&L can differ from an exact cash
accounting by under a raw unit (1e-9 dollars) per share traded; the tests assert
that bound against an independent cash ledger.

## Consequences

Per-scenario numbers on small samples are noise; the report shows trade counts
beside every figure so that is visible. Sharpe-like ratios are deliberately not
included until there are enough independent runs to mean something.
