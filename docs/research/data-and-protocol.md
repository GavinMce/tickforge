# Data, what one month can tell us, and the test protocol

## 1. The data we have

All of this is Databento, dataset XNAS.BASIC ("Nasdaq Basic with NLS Plus": top of book for all Nasdaq exchanges and the
trade reporting facilities). Facts below are from the metadata service (free) on 6 October 2026 unless a link is given.

| | |
|---|---|
| Venues in it | publishers 81 Nasdaq, 88 Nasdaq Texas, 89 Nasdaq PSX, 82 and 83 the Nasdaq-run trade reporting facilities (Carteret and Chicago), 93 a consolidated pseudo-publisher for the consolidated quote schemas |
| Coverage | on 2 October 2026 its daily bars held 65% of the consolidated share volume and 69% of the dollar volume (`docs/DESIGN.md`); why it is not all of it is not known; the likely cause is trades on other exchanges and the NYSE-run reporting facility |
| Not in it, as far as I can tell | trades from other exchanges; depth; sale conditions on trades; float, short interest, borrow, news, earnings dates, LULD bands and limit states; options |
| Schemas | trades, tcbbo (trade with the consolidated best bid and offer), cmbp-1 (every consolidated top-of-book change), cbbo-1s, cbbo-1m, ohlcv-1s, -1m, -1h, -1d, definition, statistics, status |
| History | 1 July 2024 to now |
| Statistics | opening price (type 1) and closing price (type 11) only; seen on real data for AAPL, TSLA, SPY |
| Status | trading, pre-open, halts, pauses with reason (LULD pause is reason 50), short-sale restriction changes: 34 LULD pauses on 2 October 2026 |
| Plan | Standard, $199 a month: live data with no exchange licence fees; 12 months of L0/L1 history across 12 schemas; 7 years of OHLCV; one month of L2 (MBP-10) and L3 (MBO, imbalance) history ([Databento](https://databento.com/blog/introducing-databento-us-equities)); it names XNAS.BASIC among the datasets and does not say whether XNAS.ITCH is one of them |
| Also exists | XNAS.ITCH from May 2018 (mbo, mbp-10, imbalance...), OPRA.PILLAR (options; trades and one-minute quotes from April 2013, top of book and trades-with-quotes from March 2023), EQUS.SUMMARY (daily bars) |

**Size of a day (1 October 2026, whole market, 04:00 to 24:00 ET), and what it would cost if bought by the day at pay-as-you-go
prices.** Included in the plan, these are free while subscribed; the number tells what is realistic to store and to replay.

| schema | records | pay-as-you-go |
|---|---|---|
| trades | 82.4 M | $22.09 |
| tcbbo | 82.4 M | $36.82 |
| cmbp-1 | 567 M | $50.73 |
| cbbo-1s | 67.1 M | $20.00 |
| cbbo-1m | 4.5 M | $1.35 |
| ohlcv-1s | 26.4 M | $16.53 |
| ohlcv-1m | 2.76 M | $1.73 |
| ohlcv-1h | 0.13 M | $0.20 |
| ohlcv-1d | 12.9 k | $0.02 |
| status | 54 k | $0.03 |

Twelve months of trades is about 21 billion records and of cmbp-1 about 143 billion. They can be pulled but not
replayed in full, and need not be: the protocol below uses minute bars to screen and full events only around signals.

The plan's twelve months are a *rolling* window. Anything wanted from the first months has to be pulled and kept locally
while it is still included.

Options, for later (7): pay-as-you-go prices measured the same way for 1 October 2026, one name (AAPL): option trades
205,716 records, $2.57; one-minute quotes $0.20; open interest and other statistics $0.19. All options statistics for one
day: 124 M records, $81. A study over many names and months must use whatever history the OPRA subscription includes;
that is not known yet.

## 2. What one live month can tell us, and what it cannot

A month holds about 21 trading days. For a strategy making one trade a day per name on a basket of 20 names, about 420
trades; for the opening range breakout (a handful of names, one trade each) fewer.

How many trades to see an average result at two or three standard errors (n = (z × sd / mean)²):

| effect | per-trade spread | trades for z = 2 | for z = 3 |
|---|---|---|---|
| 0.08 R (the opening range paper) | 1 R (a floor: winners are large and rare, so likely more) | 625 | 1,407 |
| 0.08 R | 2 R | 2,500 | 5,625 |
| 7 basis points (closing reversal, strong end) | 68 basis points (paper, last half-hour) | 378 | 850 |
| 3.8 basis points (closing reversal, paper's weakest) | 68 | 1,281 | 2,882 |
| 2 basis points | 68 | 4,624 | 10,404 |

Trades on one day are not independent (a market move hits all of them), so the real counts are larger.

What the month is good for, and the protocol treats as its purpose:
1. **The plumbing under the real feed.** Drops, lag, reconnects, the engine keeping up in the opening burst, capture and the
   replay check on real days (E18-S09).
2. **Fills and order behaviour.** Rejections, rate limits, partial fills, how long an order takes, how the paper account
   differs from the simulator. Paper fills are optimistic; the daily report says so.
3. **Calibrating the cost model.** Recorded quotes against the simulator's assumptions, so the history results use
   costs that a real month agrees with.
4. **A forward sample that nothing was tuned on.** Every signal is logged even when not traded, so the month is a true
   out-of-sample test for the effects the history finds, though too small to confirm a small one.

What it cannot do: tell a 5 basis point edge from none, or rank ten variants by P&L.

## 3. The protocol

1. **Write the rule down first.** Every strategy's rule, its variants and its parameters are fixed in these documents and in
   the backlog before its results are seen. The variants are listed in each strategy's section and the trial registry (S14)
   counts them; adding a variant later adds to the count.
2. **Test the refinement against the plain version.** Whenever an idea is a refinement (engulfing candle, wait for the wick, RSI 90
   plus a reversal candle, the premarket filter), the plain rule is run on the same events and the *difference* is what is reported,
   as a paired comparison.
3. **Compare with doing nothing clever.** T14 is a null strategy: entries at random times on random names of the same universe
   with the same exit rules and the same costs, repeated many times. Its distribution is what a strategy's result must beat. It also
   shows what the exit rules and the costs alone do.
4. **Costs are the quote.** Buys at the ask and sells at the bid of the recorded consolidated quote, after a delay (an assumption
   until the live month measures it), regulatory fees on sales at their published rates, borrow fees from the broker's table
   (easy to borrow: none at Alpaca), no price improvement and no queue position assumed. Orders larger than the displayed size
   are partially filled. In extended hours only limit orders and no resting stops (8).
5. **Statistics that match the data.** Per-trade results in R and in basis points, aggregated by day (trades on one day share
   the market's move); the standard error from day-level results or a block bootstrap by day, not from trade counts. A new
   effect needs more than 2: Harvey, Liu and Zhu (Review of Financial Studies 2016) review 316 published factors and argue
   that, given how many have been tried, a new one needs a t-statistic above 3
   ([abstract](https://papers.ssrn.com/abstract=2513152)). With the count of variants known, results are also reported as a
   *deflated* Sharpe ratio, which corrects for choosing the best of many trials and for non-normal returns (Bailey and
   López de Prado, Journal of Portfolio Management 2014; [abstract](https://papers.ssrn.com/abstract=2460551)); the
   reason is spelled out in Bailey, Borwein, López de Prado and Zhu (Notices of the AMS 2014): the more configurations tried,
   the greater the probability that the best is an overfit ([summary](https://scholarworks.wmich.edu/math_pubs/40/)).
6. **Two stages.** Screening on one-minute bars (ohlcv-1m for fills, cbbo-1m for the spread), cheap enough to run every variant
   over twelve months; then an event-level replay (trades and the quote) only for the symbol-days around the signals of the
   few variants that pass, which is what turns a bar-level estimate into a fill-level one.
7. **Point in time.** The reference snapshot is built only from bars up to the as-of date (E18-S02), so history runs see what the
   morning would have seen. Two things in history are *not* point-in-time and bias towards the strategies: the easy-to-borrow
   flag (today's list applied to the past, so historical shorts look easier than they were) and survivorship (whether the
   history holds names that were later delisted has to be checked in S07 before it is trusted). Both are reported with every
   result they touch and the live month is the check.
8. **Hurdles before the live month, set now.** The gate into the live month is deliberately lax, because the month is
   cheap and is itself a forward test: over the twelve months, a variant's per-trade result is positive after costs, its
   paired difference from the plain version is positive if it is a refinement, its day-level t-statistic exceeds 2, and it
   traded at least 300 times. Passing the gate is not a finding. *Claiming an edge* needs a t-statistic above 3 and a
   positive deflated Sharpe ratio given the number of variants tried. Nothing is promoted *on* the live month: its report
   states the number of variants tried and ranks nothing on P&L alone (E18-S08).
9. **Stop rules.** A variant whose live behaviour differs from the history's prediction for the same days (different signals,
   fills worse than the cost model by more than a stated margin) is stopped and examined before more is learned from it.

## 4. What this implies for the build order

The history harness (S07, S13 and S14) comes before any strategy is promoted, because it is the only place an edge can be seen, and the
foundations it needs (calendar, session features, bars and history state for cross strategies, protective orders, shorting
realism) come before that. The strategies that need an event feed (earnings, news, FOMC: S10), depth (S11) or options (S12)
wait for those.
