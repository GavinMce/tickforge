# 0047. A day is checked by replaying its capture and comparing decision logs

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-145

## Context

The live day cannot be trusted just because it ran. The same events, through the same host, must give
the same decisions; if they do not, something outside the events (a clock, a counter, a thread, a
difference between the live path and the replay path) is deciding. A raw capture of the day (ADR 0040)
makes the check possible after the fact.

## Decision

The host can keep a **decision log**: text, one record a line, with a count at the end so a cut file is
noticed. It records what the host decided, not what it saw: each intent and the gateway's answer, each tier
change, each fill, and each action by hand, with the event number and time.

The check **replays by recomputation**: a fresh host, built like the live one, takes the events again and
does the live log's actions at the events they were done at; then the logs are compared record by record. The
first difference is reported with the record, event, time of day and symbol. A replay of a raw capture reads
it twice, the first time to learn the instrument ids (numbered in the order first seen, as live) and names,
the second to stream the events, so a whole day is never held in memory. The id space and the symbol
numbering are compared first; a replay that numbered instruments differently is not a replay that decided
differently.

What the check cannot say, it says: strategies that traded on a paper broker had fills the replay can
only simulate, so they are named; and the live ingest queue (ADR 0039) may have given up events the
capture holds, so when the replay differs and the live run dropped something, the report says that this
may be why, and that the engine-input tape is the thing to replay.

A strategy given for the replay must have the fingerprint it had live; a changed one is refused rather
than compared.

## Consequences

- Equality is by construction for pure strategies, so the check's value is in failing: it caught a
  strategy that decided on a counter outside the events, a missing event, a changed price and a missing
  operator action, each at the right record, time and symbol.
- A follower promoter fed the tape holds, after every event, the same symbols as the live one (tested);
  it can differ only inside the callback that asked for an eviction. Replay by recomputation has no such
  gap, so the limit noted in ADR 0044 does not touch this check.
- Inputs the log does not carry (a budget edit, a rebalance) are not replayed, so a day with them will
  show up as a difference; they join the log when they are driven through the host.
- Scheduling it nightly and putting its report in the daily report belong to operations (E18-S09) and the
  report (E18-S07).
