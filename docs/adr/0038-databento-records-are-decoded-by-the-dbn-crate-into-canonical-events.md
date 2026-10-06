# 0038. Databento records are decoded by the `dbn` crate into canonical events

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-149

## Context

Real market data has to become canonical events (ADR 0004) without rounding prices and without an
async runtime in the engine (the provider trait is synchronous so runs are reproducible). Databento
publishes `dbn`, the official reader and writer of its binary format.

## Decision

- **Depend on `dbn` (Apache-2.0), default features only (zstd).** It is synchronous and brings no
  tokio. The live client, `databento`, does bring tokio; it is a separate decision for the live
  adapter (E06-S02) and stays out of this crate. `tf-databento` exposes a `Decoder` over any `Read`
  (plain DBN, zstd DBN, or a zstd file), so the same code reads a live stream's bytes and a capture.
- **Mapping rules** (the module docs have the full list): trades, and the trade inside a quote
  record, become `Trade`; the book in a record becomes `Quote`, quote first and then trade, sharing
  one header; an empty side is price 0 and size 0; status halts, pauses and suspensions become a
  halt, `Trading` a resume, an SSR change an SSR change; prices are copied raw. Anything else is
  returned as `Ignored`, never silently dropped. Gateway messages are returned as `Notice`s: a skip
  after slow reading is a gap, so it must reach the ingest policy.
- **The sequence** is the record's `sequence` with the publisher id in the top 32 bits, because
  each venue numbers its own (measured: venues 81 and 82 interleave in one feed); the consolidated
  schemas have none, so `ts_recv` stands in, which a replay delivers again unchanged.
- **Zero-share prints are counted apart and dropped by default.** On the real full-market feed about
  4% of midday trades (a fifth before the open), nearly all from one venue, are prints of zero
  whole shares at sub-penny prices (fractional-share executions are the likely cause; not
  confirmed). They are real, but they add no volume and a strategy's last price should not move on
  them; `keep_zero_size(true)` passes them through. A trade without a real price is `bad_trades`.
- **Tested two ways.** The committed tests write DBN with the `dbn` encoder and read it back: no
  market data is committed (Databento's terms on keeping samples in a repository are unchecked).
  `examples/check_real` checks real files locally: all 639,143 midday trades agree with Databento's
  own CSV of the same request on instrument, price, size, both timestamps and sequence, and the
  28,734 zero-share rows are counted exactly.

## Consequences

- LULD bands are `statistics` records, not status records; they are E06-S06.
- Decoding is about 12 million records a second including decompression, a hundred times the
  busiest second seen, so it is not the limit.
- `Box<dyn>` over the two decoder types costs one indirect call per record; measured above.
- The `InstrumentMap` gives dense ids by first sight in one stream. Stable ids across a day, and
  across reconnects, need the symbol mapping messages a live session sends (E06-S02/S05).
