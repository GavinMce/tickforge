# 7. Option data

All three ideas need options data, which we do not have. Databento sells it as a separate dataset (OPRA: consolidated
trades and best bid and offer across the US options exchanges, with history from 28 March 2023 on a one-day delay,
and a pass-through of exchange licence fees on a month-to-month basis; the price was not on the pages I could read, and
my earlier note of about $199 a month plus licence fees has not been reconfirmed
([Databento](https://databento.com/blog/opra-data))). The plan this project is buying (Standard, US equities) does
not, as far as I could tell, include it. These ideas are therefore a research track (S12) with an explicit decision point: buy one month of OPRA
for a study, or not, once the equity strategies have shown what a month can and cannot tell us.

What is documented is the *mechanism*: option market makers hedge in the stock, and that moves prices. What is not
documented is a retail-speed rule that trades the mechanism.

## 7a. Gamma squeeze ignition

**The claim.** Heavy buying of short-dated calls forces dealers (who sold them) to buy stock to hedge, which pushes
the stock up and forces more hedging; spot the ignition and ride it.

**What the evidence says.**
- Hedging short gamma means trading with the price move. Baltussen, Da, Lammers and Martens (Journal of Financial
  Economics 2021; abstract): across more than 60 futures from 1974 to 2020 the return in the last 30 minutes is
  positively predicted by the return in the rest of the day, and they link this to the gamma hedging demand of option
  market makers and leveraged ETFs ([abstract](https://pure.eur.nl/en/publications/hedging-demand-and-market-intraday-momentum/)).
  Index and futures level, not single stocks.
- Barbon and Buraschi, "Gamma fragility" (working paper; abstract): large dealer gamma imbalances in illiquid markets
  go with intraday momentum (negative imbalance) or reversal (positive), more so in the least liquid underlying
  securities, and with more flash-crash-like events, using a large panel of equity options to build a stock-level
  imbalance proxy ([paper](https://abarbon.com/papers/gamma-fragility)).
- Ni, Pearson, Poteshman and White (Review of Financial Studies 2021; abstract): option market maker hedge rebalancing
  affects stock return volatility and the probability of large price moves, through a channel unrelated to information
  ([abstract](https://ideas.repec.org/a/oup/rfinst/v34y2021i4p1952-1986..html)).

**What is missing.** The dealers' position cannot be seen; the studies assume customers are net long options and
dealers net short, and build the imbalance from open interest by strike and signed volume. For a given single stock
on a given day that assumption can be wrong. No source gives an ignition rule or its payoff.

**Needs.** Per-contract open interest (OPRA statistics) and signed trades by strike and expiry, live; a delta and gamma
calculator (Black-Scholes with the quoted implied volatility; the Standard equities plan has no volatility feed).

**Testable protocol (S12 study).** Events: single names whose call volume in a 15-minute window is at least 10 times
its 20-day average for that window and at least 70% of it traded at or above the ask. For each, the stock's return over
the next 5, 30 and 60 minutes and the next day against the same names at other times and against names with
equal call volume traded at the bid. Then, and only then, a rule.

**Verdict.** Grade B for the mechanism, C for the idea. Research (S12).

## 7b. Max pain Friday fade

**The claim.** On expiry Friday the stock is pulled towards the strike at which option holders would lose the most
(the "max pain" strike); fade moves away from it.

**What the evidence says.**
- Pinning is documented. Ni, Pearson and Poteshman (Journal of Financial Economics 2005; abstract): closing prices of
  optionable stocks cluster at option strikes on expiration dates, shifting the average stock's return by at least 16.5
  basis points on each expiration date, with delta-hedge rebalancing by option market makers and manipulation by
  firms' proprietary traders among the causes
  ([abstract](https://scholars.hkbu.edu.hk/en/publications/stock-price-clustering-on-option-expiration-dates-3/)). Golez and
  Jackwerth (JFE 2012; abstract): S&P 500 futures are pulled to the at-the-money strike on days when serial options on
  the futures expire, and pushed away from it right before index options expire, on the order of $115 million of
  notional per expiration day ([abstract](https://ideas.repec.org/a/eee/jfinec/v106y2012i3p566-585.html)).
- Max pain itself has one academic test I could find: Filippou, Garcia-Ares and Zapatero, "No Max Pain, No Max Gain: A
  Case of Predictable Reversal" (SSRN, 2022; from a search summary of the abstract, the page itself was not open to
  me): US stocks and options 1996 to 2021; a long-short strategy opened a week before expiration and closed at
  expiration earned an average of 0.4% a week, strongest in small, illiquid stocks
  ([SSRN](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=4140487)). That is a weekly horizon, gross, and in the
  names that are most expensive to trade.
- Pinning is pulled to *nearby high-open-interest strikes*; the max pain strike is a different object, and a pin to
  the at-the-money strike is not the same as a move to the minimum-payout strike.

**Needs.** Open interest by strike for each expiry, from the previous day (OPRA statistics), and the quotes to compute the
payout curve; one monthly expiry (the third Friday) and, for names with weekly options, four or five Friday expiries in a month.

**Testable protocol (S12 study).** For weekly expiries on liquid names: the max pain strike from the prior day's
open interest; the share of the move from Thursday's close, and from 15:00 on Friday, towards it, against the nearest
strike by open interest and against a random strike at the same distance. A strategy only if the pull is visible in
liquid names after the spread.

**Verdict.** Grade B for pinning, C for max pain. Research (S12).

## 7c. Open-interest strike fakeouts

**The claim.** Price pokes through a strike with a lot of open interest and fails; trade the failure.

**What the evidence says.** Nothing tests it. The nearest evidence is pinning (7b), which says price tends to *stay near*
such strikes on expiry day, not that breaks fail. Dealer hedging at a strike can either dampen moves (long gamma) or
add to them (short gamma), per the gamma papers above, so the sign depends on a position nobody sees.

**Needs and protocol.** As 7b, plus intraday: strikes within 1% of price with the highest open interest; for each
poke through the strike by at least a tick, the proportion that closes back inside in 5 minutes, against the same
count at strikes with low open interest and at round prices that are not strikes.

**Verdict.** Grade D. Research (S12), the last of the three.

## What would be bought, and when

A month of OPRA would serve all of 4c and 7a to 7c at once. The Standard equities month (E18) tells us whether the
equity side produces any edge worth conditioning on options data. The decision is made after the equity month, on its
results, and is a separate story (S12), not part of the first build.
