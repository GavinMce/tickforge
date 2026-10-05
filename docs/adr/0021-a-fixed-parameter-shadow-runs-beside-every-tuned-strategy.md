# 0021. A fixed-parameter shadow runs beside every tuned strategy

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-87

## Context

An agent that tunes parameters live must be judged against what would have
happened without it. DESIGN.md calls for a fixed-parameter shadow engine on the
same feed as the control; if the tuned instance does not beat it after costs, the
agent adds nothing.

## Decision

`tf_backtest::ab::run_ab` steps two sides over the same events:

- **Tuned**: the strategy with a `ParamStore`. Scripted proposals (a time and a
  `Proposal`) are checked against its store at the first event at or after their
  time; an accepted one becomes a `ParamChange` event that the tuned side applies
  and that is recorded on `AbResult::tape`; a refused one is listed with its reason.
- **Shadow**: the same strategy with the baseline parameters and no store. It is
  never shown parameter-change events.
- **Virtual fills, same limits**: each side has its own simulated broker (so its
  fills are virtual and cannot touch the other) and its own gateway built from the
  same limits (so neither is flattered by the other's risk state).
- **Observable as it runs**: a callback receives both equities after every market
  event, which is what the auto-revert policy (E12-S03) is built on.
- **Comparison** is tuned minus shadow: net and realised P&L, drawdown, slippage,
  trades, shares, hit rate, and net P&L per label. `AbResult::metrics` exports
  both reports (`tuned.*`, `shadow.*`), the differences (`delta.*`) and what
  happened to the parameters (`params.*`) as integer run-result metrics.
- `tf backtest --propose NAME=VALUE@SECS` (repeatable) runs it for the momentum
  strategy; the proposals are part of the manifest, in order.

## Properties kept as tests

- With no proposals, both sides equal an ordinary gated run and every difference
  is zero.
- The shadow equals the ordinary run whatever the proposals.
- Replaying `tape` on a fresh tuned side reproduces the tuned result exactly.
- Proposals are made at the first event at or after their time, in time order
  however they are listed.

## Limits

- The shadow only tells you something if the feed is representative; one
  synthetic session is one sample, and the metrics carry no significance.
- Scripted proposals stand in for an agent; the agent interface is E12-S06.
- Only the momentum strategy has tunable parameters so far.
