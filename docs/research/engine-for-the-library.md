# What the engine needs so the whole library can run at once

The library of `docs/research` is meant to run together, in real time, in one engine. This note says what each strategy needs
from the engine, what is already there, what is not, and what running them side by side costs. It was written on 10 October 2026
after the first two strategies (T04 and T25) and their baselines (T14 and T26) ran, with measurements from a real day. The
numbered stories are in `docs/backlog.yaml` (epic E19) and Jira (TIC-154).

## What is already there

Calendar and sessions (E19-S01), session features in Tier 0 (S02), shared bars with per-strategy claims (S03), history-derived
reference columns (S04), exits held by the strategy and extended-hours order rules (S05), short sales and the restriction in the
simulator (S06), the history store (S07), the research runner (S13), statistics and the trial registry (S14), the research command
over a store (S31), strategies as templates in a set file read by research and the live day (ADR 0071), and the cluster jobs that pull
a range of days and run it (ADR 0075). Two strategies are built (T04 closing reversal, T25 premarket spike and pullback) and two nulls
(T14, T26).

## What the engine does not do yet, found by reading the code and running a day

| | Gap | Story | Who needs it |
|---|---|---|---|
| 1 | The session features Tier 0 keeps (premarket high, low and VWAP, the regular VWAP anchored at the open, the open price, the first minute's and five minutes' volume, the 5 and 15 minute ranges) are not readable by a strategy: the member view gives only Tier 0's own state, and the one reader is a benchmark example | S48 | T01, T03, T05, T09 |
| 2 | The host built from a strategy set has `bars: None`: no strategy run by the research command or the live day can read a bar, though shared bars exist | S49 | T01, T02, T03, T06, T07, T08, T09, T10, T13 |
| 3 | An entry at a break (a stop-entry) is not an order the broker takes before the open, and each strategy would write the same trade watch | S50 | T01, T09, T10 |
| 4 | A strategy cannot see its budget, so "risk 1 percent" or "0.25 percent of the budget" is not possible; every strategy buys a fixed number of dollars | S51 | T01, T06, T07, T08 |
| 5 | The minute-history columns are built for one snapshot by `tf reference build --minutes`, but `tf research snapshots` (research and the daily prepare job) builds from daily bars only: every run said "no minute history" | S52 | T01, T02, T05, T06, T07, T08, T09, T10 |
| 6 | Volume and volatility by time of day exist at seven checkpoints only | S53 | T06, T07 |
| 7 | The cluster has no Alpaca credentials, so no easy-to-borrow flag and every short refuses | S54 (a decision) | T05, T06, T07, T08 shorts, T02 and T01 on the down side |
| 8 | A slow strategy is not seen: a panic is caught and the strategy stopped alone, a loop or a scan of four thousand names on every review delays every other | S55 | all, live |
| 9 | Nothing measures the host with the whole library running, the open's burst on the live feed's size, or contended claims | S56 | all |
| 10 | The history pull takes whole days; XNAS.BASIC premarket (20 times the trades of EQUS.MINI, free on the plan) needs a window | S57 | T25 and any premarket idea |
| 11 | No profiler: finding the cost below needed timers patched into the host | S46 | all |

## Each strategy

| | Needs beyond what is there | Notes |
|---|---|---|
| T01 opening range breakout | S48 (opening ranges), S49, S50, S51, S52 (ATR 14, five-minute volume baseline), S54 for shorts | Direction by the first N-minute candle: the close at minute N is a bar close (S49) |
| T02 hourly EMA bias and pullback | S49 (hourly and five-minute bars), S52 (EMA state, ATR), S54, S51 | The bias is also a filter for T01 and T03 |
| T03 VWAP reclaim | Premarket variant: S48 and S49 only. Regular variants: S48, S49, S52 (the in-play baseline) | Built first, 8a as the premarket variant |
| T05 open-attention gap fade | S48 (open, the day's high so far), S52 (prior close, five-minute volume baseline), S54 | The opposite bet to T01 and T09 on the same stock-days |
| T06 intraday overreaction short | S49, S53, S54, S51 | Shorts only |
| T07 RSI exhaustion and baseline | S49 (one-minute bars, two-minute from them; RSI exists), S53 (volatility profile), S54, S51 | Longs need no borrow |
| T08 prior-day sweep with control levels | S49, S52 (prior high and low), S48 (VWAP target), S54, S51 | Control levels are the prior range shifted: no engine change |
| T09 premarket-high rip and dip | S48 (premarket high), S49, S52 (first-minute volume baseline), S50 | |
| T10 absorption | S49, S50; the refill variant reads displayed sizes, which Tier 0 has | Quote-level: needs tcbbo, which the live feed gives |
| T11 retail order flow (study) | S08 (trade flags), already filed | |
| T12 event studies | S10 (event calendar), already filed; S52 | |
| T13 bull-flag fakeout (measurement) | S49 | |
| T25, T26 (built) | none; S57 for more data | |

`MomentumLong` and `TrendLong` are single-name strategies on another trait, not templates a set can name, and the first enters with orders
the broker refuses in the premarket. They are not part of this library; making either one runnable would be a rewrite as a cross strategy,
as T25 was.

## Running them together: what was measured

One real day of EQUS.MINI tbbo (8 October 2026, 16,793,799 events), release build, the research command, one core:

| | before | after the borrow fix (S47) |
|---|---|---|
| one strategy | 83.2 s | 5.7 s |
| ten strategies (four kinds) | 89.6 s | 10.1 s |

The ten strategies cost the host 4.3 s more than one, about 28 nanoseconds an event for each strategy, so twelve to twenty strategies of these
kinds are not what limits it. Before the fix the host spent 54.6 of 76 seconds in the simulated broker's per-event borrow accrual, which looped over every
instrument slot at every event (timers in the host's step; the rest of the step, Tier 0, journal, promoter and ten strategies, was about 10 s).
Now the host takes about 0.34 microseconds an event with one strategy and about 0.6 with ten, around 3 million events a second including decoding, against the
busiest second at the open of 337,000 trades (`docs/DESIGN.md`), whose burst the ingest queue already absorbs. The live host runs the same broker
for its simulated route, so the same cost was on its path.

Not measured, and what S56 does: all of the library at once (bars claimed for hundreds of names, entry triggers armed, many orders), the
XNAS.BASIC feed's size (about 80 million events a day, five times this day), the first seconds of the open through the live path, and contended
Tier 1 and bars claims. Memory for a day with ten strategies was 123 MB resident; shared bars cost about 40 KB a symbol claimed, so 500 names
are 20 MB and the whole universe of 4,500 would be 180 MB, which is why claims are bounded and refused past the limit, counted
against the strategy that asked.

How simultaneous strategies already share and are kept apart: one Tier 0 and one set of bars for all; Tier 1 and bars claims per strategy with
priority and revocation; a budget tree and loss limits per strategy through one gateway; a strategy that panics is stopped alone and flattened.
What is missing is isolation in time (S55).

## Order of work

1. S47 (done, PR #120): the host's per-event cost.
2. S48 and S49, then T03's premarket variant, which needs nothing else.
3. S46 (profiler), S50, S51, then T09 and T01.
4. S52 and S53, which open T01, T02, T05, T06, T07, T08, T10 on real history.
5. S55 and S56 before any of them runs live; S57 for more premarket data.
6. S54 needs a decision: an Alpaca paper key on the cluster, or no shorts.
