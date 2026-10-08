# 0070. The trade page shows what the engine did with Tier 1, from the day's log

- Status: Accepted
- Date: 2026-10-08
- Jira: TIC-197

## Context

A replayed day runs the same host as a live one (ADR 0068): Tier 0 takes every event, and the promoter moves names into and out of
the bounded Tier 1, on a scanner hit or because a strategy asked, and records each move in the decision log. The trade page
(ADR 0066, ADR 0067) showed nothing of this, so the engine's part in a trade, why a name was being watched closely and what else was,
could not be seen. A strategy that decides on Tier 0 alone (T04, T14) has no promotions at all, and that should be visible too.

## Decision

- **The tier changes come from the day's decision log, as recorded:** each is a time, a name, promoted or demoted, a reason code
  (scanner hit with the volume z-score, cooled off, a strategy asked, evicted for a higher priority) and the score. Nothing is
  recomputed.
- **The day's host trace of instruments now includes every instrument the day changed the tier of** (with those it decided on or
  filled), so a promoted name that was never traded still has its symbol; a name with no symbol is shown as `#number`.
- **The page shows:** on the chart a marker where the traded name was promoted or demoted (in the window the market is kept for: ten
  minutes before the entry to two after the exit, edges inclusive), in the readout how many names held Tier 1 at the cursor and which
  (the first five), and a panel with the traded name's own changes (all of the day) and the changes of every name in the window (up to
  300, saying how many there were), the traded name shaded and the rows after the cursor dimmed, and who held Tier 1 when the window
  began.
- **It says when there is nothing:** a day with no tier changes says the engine promoted no one and the strategies decided on Tier 0
  and what they took snapshots of; a day with changes but none of this name says it was never promoted.
- **A syntax error in the page's script is caught in a test** when `node` is installed (it is in CI): neither page's script is run by a
  Rust test, and an error in it showed only in a browser.

## Consequences

- With real data the scanner and the strategies' requests will fill this in; on the scripted days of the tests only T04's (no
  changes) and hand-made logs have been seen. Whether the panel is useful on a real day is not known.
- Names held in Tier 1 at the cursor are computed from the first 300 changes in the window; a busier window is cut and says so.
- What was in a strategy's own claims on Tier 1 (the pins) is not in the log beyond the promotions they cause.
