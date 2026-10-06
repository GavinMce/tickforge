# 0040. Raw capture keeps the provider's records in files that are complete or recoverable

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-52

## Context

The paid month of live data is worth most as a dataset that can be replayed bit for bit. Our
normalized event is a lossy view of what Databento sent, and the existing normalized tape
(`tf-tape`, ADR 0006) is unreadable until its writer finishes, which is not good enough for a
process that can die on a trading day.

## Decision

- **Capture the provider's own records**, not our events. `tf-capture` writes the DBN records a feed
  delivers, byte for byte, to rolling `raw-<first ts_recv>-<n>.dbn.zst` files with a zstd checksum
  on each frame. Any session can then be re-mapped by `tf-databento` (including mappings written
  after the day), and a normalized tape is made from it on demand.
- **A segment is complete or recoverable, never in between.** Records go into `NAME.part`; `sync`
  (about once a second, called by the feed loop) flushes the compressed stream to a decodable point
  and fsyncs. Closing finishes the stream, fsyncs, renames to `NAME.dbn.zst` and fsyncs the
  directory. Readers see only renamed files.
- **Opening a capture repairs it.** A `.part` is read as far as it decodes and rewritten as a clean
  segment marked `recovered`; a finished segment missing from the manifest is listed (marked
  `late`). A test cuts an unfinished file at every one of its last 300 bytes and at every 37th byte
  before that: what comes back is always a prefix of what was written, and more bytes never
  recover fewer records.
- **An append-only manifest** (`manifest.tfcap`) lists each segment with its record count, first and
  last receive time, size and FNV-1a checksum. `verify` checks files against it and reports a
  missing file, a stray file, an unfinished file, a changed size, checksum, record count or
  receive time. The checksum catches damage, not tampering.
- **Segments roll on receive time** (every 900 s of `ts_recv` by default), not the wall clock, so the
  same input always makes the same files. Replay follows the manifest, which is arrival order.
- **Replay is a `Provider`.** `CaptureReplay` maps segments in order, carrying instrument ids from
  one to the next, honours a subscription, and ends; `to_tape` writes the normalized tape, which
  `tf-tape`'s driver can replay at any pace.

## Consequences

- Real data, 788,740 records (a midday window, a premarket quote window and a day of status): written at
  1.1 million records a second including an fsync every 100,000, 16.4 MB on disk for 18.2 MB of input
  files, verified clean, and replayed event for event identical to decoding the files directly. The
  busiest second seen is 337,000 records.
- A segment that is open when the process dies loses what was written since the last `sync`:
  at most about a second of records. The feed's own replay window (24 hours at Databento) can fill
  that gap on reconnect.
- Shipping finished segments to object storage is E06-S08; nothing here touches the network.
- The capture is of what the feed delivered. What the engine was given under pressure (ADR 0039) is a
  different thing, and the replay-equivalence check (E18-S06) needs that, not this.
