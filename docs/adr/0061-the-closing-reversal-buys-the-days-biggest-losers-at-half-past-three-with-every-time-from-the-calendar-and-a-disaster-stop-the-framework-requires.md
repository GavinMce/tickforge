# 0061. The closing reversal buys the day's biggest losers at half past three, with every time from the calendar and a disaster stop the framework requires

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-172

## Context

T04 is the first strategy of the library and the cheapest to run: one decision a day, long only, 30 minutes held
(`docs/research/08-extended-hours.md`, 8c). The evidence (Baltussen, Da and Soebhag, an academic working paper) is that the
return from the previous close to 15:00 negatively predicts the return from 15:30 to the close, through the day's losers.
It is gross of costs, and the margin between gross and net is what the history run measures. It is also the first customer of
the harness (ADR 0059, 0060): it is built so that its rule is fixed before any result is seen.

## Decision

- **The rule.** At 15:30 rank the universe by the return from the previous close to the last trade at or before 15:00, buy the
  most negative `names` (10, 20 or 40), an equal dollar amount of each, and sell at 15:59:30. No short side (the paper's
  winners returned about nothing), no stops in the rule.
- **Every time is minutes or seconds before the regular close, and the close comes from the calendar.** `Ctx::day()` gives the
  strategy the session boundaries Tier 0 was given (`HostConfig::day` in a research run, `Host::start_day` live). At its first
  review of a day (a review a minute) the strategy sets three timers (the first snapshot of prices, the price, the decision),
  and the exit's timer is set when an entry fills. On an early-close day (13:00) all of them move: the decision is at 12:30. A
  host that was not told the day does nothing. A new close is a new day: the timers are set again.
- **The price at 15:00.** A due timer fires after the engine has applied the event that made it due, so at the 15:00 timer
  one symbol (the event's) may already have a later trade in Tier 0, and "the last trade at or before 15:00" would be lost for it.
  The strategy takes a snapshot of every member's last price five minutes earlier and reads the price at 15:00 from Tier 0 when
  its last trade is at or before 15:00, and from the snapshot when it is not. The rule is exact except for that one
  symbol and only when its last trade before 15:00 was more than five minutes earlier; a name with neither has no price and is
  not bought. A trade at exactly 15:00:00 counts; one a second later does not (tests).
- **Ranking and skips.** The return is in parts per million, truncated toward zero; the `names` lowest, whatever their sign (on a
  day the whole universe rose, the ones that rose least), ties to the lower instrument number. A name is skipped if it is halted
  or paused (`halted`), under the short-sale restriction (`ssr`; the rule as written, and since a restricted name is down ten
  percent on the day this removes the most extreme losers), has no prior close or no price at 15:00, has no two-sided quote (a
  crossed one counts as none; a locked one is a quote), or if its dollars buy no share. The halts and restrictions are read at
  15:30, not at 15:00.
- **Variants are parameters, all in the text of the parameters** (so another value is another fingerprint, another variant for
  the certificate and the trial registry): `names`; `extreme_bp` (only a return of at most minus that many basis points;
  exactly the floor is kept); `spread_cap_bp` (only a quoted spread of at most that many basis points of the mid at the
  decision; exactly the cap is kept); `entry_minutes` equal to `ref_minutes` (buy at 15:00 instead, the price taken just before
  the decision at the same instant); `dollars`, `collar_permille`, `ref_minutes`, `exit_seconds`. `ClosingReversalParams` parses and
  renders them, every key once, and refuses what the rule cannot run with.
- **Entry and exit.** The entry is a marketable order a collar (5 permille) around the ask, `Tif::Day`. The exit is the
  `ExitBook`'s time exit (ADR 0056), `exit_seconds` before the close: a sell a collar under the last trade, retried until
  filled, which the simulator fills at the bid. What is still held at the end of the day is flattened by the host. Only what an
  entry filled is sold, as it fills.
- **A stop that is not part of the rule.** The framework refuses an opening order in the regular session without a protective
  stop (`IntentError::MissingProtection`), and the rule has none. So every entry carries a **disaster stop**,
  `stop_permille` (default 100: 10%) under the ask, far enough not to be touched in thirty minutes on a liquid name; it is a
  parameter, so a different distance is a different variant. In the simulator and the research runner protective legs are off
  (ADR 0056), so it changes no result there; live it is a real order. **R is measured against it** (net over the money between the
  entry and the stop) and is therefore not comparable with a strategy whose stop is part of its rule: T04 is judged in basis
  points.
- **A definition.** `tf_host::library::closing_reversal(id, name, universe, params)` builds the `StrategyDef`, with the
  parameters' text as `params`. The registry that names definitions for a command is E19-S31.
- **Checks.** 21 scripted-event tests of `ClosingReversal` through a `CrossRunner` (every decision above, the early-close day, the
  next day, the exit with partial fills, the parameters); six through the host (a day with its fills, the early-close day, no day
  told, replay equality and a changed afternoon that does not replay equal, and the same afternoon as a DBN day through
  `research::run_day` with the trips and costs worked by hand, replayed through `replay_files` to equal). Mutation run on the
  strategy: see the backlog evidence.

## Consequences

- **Not yet run on real data.** No real captured stretch is stored (the history store of ADR 0058 holds one-minute bars of five
  symbols and one tcbbo day was in a scratchpad that is gone), and no Databento key is configured where this was built.
  The strategy is exercised over a DBN day made from a script, through the same runner and replay check a real day would use.
  The twelve-month run on real quotes, T04 included, is E19-S31, which needs the pull of E19-S30 or the cost of a subset.
- **The universe is the universe spec's.** Price at least $5 and the dollar-volume floor are conditions of the spec the
  definition is given, not of the strategy; the earnings and news flag of the rule waits for the event calendar (E19-S10).
- **One host, one day at a time.** Timers are set when the close changes, so a host that runs several days sets them again, but
  a strategy that carried a position across the close would not be handled here (nothing is carried: the host flattens).
- **The 15:30 decision uses the quote at the event that found it due**, and in a research run the order reaches the simulated
  broker after the cost model's latency (50 ms by default), filling at the quote then.
- The tests of this crate and the host leave their scratch directories under the system temp directory, as the existing tests
  do; they go at the next boot.
