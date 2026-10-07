# 0058. History is kept as the provider's own daily files with a manifest, and replays as a capture does

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-161

## Context

The only place an edge can be seen is twelve months of history (`docs/research`), and the plan's twelve months are a
rolling window: what is wanted from the first months has to be pulled and kept while it is still included. A history
of the whole market is hundreds of dollars at pay-as-you-go prices, so a pull must say what it will cost before it
spends, and a file that is later short, altered or missing must be found, not trusted.

## Decision

- **One zstd DBN file per day and schema, as Databento delivers it** (`DIR/<dataset>/<schema>/<YYYY-MM-DD>.dbn.zst`),
  and one text manifest, `store.tfhist`. The files are the provider's records unchanged (the same as a raw capture's
  segments, ADR 0040), so nothing is lost to a conversion and a decoder fix applies to history too.
- **The network is a script** (`scripts/pull_history.sh`), because the project has no HTTP client. It asks the metadata
  service for the cost of the whole range first (free), prints it and **refuses above `MAX_COST`** (default $5), pulls a
  day at a time (weekends skipped, a holiday said, a stored day not pulled again) and writes the day's quoted cost
  beside it.
- **The manifest line of a day** is dataset, schema, symbols, date, bytes, SHA-256, cost in millionths of a dollar
  (rounded up), records and the symbols the file maps. `tf history index` builds it by streaming each file (a day can be
  gigabytes; the SHA-256 is incremental, checked against the one-shot hash at every padding boundary), and keeps the lines of
  other datasets and schemas.
- **Notes say what the store is not.** The borrow flags are not point in time (today's list applied to every day, so
  historical shorts look easier than they were). Whether names that later left the market are present is *measured*,
  from the symbol mappings in the files' own metadata: symbols on the first day that are not on the last (delisted,
  renamed or merged), or "not known" for one day. Nothing is assumed.
- **`tf history verify`** rereads the manifest and every file and names each one that is missing, short or longer than
  listed, altered (the checksum is compared first, so a file damaged inside its compression is "altered", not
  "unreadable"), whose record count differs, or present and not listed; it exits non-zero. `show` prints the symbols per
  day, the cost and the notes.
- **Stored days replay as a capture does.** `tf_history::replay` gives the days of a dataset and schema in date order as a
  `CaptureReplay` (`CaptureReplay::from_files`), the capture's own Provider: the same decoder, ids numbered in the order
  first seen and carried from day to day. The host's replay path takes a list of files (`tf_host::replay_files`;
  `replay_capture` is that over a capture's manifest), so it runs on a store unchanged: a test stores a live day's capture
  segments as days, verifies them, and gets the same verdict and event count. A missing or short file is refused.
- **Bars are not events.** The decoder maps trades, quotes and the trade-with-quote schemas to events and ignores
  `ohlcv`; one-minute bars are read directly for the screening stage of `docs/research/data-and-protocol.md`
  (E19-S13), and trades and `tcbbo` days replay.

## Consequences

- Twelve months of the whole market's one-minute bars is quoted at $415.63 pay-as-you-go (measured 7 October 2026);
  it is not pulled before the paid plan, which includes the history. The store and the script are what that pull
  will use; see the backlog evidence for what was pulled.
- A day Databento returns no data for is left out and said; a day with no records in a file is reported by `index`.
- Prices of a day's cost are what the metadata service quoted before the pull, not an invoice.
