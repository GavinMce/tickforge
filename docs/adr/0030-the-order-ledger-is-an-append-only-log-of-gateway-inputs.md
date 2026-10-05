# 0030. The order ledger is an append-only log of gateway inputs

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-73

## Context

After a restart the engine must know exactly which orders are open and what it holds, and the
risk gateway's counters (rate window, daily loss base, kill switch) must be what they were.
Storing the resulting state invites drift between the state and the rules that produced it.

## Decision

`tf-ledger` event-sources the gateway (`tf-risk`) and the order state machine
(`tf_strategy::lifecycle::Order`). A `Journal` wraps both. Every input is applied and then
written as one record: a decision (the intent and the gateway time, with the gateway's
answer), an acknowledgement, a fill, an order ending (cancelled, rejected, expired), the kill
switch, a new day, and a mark.

- **Replay is verification.** Opening a ledger runs the records through the same code and
  compares what comes out with what was written, record by record. A decision the replay would
  make differently (limits changed, behaviour changed, the file edited) is a `Diverged` error
  naming the record, not a quiet difference. The first record states the universe size and the
  limits; opening with different ones is refused.
- **Write before relying.** A call returns only after its record is durable, so nothing leaves
  the process (an order to a broker, a fill passed on) that the ledger does not hold. If an
  append fails the journal is poisoned and refuses everything until restarted from the ledger.
  Inputs the order book refuses are validated before anything changes and write nothing.
- **Marks.** Decisions depend on the marks of instruments holding a position, so those marks
  are written when they have moved since last written, just before a decision or a new day.
  Nothing else about market data is in the ledger; after a restart the marks are the last
  written until market data refreshes them.
- **Storage is a trait** (`LedgerStore`: load, append numbered records). `FileStore` is a log
  file, one `number checksum record` line each, synced on every append, with a lock file for a
  single writer. A torn last record is removed and reported on load; damage anywhere else
  refuses to open, and a store never skips a record. `MemStore` is for tests. `conformance`
  is the suite any other store must pass; the Postgres store (E09-S09) is its next user.
- Records are text lines of integers (prices raw), readable with `tf ledger verify`.

## Consequences

- Tested by rebuilding a journal from its ledger after every step of random days (12 seeds x
  250 steps, kill switch and new days included) and by cutting the log file at every byte of
  each of the last dozen records of a day (and the edges of the rest) and checking the
  recovered state is the state before that record.
- The gateway gained `snapshot` (its decision-relevant state as comparable data) and
  `Limits::from_pairs` (the inverse of `pairs`, so a ledger can say what limits it was for).
- Not yet wired into the backtest harness or any broker adapter: the Alpaca adapter
  (E09-S05) is the first live user. Reconciling the ledger with the broker is E09-S06.
- Limits cannot change inside one ledger; a limits change starts a new ledger (a new day).
- FNV-1a detects torn or damaged lines; it does not defend against tampering.
