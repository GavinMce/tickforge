# 1. Ride the trend

## 1a. 100 EMA on the 1-hour chart as the bias, trade only that way on the 1-minute or 5-minute

**The claim.** Forget fast crosses (9/21). Use the 100-period EMA of the hourly chart as the direction; take
trades only in that direction on the 1- or 5-minute chart. "High-timeframe bias is king."

**What the evidence says.**
- Moving-average rules have a long, contested record. Brock, Lakonishok and LeBaron (1992) tested
  moving-average and trading-range-break rules on the Dow Jones index from 1897 to 1986 and found buy signals
  earned more than sell signals, with less volatility
  ([abstract](https://ideas.repec.org/a/bla/jfinan/v47y1992i5p1731-64.html)). Sullivan, Timmermann and White
  (1999; I read the introduction, the out-of-sample section and the conclusion of the
  [discussion paper](https://www.fmg.ac.uk/publications/discussion-papers/data-snooping-technical-trading-rule-performance-and-bootstrap))
  put nearly 8,000 rule variants through a data-snooping correction on the same index: the best rules held up
  over 1897 to 1986, but in the next ten years (1987 to 1996) "the best performing trading rule is not even
  statistically significant", and on S&P 500 futures from 1984 there was no evidence that any rule beat the
  benchmark. Park and Irwin's survey of 95 modern studies counts 56 positive, 20 negative and 19 mixed, and says
  most are open to data snooping, ex-post choice of rules and weak treatment of risk and costs
  ([abstract](https://ideas.repec.org/a/bla/jecsur/v21y2007i4p786-826.html)).
- Moving-average results that survive are mostly on daily or longer data, on indexes, before costs. I found
  no rigorous test of "hourly EMA as the bias for minute-chart entries on single stocks".
- Lo, Mamaysky and Wang (2000) compared daily returns of a large number of US stocks, 1962 to 1996, with the
  returns that followed technical patterns (head and shoulders, double bottoms and so on) and found that
  "several technical indicators do provide incremental information and may have some practical value"
  ([abstract](https://www.nber.org/papers/w7613)); that is daily data, not intraday.

**What is folklore.** That 100 beats 9/21; that the hourly average "is king". Both are untested claims about a
parameter; with enough parameters something always looks king in-sample.

**Our data.** An EMA over 100 hourly closes needs about 15 trading days of history if hourly bars are
regular-session only (6.5 bars a day). The engine's hourly bars start on the UTC clock hour (09:00 to 10:00 New York
time in summer, not 09:30 to 10:30), keep 120 closed bars per symbol, start empty with the process, and the module has
no time-zone database (a fixed offset the caller must change at a daylight-saving change). So the starting EMA has to
come from the reference snapshot (computed offline from one-minute history), the bars have to be aligned to the
session, and both need a calendar (S01, S03, S04).

**Testable rule.** Define the *RTH-hourly series*: bars 09:30-10:30, 10:30-11:30, ... 14:30-15:30, and the
half-hour stub 15:30-16:00, each bar's close the last trade price in it. `EMA100` is the usual exponential
average (alpha 2/101) over that series, seeded from the first close of a 30-session history, so offline and
live compute the same number. Bias is *up* if the last completed hourly close is above the EMA, *down* if below,
none until 10:30. Bias is a **filter** applied to entries, not an entry: the same entry (T01 breakout, T03 VWAP
reclaim, T02 pullback) is run with and without it, which is the only way to tell if the filter adds anything.
T02 is the standalone version: with bias up, after a 5-minute pullback of at least k ATR(5m) from the recent
high, long on a 5-minute close above the previous 5-minute high; stop under the pullback low; target 1.5 R or
2 R; flat by 15:55; mirrored for bias down. k in {0.5, 1.0}, target in {1.5, 2.0}: four variants, no more.

**Risks and costs.** A lagging filter on a day-trade entry removes some good trades and some bad ones: the
test is whether expectancy per trade rises net of the trades lost. Hourly bars give 6 to 7 updates a day, so the
bias barely changes within a day; it is mostly a measure of the last few weeks' trend.

**Verdict.** Grade D. Build as a filter plus one standalone entry (T02), tested against its own unfiltered
version. Needs S04 (history state), S03 (bars for cross strategies).

## 1b. VWAP bounce, the "smart" way: a long wick on high volume with no follow-through, not the first touch

**The claim.** Do not buy the first touch of VWAP. Wait for a long wick, high volume and no follow-through:
that is smart money stepping in.

**What the evidence says.**
- VWAP is the most widely used institutional execution benchmark, so orders cluster around it by construction;
  that part is true. Whether price reacts to it is a separate empirical question I found no academic test of.
- Zarattini and Aziz, "VWAP: The Holy Grail for Day Trading Systems" (2023), trade QQQ intraday long above VWAP
  and short below it. For 2 January 2018 to 28 September 2023 they report a total return of 671%, a Sharpe ratio
  of 2.1 and a 9.4% maximum drawdown, "net of commissions", against 126%, 0.7 and 37% for buy-and-hold
  ([page](https://concretumgroup.com/volume-weighted-average-price-vwap-the-holy-grail-for-day-trading-systems/)).
  That is a trend-following use of VWAP on an index ETF, not a bounce strategy on stocks. The page I read gives no
  entry and exit mechanics beyond "long above VWAP, short below", no bar size and no slippage assumption.
- Candlestick patterns, which is what "a long wick" is: Marshall, Young and Rose tested candlestick strategies
  on the Dow's component stocks from 1992 to 2002, positions held ten days or less, against bootstrapped
  random prices and concluded that candlestick analysis has no value; I read the abstract of
  the author's thesis version
  ([thesis](https://mro.massey.ac.nz/items/edc6c1cf-63d9-4c94-a13d-92ffe1a53481/full)); the journal article
  (Journal of Banking and Finance 30(8), 2006) was behind a paywall. Daily bars on large stocks, not minute bars.
- Pattern-statistics books and sites (Bulkowski and others) are the usual source for "X% of patterns fail"
  claims. I found no described method for survivorship or look-ahead, so I treat them as descriptive.

**What is folklore.** "Smart money steps in at the wick." Absorption by large players is real in principle
(see 3b); a candle shape does not identify it.

**Our data.** Trades give VWAP exactly (the engine already has an anchored VWAP with standard-deviation bands).
Our trades come from three Nasdaq exchanges (publishers 81, 88 "Nasdaq Texas", 89 PSX) and the two Nasdaq-run
trade reporting facilities (82, 83). Nothing from NYSE, Cboe, IEX or the NYSE-run reporting facility, so our VWAP
is not the consolidated VWAP. On 2 October 2026 the day's bars of XNAS.BASIC held 65% of the consolidated share volume
and 69% of the dollar volume (against the consolidated daily summary; why it is not all of it is not known;
`docs/DESIGN.md`). How far our VWAP is from the consolidated one, name by name, has not been measured; the research
harness must measure it (stories S07 and S13).

**Testable rule.** Session VWAP anchored at 09:30. Take names that are in play (relative volume, see T01) and
have been above VWAP for the last 10 one-minute bars (mirror for short). *First-touch variant:* a 1-minute low
within one tick of VWAP, then a 1-minute close above it: long. *Wick variant:* the touching 1-minute bar has a
lower wick of at least 60% of its range and volume at least 1.5 times the average of the last 20 bars, and the
*next* 1-minute bar closes above the wick bar's high: long. Stop: one tick below the wick low (first-touch:
below the touch low). Exit: 2 R, or VWAP plus one standard deviation, or 30 minutes, whichever first. Trades per
variant per day capped at 3 per name.

**Verdict.** Build both variants (T03). The comparison *is* the test of the claim. Grade D for the wick; C for
VWAP as a trend filter on an ETF.

## 1c. First-hour trend lock: whatever the stock does in the first 30 to 60 minutes, stick with it

**The claim.** The trend set in the first 30 to 60 minutes holds; do not countertrade it.

**What the evidence says, and it splits three ways.**
- **Index level: supports a form of it.** Gao, Han, Li and Zhou ("Market intraday momentum", Journal of Financial
  Economics 2018; summary only): on the S&P 500 ETF from 1993 to 2013, the return from the previous close to
  10:00 predicts the return in the last half-hour, and the same holds for ten other ETFs; it is stronger on
  volatile, high-volume, recession and macro-news days. Holding the ETF in the last half-hour when the first
  half-hour was positive and cash when it was negative earned 6.3% a year gross, against -0.5% for holding
  every last half-hour
  ([summary](https://alphaarchitect.com/2014/08/attention-prop-traders-the-first-half-hour-of-trading-predicts-the-last-half-hour/)).
  That is the *last* half-hour, not "the day", and it is an index.
- **Single stocks, end of the day: the opposite.** Baltussen, Da and Soebhag, "End-of-Day Reversal" (April 2025,
  read in full): across individual stocks, the return from the previous close to one hour before the close
  (overnight, first half-hour and middle of the day: "ROD3") *negatively* predicts the return in the last
  half-hour, with the half-hour in between skipped so that bid-ask bounce cannot explain it. Sample from 1993;
  t-statistics above 10. A long-short quintile portfolio earns between 3.78 and 6.86 basis points a day (9.5% to
  17.3% a year), *gross*, depending on weighting and price filter; the equal-weighted version, which gives
  smaller stocks more weight, is the stronger. It comes entirely from the stocks that were down: the
  bottom decile gained about 400% over 27 years in the last half-hour alone, the top decile about nothing. The
  authors tie it to retail "buy the dip" purchases and to short sellers opening fewer new shorts before the
  overnight, and say that with frequent rebalancing "the strategy as presented might not be exploitable by many
  investors after accounting for transaction costs", though more extreme losers show a stronger effect
  ([paper](https://www3.nd.edu/~zda/EOD.pdf)). The authors reconcile the two: "individual stock (and
  market) returns display momentum in the time-series but due to strong cross-stock autocorrelations display
  end-of-day reversal in the cross-section".
- **Opening range breakout (ORB) on "stocks in play": strong, practitioner, partly costed.** Zarattini, Barbon and
  Aziz (2024, read in full): all US-listed stocks from 2016 to 2023 (about 7,000). Trade the direction of the
  first 5-minute candle (a doji gets no trade) with a stop order at its high or low; stop loss 10% of the 14-day
  ATR from the entry; exit at the close; risk 1% of equity per trade, leverage capped at 4; commission $0.0035 a
  share. *All stocks:* total return 29% over eight years, Sharpe 0.48, maximum drawdown 13%. *Only stocks whose
  first-5-minute volume is at least 100% of its 14-day average, the top 20 by that ratio each day* (price above
  $5, 14-day average volume at least 1M shares, ATR above $0.50): total return 1,637%, Sharpe 2.81, alpha 35.8% a
  year, beta 0.00, maximum drawdown 12%, hit ratio 48.4% (the S&P 500's in the same table is 54.9%, so I read it
  as a share of days). Average trade -0.02 R below 100% relative volume, 0.08 R above it, and 0.38 R above
  30 times. **The opening-range length matters a lot: 1,637% for 5 minutes, 272% for 15, 21% for 30 and 39% for
  60**, so the "30 to 60 minutes" in the claim is the part of this family that did *not* work, and the authors
  write that the reason for the 5-minute version's advantage "is unclear". The best stocks (among them NVDA,
  AMD and TSLA) had per-trade win ratios of 17% to 24%: a low-hit-rate, large-winner strategy
  ([paper](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=4729284)).

**What is folklore.** "Don't get cute" is not a finding. The first-hour direction of a *typical* stock carries to
the close only weakly; what carried in the best study was relative volume (news), and the shortest range.

**Our data.** First-5-minute volume against its own history needs the intraday volume baseline (S04). The
opening range comes from our trades. The official open (the Nasdaq opening cross) is in the feed as a statistics
record (type 1, seen for AAPL at 09:30:01 on 2026-10-02) and could replace "first trade after 09:30" if we want
parity with how data vendors define the open.

**Costs, the part to be careful about.** The paper's stop is 10% of ATR: for a stock with a $3 ATR that is 30
cents. A round trip crossing a 1 to 2 cent spread, plus slippage on a stop order that fills worse than its
trigger, is a visible fraction of one R. The paper charges commission only. That is why the first thing the
harness does with this strategy is replay it through our simulator, which crosses the spread and delays orders.

**Testable rule (T01).** Exactly the paper's: filters as above; direction by the first N-minute candle (doji:
no trade); entry stop order at the range high/low from minute N; stop loss 10% of ATR14 from the fill; exit at
15:58 by a marketable limit; sizing by 1% risk with our budgets. Variants: N in {5, 15}; stop 10% ATR or
20% ATR; with or without the 1a bias filter. Eight variants, no more. The "first-hour lock" form (direction at
10:30 held to the close, no range-break entry) is a ninth, run only for comparison.

**Verdict.** Build T01. It is the best-documented idea on the list. Grade C.
