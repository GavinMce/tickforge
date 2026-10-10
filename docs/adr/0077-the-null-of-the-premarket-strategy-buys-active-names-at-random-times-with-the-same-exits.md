# 0077. The null of the premarket strategy buys active names at random times with the same exits

- Status: Accepted
- Date: 2026-10-10
- Jira: TIC-185

## Context

The first run of T25 (ADR 0076) over 1 to 8 October made 62 trades in four variants, 26 of them different setups, and earned a little
after costs. Without a baseline that says nothing: the exits and the spread alone can pay or cost that much. The harness compares every
strategy with a null (ADR 0062, `docs/research/data-and-protocol.md`), but T14 draws names from the universe and times before the close.
In the premarket most of a universe has not traded, so most of its draws would find no quote, and it cannot trade at that hour.

## Decision

- **T26, `PremarketNull`, template `t26`.** At the day's first review it draws `names` entry times, uniform to the second between
  `window_start_minutes` and `window_end_minutes` after 04:00, from a generator of the seed and the day's open. At each time it buys one name
  drawn at random from the members that have traded at least `min_dollars` and `min_trades` in the premarket so far, with a last price in
  the band, a quote whose spread is within `spread_cap_bp`, and not halted; none left (or none active) is a skipped time, counted.
  A name is bought once a day.
- **What it takes from T25 and what it removes.** It keeps the filters that do not depend on the signal (activity, price band, spread, a
  collar at the ask, a day order with no protective order, `dollars`, one trade a name) and removes the spike and the pullback. What T25
  earns above it per trade after costs is what its signal adds to buying an active name at a random time.
- **The same exits.** A stop `stop_permille` under the fill, raised to `trail_permille` under the highest price since the entry, and a time
  exit `flat_minutes` before the open, by the same exit book. The one difference is the first stop: T25's is under the pullback's low, which
  the null does not have.
- **Seeds.** The seed is in the parameters, so each seed is its own variant. `premarket.set` runs six; the mean of the six after costs
  is the null's result.

## Consequences

- A premarket run can say what its spike and pullback add, with a spread of outcomes to compare and not one number.
- Active is measured on the whole premarket so far, where T25's spike is measured on the last window, so the null buys names that T25 would
  also call in play on a quiet window; that favours the null a little, which is the safe side for a baseline.
- Fills are as for T25: simulated against the recorded quotes with latency; premarket spreads are wide and queue position is not modelled.
