# 0043. A strategy over many symbols reviews a member view on a grid of event time

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-142

## Context

Ten to twenty strategies will watch universes of up to thousands of symbols (ADR 0041). The
existing `Strategy` trait is called for every event, and each `Host` owns its own Tier 0, which is
right for one strategy on a handful of symbols and wrong here: twenty copies of Tier 0, and twenty
callbacks per event of which almost all do nothing.

## Decision

`tf-strategy::cross` adds a second kind of strategy, `CrossStrategy`, and a runner for it.

- **A periodic review, not a callback per event.** `on_review(ctx, view)` runs once per period. The
  view lists the strategy's members and reads their state, and ranks them (`top_k`, `top_by`).
  `on_member_event` exists for strategies that need ticks, is off unless the strategy says so
  (`WANTS_MEMBER_EVENTS`), and is called only for member symbols.
- **One Tier 0.** The runner owns no market state. The engine updates a single Tier 0 per event and
  passes it (and the reference rows) to every runner. The view shows members only: a non-member
  reads as nothing, so a strategy cannot see beyond its universe by accident.
- **Membership is a bitset** over dense instrument ids. A selector's changes (ADR 0041) apply to it
  directly; a stored selection is turned into one by symbol name, and names the symbol table does not
  know are reported rather than dropped.
- **Reviews fall on a grid of event time.** The first event at or after each multiple of the period
  triggers one review; a gap of several periods gives one review, not a catch-up burst. Time comes
  only from events, so a replay of the same events reviews at the same instants. A review is stamped
  with the time it was noticed, not the grid line.
- **Same safety path.** A cross strategy gets the same `Ctx` as any strategy: its intents are
  validated, numbered and stamped the same way and go to the same gateway.

## Consequences

- With every strategy watching every symbol, 20 strategies cost about 33 ns per event of routing and
  about 31 us per review of 5,000 members: roughly 1% of one core at the opening burst rate (benchmark
  `cross_load`). That is 1.6 times Tier 0's own cost per event; absolute cost is small, but "no
  measurable load" would be wrong to claim.
- The routing cost is a per-runner compare. A host that keeps one shared earliest-wake time can skip
  all runners for most events; that is left to the multi-strategy host (E18-S05).
- Strategies that want per-event logic on one symbol stay on the existing `Strategy` trait.
