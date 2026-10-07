# 8. After-hours and premarket

What distinguishes this family is the trading venue, not the idea: extended hours have wide spreads and thin
volume, and the paper broker accepts a narrower set of orders there. So each idea is judged on three things: the
signal, the fill, and whether the exit can rest at the broker.

**What the broker allows** ([Alpaca order documentation](https://docs.alpaca.markets/docs/orders-at-alpaca)):
extended-hours sessions are 04:00 to 09:30 and 16:00 to 20:00 ET (and an overnight session from 20:00 to 04:00 on
Sunday to Friday nights, not in our feed). Only limit orders with time in force `day` or `gtc` are accepted in them,
with the `extended_hours` flag set. Market, stop, stop-limit, trailing stop, bracket, OCO and OTO orders are not
supported there. In extended hours a protective stop therefore cannot rest at the broker: the engine has to watch
the price and send the exit itself (S05). The simulator must refuse the same things.

## 8a. Premarket VWAP reclaim

**The claim.** In premarket, price that dips below the premarket VWAP and reclaims it is turning up; buy the reclaim.

**What the evidence says.** Nothing tests it. What is documented about the session is that it is informative but
noisy: Barclay and Hendershott (Review of Financial Studies 2003; abstract) find that the low volume outside the
regular session produces significant but inefficient price discovery, that individual trades contain more
information after hours than during the day, and that price changes before the open are larger, reflect more private
information and are less noisy than after the close
([abstract](https://ideas.repec.org/a/oup/rfinst/v16y2003i4p1041-1073.html)). A VWAP over thin volume is a weak anchor,
and the 1b caveat applies (our trades are a part of consolidated volume).

**Our data.** Premarket trades carry the extended-hours flag and the consolidated quote runs from 04:00. Our VWAP
over the flagged trades would start at 04:00 (the Tier 0 VWAP today counts every trade of the day from the reset, so it
is not the 09:30-anchored one either: S02).

**Testable rule (T03, the premarket variant).**
- Candidates: stocks with premarket dollar volume of at least $500,000 by 08:00 and a premarket gap of at least 3% from
  the prior close (both are plain thresholds because no premarket baseline exists yet; S04 can add one).
- Premarket VWAP from 04:00 over flagged trades. Between 07:00 and 09:20: price trades at least 1% below it, then a
  one-minute bar closes above it. Entry: limit at the ask. Exit: limit sell at the VWAP plus 1%, or a close below
  the VWAP minus 0.5% (engine-managed, a limit order sent at the bid), or flat at 09:25 whatever happens.
- Spread filter: no entry when the spread exceeds 0.5% of the price.
- Variants: gap threshold {3%, 5%}, with or without a minimum premarket volume of 100,000 shares.

**Verdict.** Grade D. Build as the premarket variant of T03.

## 8b. After-hours liquidity trap fade

**The claim.** Spikes after the close are thin-market traps; fade them.

**What the evidence says, against the claim.**
- Jiang, Likitapiwat and McInish (Journal of Financial and Quantitative Analysis 2012; abstract): for S&P 500 stocks
  from 2004 to 2008, after-hours trading rises on earnings days, a significant portion of the price change and of
  price discovery occurs right after the release, prices in after-hours trading are informationally efficient to a large
  degree, and the trades are mainly from informed traders
  ([abstract](https://ideas.repec.org/a/cup/jfinqa/v47y2012i06p1303-1330_00.html)). Barclay and Hendershott (above)
  agree that after-hours trades are more informative.
- Berkman and Truong (Journal of Accounting Research 2009; abstract): more than 40% of earnings announcements are made
  after hours, and the volume and price reaction appears one trading day later than the date in the standard databases
  ([abstract](https://ideas.repec.org/a/bla/joares/v47y2009i1p71-103.html)). Event timing is a data problem here as much as
  a signal problem (S10).
- For: very thin after-hours books can overshoot on a single order. No study I found measures it.

**Costs.** Spreads are wide, only limit orders, no resting stops, no overnight cover for a position held past 20:00, and a
fade of a move that is informed is a bet against the informed.

**Verdict.** Grade X for the "trap" reading, B for the idea that thin trading has costs. After the event feed (S10) and
an extended-hours cost model (S05, S13); no strategy now.

## 8c. Closing-bell liquidity grab

**The claim.** Moves in the last half-hour are liquidity grabs that reverse; trade against them.

**What the evidence says.** This is the one idea in this family with research behind it, and the research
(1c, read in full) says: across stocks, the return from the previous close to 15:00 *negatively* predicts the return
from 15:30 to 16:00, entirely through the day's losers, which rise into the close on retail buying and fewer new
shorts, then give some of it back the next day. It is gross of costs, 9.5% to 17.3% a year for a long-short quintile,
and the authors say it might not survive costs for most investors; it is stronger for more extreme losers, and the
strongest result is the bottom decile (about 400% over 27 years, in half-hours alone). Index-level evidence points the
other way (momentum into the close), which is why the strategy is cross-sectional and not a bet on the index.

**What we can and cannot see.** Trades and quotes: yes. The Nasdaq closing cross imbalance information: not in
XNAS.BASIC (the imbalance schema belongs to XNAS.ITCH). The official closing price (statistics type 11) arrives at 16:00.

**Testable rule (T04).**
- Universe: the E18 universe, price at least $5, average dollar volume above a threshold the E18 universe already
  holds, easy-to-borrow or not (long only: no borrow needed).
- Signal at 15:30: ROD3 is the return from the prior close to the last trade at or before 15:00 (the paper skips the
  half-hour in between). Rank the universe; take the twenty most negative, excluding names under a halt, a pause
  or the short-sale restriction, and any with an earnings or news flag (S10) when it exists.
- Entry: marketable limit buy at the ask at 15:30 (the paper's timing), equal weight. Exit: marketable limit sell at the bid at
  15:59:30; a position still open at 15:59:50 is closed by the host's end-of-day flatten. No stops (the holding period is 30
  minutes; the risk is capped by position size).
- Variants: the 10, 20 or 40 most negative; extreme only (ROD3 at most -3%); a spread cap (quoted spread at 15:30 at most
  5 basis points of the price); a variant that buys at 15:00 instead of 15:30 (to see whether the skipped half-hour
  matters).
- Short side: not built. The paper's own result is that the winners returned about nothing.

**Costs, said plainly.** 3.8 to 6.9 basis points a day across a quintile is about the cost of a one-cent spread on a $30
stock in and out. The bottom-decile effect is larger, and so is the effect in small stocks (the paper's six-factor alpha
for the long-short strategy is 14.7 basis points a day in the smallest fifth of stocks and 3.4 in the largest fifth), but small
stocks are where the spread costs most. The margin between gross and net is what the test measures.
With 20 names a day and about 21 days a month, a month holds about 420 trades. The paper reports a standard deviation
of 0.68% (68 basis points) for the last half-hour return of a typical stock-day; at that spread of outcomes 420 trades
give a standard error of about 3 basis points, so one month can only confirm an effect of about 7 basis points or more
(two standard errors). Twelve months of history (about 5,000 trades, standard error about 1 basis point) can confirm
about 2.

**Verdict.** Grade B. Build T04. It is the cheapest strategy in the set to run (one decision a day, long only), has
evidence at the right horizon and the right kind of instrument, and tells us the least about everything else, which is
why it is a good first test of the harness.
