# 0010. Strategies are event-driven and cannot reach the clock or the outside world

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-63

## Context

The same strategy code must run in a backtest, a replay and paper trading and
make the same decisions on the same events (ADR 0003). That fails the moment a
strategy reads the wall clock, a file, the network, the environment, or iterates
a hash map. Review alone will not catch that reliably across many strategies.

## Decision

- A strategy implements `Strategy`: `on_event`, `on_timer`, and optionally
  `on_order_update`. Everything it may do goes through `Ctx`: read time
  (`now`), read Tier 0 state and rolling windows, `submit` an intent, and set,
  replace or cancel numbered timers.
- A `Host` drives it. For each event it first fires every timer due at or before
  the event, each with `now` set to the timer's own time and in (time, id)
  order, then lets Tier 0 absorb the event, then calls the strategy. So a
  timer never sees an event that arrives after it, and a 30 s exit fires at
  entry + 30 s, not at the next print. A timer set for "now" or earlier fires at
  the next step, never inside the call that set it.
- `submit` stamps the intent with event time and the strategy's own dense
  sequence number, and validates it. A malformed intent is refused, counted
  (`invalid_intents`) and does not use a sequence number.
- One step fires at most `MAX_TIMER_FIRES_PER_STEP` timers. A strategy that
  re-arms every nanosecond is counted as a storm and continues on the next
  step (late, at the then-current time) rather than hanging a backtest.
- **Enforcement is a per-crate `clippy.toml`** in `tf-strategy` that bans the
  wall clock (`SystemTime`, `Instant`, `SystemClock`), sleeping and spawning,
  `std::fs`, `std::net`, `std::process`, `std::env`, and `HashMap`/`HashSet`.
  CI already runs clippy with `-D warnings`, so a violation fails the build. It
  was checked by injecting each kind of violation. A test asserts the ban list
  still names those items so it cannot be emptied unnoticed. Printing is
  denied in the crate too.

## Consequences

- Strategies cannot be accidentally non-deterministic in the ways clippy can
  see. It is a lint, not a sandbox: a strategy can still add its own
  dependency that does I/O, and a deliberate `#[allow]` defeats it. Code
  review of those two things remains.
- Strategies written outside this crate need the same `clippy.toml`.
- The strategy trait is synchronous and single-threaded per instance (`Send`,
  not `Sync`), matching the one-writer model of the engine.
