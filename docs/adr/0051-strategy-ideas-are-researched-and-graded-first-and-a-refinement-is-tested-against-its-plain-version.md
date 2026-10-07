# 0051. Strategy ideas are researched and graded first, and a refinement is tested against its plain version

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-154 (epic E19)

## Context

The live month (E18) is meant to run 10 to 20 strategies, and the first 24 ideas to build from came out of
day-trading teaching: trend, mean reversion, liquidity traps, scalping, reactions to news, psychology, option
data, extended hours. Reading what is published on each (docs/research) gave four facts that decide how to build.

- For most of the ideas no rigorous test exists; for a few the published evidence points the other way
  (the first hour of a stock's day carries to the close for an index and reverses for single stocks; after-hours
  trading is informed; a merger target above the offer is not a free short).
- Several ideas are *refinements* of a plain rule (the first red engulfing candle after a run; wait for the wick
  rather than the first touch; RSI 90 and then a reversal candle). A refinement can look good only because the plain
  rule is good.
- A month holds a few hundred trades, which confirms an average result only if it is large: 0.08 R a trade needs
  625 or more at a spread of 1 R; 7 basis points needs 378 at the spread the closing-reversal paper reports.
- The system lacks what the rules need: no calendar or daylight saving, Tier 0's high, low and VWAP include
  extended hours, cross strategies cannot see bars, the simulator ignores stops, nothing reads the restricted
  flag on status records, there is no way to run twelve months.

## Decision

1. **Write the research and the rule down before testing.** Each idea has its claim, the evidence with what was
   read in full and what only as abstract, what is folklore, what our data can test, a rule with few parameters and
   a verdict on a five-step grade. The grade says how good the evidence is, not whether the idea works.
2. **Test a refinement against its plain version, and everything against a null.** Variants are listed in
   advance and counted (a trial registry feeds the deflated Sharpe ratio). A level-based rule is run on control
   levels, a follow rule beside its fade, a strategy beside random entries with the same exits and costs.
3. **Look for edges on the twelve months of history the plan includes, not on the live month.** The live month
   tests plumbing, fills and the cost model and gives a forward sample nothing was tuned on. A gate (positive
   after costs, positive paired difference, t above 2, at least 300 trades) is set before and only admits a
   variant to the month; claiming an edge needs more.
4. **File what the system lacks as general stories, not as code inside a strategy.** A calendar crate with the
   daylight-saving rule and no time-zone database; session features in a *parallel* array beside Tier 0, so the
   existing conventions and golden hashes do not move; bars owned by the engine and reference-counted by
   strategy; history-derived columns in the reference snapshot; exits simulated at the broker's rules, with
   engine-held exits for extended hours where only limit orders are accepted; the restricted flag read from
   every status record; a research store, runner and statistics that use the host and the simulator the live day
   uses. Epic E19 (TIC-154) holds them with the strategies and the gate; E18-S08 now depends on the gate.
5. **Nothing is bought for the ideas that cannot be tested yet.** Depth (E19-S11) and OPRA (E19-S12) wait for a
   reason: an entitlement answer, and the equity results.

## Consequences

- More work lies between this decision and the first strategy run, and the first strategies built are the
  cheapest to test (closing reversal, the null) so that the harness is proven on something simple.
- The set that reaches the live month will be smaller than the 24 ideas, and the report will say how many
  variants were tried and what it cannot show (survivorship, today's borrow flags, the cost model, the 65% of
  share volume the feed holds).
- Ideas with no evidence stay in the library as honest tests (grade D), not as expected earners, and
  the ones the evidence contradicts (a "free" merger short, after-hours fades) are recorded and not built.
- The research is a snapshot: abstracts and summaries were relied on where a paper could not be read, and the
  documents say which. They are to be corrected, not defended, when a result disagrees.
