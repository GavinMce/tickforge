# 0056. Protective orders in the simulator, extended-hours rules, and exits the strategy holds

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-159

## Context

The simulator ignored the stop and target on an opening intent (ADR 0011), so a backtest of a strategy that relies
on them was optimistic. Alpaca accepts, outside the regular session (04:00 to 09:30 and 16:00 to 20:00 New York
time), only plain limit orders with time in force day or good-til-cancelled: no market or stop orders, no bracket,
one-triggers-other or one-cancels-other. Yet every opening intent had to carry a stop (`Intent::validate`), so a
strategy could not open a position in the premarket at all. And exits that live at the broker cannot be the same live
and simulated.

## Decision

- **Protective orders in the simulator, when asked for** (`SimBroker::with_protective_orders`, off by default, so
  every earlier backtest is unchanged). They exist for the shares the opening order has filled so far (a partial
  fill carries through) and share one pool of shares, so a leg's fill shrinks the other and they end together. The
  *target* is a resting limit: a long's sells at the bid when the bid is at or above it. The *stop* is armed by a
  trade at or through its trigger and stays armed; it is then a market order (fills at the bid, the ask for a short,
  whatever it is: a gap fills at the gapped price) or, with a `stop_limit`, a limit order that fills only at that
  price or better, so it can be gapped through. A leg fill is a `Fill` with its `leg` set, moves the position, never
  takes it past flat, and is a `Kind::LegFill` event on the parent. Legs end with the day. A halt blocks them.
  Rejected: triggering on quotes (the broker triggers on trades).
- **One oracle for the simulator and the Alpaca mapping** (`tf_strategy::testing`): six scripted markets (a stop
  gapped through, a target that cancels the stop, partial entry fills carried into the legs, a stop-limit gapped
  through, a short, a stop never reached) with the broker events each must give. The simulator is run on the market;
  the Alpaca mapping is given the `trade_updates` frames of the same story and must translate to the same events.
  `check_events` was wrong about legs (a leg fill after the parent's last fill counted as an event after the order
  ended); it now needs only that the parent was acknowledged.
- **The extended-hours rule is one function** (`session_rules::extended_hours_refusal`), used by the simulator
  (`place` and `submit`) and the Alpaca order mapping: an intent decided in the premarket or after-hours that carries
  protective orders, or is immediate-or-cancel, is refused (status 422 and the reason), and a plain limit order in
  those hours carries `"extended_hours": true`. The session is that of the instant the strategy decided; an instant
  the calendar cannot place counts as the regular session, so no order is refused for a date nothing is known about.
  `Tif::Gtc` is added (the ledger encodes it; a good-til-cancelled order is not expired at the end of the day).
- **An open may carry no protective orders in the extended hours.** Elsewhere it still must. The risk gateway never
  read the protective orders; only `validate` insisted on them.
- **Exits the strategy holds** (`tf_strategy::ExitBook`): a stop, a target and a time exit per instrument, armed by
  the strategy when an entry fills and fed trades, timers and order updates. A trade at or through the stop or
  target, or the time (`flat_by`, from the calendar: so many minutes before the close of that day, 13:00 on an early
  close), sends a closing intent, a collar away from the price that triggered it, day by default. One exit is out per
  instrument; an exit that ends short leaves the rest to retry after a wait; a closed position is forgotten. It reads
  no clock and keeps its instruments in order, so live, simulated and replayed behaviour are the same.

## Evidence

- A premarket day through the multi-strategy host (`tf-host` `exit_tests`): two strategies enter with no protective
  orders, one leaves by its stop at the first print through it and one by the clock at exactly the instant the
  calendar gave (08:01:00, 479 minutes before the close); both are flat in the ledger; the decision logs compare
  equal over a replay, and a replay with a different stop is refused.
- Mutation run on the new code: 65 mutants (simulator legs, the extended-hours rules, the exits, the Alpaca wire
  mapping, validation, the event checker). Eleven survived at first; six tests were added and all are killed except two
  that cannot be observed (an empty bid already means no size to fill; a timer id below the book's base cannot match
  an instrument).

## Consequences

- The host's simulated broker keeps its protective orders off: the ledger cannot record a leg fill yet (E09-S11), and
  the host says so as an anomaly if one arrives. The simulator's legs are for the standalone backtest path and for the
  conformance tests; the strategies of E19 hold their exits themselves, which is what replays and what the extended
  hours need. `MomentumLong` and `TrendLong` still send brackets in the regular session, unchanged.
- The refusal messages state Alpaca's rule in its own terms, not its wire text; checking them needs paper credentials
  (E18-S11).
- Not modelled: a stop triggered by a quote, and another order closing a position whose legs still stand (Alpaca
  refuses it for want of available shares); in the simulator the legs then just have nothing to sell.
- An exit the strategy holds is only as good as the trades it sees: through a gap it fires at the first print on the
  far side, and a collar narrower than the gap leaves the order resting unfilled (the book keeps the position).
