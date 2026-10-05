# 0006. Raw tape format and the zstd dependency

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-38

## Context

Every session must be replayable bit-exactly, so we keep the events as received
(a "raw tape"), separately from queryable Parquet. A busy session is ~10^7
events and ~500 MB uncompressed. Backtests and replays need to start from an
arbitrary time without reading the whole file. The archiver writes tapes while
the engine runs, so writing must be streaming.

## Decision

A tape (`crates/tf-tape`) is a header, a run of independently compressed
blocks, an index and a fixed-size trailer:

- Blocks hold up to 65,536 events as `[len:u16 | encoded event]`, compressed with
  zstd (level 3) with the frame checksum on. Events use the normal event encoding
  (ADR 0004) at the schema version named in the header, so older tapes decode.
- The index has one 32-byte entry per block (first and last `ts_recv`, offset,
  counts) and is read at open. The trailer (index offset, counts, magic) is read
  first, so opening reads only header, trailer and index.
- **Seek by `ts_recv` is a binary search over the index plus a scan of one
  bounded block.** That needs events in non-decreasing `ts_recv`, which the
  writer enforces. Several blocks may share a boundary timestamp; the search
  starts at the first block that can contain the time.
- A tape is valid only once `finish()` has written the index and trailer. A tape
  whose writer died reports a missing footer; it is not guessed at or repaired
  here.
- Every size read from the file is checked against the file length before
  anything is allocated from it. Damage is an error or harmless, never a panic and
  never different events (tests flip every byte and truncate at every length).

## Consequences

- **First third-party dependency:** `zstd` (and `zstd-sys`, which compiles the C
  zstd library, so builds need a C compiler; CI runners have one). The
  workspace's `unsafe_code = "forbid"` applies to our crates, not to dependencies,
  which use `unsafe` internally. Compressed output is not part of any golden hash,
  so a zstd upgrade cannot move one, but it can change tape bytes and size.
- On the synthetic stream a tape is ~36% of the raw size (10.18M events: 502 MB to
  183 MB). Real data will differ; measure it with E06-S01.
- Compression and checksums cost CPU on the archiver, not on the engine path.
- Recovery of unfinished tapes (scan the blocks without an index) is possible
  because blocks are self-describing, but is not built.
- Rejected: one big zstd stream (no seek), per-event compression (poor ratio),
  Parquet for raw events (we want the provider-order, bit-exact record).
