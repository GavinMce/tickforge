# 5. Trading the reaction

Three ideas about what happens after a scheduled or announced event. The honest summary: the first has about eight
observations a year, the second has two bodies of evidence that disagree, and the third is not what it says.
What this family needs from the system is an event calendar (S10), which is also useful to every other strategy
(for example, not opening a breakout five minutes before the Fed).

## 5a. FOMC fade

**The claim.** The first move after the FOMC statement (14:00 ET) overshoots; fade it.

**What the evidence says.**
- The documented FOMC effect is a drift *before* the announcement. Lucca and Moench (Journal of Finance 2015;
  abstract): the S&P 500 rose on average 49 basis points in the 24 hours before scheduled announcements, measured
  from 14:00 the day before to 14:00 on the day, and that accounts for over 80% of the equity premium over the 17 years
  they study ([paper](https://ideas.repec.org/a/bla/jfinan/v70y2015i1p329-371.html)). Kurov, Wolfe and Gilbert
  (Finance Research Letters 2021; abstract) extend the sample to December 2019 and find the drift all but gone
  after 2015 ([abstract](https://ideas.repec.org/a/eee/finlet/v40y2021ics1544612320315956.html)).
- Fading the *reaction* has one study I could find, Baglioni and Ribeiro, "The FOMC Announcement Reversal"
  (SSRN, August 2022): trade the E-mini S&P 500 from 13:50 against the sign of the previous 24 hours' return, close
  at the end of the day, buying at the ask and selling at the bid, October 1997 to January 2020, 180 announcements.
  I could not read the results (only a summary of the design was open to me), so it is evidence that someone tested
  it, not evidence of the outcome.
- Claims that the 14:30 press conference reverses the statement's first reaction are common in market commentary; I
  found no study I could open that measures it, and do not use them.

**Folklore.** That the fade "always" works, or the size of the move.

**Sample size.** There are eight scheduled meetings a year (2026: January 27 to 28, March 17 to 18, April 28 to 29,
June 16 to 17, July 28 to 29, September 15 to 16, October 27 to 28, December 8 to 9;
[Federal Reserve](https://www.federalreserve.gov/monetarypolicy/fomccalendars.htm)). Twelve months of included history hold at most
eight events and the live month one. No result from that can be told from chance. The question can only be asked of
a longer history (the dataset starts in July 2024, which adds about ten more, July 2024 to September 2025, at
pay-as-you-go prices).

**Our data.** SPY and QQQ trade and quote data are in XNAS.BASIC (the consolidated best bid and offer; our trade
slice is only part of volume). The calendar is public and needs no feed.

**What to build.** The calendar and a *risk* use of it: strategies can ask "is a scheduled announcement within N
minutes" (S10). A fade strategy is not written; a single study on SPY, with the Baglioni and Ribeiro design as the
protocol, is filed (T12) and sized to say "no evidence" honestly.

**Verdict.** Grade C for the drift (and it has faded), D for the fade. Study only.

## 5b. Earnings overreaction reversal ("80% fade")

**The claim.** Earnings gaps overreact and fade; the claim usually quoted is that about 80% do.

**What the evidence says, in two groups that disagree.**
- *Continuation.* Post-earnings-announcement drift was the classic continuation result, but Martineau (Critical
  Finance Review 2022; abstract) finds that prices now reflect earnings surprises fully on the announcement day and that for large
  stocks drift has been non-existent since 2006
  ([abstract](https://ideas.repec.org/a/now/jnlcfr/104.00000122.html)). Savor (JFE 2012; abstract): large price moves
  with information (such as an analyst recommendation change) are followed by drift, those without by reversal
  ([abstract](https://ideas.repec.org/a/eee/jfinec/v106y2012i3p635-659.html)). In the opening range paper (1c) the stocks
  in play (the paper's list of causes starts with earnings reports, warnings and surprises) *continue* in the
  direction of their first five minutes: an average of 0.08 R a trade above 100% relative volume and 0.38 R above
  30 times.
- *Reversal.* Berkman et al. (4a): positive overnight returns reverse intraday, in high-attention stocks.
  Zawadowski et al. (2): intraday moves in liquid stocks reverse over the next hour (2000 to 2002).
- *The 80%.* The number I found comes from retail-education pages about gaps filling, with no data or method given.
  The academic result closest to "earnings moves reverse" is Ben-Rephael, Hitzemann and Xiao, "Mind the Gap: The
  Non-Fundamental Role of Earnings Days" (December 2024; I read the abstract and introduction): about 50% of the
  earnings-day return associated with their "return-earnings gap" (the market's reaction relative to the size of
  the surprise) reverses afterwards, and "strikingly slow, taking about three years"
  ([paper](https://haslam.utk.edu/wp-content/uploads/2024/11/Ben-Rephael-Paper.pdf)). That is neither 80% nor a
  same-day fade.

**Our data.** Prices, yes. *Which* names report, on which day and before or after the market, is not in our data
(S10: SEC 8-K Item 2.02 filings carry an acceptance time stamp, which is the latest moment the market could have known; the
press release itself, from a wire, can be earlier). The SEC's fair-access limit is 10 requests a second with a declared
user agent, and it offers a JSON submissions API
([SEC](https://www.sec.gov/search-filings/edgar-search-assistance/accessing-edgar-data)).

**Testable protocol (T12).** On every reporting day in the twelve months of history, for stocks above $5 and one
million shares of average volume: the gap (open against prior close), the first-5-minute volume ratio, and the return from
the first 5 minutes to the close, to 11:00 and to the next day, by gap size and direction. Two pre-registered rules on the
*same* events: follow (T01 on these stocks only) and fade (T05's logic on these stocks only). The data decide which sign.

**Verdict.** Grade C/D, mixed. After S10; study first, no strategy until it reports.

## 5c. Merger arbitrage "scalp": short the target when it trades above the deal price

**The claim.** In a cash deal the target cannot be worth more than the price, so a target trading above it is a free
short.

**What the evidence says.**
- Mitchell and Pulvino (Journal of Finance 2001; abstract), 4,750 mergers from 1963 to 1998: returns to risk
  arbitrage resemble those of selling uncovered index put options; after transaction costs the excess return
  is about 4% a year
  ([paper](https://ideas.repec.org/a/bla/jfinan/v56y2001i6p2135-2175.html)). The payoff is small, steady, and loses
  badly in a market crash, which is what a short put does. The same applies, mirrored, to *shorting* the target: the gain
  is capped at the spread to the price and the loss is not.
- A target above the offer is the market saying a higher bid may come. Competing bids are rare (95% of bids in the
  sample studied by Betton and Eckbo were single-bid contests, summary), but when they come they produce large jumps
  ([summary](https://ideas.repec.org/a/oup/rfinst/v13y2000i4p841-82.html)). That is a fat right tail for a short.

**What shorting this costs, specifically.** Targets are crowded with arbitrageurs, so borrow is often scarce; a short can be
recalled (D'Avolio, 2a); the stock can be halted on news with the stop unable to execute; and the position is a bet
against the one event that decides the stock.

**Folklore.** "Free". It is a small expected gain with a large tail.

**What to build.** Nothing. The event feed (S10) can mark names as "under agreed offer" so other strategies avoid them,
which is the use that has value.

**Verdict.** Grade X: the payoff is not free. Do not build.
