# 0011. The simulated broker fills against recorded quotes, with latency

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-68

## Context

A backtest that fills every order at the last trade price flatters a strategy,
and most of all the low-float momentum strategies planned here, whose edge is
easily smaller than the spread, the slippage and the borrow fee.

## Decision

`tf-strategy::sim::SimBroker` matches orders against the recorded quotes only:

- An intent reaches the venue `latency_ns` after it was decided and sees the
  market as it stood at that instant. A quote arriving at the same instant is
  not visible yet, so the order never trades on information it could not have.
- Buys cross when the ask is within their worst price (sells: the bid), at the
  quote's price, for at most the size shown. Each quote's size is consumed once
  across all orders. IOC remainders expire on arrival; day orders rest and retry
  on later quotes. Nothing fills while halted.
- Slippage is recorded per fill against the intent's reference price. Borrow fee
  accrues on short positions in integer arithmetic, from elapsed event time,
  priced at the last trade, rounded up.
- `run_backtest` fixes the order of operations per event: broker, then order
  updates to the strategy, then the strategy's view of the event, then the new
  intents to the broker.

## Not modelled, and what that means

Queue position, depth beyond the best quote, our own market impact, commissions,
and the protective stops and targets on opening intents. Results are therefore
optimistic for strategies that depend on those, and the backtest report
(E08-S07) must say so. Borrow availability and fee are one flat rate; real
rates for hard-to-borrow names move daily and can be much higher. Overnight
time is charged as elapsed time.

## Consequences

Backtests are pessimistic about latency and honest about size, but only as good
as the recorded quotes: with sparse quotes the sim fills against stale prices.
