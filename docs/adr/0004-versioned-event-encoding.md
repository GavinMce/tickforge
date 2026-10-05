# 0004. Versioned event encoding and event identity

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-22

## Context

Events are the contract between providers, the engine, the tape and the
archiver. They must stay `Copy` and at most 64 bytes (`Quote` already uses 56
bytes of payload). Tapes outlive code, so the encoding has to be readable by
later versions. Alpaca sends corrections and cancel/errors that refer to an
earlier trade, and news with text; neither fits an event as is.

## Decision

- **Schema versions.** v1 is bare events with no header (tags 1-3). v2 adds
  tags 4-6 (correction, cancel-error, news) and an optional 8-byte stream header
  (`"TFEV"`, version `u16`, reserved). A stream without a header is v1. Event
  tags stay below `b'T'` (checked at compile time) so the two cannot be
  confused. The per-event layout of existing kinds never changes, so older data
  decodes as is; changing it would move every golden hash.
- **Schema v3 (ADR 0020)** adds tag 7, a parameter change. Nothing earlier changes;
  a v1 or v2 stream containing tag 7 is corrupt.
- **Schema v4 (ADR 0024)** adds tag 8, a tier change. Nothing earlier changes; an older
  stream containing tag 8 is corrupt.
- **Old readers/new data.** A stream containing a tag newer than its version is corrupt, a
  header with version 0 or newer than the build is an error, never a guess.
- **No trade id.** The canonical `Trade` has none, so a `Correction` or
  `CancelError` identifies its original by (instrument, original `ts_event`,
  original price, original size), with the event's `hdr.ts_event` set to the
  *original* trade's time.
- **News text lives outside the event.** `News` carries `article_id`;
  `ts_event` is publication and `ts_recv` is arrival, which is the
  point-in-time boundary for features and backtests.

## Consequences

- Old tapes keep decoding and new kinds can be added by taking the next tag and
  bumping the version.
- Two identical prints on one symbol in the same nanosecond cannot be told
  apart, and Alpaca's own trade ids and condition codes are not kept. If that
  ever bites, add an id to `Trade` in a new version.
- Consumers need a separate lookup for article text, which keeps the hot path
  free of heap data.
