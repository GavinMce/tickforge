# 0035. Budget edits are requests in an inbox that the engine records

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-132

## Context

The budget editor (E17-S13) is the first screen that changes anything. The ledger has one writer, the
engine, and holds a lock while it runs (ADR 0030), so the workspace service (ADR 0034) cannot append
to it. A scheduled change must still end up in the ledger, as a recorded input, so that replay
reproduces what the gateway enforced (ADR 0031/0033).

## Decision

- **The service writes requests, not the ledger.** A person's edit becomes one small file in
  `inbox/` beside the ledger (`tf_ledger::inbox`): written whole under a number no earlier request
  has had, never changed afterwards. The service never opens `ledger.log` for writing; a test
  compares its bytes before and after.
- **The engine records them.** At a point it chooses (before the rebalance), the engine calls
  `inbox::apply`, which checks each request again against the budgets in force and what each
  strategy has in use at that moment, records it with `schedule_budgets`, and removes the file. A
  request that no longer fits is kept as `.rej` with the reason and not retried. With no engine
  running, `tf ledger apply-inbox DIR` does the same (it takes the lock, so it refuses while an
  engine writes). Nothing calls `apply` from an engine yet: there is no live engine (E09).
- **One rule, in one place.** `tf_budget::check_edit` says whether an edit is allowed: the same
  groups and strategies (an edit moves shares and loss limits; it adds and removes nothing), a
  valid tree, and no strategy left with less budget than it has in use (one already over budget
  may not be cut). The per-node ranges the sliders show come from the same module, and a random
  test holds the two together: every value inside a range passes the check, one step outside
  fails it. The page does not know the rules; on every edit it asks the server
  (`POST /api/budgets/preview`) for the allowed ranges with the reasons, and for the changes in
  dollars. Scheduling checks the draft again, and so does the engine.
- **Writing is narrow.** Three POST routes only (`preview`, `schedule`, `withdraw`), each needing
  sign-in and the header `X-Requested-With: workspace` (a page on another site cannot send it;
  the cookie is also SameSite=Strict). Every other method on every other route is still 405. The
  page has one helper that sends a non-GET request and a test pins it to those three paths.
- **A scheduled change takes effect at the next rebalance**, as before (ADR 0033): requesting a
  change moves no money and stops no order. The editor says so, lists what is waiting, and can
  ask for what is scheduled to be cancelled.

## Consequences

- There is a delay between "requested" and "scheduled" until an engine applies its inbox. The
  overview says how many requests are waiting.
- Requests carry no identity beyond "the workspace app (shared token)" until E17-S17 gives the
  service per-user sign-in.
- Event time of an applied request is chosen by whoever applies it (the engine's clock, or the
  operator's wall clock for the command): a person asked at a real time, and replay needs only
  that the recorded time is recorded.
