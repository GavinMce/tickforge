# 0001. Rust for the hot path

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-23

## Context

The engine ingests ~5,000 symbols. The back-of-envelope estimate in `DESIGN.md`
(to be replaced by measurement, E06-S01) is ~10^4 trades/s sustained and ~10^5
quote messages/s in bursts if L1 quotes are subscribed for all of them, keeps per-symbol state in RAM and must hold a good
p99 during open-bell bursts. The tick-to-order budget is 50-300 ms, so
microsecond tuning is wasted (Alpaca REST round trips dominate), but pauses
are not: a stop-the-world pause during a burst is exactly when the engine must
not stall. Databento, the primary data source, ships an official Rust client.

## Decision

The hot path (ingestors, engine, risk gateway) is written in Rust. `unsafe` is
forbidden workspace-wide (`unsafe_code = "forbid"`); allowing it anywhere needs
a new ADR that names the code and the measured reason.

Slow-loop components (agents, orchestration, reports) may use other languages
where that is clearly the better tool, but they sit behind the bus or the MCP
server and never in the trade loop.

## Consequences

- No garbage-collection pauses; memory layout is explicit, which suits dense
  per-symbol arrays and fixed-size events.
- One official vendor client, no FFI shim for Databento.
- A smaller hiring and tooling pool than Go or Python, and slower iteration on
  throwaway research code, which is why research stays out of the hot path.
- Rejected without a benchmark, on the design's reasoning: a garbage-collected
  runtime (pauses during bursts), and an interpreted one for the engine itself.
  Revisit if measurement shows the budget is far looser than assumed.
