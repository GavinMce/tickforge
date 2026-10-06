# 0048. The daily report is a plain reading that says what was not measured

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-146

## Context

At the end of each session of the month someone has to be able to see, without reading logs, what each strategy
did and whether the system itself behaved. Two failure modes matter more than the arithmetic: a report that
makes simulated results look real, and one that fills in what nobody measured.

## Decision

`DailyReport` is built from the host's state and from `SystemInputs` the live driver provides. It reads and
does not decide; the same host and inputs always give the same text.

- **It opens by saying the fills are not real.** Each strategy is labelled by where its fills came from:
  simulated (against the recorded quotes with latency; no queue position, market impact or commissions) or
  paper (optimistic). Profit and loss is labelled an upper bound.
- **Per strategy:** how it ended, what it watched, what it asked for and what became of it (refusals by the
  gateway's own reason names, the broker's refusals), profit and loss from the gateway's books, what the host
  had to close for it, where it traded, how it fared for Tier 1.
- **For the system:** event rates and the busiest second, how late the feed was (a quantile as the upper
  bound of a power-of-two bucket, so it is a bound, not a pretence at precision), the ingest queue's counters,
  gaps, engine lag, capture facts, what the ledger refused, and the replay check's result.
- **What was not measured says so.** The host reads no clock, so engine lag, the queue's counters and the
  capture's size come from the driver; if it gave none, the line says "not measured". Lost events are called
  lost, in capitals, because the engine then saw less than the feed sent.
- Text is the format. A page in the workspace or an e-mail can wrap it later.

## Consequences

- Reading the report needs no knowledge of the code, and it cannot be mistaken for evidence of an edge.
- The values come from the host's running tallies, so they cost a few counters per event and no allocation
  beyond a map keyed by second.
- Scheduling it, keeping it and sending it are operations (E18-S09).
