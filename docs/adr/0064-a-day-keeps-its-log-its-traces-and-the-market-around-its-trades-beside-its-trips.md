# 0064. A day keeps its log, its traces and the market around its trades beside its trips

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-187

## Context

A results directory (ADR 0059) kept one file a day, the round trips. To show a trade (E19-S35) the backtest view needs more:
the decision log (what was decided, ordered, filled and refused, and when), the strategies' own account of why (ADR 0063), and
the market around the trade to chart. And it must be possible to look at a result with neither the strategies' code nor the
store of market data to hand: the store is files of hundreds of gigabytes that may be on another machine.

## Decision

- **Companion files beside `<date>.trips`:** `<date>.log` (the host's decision log, as the replay check reads it),
  `<date>.trace` (the day's traces, ADR 0063) and, when asked for, `<date>.evidence.zst`. The day file's format is unchanged,
  so results made before still read; they have no companions and a run makes such days again.
- **Each companion says what it belongs to and is checked.** It begins with its kind, the day, the configuration's fingerprint and the
  day's outcome hash, and ends with a checksum of everything before it. A file from another day, another run or another
  configuration, a damaged, cut or extended one, or one of another kind, is refused when it is read; the evidence is checked the same way
  once decompressed. `Results::log`, `traces` and `evidence` read them.
- **The trips file is written last.** The companions go first (each through a `.part` file and a rename), so as before a day is
  there complete or not at all. A day is complete when its trips, its log and its traces read back, and its evidence if the run asked
  for it: a day without them is made again, a day made again without evidence leaves none from before, and asking for evidence of
  a directory that has none makes its days again.
- **Evidence is a second, sequential read of the day's files after its run** (`RunOptions::evidence`, `run_with`; `run` keeps no
  evidence), because only then is it known which symbols were traded and when. For each symbol traded it keeps every trade, quote
  and status event in a window around its trips (by default ten minutes before the entry to two after the exit; overlapping windows of one
  symbol are one), **as the host saw them** (what the gateway sent twice taken once). It reuses the symbol table the run
  built, so it is one read, not two. Text, compressed with zstd, deterministic: two runs leave the same bytes. A trip with nothing kept
  around it is named in the run's report (`RunReport::no_evidence`).
- **It does not change a result:** the options are not part of the configuration, so a directory run with and without evidence
  holds the same trips.

## Measured

On a synthetic hour of 8.64 million events (six names at a busy name's pace; 553 MB of DBN, 58 MB compressed), keeping three
names' 42-minute windows (2.8 million events) takes **5.6 s**, about 1.5 million events a second. The symbol pass the run already makes
takes 4.7 s of its own. At that rate a whole-market tcbbo day (82.4 million records) would take about **55 s**, against about 133 s for the
host pass itself at the 620,000 events a second measured earlier on small days (unverified at whole-market scale). The size of a real T04
day's evidence is **not measured** (no real day is stored): by estimate, twenty names with a few thousand events each in the last
forty minutes is a hundred thousand events and a few megabytes of text, one to a few compressed; cmbp-1 would be about seven times that.
The scripted three-day tests keep three names' windows of a few thousand events.

## Consequences

- Only symbols that were **traded** have evidence: the view shows the market of a trade, and, for a name not bought, only what the
  strategy's trace says of it.
- The pass needs the store once more per day (a second read of the files after the run) and is optional.
- Everything the view needs for a day is in the results directory, which can be copied to the dev server without the store.
- The decision log is kept for every day whether or not evidence is. Traces are always asked for in a research run (one `rank` trace a
  day for T04; a few small ones for each null seed).
