# 0005. Security master: stable ids, dated tickers, dated provider keys

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-21

## Context

`InstrumentId` indexes arrays in the hot path and is stored in tapes, so it has
to mean the same security tomorrow and next year. Tickers do not: they change,
and are reused by other companies after a delisting. Databento's `instrument_id`s
are reassigned over time. Alpaca identifies securities by ticker only. Splits and
dividends do not change what a security is.

## Decision

- **An id is an index into an append-only list of securities and is never
  reused.** A delisted or acquired security keeps its row. The persisted file
  enforces this: ids must be `0, 1, 2, ...` in order, so deleting or reordering a
  row fails to load instead of silently renumbering.
- **A ticker is a dated attribute, not an identity.** A rename is a second symbol
  span on the same id; a reused ticker is a span on a different id. Resolution is
  always `(symbol, date)`. Two securities may not hold one symbol on the same day.
- **Provider keys are dated mappings.** Databento `(instrument_id, date) -> id`;
  one key may not map to two securities on a day, nor one security to two keys.
  Alpaca resolves through the symbol history.
- **Corporate actions have no record.** Splits and dividends do not change
  identity; adjustment belongs to price handling. A merger is a delisting of the
  target; a spin-off is a new security.
- **Hot path uses a `Session`**: dense arrays for one trading date, built at
  startup. Native key to id and id to symbol are plain array indexing. Lookups by
  ticker hash and are for adapters, once per subscription.
- **Persisted as a line-based text file** (`tfsm 1`), canonical and sorted, so
  changes review as diffs. Dates are `YYYYMMDD` integers; spans are inclusive.
  Contradictory data is a load error, never resolved by guessing.

## Consequences

- Ids are safe to store in tapes and Parquet and to use as array indexes.
- Native keys at or above 2^22 are rejected at load, because the session indexes
  by key. Real Databento ids must be checked against that limit (E06-S01/E06-S05);
  if they do not fit, switch that table to a sorted vector, keeping the same API.
- The updater that writes new securities and spans from Databento symbology and
  definitions is E06-S05; this master only holds and validates what it is given.
- The date calendar is not checked (Feb 30 parses). Spans are compared as
  integers, which is all the ordering needs.
- Rejected: using the ticker or a hash of it as the id (not stable), reusing ids
  after delisting (a stored id could silently change meaning), a database for
  the master (heavier than a reviewable file for a few thousand rows).
