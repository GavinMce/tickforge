# 0012. The risk gateway decides every order, and its limits are fixed

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-70

## Context

Strategies, and later agents, can be wrong in ways nobody has thought of. The
only protection that holds is a layer they cannot talk their way past, with
rules that are boring, explicit and tested at their edges.

## Decision

`tf-risk::Gateway` answers every intent with `Accepted(OrderId)` or a
`Rejected(RejectReason)`; nothing reaches a broker without an accepted decision.

- **Checks**: well-formedness, known instrument, kill switch, daily loss, order
  rate, close-not-larger-than-held, no opens against an opposite position,
  order notional, position size, gross notional. Sizes are measured at the
  order's limit price, so an order's worst case is what is capped.
- **Closes are special**: they skip the kill switch, daily-loss latch and size
  caps, because the response to a problem must always be able to reduce risk.
  They are still validated and still counted against the order rate. A rate limit
  can therefore slow a flatten; that is accepted, to stop a runaway loop of
  closes as well.
- **No flipping**: an open against an existing or working opposite position is
  refused, so a position changes sign only by an explicit close followed by an
  open, and short-specific checks (E09-S02) always see a fresh short.
- **Explicit, logged, metriced**: every decision is an audit entry
  (`drain_audit`) and every rejection increments a counter named by
  `reason_name`. The gateway does no I/O itself; the process that hosts it ships
  the audit log and counters.
- **Fixed limits**: `Limits` has private fields, validates that none is zero,
  and is moved into the gateway; nothing on the gateway can change it. The kill
  switch can be engaged, never released, and a daily-loss trip latches until
  `new_day` even if the price recovers. Releasing the switch means building a
  new gateway, i.e. a restart under the operator's control.
- The gateway keeps its own position book from fills reported to it and marks
  from `mark`. Daily P&L is realised plus marked, from average cost.
- The crate carries the same clippy ban list as strategies (ADR 0010).

## Limits of this decision

"Not agent-tunable" is the shape of the API plus where the limits come from.
It is not a sandbox: an agent that can edit the operator's configuration, or
that shares the gateway's process, defeats it. The gateway process (still to be
built) must run separately, with limits read from a file the agent cannot
write. Daily loss ignores costs not yet known (borrow fees, commissions) and is
only as good as the marks it is given. Short-specific checks, gap-based sizing
and persistence across restarts are later stories (E09-S02..S04).
