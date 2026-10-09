# 0076. A premarket volume spike, and the first small pullback in its run, is bought long with exits the strategy holds

- Status: Accepted
- Date: 2026-10-09
- Jira: TIC-185

## Context

The wish is a strategy for premarket volume spikes: identify a name whose momentum is strong and buy a small pullback. The nearest
things in the library did not fit. `MomentumLong` (ADR 0015) is a single-name strategy over Tier 1 that enters with an immediate-or-cancel
order and a broker-side stop, both of which the broker refuses in the extended hours (`session_rules`), and it is not a template the
set file can name or the research runner can run. T09 (docs/research 4a) is about the first minute of the regular session. Premarket
VWAP reclaim (8a) is graded D. Nothing in docs/research tests this idea, and the nearest relatives point both ways (Berkman and
others on overnight gaps that reverse; Zarattini and others on opening continuation), so it is built as a thing to measure, with the
null strategy beside it, and not as a claim.

What the feed shows decides how much it can learn. Measured on 8 October 2026 with free record counts and CSV slices:

| | `EQUS.MINI` | `XNAS.BASIC` |
|---|---|---|
| trades from 04:00 to 09:30 | 106,754 | 2,281,426 |
| symbols with any premarket trade | 1,371 | 6,985 |
| symbols with 100,000 shares or more | 15 | 336 |
| complete setups (below) that day | 1 | 7 |

The big ones show in both feeds (`DKI` is in each). `XNAS.BASIC` `tcbbo` is included in the plan and costs nothing to pull; a premarket
window of it is about 2 million records a day. The user chose `EQUS.MINI`, the store already pulled, for the first runs.

## Decision

- **`PremarketPullback` (T25), a cross strategy, template `t25`,** over the premarket only (04:00 to the open), long only.
- **Spike:** once a minute, for each name that has traded, the shares of the last `window_minutes` (5) are at least `spike_x10` / 10 (3.0)
  times the average window of the minutes before them, with at least `min_dollars` (50,000) and `min_trades` (20) in the window, a last
  price between `min_cents` and `max_cents` ($1 to $30), no earlier than `min_history_minutes` (15) into the premarket, and the price up
  at least `min_thrust_bp` (300) from its lowest at the start of the window. The lowest price is the base of the run. Volume is the
  strategy's own count from Tier 0 at each minute, so no minute history or baseline column is needed.
- **Pullback and turn:** every `review_secs` (5) the run's high is followed. A pullback is 10 to 30 percent of the run (high minus
  base) given back; past 30 percent the run has failed and the name is dropped for the day; a new high is a new run. The first rise of
  `turn_bp` (30) from the pullback's low is bought: `dollars` ($1,000) at the ask with a collar, a day order, no protective order.
  Not later than `last_entry_minutes` (10) before the open, at most `names` (3) at once, one trade a name a day, spread at most
  `spread_cap_bp` (100) of the mid.
- **Exits are the strategy's** (`ExitBook`, ADR 0034): a stop `stop_buffer_permille` (5) under the pullback low, **raised** to
  `trail_permille` (30) under the highest price since the entry, and a time exit `flat_minutes` (5) before the open. `ExitBook`
  gains `raise_stop` for that: it moves a long's stop up only, never down, and never a short's.
- **Why a cross strategy and not `MomentumLong`:** it runs through the same host, research runner and set file as T04 and T14, and
  it decides on what the broker accepts in the premarket. Every threshold is a parameter in the variant's text, so another value is
  another variant.
- **Defaults are a starting point from one day's measurement, not a result.** They were chosen so a setup is rare enough to mean
  something (about one a day on `EQUS.MINI`), not tuned on outcomes.

## Consequences

- The strategy can be named in a set file (`strategy 5 pm t25 universe=... names=3`) and run over stored days.
- On `EQUS.MINI` a month holds on the order of twenty trades, too few to judge it. `XNAS.BASIC` premarket windows give about seven times
  the setups; the pull script takes whole days and would need a time-of-day window to pull only the premarket cheaply.
- No minute history, float or news flag is used, so the picks include thin ETFs and names with a catalyst the strategy cannot see; a
  universe file and the dollar floor are the only filters.
- Fills are simulated against the recorded quotes with latency; premarket spreads are wide and queue position is not modelled, so profit
  here is an upper bound.
