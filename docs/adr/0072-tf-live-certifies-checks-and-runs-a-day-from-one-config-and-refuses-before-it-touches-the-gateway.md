# 0072. `tf live` certifies, checks and runs a day from one config, and refuses before it touches the gateway

- Status: Accepted
- Date: 2026-10-08
- Jira: TIC-198

## Context

`tf-driver` (ADR 0050) runs a live day, but nothing started one: a person would have had to assemble a host configuration, the
strategies with their certificates, the day's reference snapshot, a ledger and a close time in code. A day that begins at four in the
morning must not depend on remembering a dozen steps, and every check that can be made without the gateway must be made before it
is opened.

## Decision

- **One config file** (`live config v1`, `docs/examples/live/live.cfg`): the dataset and what to subscribe to (a schema and symbols
  for each), the environment variable the key is read from, the strategy set (ADR 0071) and the directories of the reference snapshots
  and of the day's files, and a few timings. Paths are relative to the config. The key is never in a file.
- **The strategies are the strategy set's,** so the variants run live are the lines that research ran. Each must come with a
  **certificate**: the host admits a strategy only if a replay on a tape was made of exactly that definition (ADR 0046). `tf live
  certify` replays each strategy over a stored day of the history store, streamed through a scratch host and not held in memory
  (`certify_files`), and writes the certificates to a file (`certificates v1`; a certificate is one word of text with its seal, and one
  that was edited does not read). It writes nothing if any strategy fails. At the start of a day each strategy is matched to the
  certificate for its number **and its current fingerprint**, so a change of parameters, universe or priority needs certifying again.
- **`tf live run` checks everything before it opens the gateway:** a trading day (the calendar), the set, a snapshot for the day that is
  as of a session before it (ADR 0071), a certificate for each strategy, a stop file not left over, and the ledger. A refusal
  costs nothing and the gateway is never contacted (a test pins that it is not). Then it waits for `--start-at` (New York time, counted
  from the premarket), runs `tf_driver::run` to the close (the end of after-hours trading by default, or the regular close with
  `end close`), and says how the day ended; it is an error unless it closed normally.
- **One file ledger for all the days** (`dir/ledger`, synced at every record: an order must survive a crash) and **a directory for each
  day** (`dir/YYYY-MM-DD`: the raw capture, the decision log, the report). A restart in the same day gets `YYYY-MM-DD-2` and the
  ledger continues, so the morning's log is not overwritten.
- **Stopping is a file.** A file named `STOP` in the directory ends the day cleanly (the capture finished, the report written). The
  workspace forbids `unsafe`, so there is no signal handler; Ctrl-C ends the process and leaves the capture's last segment for the
  next start to repair and no report.
- **`tf live check`** logs in with the key from the environment and reads for a stated time without placing or keeping anything, and
  says the session, the instruments named, the records and gaps seen, and that nothing arrived if so (a closed market, or an
  entitlement that does not cover the dataset or schema).
- **Tested end to end against the fake gateway:** a day from the config to the close leaves its ledger (read as a live ledger is read:
  the strategy has a session with trades), capture, decision log and report, and the capture replays to the same decisions; a stop file
  ends a day that would not end; a start time in the future waits and a stop during the wait never opens the gateway.

## What this does not do

- **Nothing here has run against the real gateway** (ADR 0050 still holds): the login, subscription, stream, heartbeats and
  reconnects of the real service are exercised by the first paid session. The fake was written from the official client's behaviour.
- **No orders reach a broker.** Strategies are on the simulated broker; Alpaca paper needs its transport (E09-S10).
- It does not schedule itself (a cron job or a cluster job starts it), alert anyone, or discipline the clock (E18-S09).
- The reference snapshots lack the columns that need minute history (ADR 0071), so only strategies that do not need them can run.

## Consequences

- A day is one command after a few preparations, written down in `docs/runbooks/first-live-day.md`.
- The certificates are an artifact beside the set: certifying on a real stored day is the staged-rollout step the host already demanded.
