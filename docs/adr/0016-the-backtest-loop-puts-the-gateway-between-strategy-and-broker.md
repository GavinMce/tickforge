# 0016. The backtest loop puts the gateway between the strategy and the broker

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-107

## Context

The strategy, the risk gateway, the simulated broker and the report were each
tested alone. A backtest that skips the gateway shows what a strategy would do
with no limits, which is not what would be run.

## Decision

`tf-backtest::run_gated` is the one loop, and it is the loop a paper-trading
process will follow. Per event: the broker processes the event; its fills go to
the gateway's position book and the report, orders that finished short of their
size release the gateway's working exposure, and the strategy hears what became of
its orders; then the strategy sees the event; then every intent it emitted goes
to the gateway. An accepted intent is sent to the broker; a rejected one goes
straight back to the strategy as a rejection, so it frees what it was holding.

- The gateway is called with the intent's own event time, so rate windows follow
  the data, not the order in which the loop runs.
- The result carries the gateway's rejection counts and audit log and both
  position books, and a count of bookkeeping errors (a fill for an order the
  gateway did not accept, or a fill or close the gateway refused). Tests require
  the books to agree and the errors to be zero.
- `tf backtest` runs it on a synthetic session of healthy and dangerous runners
  and quiet names. With `--store` the result is keyed by a manifest of the whole
  setup: seed, git sha, session, simulator settings, every gateway limit and every
  strategy parameter. `MomentumParams::pairs` and `Limits::pairs` destructure
  their structs, so a new setting that is not recorded fails to compile; a test
  checks that each command-line setting changes the key.

## Consequences

- An exit can be refused (for example by the order-rate limit, which also applies
  to closes, ADR 0012). The strategy retries, and the result shows the refusals;
  a run can end with a position still open, and the report says so.
- The strategy-facing side of the loop is generic over `Strategy`, so the short
  side and Strategy 2 will use it unchanged.
- Results from the demo session say nothing about real markets (ADR 0015).
