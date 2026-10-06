# 0045. Brokers speak one interface, and their events are checked by one rule

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-77

## Context

The simulated broker (E08-S06) and the Alpaca adapter (E09-S05) were built separately and did not
share an interface: the simulator invented its own order ids and reported through order updates; the
adapter took the gateway's order id and reported through the ledger's events. A multi-strategy host
(E18-S05) has to run against either, and the ledger has to be able to take what either says.

## Decision

`tf-strategy::broker` defines `Broker`: `place(intent, order)`, `cancel_order`, `observe` (a market
event), `close_day`, `take_events`. The order id is the gateway's, so the events a broker returns
(`BrokerEvent`: acknowledged, filled for so many shares at what price, closed as cancelled, expired or
rejected) go to the ledger unchanged.

A placement has four outcomes, taken from what Alpaca does: accepted, refused (with a code and the
broker's words), rate limited (try again after a time; the order did not happen), and unknown (no usable
answer; the order may exist). An unknown placement is neither retried under a new id nor closed: it
stays working until the event stream or a reconciliation settles it.

`check_events` is the one rule for what a broker may say, applied to every implementation: an
acknowledgement comes before any fill, expiry or cancel-after-ack; a rejection comes before any
acknowledgement, because an accepted order cannot become rejected; nothing follows an end; fills are of
at least one share and never exceed the order; time does not go backwards. A complete fill ends an order
without a close event.

The simulator gets a `FaultPlan` so a host can be shown to cope with failure: refusals, rate limits,
no-answer placements (alternate ones arrive anyway) and rejections by the venue, chosen by the count of
placements, so a run is reproducible. It keeps no event log unless a placement came through the
interface, so existing backtests cost nothing more.

## Consequences

- The Alpaca tracker now reports a rejection without an acknowledgement before it. Acknowledging first
  left the ledger holding an accepted order that could never close; found by running the simulated
  broker's rejection through the real ledger.
- The Alpaca side is tested against a fake transport and fixtures only. Real placements and the stream
  arrive with the transport (E09-S10).
- Protective-leg fills have no home in the ledger yet (E09-S11).
- Rejected: one trait for both the order path and the market data path in a single call; `observe` is a
  separate method because a real broker has no use for it and a simulator cannot work without it.
