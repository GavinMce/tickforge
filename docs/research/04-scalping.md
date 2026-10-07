# 4. Scalping

Three ideas that trade over seconds to a few minutes. At that horizon the spread, not the signal, decides most
of the outcome, so every test here is first run on a question that costs nothing to answer: does anything
happen after the signal that does not happen otherwise? Only then is a cost applied.

## 4a. "1-minute rip and dip": the first one-minute candle breaks the premarket high, the next dips, then reclaims

**The claim.** A stock whose first regular-session minute breaks the premarket high is in play. If the next
candle dips and then reclaims, buy the reclaim.

**What the evidence says.** Nothing tests this pattern. The nearest relatives point in different directions:
- Continuation: Zarattini, Barbon and Aziz (1c) trade in the direction of the first 5-minute candle, only in stocks
  with high opening relative volume, and hold to the close. That is a breakout rule on exactly the stocks this idea
  watches, over a longer horizon.
- Reversal: Berkman, Koch, Tuttle and Zhang (Journal of Financial and Quantitative Analysis 2012; abstract) find a
  strong tendency for positive overnight returns followed by reversals during the day, "driven by an opening price
  that is high relative to intraday prices", concentrated in stocks that recently attracted retail attention and more
  pronounced for stocks that are hard to value and costly to arbitrage; the extra implicit cost for retail traders
  buying high-attention stocks near the open "frequently exceed[s] the effective half spread"
  ([abstract](https://ideas.repec.org/a/cup/jfinqa/v47y2012i04p715-741_00.html)). A stock breaking its premarket
  high at the open is the textbook high-attention case.

**Folklore.** The pattern as a named setup, and the claim that the dip is a shakeout that "flushes weak hands".

**Our data.** The premarket high needs premarket trades. The live driver is to start the feed at 04:00 (E18-S09), and trades carry an
extended-hours flag, so the premarket high is the highest flagged trade before 09:30 (it includes the Nasdaq venues
and two trade reporting facilities only; the harness measures how often our premarket high differs from the
consolidated one). Opening-minute relative volume needs the first-minute baseline from S04.

**Testable rule (T09).**
- Candidates: the E18 universe, price at least $5, in play by relative volume (first-minute volume at least twice its
  20-day average, the baseline from S04).
- Setup: PMH is the premarket high. The first regular-session one-minute bar B1 trades above PMH by at least one tick
  and closes above it. Within the next five minutes a bar trades down to at least 40% of B1's range below B1's
  high and no lower than PMH; then price trades above B1's high: that is the reclaim.
- Entry: marketable limit buy at the ask on the reclaim. Stop: the dip's low minus one tick. Target: 2 R. Time
  stop: 15 minutes. One trade per name per day; nothing after 10:30.
- Variants, four: with or without the relative-volume filter; entry on the reclaim or a resting limit at the middle of
  B1's range. The tests that matter: the no-filter variant against the filter, because that says whether "in play"
  is doing the work, and the result against T05 (6c), which takes the *opposite* side on the same stocks.

**Verdict.** Grade D, with a warning from B evidence that the setup is costly. Build T09, run head to head with T05.

## 4b. Scalping a big hidden bid on Level 2, low-float stocks

**The claim.** A large hidden (reserve) bid refilling at a price lets you buy just above it with a stop just below
and little risk.

**What the evidence says.**
- Order book imbalance does predict price over seconds: Cont, Kukanov and Stoikov (3b) find a linear relation between
  order flow imbalance at the best quotes and short-horizon price changes. That is a statement about visible
  quote changes across many stocks, not about a hidden order in one.
- Detecting icebergs from order-level messages is possible (Frey and Sandås, 3b), and traders respond once they do,
  which is the point: a hidden bid that is found stops being an edge.
- Support at depth peaks (Kavajecz and Odders-White, 3a).

**Our data.** None of this is in a top-of-book feed. A trade larger than the displayed size at the best bid is
a *signature* of a non-displayed order and is in our data (T10 refill variant, 3b), but it is a signal for study,
not a scalping system. Float is not in our reference data at all (E14), so "low float" cannot be a filter, only
a proxy such as price and dollar volume. Depth needs XNAS.ITCH or similar (S11), whose inclusion in the Standard plan I
could not confirm, and it shows only Nasdaq's own book, so a hidden bid on another venue is invisible.

**Verdict.** Grade B for imbalance as a short-term predictor, D for the trade. After S11, research only. T10 carries
the part that is testable now.

## 4c. Options chain as a stock signal: big calls bought at the ask

**The claim.** Large call purchases at the ask ("spoofing" in the user's list) show where informed traders expect
the stock to go; trade the stock in that direction.

**What the evidence says.**
- A word on the label: trades executed at the ask are real trades, not spoofing. Spoofing is entering orders to cancel.
  What is meant is the direction of aggressive option flow.
- Pan and Poteshman (Review of Financial Studies 2006; abstract): put-call ratios built only from option volume
  initiated by buyers to *open* new positions predict the stock: low-ratio stocks beat high-ratio stocks by more than 40
  basis points the next day and more than 1% over the next week on a risk-adjusted basis, more so where informed
  traders are concentrated and for more leveraged contracts
  ([abstract](https://www.nber.org/papers/w10925)). Their data are CBOE records that say whether a trade opens or
  closes a position and who initiated it, which public trade data do not.
- With public data the open/close flag is missing and the aggressor is inferred from the quote, so the measured
  signal is a noisier relative of the published one; I found no study of how much weaker.

**Our data.** None: options come from OPRA, which is a separate product (S12). Even with it, the sign and the opening
flag are not given.

**Verdict.** Grade B for the idea in its published form, with the data caveat. Research (S12), not a strategy.
