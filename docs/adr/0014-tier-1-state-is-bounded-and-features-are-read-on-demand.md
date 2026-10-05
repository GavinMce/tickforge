# 0014. Tier 1 state is bounded, and pullback features are read on demand

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-60

## Context

Strategy 1 classifies a pullback from its depth, volume, higher-low structure,
tape speed and spread (DESIGN.md). Those need history that Tier 0's 60-second
window cannot hold (an impulse and its pullback can run over a minute), and
keeping it for every symbol would cost far more memory than the few symbols that
matter.

## Decision

- A promoted symbol gets a `Tier1Symbol` in `tf-engine`: 256 trades, 64 quotes
  and 256 seconds of one-second bars in fixed arrays. It is `Copy` (so it cannot
  own heap memory) and updates never allocate.
- `Tier1` boxes one per promotion and refuses promotions past its bound, so
  memory is capped. Whom to promote and demote is policy and belongs to E07-S05;
  this change provides the mechanism and the bound.
- Features are computed by `features()` on request, scanning at most 256 bars,
  rather than maintained on every event. A strategy asks once a second or when a
  decision is due. That keeps the per-event cost to a few stores.
- The swing high is the latest occurrence of the highest price in the window, the
  swing low is the latest lowest price before it, and the pullback low is the
  latest lowest price since. Ties go to the later one, so a retest of a low
  restarts the higher-low count.
- Depth, ratios and rates are integers (permille, times-1000). Missing data is
  `None`, never zero.

## Consequences

An impulse older than 256 s is outside the window; a name that grinds slowly
looks different from the one the thresholds were chosen on. Thresholds are
the strategy's parameters, set against the synthetic scenarios first and then
real data (healthy: about 12% depth on 6% of the impulse's volume rate;
dangerous: about 43% on 50%). The pinned feature hash covers the synthetic
runner only.
