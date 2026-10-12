# 0082. The opening range breakout trades the first candle of names in play, with entries and exits the strategy holds

- Status: Accepted
- Date: 2026-10-11
- Jira: TIC-169

## Context

T01 of `docs/research/01-trend.md` (1c) is the first regular-session strategy of the library: trade the direction of the first candle of the session on the names whose volume in it is
large against their own normal, entered at a break of the candle, stopped a small part of the ATR away, flat before the close. It needs what the earlier stories built: the shared bars
(E19-S03), the session state (E19-S02), the minute-history columns of the snapshot (E19-S04, S52, S53), the entry triggers and the budget a strategy can read (ADR 0081) and the
asset list for shorts (E19-S54). It is the first strategy that opens in the regular session, where an opening order must carry a broker-side stop.

## Decision

- **A cross strategy, `OpeningRange` (`tf-strategy::opening_range`), template `t01`, with 13 parameters** (every one in the set file, with defaults for the paper's setting). The members
  are the universe's names; the static filters of the paper (price over $5, average volume over a million shares, ATR over fifty cents) belong to the universe spec, which must also
  require the columns the rule reads (`atr14`, `vol_first5`, the profile's first quarter hour).
- **The candle is the shared bar of `range_minutes` (5 or 15) that starts at the open.** Bars are claimed for the members in the premarket only (a bar claimed after the open misses the
  candle, so a strategy that starts late trades nothing), up to `max_tracked`; a refusal is counted. At `settle_seconds` after the candle ends, once, each claimed name whose bar has
  closed is judged (a bar closes when a later trade of the name arrives); a name whose bar has not closed is skipped for the day. The claims are then let go.
- **Direction.** A close over the open is a long, under it a short, equal to it (a doji) no trade. A short only in a name the asset list says is easy to borrow (unknown is no).
- **Stocks in play.** Relative volume is the candle's volume over the name's average for the same window (`vol_first5` for five minutes, the profile's first quarter hour for fifteen),
  at least `min_relvol_pct` percent of it. The `names` highest ratios are traded; ties go to the lower instrument number.
- **Entry.** An `EntryBook` trigger (ADR 0081) at the candle's high (a long) or low (a short), from the end of the candle until `entry_minutes` after the open: the first trade at or
  through the level sends a marketable limit a collar around that trade, once. One trigger a name a day, so at most `names` positions at once. The quoted spread is checked at the
  fire (`spread_cap_bp`, 0 for no cap); a trigger the check refuses is spent.
- **Size** is by the risk to the stop: `risk_ppm` millionths of the strategy's budget over a stop `stop_atr_pct` percent of the 14-day ATR from the level
  (`BudgetView::shares_for_risk`), capped by the room the budget has left and by `max_position_permille` of it. With no budget in force, `dollars` of the level.
- **Exits are the strategy's own** (`ExitBook`): the stop at that distance, held by the engine and sold at the first print through it, and a time exit `flat_minutes` before the close
  (15:58 by default). The opening order also carries the same stop for the broker, as the framework requires of an opening order in the regular session.
- **Eight variants of the paper are parameters**: candle 5 or 15, stop 10 or 20 percent of the ATR, with and without the hourly bias filter (the filter is T02, E19-S16). `orb.set`
  carries the four without it.

## Consequences

- The strategy reads only what the snapshot and the shared bars give, and nothing reads a clock; the same events give the same orders, and a decision log replays equal.
- The simulator does not model the broker-side stop that rides on the opening order. A backtest therefore shows the engine's stop and time exit alone. Live, the broker's stop and the
  engine's exit stand on the same shares at once; what should happen then (cancel the leg first and wait, hold only the time exit, or place the leg far as a disaster stop) is the open
  question of E19-S58 (TIC-213).
- The run over stored days needs the minute history on the cluster (E19-S52); until the snapshots carry it, the universe refuses to run the strategy.
- Shorts depend on the asset list (E19-S54), whose flags are today's and not point in time.
