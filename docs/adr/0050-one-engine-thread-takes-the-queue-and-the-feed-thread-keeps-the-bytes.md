# 0050. One engine thread takes the queue, and the feed thread keeps the bytes

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-153

## Context

A live day has a feed that must never wait (ADR 0039), a raw capture that must hold what the gateway sent
(ADR 0040), a host that decides (ADR 0046) and a replay that must reproduce the decisions (ADR 0047).
Something has to join them, and the joins decide whether those properties hold.

## Decision

`tf-driver::run` is the engine thread. The feed thread (ADR 0049) does two things with each record: it hands
the bytes to the **capture sink**, then decodes and offers the event to the ingest queue. The capture is
therefore the gateway's own stream whatever the engine does, and a capture that cannot be written ends the
session: the driver throws the kill switch and ends the day, because a day that cannot be kept is not
traded blind. The engine thread takes deliveries from the queue and gives them to the host.

- **Names first.** The gateway names instruments as the session starts. The driver waits for a quiet
  spell (or a limit) before building the host, so strategy universes are resolved against real names;
  the ring holds what arrives meanwhile.
- **One rule for repeats.** After a reconnect the gateway may send an event again. `Dedupe` drops an
  exact repeat within one receive time. The capture holds the repeat (it is what was sent), so the
  replay of the capture applies the same rule; otherwise a day with a reconnect would never replay equal.
- **The replay stops where the day stopped.** The decision log records the day's end; a replay does not feed
  events past it, because a capture can hold more than the engine took (the feed reads ahead of the close).
- **Resume from the last event.** A session that ends or goes silent is reopened with the last event's
  time as the replay start, with backoff, up to a limit; beyond it the day ends and the report lists
  what is still open, since without data nothing can be closed.
- **Measure what the host cannot.** The host reads no clock; the driver times each event's step and
  reports the 99th percentile and the worst, with the queue's counters and the capture's size.
- **The log is a file as the day goes**, appended and synced about once a second, closed at the end; a log
  without its closing line is read as far as it goes.

## Consequences

- The capture may hold records the engine never saw (read ahead of an early close); the replay accounts
  for that by stopping at the live end of day. It may also hold fewer records than were sent if the driver
  closes the session first; what matters is that every event the host decided on is in it.
- Nothing here has run against the real gateway. The first paid session exercises subscription, stream,
  heartbeats and intraday replay for the first time; the driver is built to stop cleanly and say what it saw.
- Scheduling the 04:00 start, a signal handler that sets the stop flag, and the ledger on disk belong to
  operations (E18-S09).
