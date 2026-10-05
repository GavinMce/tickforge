# 0028. Strategies take Tier 1 from the shared promoter

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-116

## Context

ADR 0024 made promotion to Tier 1 a hysteretic, bounded decision written on the tape, so a
replay sees promotions at the moments the live run did. MomentumLong still ran a private Tier 1
with its own spike scan, so none of that applied to it: its watch list was not on the tape and
a replay from the tape could only reproduce it by re-running the strategy's own scan.

## Decision

`MomentumLong` watches only symbols the host's `Promoter` has promoted. Build the host with
`MomentumLong::host(universe)`, which attaches the default promoter.

- **Kept in the strategy:** its own spike gate (`spike_secs`, `spike_permille`,
  `spike_min_volume`, still agent-tunable), its price and spread filters, the cap of
  `max_watched` engaged symbols (a promoted symbol that shows the spike but finds the strategy
  full is counted once per promotion in `promotions_refused`), and a cool-down per symbol.
- **Moved to the promoter:** membership, history, bounds, demotion, and the tape record.
- **Pinning:** the strategy pins a symbol from the moment it takes it up until it lets go
  (after a decline, or when the position is closed), so it is never demoted under an open
  position. A symbol demoted while merely watched is dropped and its slot freed.
- **No promoter, no trading:** a host without one leaves the strategy idle, and every trade it
  could not act on is counted in `no_promoter` rather than silently ignored.
- **Replay:** a follower host fed the tape's `TierChange` events reproduces the live run's
  intents, entry evidence, declines and counters exactly (tested on four seeds).

## Measured

Old private scan against the shared promoter, same sessions, 24 seeds x 3 configurations (72
runs, lead-in at least 70 s): the new code never produced fewer trades than the old in any run;
healthy trades 160 -> 191; dangerous trades 0 -> 0. On the 60-seed unit scenarios the healthy
runners with no entry are the same three before and after (seeds 10, 28, 53). Net P&L rose with
the trade count, but it is the synthetic generator's profit, not evidence of an edge. I did not
isolate why the shared promoter finds more.

## Consequences

- **Warm-up:** the scanner scores a symbol against its own baseline (one sample per 10 s, six
  needed), so a runner that starts before about a minute of data is not seen. Measured at a
  20 s lead-in: 0 of 18 healthy runners found; from 60 s: 18 of 18. The old private scan needed
  no baseline. Demo sessions now default to a 70 s lead-in (`tf_backtest::DEMO_LEAD_SECS`) and
  the unit scenarios were lengthened to match. Live, symbols have hours of baseline.
- **Stored runs:** runs stored before this change (default lead 20) do not reproduce and are
  refused by `tf explore` / marked `drifted` by `tf runs --check`, as designed.
- A promoted symbol stays in Tier 1 across the strategy's cool-down; the strategy waits for a
  fresh spike on it rather than for demotion, as before.
- Scanner and promoter settings are the host's; the strategy's spike parameters no longer decide
  promotion, only whether it takes a promoted symbol up.
