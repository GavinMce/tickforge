# 0003. Determinism rules

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-23

## Context

Live trading, replay and backtesting must be the same code with a different
event source and clock, or backtests prove nothing. Thousands of independent
runs must be reproducible and cacheable by their inputs. Golden hashes
(`golden_stream_hash*` in `crates/tf-synth/src/tests.rs`) are how we notice
that something which should be deterministic changed, and they must come out
identical on x86_64 and aarch64 (CI checks both).

## Decision

Anything that feeds a golden hash, and all engine and strategy code, follows
these rules:

1. **Time comes from a `Clock` or from event timestamps**, never from
   `SystemTime` or `Instant`. Only `SystemClock` touches the wall clock.
2. **Engine code does no I/O.** A run is a pure function of
   (provider, subscription, sink, config).
3. **Prices are fixed-point integers** (`Px`, 1e-9 dollars). Floats appear only
   at JSON adapter boundaries.
4. **No `rand`, no floating-point transcendental functions, no hash-map
   iteration order.** The synthetic generator uses its own `SplitMix64` and
   integer arithmetic so output does not depend on a dependency upgrade or a
   libm.
5. **The on-disk and hashed byte layout is explicit little-endian**, field by
   field, never a copy of an in-memory struct (padding and layout are not a
   contract).
6. **A golden changes only on purpose.** Regenerating one is a reviewed change
   that says why. If only one architecture disagrees, the code depends on the
   platform: fix the code, not the golden.

## Consequences

- Replays, backtests and parallel sweeps are reproducible and can be deduped by
  a hash of their inputs.
- We give up convenience: no `HashMap` where order leaks into output, no `f64`
  maths in the engine, no `rand`.
- New golden tests must be named `golden_stream_hash_<name>` so the
  `determinism` CI job runs them on both architectures.
