# 0068. A replayed day is a live day: the host over a file ledger in the day's own directory, with its daily report

- Status: Accepted
- Date: 2026-10-08
- Jira: TIC-195

## Context

The design says one engine serves live, replay and backtest, differing only in event source and clock. Research runs (ADR 0059) used
the live host, with its Tier 0, promoter, strategies, gateway and simulated broker, but over an in-memory ledger, and turned what
happened into their own records (trips, a log, traces). A live day writes a file ledger, a decision log and a daily report, and the
workspace reads the ledger (run catalog, overview, run view). So a replayed day left no ledger for any of that, and "what the
backtest did" lived only in files the live system never writes.

## Decision

- **A research day runs over a file ledger in its own directory, `<date>.ledger/`** (`run_day_to`), the same `FileStore` a live
  ledger is, written by the same host built the same way (`replay_host_on`, generic over the store). The data comes from the stored
  history instead of the gateway; everything after the feed is unchanged.
- **Each day has the report a live day has** (`<date>.report.txt`), built by the same `DailyReport` from the same host: what each
  strategy did, refusals, the system section, and how much of Tier 1 was held. What a live day measures with a clock or a queue
  (engine lag, ingest counters, capture) is "not measured" in a replay, as the report says.
- **The ledger is the record; the trips are derived from it.** A test reads a replayed ledger as a live one is read (one session per
  strategy per day, as the catalog does) and requires its trades and profit to equal the trips' count and gross for each strategy
  and day, and `tf ledger verify` replays it and reproduces every recorded decision. Two runs of a day leave a byte-identical ledger.
- **The report carries the ledger's checksum**, so a day's ledger that is missing, cut, altered or taken from another day or run is
  refused (`Results::ledger`), and a day without a good ledger and report is made again, like one without its log or traces. A day made
  again removes the old ledger first and never appends to it.
- **The ledger is made durable when the day ends, not at every record** (`FileStore::open_buffered`): a live ledger syncs every
  append because a crash must not lose an order; a replayed day that did not finish is simply run again, and an fsync per record would
  make a whole-market day wait on the disk. The bytes are the same.
- **Days stay independent**, each with its own ledger, as before: a live month restarts the engine every morning over one ledger, but
  nothing yet carries between days (the end-of-session rebalance is not wired into the host), so independent days are the same
  thing and can still be run in any order or in parallel. When a rebalance is wired, a scenario that uses it must run its days in
  order over one ledger, and this decision is to be revisited for it.
- **What a replayed day still does not do that a live one does:** read from the gateway (names come from a first pass over the
  files, ADR 0059), queue and drop under load, capture raw records (the stored history is the capture), reconnect, or take orders
  to a paper broker (the paper route is stood in for by the simulated broker, as in the live replay check). Strategies are installed
  without a certificate because this replay is what makes one (`certify`).

## Consequences

- The workspace can show a replayed day with the views it has for a live one (E19-S42), and the replay page can show what the engine
  did with a symbol (E19-S43).
- A day's ledger is small for a handful of strategies (about a hundred records for T04 on six names) and grows with orders: a null run
  of a hundred seeds writes a few thousand records a day. Not measured at whole-market scale.
- The earlier research paths (`tf backtest` and its explorer for MomentumLong) still use their own loop; moving them onto the replayed
  day is not done here.
