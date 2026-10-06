# 0046. The host runs many strategies through one gateway and stops one without stopping the rest

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-144

## Context

Ten to twenty strategies are to run on one day's live data. Each needs its own money and its own limits;
a bug or a bad day in one must not stop or hurt the others; and nothing should join the day that has not
been run through the same machinery first.

## Decision

`tf-host` is one engine step per market event for all strategies, in a fixed order: the brokers see the
event and what they did goes into the ledger and to the owning strategy; marks; Tier 0 and the shared
promoter; each running strategy and each dynamic universe; the intents through the gateway to the
strategy's broker; once a second the loss limits and any flattening still under way.

- **Sub-accounts are the budget tree.** A strategy's intents carry its number, and the gateway (ADR 0031
  onward) already holds a budget, loss limits and positions per strategy. A strategy with no node in the
  tree is not admitted.
- **A failing strategy is stopped alone.** A panic in any callback is caught at the host boundary and
  stops that strategy; a soft loss limit stops its events and reviews and cancels what it has resting;
  a hard limit or an operator's kill also closes what it holds. Closing is done by the host with
  marketable limit orders (within 10% of the last price) through the same gateway and ledger, numbered apart
  from the strategy's own, repeated each second until nothing is left. What a stopped strategy left in its
  queue never reaches the gateway.
- **The kill switch is the gateway's.** It refuses opens; the host also cancels what is working to open.
  Strategies keep running so they can exit.
- **Admission by certificate.** `certify` replays a strategy, built by the same builder that will run it
  live, over a tape through a scratch host with the same limits and budgets and simulated brokers, and
  issues a sealed `Certificate` over the strategy's fingerprint (number, name, parameters, universe,
  priority, route), the tape's id, counts and an outcome hash. The host admits only a certificate that is
  intact, for exactly this strategy as configured, from a replay of enough events. The fingerprint covers
  configuration, not code: a rebuilt binary with the same text is the same strategy to the host.
- **Holds come from the ledger.** A strategy holds a symbol in Tier 1 while it has a position or any
  order working in it, including an order whose placement got no answer (it may exist). The host tells
  the promoter; it is not the strategy's job to remember.
- **The ledger is told, not forced.** A broker's event that the order book will not take is counted and
  noted and the host goes on; only a failure of the ledger itself stops the host.

## Consequences

- One panic costs one strategy; the others' decisions and counts are unchanged (shown by running them
  with and without the broken one).
- A certificate is evidence that a strategy ran through this machinery on that tape without a panic,
  a stop or a ledger refusal. It says nothing about profit.
- Caught panics print to the process's stderr through the default hook; a deployment may want its own.
- The paper broker path is built and tested against a simulated broker and a fake transport; running it
  against Alpaca is E18-S11.
