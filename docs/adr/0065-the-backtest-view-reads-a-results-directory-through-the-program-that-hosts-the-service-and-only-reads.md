# 0065. The backtest view reads a results directory through the program that hosts the service, and only reads

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-188

## Context

A research run (ADR 0059) leaves a results directory: a configuration, and for each day its trips, decision log and traces (ADR 0064).
To judge a strategy the user must be able to see, in the workspace they already use for budgets, which scenarios there are, what each
was made of (strategies, parameters, universes, the budget split, the cost model), and each strategy's trades on each day. The
workspace service (`tf-workspace`) is a small dependency-free HTTP server that knows nothing of how results are read; the stored-run
explorer already solves that with a callback the hosting program provides (ADRs 0027 and 0034).

## Decision

- **`ResearchView` is the explorer's callback pattern for a directory of scenarios:** the service asks four questions (the runs, the
  scenarios, one strategy's trades on a day, one trade's page) and the program (`tf serve --research DIR`, using
  `tf_host::research::view`) answers them. `tf-workspace` gains no dependency on the harness. Every method only reads.
- **A scenario is a subdirectory of the root holding a `research.cfg`;** its name is one plain path component (letters, digits,
  `-`, `_`, `.`, not leading with a dot, at most 100) checked before anything is joined to a path, and a day is a date. A name or a day
  that is not one is "not found", not a path. A directory that is not a scenario is not listed; one that is, and cannot be read, is
  listed with its error so one damaged scenario does not hide the others.
- **The budget split is part of what a scenario was made of, so the configuration keeps it** (`budget` lines in `research.cfg`: the
  balance, the tree, and the number each strategy id was given). A scenario run with another split is another configuration (ADR 0059),
  so splits compared as scenarios (25/25/50 and the next) are separate directories that the view shows side by side; a run
  without budgets says so.
- **Strategies are runs of the catalog** (`Source::Research {scenario, variant}`; `Kind::Backtest`), kept apart from stored
  backtests, paper and live sessions by their source, so the run screen and the backtest view agree on what exists. A research run has
  no explorer page; the run screen points to the Backtests screen instead.
- **What a day says of a strategy comes from what the harness kept, not from a recomputation:** the trades from the trips, the
  orders accepted, refused by the limits and refused by the broker from a per-strategy `stats` trace the run writes, and why each
  refusal from the decision log. Money is text to the cent, prices to four places, times in New York time with milliseconds, R in
  thousandths, all formatted with integers.
- **Routes are GET only and behind the sign-in:** `/api/research`, `/api/research/trades?scenario=&day=&strategy=` and the trade page
  `/research/trade?...` (E19-S35, in the explorer's self-contained-page policy). A missing result is 404, a damaged one 422 with its
  reason as text, a request without its parts 400, and any other method 405. The screens fetch with GET only and write text with
  `textContent`; a test pins both.
- **The trade table filters and sorts in the page** by symbol, side and result, and by time, net dollars, net basis points or symbol,
  and keeps its choices through the page's refresh.

## Consequences

- The service can show results made on another machine: copy the directory (ADR 0064) and point `--research` at it.
- A scenario is listed by reading its configuration and trips; with hundreds of scenarios this would be slow, and an index would be
  worth having then. Not needed for the dozens of scenarios of the plan.
- The view shows what was recorded, so a day made before this change (no traces, no budget lines) shows its trades and says what is
  missing; it is made again to have them.
