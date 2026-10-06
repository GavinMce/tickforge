# 0033. The rebalance moves realised profit into the strategy that made it

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-124

## Context

Budgets are reserved shares of a live balance (ADR 0031) and change only at a rebalance after
the session. What happens at that rebalance had to be fixed: where profit and loss go, how far a
winner may run, and what a person's pending edit does.

## Decision

- **Realised profit moves with its maker.** At a rebalance each strategy's dollar budget gains the
  profit it realised since the last rebalance (or since budgets were last set), never going
  below zero. Dollars a group or the workspace holds unassigned stay as they were. The balance
  becomes the old balance plus all the profit, or the account's real balance if the broker
  reported one. Open positions and paper profit are not counted, and a smaller budget never
  forces a position to resize: it only stops new opens.
- **New shares are the new dollars over the new balance**, to the nearest basis point, so a
  rebalance with no profit and no loss changes nothing (tested over 300 random trees,
  repeatedly), and rounding can leave a sliver unassigned.
- **Bounds around targets.** The targets are the shares a person last set. A rebalance may take a
  node between half and twice its target (configurable), measured from the target and not from
  where the last rebalance left it, so a winner cannot compound without limit. If clamping
  overfills a level, the excess is trimmed one basis point at a time from the sibling furthest
  above its floor (lowest index on a tie). A node whose target is zero stays at zero.
- **A scheduled change waits.** A person's change made during the day is recorded as scheduled
  and takes effect only at the next rebalance, where it replaces the profit adjustment for that
  rebalance (the profit still changes the balance) and becomes the new targets. Withdrawing it
  is also recorded. A schedule must keep every strategy that is in force. Setting budgets
  directly clears anything scheduled and starts counting profit from there.
- **All of it is in the ledger** (`schedule`, `rebalance`, `budgets` records) and replays
  exactly; `tf ledger verify` shows a pending change.

## Consequences

- A strategy that loses repeatedly shrinks to half its target and stops there; one that wins
  doubles and stops there. Targets only move when a person (or an approved agent proposal,
  E17-S14) changes them.
- Nothing yet triggers the rebalance at the end of a session: a runner calls
  `Journal::rebalance` after the close, with the broker's balance once E17-S06 supplies it.
- Unrealised profit counts the next day if it is realised; a strategy holding a large paper gain
  overnight is not rewarded until it closes.
