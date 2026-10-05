# 0025. The strategy records why it acted, and the viewer only reads

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-111

## Context

To judge an entry you need the evidence the strategy had at that moment, not a
reconstruction after the fact.

## Decision

- `MomentumLong` keeps an `EntryTrace` (features, impulse, parameters in force) for each
  entry and a `Decline` for each give-up. Recording does not change behavior (a test
  compares traced and plain runs).
- `tf backtest --export FILE` writes one deterministic JSON document: bars, ticks,
  quotes, scanner hits, tier moves, orders, fills, stop path, and each trade's eight
  condition tests with value, threshold and pass/fail.
- The viewer (`crates/tf-backtest/explorer/viewer.html`) is a static page that reads
  that JSON. It draws only data up to the replay cursor. The feature readout before
  the decision is recomputed by the exporter and is context; the condition table is the
  strategy's own record.

## Consequences

Single momentum runs only (not trend, not `--propose`). The prototype is not wired into
CI beyond the Rust tests; the HTML is checked by hand.
