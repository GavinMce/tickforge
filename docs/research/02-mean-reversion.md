# 2. Mean reversion

The one piece of real evidence for this whole family is a reversal after sharp intraday moves in liquid
stocks (Zawadowski, Andor and Kertész, below). It is old (2000 to 2002), small in sample, and it is strongest on
the *buy-after-a-drop* side. The shorting ideas in this family sit on the weaker side of that evidence and carry
the borrow and squeeze risks. The library therefore builds a plain version of the effect and tests the user's
refinements (engulfing candle, RSI 90, wait for the 2-minute reversal candle) against it.

## The evidence common to 2a and 2c

- **Intraday reversal after large moves, liquid stocks.** Zawadowski, Andor and Kertész, "Short-term market
  reaction after extreme price changes of liquid stocks" (arXiv preprint, 2004; read in full): NYSE and Nasdaq
  TAQ data for 2000 to 2002, stocks above $10, the most liquid names (101 to 144 Nasdaq stocks passed their
  filter). An event is a move of at least about 4% within 60 minutes that is also at least 8 times the stock's
  normal volatility for that time of day; the first 5 and last 60 minutes of the session are excluded; one event
  per stock per day and at least 60 minutes apart. They find "significant reversal for both intraday price
  decreases and increases" over the next 30 to 60 minutes. Buying at the ask at the end of the move and selling at
  the bid an hour later earned 1.6% on Nasdaq after 60-minute drops (t = 3.87) and 2.69% after 120-minute drops
  (t = 4.02, 159 events); still 2.15% when the buy comes two minutes late. On the NYSE the spread widened at the
  event and most of the profit disappeared. Their caveats: only the best bid and ask were used, sizes are small,
  and "further studies should examine the exact profitability, if any ... taking into account all other costs".
  Only the *buy after a drop* side is costed in their profitability table
  ([paper](https://arxiv.org/abs/cond-mat/0406696)).
- **Why it exists, and why it is hard to harvest.** Heston, Korajczyk and Sadka (Journal of Finance 2010;
  abstract only) show that short-term return reversal "is driven by temporary liquidity imbalances lasting less
  than an hour and bid-ask bounce"
  ([abstract](https://www.kellogg.northwestern.edu/academics-research/research/detail/2010/intraday-patterns-in-the-cross-section-of-stock-returns/?p=1)).
  Nagel (Review of Financial Studies 2012; abstract only): reversal returns are compensation for liquidity
  provision and are large when the VIX is high
  ([abstract](https://nber.org/papers/w17653)). Part of what a reversal rule "earns" is the spread it would
  have to pay, so every test below is run with fills at the quote, not at the last trade.
- **Longer horizons agree on direction but not on size.** Bremer and Sweeney (1991) found abnormal rebounds
  over about two days after very large drops in large stocks; Atkins and Dyl (1990) found such reversals not
  significant once bid-ask bounce and costs were taken out; Cox and Peterson (1994) found the effect had faded
  to almost nothing by 1991 (summaries in [Zawadowski et al.](https://arxiv.org/abs/cond-mat/0406696) and
  [IDEAS](https://ideas.repec.org/a/bla/jfinan/v46y1991i2p747-54.html); I did not read these three).
- **After a drop with news, prices tend to keep going; with no news, they tend to reverse.** Savor (Journal of
  Financial Economics 2012; abstract only): "price events accompanied by information are followed by drift, while
  no-information ones result in reversals" ([abstract](https://ideas.repec.org/a/eee/jfinec/v106y2012i3p635-659.html)).
  Chan (JFE 2003; abstract only): strong drift after bad news, reversal after extreme moves with no news, monthly
  horizon, mostly in smaller and less liquid stocks
  ([abstract](https://ideas.repec.org/a/eee/jfinec/v70y2003i2p223-260.html)). Our data has no news (story S10), so
  for now we can only say "a move" and not "a move with no news", which is the cleaner reversal case.
- **Lottery-like winners underperform, at a monthly horizon.** Bali, Cakici and Whitelaw (JFE 2011; abstract
  only): stocks with the highest maximum daily return over the past month earn over 1% a month less than the
  lowest ([abstract](https://ideas.repec.org/a/eee/jfinec/v99y2011i2p427-446.html)). This is why the short side
  is plausible and also why it is slow: the evidence is about weeks, not minutes.

## 2a. Broken parabolic short: five or more green one-minute candles, then the first red one that engulfs

**The claim.** After 5+ green 1-minute candles in a row, the first red candle that engulfs the last green one marks
the top; short it.

**What the evidence says.** The reversal after a sharp run is documented for liquid stocks over 30 to 60 minutes
(above), with the profitability result stated for buying drops, not for shorting run-ups. Nothing I found tests an
engulfing candle: the only candlestick study is the daily-bar test in 1b, which found no value. The parabolic names
the claim usually has in mind (small float, big relative volume, price under $10) are outside the evidence: the
paper uses liquid stocks above $10.

**Folklore.** That an engulfing candle marks the top; that the odds are "high". A run of five green minutes is
common in any active stock, and a short entered into it has unbounded risk.

**What shorting adds, with sources.**
- *Borrow.* D'Avolio (JFE 2002; abstract): specials and recalls are rare on average, and their incidence rises with
  the divergence of opinion among investors, which is exactly the parabolic stock
  ([abstract](https://ideas.repec.org/a/eee/jfinec/v66y2002i2-3p271-306.html)). Alpaca (the paper broker) has so far
  allowed shorts only in easy-to-borrow names (5,000+, $0 borrow fee through the Trading API). It announced hard to
  borrow shorting with a locate API on 24 June 2026: a locate is quoted and requested per symbol in round lots,
  costs a fee that is not refunded if unused, expires, and is single use
  ([Alpaca](https://alpaca.markets/blog/htb-trading-api-locates/)). Our simulator has neither locates nor a
  borrow-fee model (story S06).
- *Short sale restriction.* SEC Rule 201: once a stock trades 10% or more below the prior close, short sales on
  that day and the next may only be made above the national best bid
  ([summary](https://www.wilmerhale.com/en/insights/publications/the-return-of-a-short-sale-price-test-sec-adopts-alternative-uptick-rule-in-split-vote-february-25-2010)).
  Our rule skips a name while the restriction applies, and the broker model must reject a sell at or below the bid
  for such a name (S06).

**Our data.** One-minute bars from trades (the engine has them for per-symbol strategies; cross strategies need S03),
the relative volume baseline (S04), the easy-to-borrow flag (already in the reference snapshot from the Alpaca assets
list, E18-S02; it has not yet met the real endpoint, E18-S10), and exits in the simulator (S05).

**Testable rule (T06).**
- Universe: the E18 universe, easy to borrow, price at least $10 (the paper's floor; a $5 run is the second variant
  only if the first shows anything).
- Run: N consecutive one-minute bars with close above open and close above the previous close, N at least 5, the
  run starting after 09:45 and the signal before 15:00; total gain over the run at least 3%; the run's volume at
  least 3 times the average volume of the same number of minutes at the same time of day (S04).
- Trigger: the next bar closes red (close below open) and engulfs (open at or above the prior close, close at or
  below the prior open). Entry: marketable limit sell at the bid at the start of the next minute; skip if the quote
  moved more than a quarter of the engulfing bar's range against us.
- Stop: the run's high plus one tick, held by the engine (S05). Size so that the stop risks
  0.25% of the strategy budget; skip if the stop is more than 2% away. Target: half of the run's gain given back.
  Time stop: 30 minutes. Flat by 15:55. One trade per name per day.
- Variants, four in all: N in {5, 8}; trigger in {engulfing red bar, first bar to close below the previous low}. The
  second trigger is the plain version: if it does as well as the engulfing, the candle shape adds nothing.

**Verdict.** Grade B for the reversal (liquid stocks, 2000 to 2002, buy side costed), D for the candle rule. Build
T06 for easy-to-borrow names only, with the stop and size caps above. Needs S03, S04, S05, S06.

## 2b. The "fake halt" trap: a spike that looks like a halt, with no halt, short immediately

**The claim.** When a stock spikes as if it were going to be halted and no halt follows, the move has no follow
through; short it at once.

**What I could establish about halts (primary sources, read in full).**
- The mechanism, from the [LULD Plan annual report for 2025](https://cdn.luldplan.com/reports/LULD-2025-Annual-Report.pdf):
  bands are a percentage either side of a reference price, the mean price of eligible trades over the prior five
  minutes (updated after 30 seconds only if it would move at least 1%). If the national best bid equals the upper
  band, or the best offer the lower band, a *limit state* starts and lasts 15 seconds; if the quote is not
  executed or cancelled within that time there is a 5-minute *trading pause*. Percentages by previous close: above
  $3, 5% for Tier 1 (S&P 500, Russell 1000, some ETPs) and 10% for Tier 2 (everything else); $0.75 to $3, 20%;
  below $0.75, the lesser of $0.15 or 75%; doubled for Tier 1 in the last 25 minutes. The report's sentence on which
  Tier 2 prices are doubled then is unclear, and it does not say how the first 15 minutes are treated, so S09 takes
  both from the plan text.
- Frequency: in 2025 there were 97,195 limit states and 10,763 trading pauses, so 11.1% of limit states became
  pauses (11.7% in 2024). About 89% of limit states resolve on their own. About 65% of pauses were in Tier 2
  stocks priced at $3 or more, a group that holds 41% of symbols.
- What happens after, from the SEC staff study of the pilot (Moise and Flaherty, March 2017, data from 2012 to
  2014; [paper](https://www.sec.gov/files/dera-luld-white-paper.pdf)). Most limit states reverse within 15
  seconds, over 90% within 5; the exchanges label most of them "liquidity gaps", though the authors caution that
  the label is used unevenly. After a trading pause, 83% (first phase) and 87% (second phase) of Tier 1 pauses
  were followed within a minute of the reopening by a price within 5% of the price before the first pause; for
  Tier 2 (tested against 10%) it was 46% in the second phase, with another 53% of Tier 2 pauses following low-volume
  periods and analysed separately. Price continuation, which the authors read as a fundamental move, followed 17%
  of Tier 1 pauses in the first phase and 1% of Tier 2 pauses in the second.

**What this means for the claim.** Spikes that hit the band are mostly short-lived liquidity gaps, and prices mostly
return afterwards: that is *consistent with* a fade. It is not evidence that a retail-speed short at the band earns
money: the limit state lasts at most 15 seconds and mostly under 5, our order path (engine to Alpaca over HTTPS) is
probably slower than that for the part that matters (to be measured, not assumed). The study's data are from the
pilot period and the plan has been amended since (the reopening process, for one). And a "spike with no halt" that
never reaches the band is not covered by any of this.

**Folklore.** That the trap has a base rate you can bank on; no source gives one.

**Our data.** XNAS.BASIC has the consolidated best bid and offer and our slice of trades, and a status schema with
trading status changes. LULD *pauses* are in it (action "pause", reason 50): on 2 October 2026, the one day I
pulled for the whole market (about 3 cents), there were 34: 25 lasted 5 minutes, 7 about 10, one 15 and one 25 minutes;
the first started at 09:30:15. LULD *bands* and *limit states* are not: the statistics schema for AAPL, TSLA and SPY that day held
only the opening price (type 1) and the closing price (type 11), though the schema defines upper and lower price
limit types. Computing the bands needs the reference price, which is built from *all* trades in the stock, and we
see a subset, so any computed band is an estimate; S09 measures how good by comparing with the pauses in the status
feed.

**Testable rule.** Study first, in history (S07 and S13 with S09): from consolidated best bid and offer, mark every moment
the computed band is reached and then record what the price does 5, 15, 60 seconds and 5 minutes later, split by
whether a pause followed. Only if the fade survives the *quote* at second resolution is a strategy written, and it
is then a Tier 2, $3-plus, ETB-or-long-side-only rule.

**Verdict.** Grade C for the facts, D for the trade. After S09; study first. No strategy in the first set.

## 2c. RSI exhaustion: RSI above 90 or below 10, then the first 2-minute reversal candle

**The claim.** RSI at an extreme means exhaustion; wait for the first 2-minute candle against the move and trade
the reversal.

**What the evidence says.** For RSI thresholds on minute data I found no rigorous test. RSI at 90 on 14 one-minute
bars means the average up move over the lookback is nine times the average down move: it is another way of saying
"a steep, nearly unbroken run", which is the same situation as 2a. The documented effect behind both is the
reversal after large intraday moves (above). Wilder's RSI is a smoothing of ups and downs; there is nothing in
it beyond what a return and a volatility measure already say, so the plain version is the Zawadowski event.

**Folklore.** The specific thresholds, and the improvement from waiting for the 2-minute reversal candle.

**Our data.** One-minute bars and, from them, 2-minute bars (S03); relative volume (S04) for the event baseline.

**Testable rule (T07).** RSI(14) with Wilder smoothing on 1-minute closes from the same session (no signal before
09:45 and none after 15:00). Short when RSI is 90 or above and the next completed 2-minute bar closes red; long
when RSI is 10 or below and the next 2-minute bar closes green. Entry by marketable limit at the start of the next
minute; stop beyond the extreme of the move plus one tick; target half of the last 30 minutes' move; time stop 30
minutes; flat by 15:55. Shorts: easy to borrow only, with the 2a caps. Longs need no borrow and are the better
supported side.
- Variants: threshold {90/10, 85/15} by entry {the first 2-minute reversal bar, immediately at the extreme}: four.
- The baseline is the plain Zawadowski event: a move of at least 4% in at most 60 minutes and at least 8 times
  the stock's normal 60-minute volatility for that time of day, price at least $10, entry at the end of the move,
  exit 60 minutes later or at the stop. If the baseline is as good as the four RSI variants, the RSI adds nothing.

**Verdict.** Grade D for the RSI rule, B for the baseline. Build T07 as the baseline plus four variants. Needs S03, S04, S05.
