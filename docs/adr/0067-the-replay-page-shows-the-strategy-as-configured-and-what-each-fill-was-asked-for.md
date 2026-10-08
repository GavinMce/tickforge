# 0067. The replay page shows the strategy as configured and what each fill was asked for

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-194

## Context

The trade page (ADR 0066) showed what happened and the strategy's own trace of the day, but not the strategy: its parameters, the
universe it was run over, the price each order was measured against, or the stop it carried. The stored-run explorer shows these for
MomentumLong (the features it saw, the stop and the trail; ADR 0025 and ADR 0027), and a trade of a library strategy should say as
much. The round trip keeps only totals (slippage, R), and the log does not carry the protective stop.

## Decision

- **Each day keeps a host trace of its executions** (`fills`, strategy 0, beside `instruments`): for every fill the strategy, symbol,
  time, side, purpose, the strategy's reason code, shares, price, **the price the intent was nearest to** (its limit, or a collar's
  reference) and **the stop trigger of its protective orders**. The host already has all of these when it assembles trips; this only
  keeps them. Text, in the day's checked traces file, so nothing about the trips file changes and earlier days read as before.
- **A new panel, "The strategy", on the trade page:** the strategy's name and parameters (as chips), its universe filter as written,
  where the strategy's own account names the symbol (for T04 the row of the ranking and its status), and for each execution of the
  trade the reference price, the fill, what the fill cost against the reference (positive worse: paying above on a buy, receiving
  below on a sale) in dollars and basis points, the protective stop and its distance from the fill, and for an exit why it exited.
- **"Reference price", not "the price asked for".** For a collared order the reference is the market price at the decision and the
  limit is the collar around it; the panel says what the number is rather than calling it the limit. The cost panel's slippage line is
  the same quantity summed over the legs.
- **It does not say whether the symbol passed the universe filter.** The features the filter reads are not kept with the results, so
  the panel would have to recompute them from reference data it does not have. Where the strategy's own account lists the symbol that is
  shown as what it is, the strategy's statement of membership.
- **A day kept before this** has no `fills` trace: the panel says what each fill was asked for was not recorded, shows the definition,
  and a note on the page says to run the scenario again.

## Consequences

- One more row per fill in a day's traces: for T04 about forty rows a day, for a null run of a hundred seeds a few thousand rows a day
  (about a hundred bytes each, uncompressed). Not measured at whole-market scale.
- The stop shown is the trigger the strategy sent; whether the host's disaster stop or the strategy's own exit closed the trade is the
  exit reason beside it.
