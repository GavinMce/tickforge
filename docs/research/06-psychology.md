# 6. Psychological warfare

Three ideas about other traders' mistakes. The research here is stronger than for most of the list on the question
"do such mistakes move prices", and weaker on the question that matters, "can a rule trade them after costs". One
of the three (6c) can be tested head to head against an idea from another family (4a), which is the best test
available.

## 6a. Bagholder bounce: after a flush on a gap down of 20% or more, the first reclaim is a long

**The claim.** Holders capitulate into a big gap down; once the selling is exhausted, the stock bounces. Buy the
reclaim of the flush.

**What the evidence says.**
- Large drops are followed by rebounds in the older literature, which then fades. Bremer and Sweeney (Journal of
  Finance 1991; abstract only): very large drops in large stocks are followed on average by larger than expected
  gains for about two days ([abstract](https://ideas.repec.org/a/bla/jfinan/v46y1991i2p747-54.html)). Atkins and Dyl
  (1990) found the reversals not significant once bid-ask bounce and costs were counted, and Cox and Peterson (1994)
  found the daily-data effect had shrunk towards zero by 1991 (both as summarised in
  [Zawadowski et al.](https://arxiv.org/abs/cond-mat/0406696), 2).
- *Whether there was news matters more than the size of the drop.* Savor (JFE 2012; abstract) and Chan (JFE 2003;
  abstract): moves with news continue (drift; Chan's finding is for bad news), moves without news reverse (2). A gap
  down of 20% overnight usually comes with news (I did not measure how often); the bounce this idea wants lives in
  the rarer no-news case.
- Intraday reversal after big moves in liquid stocks, including drops, with profit after the spread on Nasdaq
  (Zawadowski et al., 2).
- Who buys: the end-of-day paper (1c) finds retail "buy the dip" purchases concentrated in the day's losers.

**Folklore.** That "bagholders" capitulate at a predictable moment. The mechanism may be real; the timing is not given
by anything I found.

**Our data.** The gap, the flush low and the reclaim are in our trades. The *news* flag is not (S10); without it the
rule cannot separate the case the evidence supports from the case it contradicts.

**Testable rule (T12).** Gap: first regular trade at or below 0.8 of the prior close, price at least $2 after the gap,
prior-day average volume at least a million shares. L15 is the low of the first 15 minutes. After 09:45, the first
5-minute bar that closes above the 09:30-to-09:45 VWAP is the reclaim. Entry: marketable limit at the ask at the
next open of a minute. Stop: L15 minus a tick. Target: the lesser of 2 R and half the gap. Time stop 11:30. Variants:
all gaps, or only gaps with no filing or headline since the prior close (S10). The pair is the test of the
news idea.

**Verdict.** Grade B for rebounds, but the news dependence is the whole question. After S10.

## 6b. Retail fakeouts: bull flags that break down first, then go

**The claim.** Retail traders buy the breakout of a bull flag; the market first breaks the flag the other way to stop
them out, then runs.

**What the evidence says.** Nothing tests flags. Lo, Mamaysky and Wang (1) used kernel regression to find some
patterns informative in daily US stock data, 1962 to 1996; a bull flag on minute bars is outside that.

**Folklore.** All of it, including that retail "buys the breakout" in a way that can be shaken out.

**Testable protocol (T13, a measurement study on the research runner, S07 and S13).** First define the pattern so a machine can find
it: a pole of at least 5% within 15 minutes on at least twice normal volume; a flag of 3 to 10 one-minute bars that
retraces at most half of the pole with falling volume; the breakout level is the flag's high, the flag's height is
R. Then count, over twelve months, what follows within 15 minutes: breakout to +1 R before -1 R; a failed breakout
(trade above the flag's high, then below the flag's low); a *shakeout* (trade below the flag's low by at least
0.25 R and then above the flag's high). The claim predicts that the shakeout is common and followed by gains; the
control is the same count for other consolidations of the same height. No strategy is written until the counts
say there is something to trade.

**Verdict.** Grade D. After the research runner (S13); measure first.

## 6c. The 9:45 reversal: the opening move reverses by about 9:45

**The claim.** Whatever the open does in its first 15 minutes, the market reverses it around 9:45.

**What the evidence says.**
- For stocks that attract attention, the opening is priced high relative to the rest of the day: Berkman, Koch,
  Tuttle and Zhang (4a; abstract) find positive overnight returns followed by intraday reversals in stocks that
  recently drew retail attention, hard-to-value and costly-to-arbitrage stocks and in high-sentiment periods
  ([abstract](https://ideas.repec.org/a/cup/jfinqa/v47y2012i04p715-741_00.html)). Lou, Polk and Skouras (JFE 2019;
  abstract): across 14 well-known strategies, profits are earned either entirely overnight or entirely intraday, and
  usually with opposite signs ([abstract](https://ideas.repec.org/a/eee/jfinec/v134y2019i1p192-213.html)).
- Against it: for stocks in play, the first five minutes' direction *continues* to the close on average (1c).
- Nothing singles out 9:45. Heston, Korajczyk and Sadka (2) find intraday return patterns that repeat at the same
  clock time on following days; that is not a reversal at 9:45.

**Folklore.** The clock time.

**Our data.** All from trades and the prior close.

**Testable rule (T05, the open-attention gap fade).**
- Candidates: the stocks T01 would trade (price above $5, opening-range relative volume at least 100%, the top 20
  of them by that ratio) with an opening gap of at least 2% either way.
- At 09:45: if the stock gapped up and still trades at least halfway between the prior close and the open, sell short
  (marketable limit at the bid). If it gapped down and still trades at least halfway below, buy. Stop: the
  day's high (low) so far plus (minus) a tick, rejected if more than 3% away. Target: the prior close. Time stop 11:30.
  Shorts easy to borrow only.
- Variants, eight: entry clock {09:45, 10:30}; gap threshold {2%, 5%}; with or without a requirement that at the
  entry time the price is back inside the stock's first 5-minute range (the opening drive has stalled).
- The test: on the same stock-days T01 and T09 take the other side. The library keeps the three results side by
  side on the same days, so that "follow" and "fade" are compared with the same stocks, the same costs and the same
  gap sizes.

**Verdict.** Grade B for the attention reversal, D for the 9:45 clock. Build T05, as the opposite bet to T01 and T09.
