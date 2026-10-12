# 0083. The hourly bias is the last hourly close against an EMA carried from the snapshot, and a filter asked when a trigger fires

- Status: Accepted
- Date: 2026-10-11
- Jira: TIC-170

## Context

`docs/research/01-trend.md` (1a) defines a bias for intraday entries: the last completed regular-session hourly close above or below the EMA(100) of such closes, none until the first
bucket ends at 10:30. It is a filter on entries (T01, T03, the T02 pullback), to be run with and without it, "the only way to tell if the filter adds anything". The engine's own hourly bars
start on the UTC clock hour (09:00 to 10:00 New York time, not 09:30 to 10:30), keep 120 bars, start empty with the process and have no time-zone database, so they cannot be the series. The snapshot
already carries the state of the EMA at the previous close (`ema100h_state`, `ema100h_count`, E19-S04), computed offline over regular-session hourly closes with the simple-average seed.

## Decision

- **`HourlyBias` (`tf-strategy::hourly_bias`)** follows a set of names for a day. Each name's EMA is resumed from the snapshot (`Ema::resume`, exact); a name whose snapshot has no state, or fewer
  than 100 closes, is not followed and has no bias.
- **The closes of the day are folded in as their buckets end**: 10:30, 11:30 ... 15:30 (six; the half hour to the close is not needed in the day). A bucket's close is the close of the shared 15 minute bar
  that ends with it, which is the last trade price in the hour; a quiet quarter hour is filled with a flat bar by the bars themselves, and when the bar is missing the last bar before it is used. A bucket is folded
  `settle_seconds` after it ends. The live state equals one offline pass over the same closes (a test carries a snapshot through a day and compares it with one `Ema` fed everything).
- **A bar that has not closed yet is waited for, per name** (`Close::NotYet`): the shared bars move on with trades, so at the instant a bucket ends the bar may still be forming. A name with no bar at all is
  `Missing` and is broken for the day: its series would not be the offline one, so it has no bias rather than a different one.
- **The bias** is up when the last folded close is over the EMA, down when under, and none before the first bucket is folded, on a tie, and for a broken name. `allows(id, side)` is true for a buy
  with up and a sell or short sale with down, and never with none.
- **The filter is asked when a trigger fires**, not when it is armed. T01 gets the parameter `bias` (0 or 1; `bias=1` in its set line): the armed names keep their bars to the end of the day for the closes (the
  others let go as before), the fold runs before each trade of an armed name is checked as well as at the reviews (the trade after a quiet quarter hour is what fills its bar), and a trigger whose side the
  bias does not allow is spent and counted (`biased_out`), as a trigger refused for its spread is.
- **No bias before 10:30 means no trade before 10:30 with `bias=1`**, as in the research.
- **`bias=2` takes the morning's bias from the previous close.** A follower built with `carrying_prior` reads the snapshot's `prev_close` (the last price of the last hourly bucket the carried EMA already
  contains) against the carried average until the first bucket of the day is folded, and the day's closes decide from then on. A name with no previous close has none until 10:30, as with `bias=1`.
  `orb.set` carries the four unfiltered variants, the four with `bias=1` and the four with `bias=2`.

## Consequences

- With `bias=1` the filtered variants cannot trade the breaks of the first hour, which is where most of an opening range breakout's trades are, so a difference from the unfiltered twin is the filter and the
  lost first hour together. `bias=2` keeps the hour (its bias is then the previous session's trend, not the morning's), so the three together separate the two effects.
- The bias costs the armed names' bars for the day (about 40 KB each, at most `names` of them) and one pass over the followed names at each trade of an armed name.
- Other filters and the standalone pullback entry of T02 use `HourlyBias` as it is: they follow their names, call `advance` and ask `allows`.
