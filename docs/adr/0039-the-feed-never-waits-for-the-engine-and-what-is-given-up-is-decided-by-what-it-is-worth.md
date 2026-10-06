# 0039. The feed never waits for the engine, and what is given up is decided by what it is worth

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-48

## Context

The opening second of the session is the hardest traffic there is: measured on 2026-10-02, the
full-market feed delivered 337,231 trades in one second (about 236,000 once zero-share prints are
set aside), against an engine that absorbs more than 40 million events a second. A feed reader that
waits for the engine is a disconnect (and since April 2026 Databento's gateway drops quote records
itself when it is read too slowly). Something has to give when the engine stalls, and it should be
chosen, not left to chance.

## Decision

`tf-ingest` is one bounded first-in-first-out ring between a feed thread and the engine thread.

- **The producer never blocks.** `push` returns what happened to the event (queued, conflated,
  dropped) and never waits. A consumer that has gone makes the feed drop, not hang.
- **What an event is worth decides when it stops being admitted.** Quotes are state: below half the
  ring they are queued; above it each is *conflated*, replacing the one waiting for its symbol, and
  the waiting ones are queued when the ring has drained below a quarter. Trades are events: queued up
  to 90% of the ring, dropped beyond. Control events (halts, corrections, cancels, tier and
  parameter changes) take what is left of the ring and are dropped only when it is completely full.
- **A loss is never silent.** Every drop is counted by kind, and an in-band `Gap` marker (kind, count,
  first and last time) is delivered before the next event that follows the break. A consumer that
  cares (a strategy mid-trade, the capture) knows the stream is not continuous.
- **Order.** One ring, so events of one symbol arrive in order; a waiting quote goes out ahead of the
  next event of its symbol. Across symbols a conflated quote can arrive after events that followed
  it, which is only seen under pressure.
- **No `unsafe`, no new dependency.** A standard bounded channel carries the messages and an atomic
  counts the depth; the thresholds read the counter.
- **Capacity.** Two million messages by default (about 160 MB). On the real opening that is far more
  than needed: a six-second engine stall left the ring 16% full. Trades only start to drop when the
  ring is smaller than the burst.
- **Counters** (offered, queued, conflated, flushed, dropped by kind, gaps, delivered, deepest the
  ring has been) are shared atomics, readable from either end; exporting them is E13-S03.

## Consequences

- Conflation depends on timing, so what the engine consumed under pressure is not reproducible
  from the raw capture. The replay-equivalence check (E18-S06) replays the engine's own input (what
  it was handed, gaps included), and a day with a gap or conflated quotes is flagged as one where the
  capture and the engine's input differ.
- The producer calls `tick` when the feed is idle and at least once a second, or a waiting quote or
  marker waits for the next event.
- In the harness the tail of the queue latency (p99.9 about 28 ms, maximum about 35 ms, with no
  stall) comes from the scheduler of the machine it ran on, not from the ring: the median is 0.2
  microseconds. A live engine needs a core of its own (E13-S01).
