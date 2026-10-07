# 0054. Bars are shared by claims, aligned to the session by the calendar, and read through the strategy's own claim

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-157

## Context

Cross-sectional strategies (ADR 0043) saw Tier 0 only: `ctx.track_bars` told them the host had no aggregator.
The strategies in `docs/research` need one-minute candles, an hourly EMA and RSI (1a, 2a, 2c, 3a, 3b). Three
things stood in the way. A per-symbol `Host` owns its bars, so twenty strategies would build twenty copies of a
symbol's 48 KB. Hourly bars were UTC clock hours (09:00 New York time in summer, never 09:30) and the day bar a
fixed offset the caller changes twice a year (ADR 0052). And `MtfBars::on_event` offered time to every tracked
symbol on every trade, which is fine for 16 symbols and not for 2,000 at 337,000 trades a second.

## Decision

- **One aggregator, claims per strategy** (`tf_engine::SharedBars`, in the manner of ADR 0044). A symbol is tracked
  while any strategy claims it; a strategy letting go leaves the others' bars untouched and the last to go drops
  them. The tracked set is bounded (`HostConfig::bars.max_tracked`, 48.6 KB a symbol). A claim for a new symbol
  past the bound is refused (`ctx.track_bars` returns `Track(Full)`), counted against the strategy that asked and
  exported as `bars_*{strategy="N"}` lines; a symbol already tracked is always granted. The host drops a stopped
  strategy's claims (panic, soft or hard loss limit, kill).
- **Read through your own claim.** `ctx.bars(id)` shows a cross strategy only the symbols it claimed, so what it
  sees does not depend on which other strategies run. Bars begin with the first trade after the first claim and
  are never back-filled; a strategy that joins a symbol another already tracks sees the bars from the earlier
  start (more history, never a different bar).
- **Alignment is a choice of the aggregator** (`MtfConfig::alignment`). *Clock* is the old behaviour (epoch-aligned,
  day bar at a fixed offset). *Session* asks the calendar: hourly bars start at the beginning of their session and
  the last is cut short at the session's end (09:30, 10:30 ... 14:30, then 15:30 to 16:00; 09:30 to 12:30 and the
  12:30 to 13:00 stub on an early close; the premarket from 04:00; after-hours from 16:00). The **day bar is the
  trading day, 04:00 to 20:00 New York time**, the same span Tier 0's day-wide fields count (the regular session
  alone is in `Sessions`, ADR 0053); its UTC offset is the calendar's for that date. 1m, 5m and 15m stay on the
  epoch grid (a session boundary is a multiple of 15 minutes). A trade outside every session, or on a day the
  calendar does not cover, builds nothing and is counted (`unplaced`). Gap filling is a clock option and is ignored
  in session alignment. Rejected: a day bar from the open for 24 hours (what the fixed offset did: it put tomorrow's
  premarket in today's bar).
- **Time is looked at when a bar is due.** `MtfBars` keeps the earliest end of any forming bar and visits every
  tracked symbol only when time reaches it (once a minute in practice), not on every trade. Outputs are unchanged:
  the brute-force grouping over 80 random streams and the pinned digest pass as before.
- **Indicators as pure functions over closed bars** (`tf_engine::bar_fns`: `ema`, `rsi`, `atr`, `vwap`). Each runs
  the streaming indicator over the bars it is given, so there is one definition; bars with no trades are skipped.
  They see only the last 120 closed bars of a timeframe, so for a recursive indicator they equal the streaming one
  only while the series fits the ring; a longer memory (the EMA(100) of hourly closes) is seeded from history
  (E19-S04).

## Evidence

- Real day, 2 October 2026, 25 symbols, 2,035,629 trades from the premarket to after-hours (`real_bars check`):
  12,715 one-minute bars inside the covered windows equal Databento's `ohlcv-1m` on open, high, low, close and
  volume, none only ours or only theirs; all 300 hourly bars start on their session's grid; 850 comparisons of the
  functions with the streaming indicators (EMA both seeds, RSI, ATR at 2, 5, 14 and 20, VWAP, on 15-minute and
  hourly bars) agree.
- 2,000 busiest symbols (88.8% of the trades) over the real opening five minutes, 2,425,084 trades from 12,184
  symbols (`real_bars load`, per-event timing included, a timer reading costs about 35 ns): Tier 0 alone 14.8 million
  events a second, with the bars 9.6 million; the bars add about 37 ns an event (median per event 20 to 50 ns, 99th
  percentile 280 to 400 ns, 99.9th 570 to 680 ns). The worst single event is 0.26 ms: the once-a-minute pass over
  2,000 symbols. The busiest second (304,321 events) takes 18 ms, 55 times faster than real time. Memory is 48,576
  bytes a symbol, 97 MB for 2,000 (92 MB resident on claiming, 9 MB more while running). There is no documented
  engine lag budget yet (E07-S07); against the design's 50 to 300 ms tick-to-order the worst event is a few
  thousandths of it, and against arrival, the busiest second is under 2% of a second.

Mutation run on the new code: 51 mutants (the aggregator's placement and due logic, the claims, the functions, the
host's release and lending, the context's answers). Eight survived at first; six tests were added and all are killed
except two in the earliest-due bookkeeping that cannot be told apart while time does not go backwards.

## Consequences

- The earlier "about 40 KB a symbol" (ADR 0017) was low; it is 48.6 KB, and 1,000 symbols are about 49 MB.
- `HostConfig::bars` is part of a run: a replay host built from the same configuration reproduces the decisions of
  strategies that read bars, and one built without them does not (a test shows both).
- Closed bars are not delivered to cross strategies (`on_bar`); they read them at a review. Nothing needed more.
- Time passes for the bars only when a trade arrives, as before; the multi-strategy host never calls
  `advance_to` (no runner's either), so a last bar closes at the next trade, not at 16:00.
- `Host::start_day` is still called by nothing but a test, so a live host has no session state until operations
  (E18-S09) decide the day. Session-aligned bars do not need it (they ask the calendar), but E19-S02's features do.
- Not built: bars from history (E19-S04), a bar close callback, per-timeframe depth other than 120.
