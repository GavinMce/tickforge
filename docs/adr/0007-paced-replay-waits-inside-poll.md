# 0007. Paced replay waits inside `poll`, on an injected pacer

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-35

## Context

A recorded tape should be replayable through the `Provider` trait at 1x, 10x or
as fast as possible, so strategies and ingestors see it exactly like a live
feed. `Provider::poll` is pull-based and synchronous. The run loop counts
consecutive `Poll::Idle` results and fails with `Stalled` after a limit, so
"not due yet" cannot be reported as `Idle`: a 1x replay of a quiet minute would
look like a dead source.

## Decision

- `TapeProvider` (in `tf-tape`) **waits inside `poll`** until the next event is
  due, then returns it together with any others that are due by then. A poll
  never returns an event before its time, and never holds back events that are
  already due.
- Waiting uses an injected `Pacer` (`now()` and `wait_until()`), not the wall
  clock directly. `WallPacer` is the real one and the only code in the
  workspace that sleeps. `SimClock` also implements `Pacer` by jumping to the
  target, so paced replay is deterministic and instant in tests.
- Speed is an integer permille (1000 is 1x), not a float.
- The event stream is identical at every speed; speed changes only when events
  are released. `Speed::Max` never touches the pacer.
- A read error on the tape is **not** end of tape. It is reported as
  `ProviderError::Source` (through `reconnect`, since `Poll` has no error
  variant) so a backtest fails instead of quietly using a truncated tape.

## Consequences

- A paced `poll` can block for as long as the recorded gap, including a long
  overnight gap in a multi-day tape. If that matters, add a cap that shortens
  gaps; it is not built.
- The run loop's `Idle` stall detection stays meaningful for real sources.
- Pacing is relative to the first event delivered after a (re)start; a
  reconnect re-anchors.
- `ProviderError` gained a variant, which any exhaustive match on it must handle.
