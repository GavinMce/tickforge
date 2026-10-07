# Strategy research (October 2026)

What this is: for each of 24 trading ideas, what the published evidence says, what is folklore, whether
our data can test it, a testable rule written down before any test, and a verdict. It is the input to
the strategy library (epic E19) and to the choice of strategies for the paid month (E18-S08).

What this is not: evidence that any of these makes money. Nothing here has been backtested yet. Most of the
ideas come from discretionary day-trading teaching; for a few there is real published research, for most
there is none I could find, and for some the research points the other way. The point of the work is to
sort them before spending a month of live data on them.

## How the research was done, and its limits

- Web searches (a general search tool, not a library database) for each idea, then reading the primary
  source where I could get it. **Read in full:** the opening range breakout paper (Zarattini, Barbon, Aziz),
  the end-of-day reversal paper (Baltussen, Da, Soebhag), the intraday reversal paper (Zawadowski, Andor,
  Kertész), the LULD plan's 2025 annual report, the SEC staff study of the LULD pilot and the Nasdaq LULD
  FAQ. **Read in part:** Sullivan, Timmermann and White (introduction, out-of-sample section and conclusion)
  and Ben-Rephael, Hitzemann and Xiao (abstract and introduction). **Abstract or summary only:** every other
  paper named in these files; each says "abstract" or "summary" where it is cited. Figures from this
  work that I measured myself are marked with their date: a day of status records for the whole market, one
  day's record counts and prices by schema, three symbols' statistics, and the Databento metadata.
