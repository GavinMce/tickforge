# 0081. Entry triggers the strategy holds, and the budget a strategy can read

- Status: Accepted
- Date: 2026-10-11
- Jira: TIC-205, TIC-206

## Context

Two things stood between the library and its first regular-session strategies (`docs/research/engine-for-the-library.md`, gaps 3 and 4). An entry at a break ("buy
when the price trades through the high of the opening range") is a stop-entry: the broker takes none before the open, a stop resting at the broker is a different order from the one a
simulation models, and each strategy would write the same trade watch. And a strategy could not see its budget, so "risk 1% of the budget over the distance to the stop" could not be
written; every strategy bought a fixed number of dollars.

## Decision

- **`EntryBook`** (`tf-strategy::entries`), the exit book's twin. The strategy arms an `EntryTrigger` for an instrument and a side (`Buy`, which fires at a trade at or above the level, or
  `SellShort`, at or below it): level, size, collar, time in force, the reason code, the protective stop and target to send (fixed when it is armed, since the regular session requires a
  broker-side stop on an opening order), `not_before` and `lapse_at`. It forwards trades and timers; at the first trade through the level the book sends a marketable limit a collar away from
  *that trade's* price, once. A name can have a buy and a short sale armed together (the two sides of a range) and the strategy disarms the other when one fires.
  - Never fires before `not_before`, nor at or after `lapse_at` (a timer drops it when no trade comes, and says which); arming again replaces; it reads no clock.
  - `on_trade_checked` asks the strategy's own check before sending (a name already held, no room): a refusal there, or by the intent's validation, spends the trigger and is counted
    as refused. A trigger is never kept to try again: a trade is still through the level and the strategy re-arms if it still wants the trade.
  - Reasons 0xE5xx are the books'; a trigger carries the strategy's own.
- **`Ctx::budget()`** gives `BudgetView { total, used }` (raw units, `left()` is the difference, never negative) from a `BudgetSource` the host passes in the `Market` for every callback that has one (a
  review, a member event, a timer, a tier-1 revocation; an order update has no market and reads none). The gateway is the source: `used` is `strategy_charge`, the number its budget check
  uses (each position at the higher of cost and mark, a short charged with the gap rule's assumption, and the working opening orders). It is asked for only when the strategy asks, so a
  strategy that never does decides as before; the golden hashes hold. Without budgets in force, or for a strategy not in the tree, it is `None`.
- **`BudgetView::shares_for_risk(risk_ppm, stop_distance, price)`**: the shares that lose `risk_ppm` millionths of the *whole* budget over the stop distance, no more than the room that is
  left holds at the price. Fewer as the budget is used (the room shrinks), none when nothing is left, none for no distance or no price. For a short sale the caller passes the price times the
  gap rule's assumption, since a short is charged more than its notional; the gateway still enforces the budget exactly.

## Consequences

- T01 and T09 can be written without their own trade watch or a fixed dollar size; the host's Market now has a budget field (it is `None` in the tests and benches that build one).
- `Tier0View` borrows the reference (ADR 0080), which is bigger now.
- The book does not track the order it sent after it is sent: the strategy keeps its own account of an entry (it is told by the order updates) and arms the exit book when the entry fills.
