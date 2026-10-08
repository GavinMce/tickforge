# 0069. The workspace reads a replayed day's ledger as it reads a live one

- Status: Accepted
- Date: 2026-10-08
- Jira: TIC-196

## Context

Once a replayed day has a file ledger (ADR 0068), the views the workspace already has for a live ledger (the run catalog, the
overview with budgets, use and day profit, the strategy run view) can be shown for it. Until then research scenarios had their own
catalog entries (one per strategy per scenario, built from the trips) and their own screens, which showed none of that.

## Decision

- **A replayed day's sessions are catalog runs read from its ledger,** exactly as a live ledger's are: one run per strategy that acted
  that day, `Kind::Backtest`, kept apart from stored, paper and live runs by its source `Source::Replay { scenario, day }` (it replaces
  the per-strategy `Source::Research` of ADR 0065). The trades and profit are the ledger's, which the harness's tests tie to the trips.
- **Strategies are named as a live ledger names them, by the budget tree** (`s1`, or whatever the tree calls them), not by the
  definition's name; the Backtests screens, which list definitions, give each strategy's number so the two meet.
- **The program that hosts the service says where a day's ledger is** (`ResearchView::replay_ledger`), checked against the report the
  day was made with (ADR 0068); the service then opens it with the code it uses for a live ledger. A run's detail (fills, the profit
  after each, refusals) is the same function for both; a test requires the same trades, curve and refusals from a replayed session and
  from the same ledger served as a live one.
- **`/api/overview?scenario=&day=` is the overview of one replayed day**: the same JSON as a live ledger's, with kind `backtest`.
  Not found (404) for a day that does not exist or was made before ledgers were kept (it says so), refused (422) for a ledger that
  does not match its report (the reason as text), 400 without both parameters. Read only and behind the sign-in like the rest.
- **A day without a good ledger is not in the catalog and the scenario list says why** (`ledgers`: for each day whether it is usable and,
  if not, the reason), so one old or damaged day neither hides the others nor passes as a live-like day.
- **The screens:** the scenario card links each day to its overview; the day overview draws the live overview's own pieces (totals,
  split, group cards, strategies, loss limits) from that day's data and links each strategy to its run and to its trades and replay; the
  run view of a replayed day says what it is and links back; the catalog's runs for a strategy list its days newest first.

## Consequences

- Anything added to a live ledger's views later appears for replayed days without further work.
- Times in the ledger-based views are UTC and instruments are numbers, as they are for a live ledger; the replay page and trade table,
  which read the day's own files, show New York time and symbols.
- The overview of a day is its state at the close: what was in use then (nothing, for strategies that flatten) and the day's profit.
  Meters over the day are E19-S36.