- Several retail-education pages turned up with confident numbers ("50% of breakouts fail and of those 50%
  reverse, so 75% fail" is arithmetic nonsense). I did not use such numbers as evidence.
- Not finding research is not finding that an idea is false. Grade D means "I found no rigorous test",
  not "it does not work".
- The two day-trading papers I lean on most (opening range breakout; VWAP trend) are SSRN working papers by
  Zarattini, a researcher at Concretum Research, with Aziz, who runs a day-trading firm and a trading-education
  company (Bear Bull Traders), and, on the opening range paper, Barbon of the University of St. Gallen. I found no
  independent replication and did not check whether a journal version exists. The opening range paper charges
  commission only ($0.0035 a share), not spread or slippage; the VWAP paper says "net of commissions" and gives
  no detail. The end-of-day reversal paper (Baltussen, Da, Soebhag, April 2025) is an academic working paper.


## Evidence grades

| Grade | Meaning |
|---|---|
| A | Peer-reviewed or well documented, same kind of instrument and horizon, survives costs in the source |
| B | A documented effect, but at another horizon or instrument, or it does not survive costs, or it is not replicated |
| C | A practitioner or working paper with strong claims, not independently replicated, costs only partly modelled |
| D | Folklore: no rigorous test found |
| X | The evidence found points the other way |

## Verdicts at a glance

"Build" means a strategy in the library (T01 to T14, backlog E19-S15 to E19-S28). "After" means it needs a system
addition first (S01 to S14, backlog E19-S01 to E19-S14). "Study" means a measurement on history before any strategy.

| # | Idea | Grade | Verdict |
|---|---|---|---|
| 1a | 100 EMA on the 1H as bias for 1m/5m entries | D | Build (T02), as a filter tested against the same entries without it; needs session-aligned bars and history state (S03, S04) |
| 1b | VWAP bounce after a wick on volume, not the first touch | D (VWAP trend on QQQ: C) | Build (T03) with both variants, so "wait for the wick" is tested against "first touch" |
| 1c | First-hour trend lock | index: B; single stocks at day end: X; opening range breakout on stocks in play: C | Build the opening range breakout (T01); "stick with the first hour" is a variant, not the main bet |
| 2a | Broken parabolic short | B (reversal after sharp intraday moves in liquid stocks) / D (the candle rule) | Build (T06), easy-to-borrow only, hard risk caps |
| 2b | "Fake halt" short | C (LULD facts) / D (the trade) | Study first; needs computed bands (S09) |
| 2c | RSI exhaustion rubber band | D | Build (T07) with the plain reversal event as its baseline |
| 3a | Stop-hunt reversal at prior day high/low | B (FX stop clustering; depth at support) / D (equities) | Build (T08) with control levels |
| 3b | Market-maker refill zone (volume, no movement) | B / D | Build the L1 version (T10); real icebergs need depth (S11) |
| 3c | Dark-pool footprints | D (levels) / B (retail flow, noisily identified) | Replace with a retail order flow study (T11) after trade flags (S08) |
| 4a | 1-minute rip and dip | D | Build (T09), run head to head with T05 |
| 4b | Big hidden bid scalping on Level 2 | B (imbalance) / D (the trade) | After depth data (S11); research only |
| 4c | Option flow as a stock signal | B (open-buy put-call ratio, with data the public does not have) | Research (S12), not built |
| 5a | FOMC fade | C (the drift, since faded) / D (the fade) | Study only (T12); eight events a year; calendar in S10 |
| 5b | Earnings overreaction reversal | B/C, mixed | Study (T12) after the event feed (S10), follow and fade on the same events |
| 5c | Merger arbitrage "free short" | X (the payoff is not free) | Do not build |
| 6a | Bagholder bounce after a gap down of 20% or more | B, depends on news | Study (T12) after S10, with and without a news flag |
| 6b | Retail fakeouts of bull flags | D | Measure first (T13) |
| 6c | 9:45 reversal | B (open attention reversal) / D (the clock time) | Build as the open-attention gap fade (T05) |
| 7a | Gamma squeeze ignition | B (mechanism) / C | Research (S12) |
| 7b | Max pain Friday fade | B (pinning) / C (max pain, weekly) | Research (S12) |
| 7c | Open-interest strike fakeouts | D | Research (S12) |
| 8a | Premarket VWAP reclaim | D | Build as the premarket variant of T03 |
| 8b | After-hours liquidity trap fade | X (after-hours trading is informed) | After the event feed and an extended-hours cost model; no strategy now |
| 8c | Closing-bell liquidity grab | B | Build (T04), the first to build: long only, one decision a day |

Plus a null strategy (T14: random entries on the same universe with the same exits and costs) that every result is
compared with.

## Files

- [01-trend.md](01-trend.md): 1a, 1b, 1c
- [02-mean-reversion.md](02-mean-reversion.md): 2a, 2b, 2c
- [03-liquidity.md](03-liquidity.md): 3a, 3b, 3c
- [04-scalping.md](04-scalping.md): 4a, 4b, 4c
- [05-news-reaction.md](05-news-reaction.md): 5a, 5b, 5c
- [06-psychology.md](06-psychology.md): 6a, 6b, 6c
- [07-options.md](07-options.md): 7a, 7b, 7c
- [08-extended-hours.md](08-extended-hours.md): 8a, 8b, 8c
- [data-and-protocol.md](data-and-protocol.md): the data we have and what it costs, what one live month can and
  cannot tell us, and the test protocol (costs, trials, sample sizes)

## What the library needs from the system

Filed as epic E19 (TIC-154). The S numbers are the story numbers (E19-S01 is TIC-155, and so on, one more each).

| | What | Why (the ideas that need it) |
|---|---|---|
| S01 | `tf-calendar`: trading days, sessions, New York time with daylight saving | Nothing has a calendar today: bars use a fixed UTC offset the caller must change twice a year; every time-of-day rule, the 04:00 start and the 15:30 decision need it |
| S02 | Session features in Tier 0: premarket, regular session, after-hours state per symbol | Tier 0's high, low, volume and VWAP count every trade including extended hours, so there is no 09:30-anchored VWAP, no premarket high, no opening range (1b, 1c, 4a, 6c, 8a) |
| S03 | Shared bars and indicator functions for cross strategies | Cross strategies see Tier 0 only; one-minute candles, hourly EMA and RSI need bars (1a, 2a, 2c, 3a, 3b) |
| S04 | History-derived reference columns | Previous high and low, ATR in price units, first-minute and time-of-day volume baselines, EMA state (1a, 1c, 2a, 3a, 6c) |
| S05 | Exits in the simulator, engine-held exits, extended-hours order rules | The simulator ignores stops and targets today; extended hours accept limit orders only (every strategy with a stop; 8a) |
| S06 | Short sales in the simulator: restriction and easy-to-borrow | Shorts are in 2a, 2c, 3a, 6c; the decoder does not read the restricted flag on status records |
| S07 | Research history store | The only place an edge can be seen is twelve months of history |
| S08 | Trade flags: trade reporting facility, sub-penny | Retail order flow (3c) |
| S09 | LULD bands, limit states, halt reasons | 2b; E06-S06 narrowed to what the feed gives |
| S10 | Event calendar: FOMC, earnings filings | 5a, 5b, 6a, 8b, and a risk filter for every strategy |
| S11 | Depth data (entitlement first) | 3b, 4b |
| S12 | OPRA research track | 4c, 7a, 7b, 7c |
| S13 | Research runner with costs and per-trade records | The harness |
| S14 | Statistics, trial registry, deflated Sharpe | The harness |

The strategies and studies:

| | Strategy | Backlog | Jira |
|---|---|---|---|
| T01 | Opening range breakout on stocks in play | E19-S15 | TIC-169 |
| T02 | Hourly EMA(100) bias and pullback | E19-S16 | TIC-170 |
| T03 | VWAP reclaim (wick, first touch, premarket) | E19-S17 | TIC-171 |
| T04 | Closing reversal | E19-S18 | TIC-172 |
| T05 | Open-attention gap fade | E19-S19 | TIC-173 |
| T06 | Intraday overreaction short | E19-S20 | TIC-174 |
| T07 | RSI exhaustion and the plain reversal baseline | E19-S21 | TIC-175 |
| T08 | Prior-day sweep reversal with control levels | E19-S22 | TIC-176 |
| T09 | Premarket-high rip and dip | E19-S23 | TIC-177 |
| T10 | Absorption (volume without movement) | E19-S24 | TIC-178 |
| T11 | Retail order flow study | E19-S25 | TIC-179 |
| T12 | Event studies: earnings, FOMC, flush bounce | E19-S26 | TIC-180 |
| T13 | Bull-flag fakeout measurement | E19-S27 | TIC-181 |
| T14 | Null strategy | E19-S28 | TIC-182 |

E19-S29 (TIC-183) runs everything built over twelve months and applies the gate before the live month; E18-S08 (the
strategy set for the month) now depends on it.

Build order: S01 to S06; then the harness (S07, S13, S14) with T04 and T14, the cheapest strategy and the
baseline, as its first customers; then T01 and T02, T05, T09, T03, T06, T07, T08, T10; then S08, S09 and S10 and what
they open. S11 and S12 wait for a reason.

## The five things that matter most

1. **A month of live data cannot validate an edge.** The best-documented idea here (opening range breakout on
   stocks in play) earned 0.08 R a trade in its paper; at a per-trade standard deviation of 1 R, seeing that at two
   standard errors takes 625 trades, and the spread is likely larger (2,500 at 2 R). A month gives a few hundred. Live data
   in one month tests plumbing, fills and failure modes. Edges have to come from history: the Standard plan includes
   twelve months of L1 and trade history, which is the right place to test (data-and-protocol.md).
2. **Several of these ideas are tested by the ideas themselves.** "Wait for the wick, not the first touch", "first
   red engulfing candle", "RSI 90 and then the first reversal candle": each is a *refinement* claim. The library builds
   both versions so the refinement is tested against the plain version.
3. **At the single-stock level, the first hours of the day do not carry forward to the close.** The best evidence on
   individual stocks (Baltussen, Da and Soebhag 2025) is *reversal* into the last 30 minutes, and entirely in the
   day's losers. The index-level result (Gao et al.) is momentum. These pull in opposite directions, and "trend is
   set, don't countertrade" sits on the wrong side for single stocks late in the day.
4. **Shorting is where the cost and the ruin risk live.** Parabolic fades, fake-halt shorts and merger "free shorts"
   all need borrow, and the ones that look best (small, spiking) are the hardest to borrow and the most likely to
   squeeze. Alpaca shorts easy-to-borrow names with no fee and, since 24 June 2026, hard-to-borrow names through a
   per-order locate; a restriction applies once a stock is 10% below the prior close. The simulator models neither.
5. **Retail order flow is a real signal and our feed sees part of it.** The off-exchange prints reported to the two
   Nasdaq facilities are in our feed (publishers 82 and 83), and a fraction of a cent in the price marks a
   wholesaler's retail fill. It predicts returns over days to weeks (about 10 basis points over a week in the
   original paper), and it is identified noisily: the published test found the rule recognises 35% of retail trades
   and signs 28% of those wrongly, 5% when the quote midpoint is used. That is the evidence-backed form of "dark pool
   footprints", and it needs one small system addition (S08).
