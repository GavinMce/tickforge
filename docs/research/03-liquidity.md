# 3. Liquidity traps

Three ideas about where other people's orders sit: stop orders beyond a level, hidden orders refilling at a price,
and big off-exchange prints. The literature supports the *existence* of all three kinds of order. It supports a
way to trade them only for the third, and only at a horizon (weeks) that is no use to a day trader. The library
builds the first two as small, controlled tests and turns the third into a measurable feature.

## 3a. Stop-loss hunting reversal at the previous day's high or low

**The claim.** Price pushes through the prior day's high (or low) to trigger the stops resting beyond it, then
reverses back inside; trade the reversal.

**What the evidence says.**
- Stop orders do cluster. Osler (Journal of Finance 2003; summary only), using stop-loss and take-profit orders at a
  large foreign exchange dealing bank, finds that both are heavily clustered at round numbers, that take-profit
  orders tend to reflect price trends while stop-loss orders intensify them, and that this explains why trends
  reverse at predictable support and resistance levels and gain momentum once those levels are crossed
  ([New York Fed staff report version](https://www.newyorkfed.org/medialibrary/media/research/staff_reports/sr125.html)). That is currency
  data from one bank, and the levels are round numbers, not previous highs and lows.
- In equities, Kavajecz and Odders-White (Review of Financial Studies 2004; abstract only) show that support and
  resistance levels coincide with peaks of depth in the limit order book, and that this comes from technical rules
  locating depth that is already there
  ([abstract](https://ideas.repec.org/a/oup/rfinst/v17y2004i4p1043-1071.html)). Again a statement about resting
  limit orders, not about stop orders, and not about the prior day's extremes.
- I found no test of "the previous day's high or low" as a stop-cluster level for single stocks, and no public data
  that would show where stops rest (stop orders are not displayed).

**Folklore.** That someone is "hunting" the stops. A price that moves through a level and returns is also what
ordinary noise and a mean-reverting book do; without order-level data the two cannot be told apart. The testable
part is the price pattern, not the motive.

**Our data.** The prior day's regular-session high and low from the reference snapshot (S04). Our feed sees only
part of consolidated volume (see 1b), so *our* high can be lower than the official high; the harness measures the
difference per name before the strategy is run. One-minute bars for the sweep.

**Testable rule (T08).** Level L is the prior regular-session high (mirror for the low).
- A sweep: a one-minute bar trades above L by at least one tick and at most 0.5% of the price, with volume at
  least 1.5 times the average of the last 20 minutes, and that bar or one of the next two closes back below L.
  Signals between 09:45 and 15:30; one trade per level per day.
- Entry: marketable limit sell at the bid at the start of the minute after the close back below L. Stop: the
  sweep's high plus one tick. Target: 1.5 R or the day's VWAP, whichever comes first; time stop 30 minutes. Longs
  mirror this at the prior low and need no borrow; shorts are easy to borrow only.
- **Control levels, the part that makes it a test.** The same detector is run on pseudo-levels in the same stock
  and day: the prior high shifted up and down by 25% and 50% of the prior day's range. If sweeps of the real
  previous high do not beat sweeps of those pseudo-levels, the "known level" idea has no support and the strategy
  is just a failed-breakout rule.

**Verdict.** Grade B for clustering of orders (FX; depth at support and resistance), D for the equity rule. Build T08
with the control levels.

## 3b. The market-maker "refill zone": heavy volume, no movement

**The claim.** When a lot trades at a price and the price does not move, a large player is absorbing the flow
(refilling the quote); that level will hold, and the next move away from it is a trade.

**What the evidence says.**
- Hidden ("iceberg") orders are real and detectable from order-level data. Frey and Sandås ("The impact of iceberg
  orders in limit order books"; abstract only): once an iceberg is detected, others respond with matching market
  orders; the more of an iceberg is executed, the smaller its price impact, "consistent with liquidity rather than
  informed trading"; an iceberg earns the most when undetected; and they build an algorithm that infers hidden
  depth from public data, using order-level messages
  ([abstract](https://ideas.repec.org/p/zbw/cfrwps/0906.html)).
- Cont, Kukanov and Stoikov (Journal of Financial Econometrics 2014; abstract only): over short intervals, price
  changes are mainly driven by order flow imbalance at the best quotes, with a linear relation whose slope is
  inversely proportional to depth ([abstract](https://arxiv.org/abs/1011.6402)). That implies the same imbalance
  moves price less in a deep book, and "volume without movement" is what a deep book looks like.
- None of this tests a trading rule built on it. And a *consolidated* top of book (what we have) adds up the
  displayed size of every venue, so a refilling quote can be many separate participants and not one hidden order.

**Folklore.** That "market makers" run the zone and that the level then holds. High volume with a small range is as
common before a break as before a bounce.

**Our data.** Trades and one-minute bars (T10 bar variant); consolidated best bid and offer with sizes from the
quote stream (T10 refill variant: successive trades at the bid while the best bid price stays put and the traded
volume exceeds a multiple of the displayed size). Real hidden-order inference needs depth messages (S11). Databento
describes the Standard plan as including one month of depth history; which depth dataset that covers, and whether
it includes Nasdaq's own order-by-order feed, I have not confirmed.

**Testable rule (T10).**
- Bar variant: a one-minute bar with volume at least 4 times the average of the last 30 minutes and a range at most
  half of the 30-minute average true range, inside the lowest (highest) tenth of the last 30 minutes' price range.
  Long (short) when a later bar trades above (below) the absorption bar's high (low) within 5 minutes. Stop at the
  other end of the absorption bar. Target 1.5 R; time stop 10 minutes.
- Refill variant: in a 30-second window at least five trades hit the same best bid (offer), the best bid price does
  not change, and their total size is at least five times the largest displayed bid size seen in the window; long
  (short) one tick above (below) the level with a stop one tick beyond it; same target and time stop.
- Scalp targets are a few ticks; the spread is one tick. Both variants are first measured on whether the *next
  five minutes' drift* after a signal differs from unconditional drift at all, before any cost is applied.

**Verdict.** Grade B for the underlying microstructure, D for the trade. Build the L1 version (T10); real icebergs
need depth (S11) and are research only until then.

## 3c. Dark-pool footprints

**The claim.** Large off-exchange prints mark institutional accumulation or distribution; the print price becomes
support or resistance, and the direction of the prints shows who is in control.

**What the evidence says.**
- Informed traders do not hide in dark pools. Zhu (Review of Financial Studies 2014; abstract only): informed
  traders crowd on one side, face higher execution risk in a dark pool, and so prefer exchanges; dark pools attract
  uninformed traders ([abstract](https://dspace.mit.edu/handle/1721.1/88124)). It is a model, but its prediction is
  that off-exchange volume is more likely to be uninformed than informed, the opposite of "smart money".
- In the public tape off-exchange prints come from alternative trading systems *and* from wholesalers and brokers
  filling retail orders; the tape does not say which. Large single prints as price levels: no test found.
- What is documented is **retail** flow. Boehmer, Jones, Zhang and Zhang (Journal of Finance 2021; abstract only)
  identify marketable retail orders from sub-penny prices on trades reported to the FINRA/Nasdaq trade reporting
  facilities (wholesalers give retail orders a fraction of a cent of price improvement) and find that stocks with net
  retail buying outperform those with net retail selling by about 10 basis points over the next week
  ([paper](https://ideas.repec.org/a/bla/jfinan/v76y2021i5p2249-2305.html)). Kelley and Tetlock (JF 2013; abstract only): both aggressive
  and passive retail net buying predict monthly returns with no reversal; only aggressive orders predict news; only
  passive buying follows negative returns, as liquidity provision
  ([paper](https://ideas.repec.org/a/bla/jfinan/v68y2013i3p1229-1265.html)).
- The identification is noisy. Barber, Huang, Jorion, Odean and Schwarz (JF 2024; abstract), who placed 85,000
  trades of their own: the sub-penny rule recognised 35% of their trades as retail, signed 28% of those wrongly, and
  gave uninformative order imbalance for 30% of stocks; signing by the quote midpoint instead cut the sign error to
  5% ([abstract](https://ideas.repec.org/a/bla/jfinan/v79y2024i4p2403-2427.html)). Battalio, Jennings, Saglam and
  Wu (working paper, "No Shortage of False Negatives and False Positives", since retitled; I saw only an abstract
  in a search result and its page has moved) find the method recognises under a third of retail trades and also
  flags institutional trades that print at sub-penny prices on the facility.
- The rule as the end-of-day paper (1c) describes it: a trade reported to the TRF with a fraction of a cent between
  0 and 0.4 is a retail sell, between 0.6 and 1 a retail buy (the text I read prints the first range as "(0,0.04)",
  an evident typo); the midpoint version signs by the quote midpoint instead.
  The horizons in these papers are days to months. The one intraday use I found is the end-of-day paper, which
  shows retail buying in the last half-hour concentrated in the day's losers, which is part of why that reversal exists.

**Folklore.** Dark pools as "smart money". The evidence points to the opposite.

**Our data.** Publishers 82 and 83 are the two Nasdaq-run reporting facilities; the NYSE-run one is not in the feed,
so we see only the retail flow that wholesalers report to Nasdaq. Trade events already carry the publisher in the upper half of their sequence number (the decoder puts it there, so
82 and 83 mark the two facilities); S08 turns that and the sub-penny test into named trade flags so strategies
do not parse the sequence. The consolidated best bid and offer gives the midpoint.
Coverage is partial and has to be measured. The decoder also drops prints of zero whole shares (about 4% of
midday trades and a fifth of premarket ones in the 2 October sample, almost all from one venue, at sub-penny prices;
`docs/DESIGN.md`); they are probably fractional-share prints, which would make them part of the retail footprint, and
S08 decides whether to keep them.

**Testable rule.** A study, then a strategy only if the study earns one. Compute for each stock and half-hour the
midpoint-signed retail order imbalance from sub-penny TRF prints; relate it, across stocks, to the return over
the next 30 minutes, to the close and to the next day, with costs from our quotes. The first strategy candidate is
the one with prior support: at 15:30, among the day's biggest losers (T04), does a positive retail imbalance in the
last hour add to the closing reversal?

**Verdict.** Grade D for "dark pool footprints", B for retail order flow as a signal at longer horizons. Replace
with the retail flow study (T11) after S08.
