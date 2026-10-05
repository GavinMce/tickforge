# 0002. No Redis on the hot path

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-23

## Context

Per-symbol state (last price, bid/ask, VWAP, rolling windows, scanner inputs)
is read and written on every event. The engine is the only writer. A network
store adds a round trip and a failure mode to every one of those accesses, and
at burst rates that cost is paid ~10^5 times a second.

## Decision

Hot state lives in process memory, in arrays indexed by dense `InstrumentId`;
there are no hash maps and no network calls in the per-event path. The engine
is the single writer.

Redis is allowed for things that are not on that path: 1 Hz snapshots for
other readers, watchlists and configuration. Out-of-process consumers (archiver,
agents) read from NATS JetStream or the snapshots, not from engine state.

## Consequences

- In-process memory beats any network store on latency and removes a
  dependency from the live path.
- State is lost on restart and must be rebuilt by replay from the tape or the
  bus; this is acceptable because every run is reproducible from its inputs
  (see 0003) and the order ledger lives in Postgres, not in engine memory.
- Other processes cannot read live engine state directly. If measurement ever
  shows a second process needs raw ticks at microsecond latency, add a
  shared-memory ring then, with its own ADR; do not add a shared store first.
