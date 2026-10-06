# 0042. Reference rows come from dated daily bars and an asset list, and say what they lack

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-141

## Context

The static layer of a universe (ADR 0041) is judged from a snapshot of what was known before the
session. Its numbers have to be reproducible, must not leak the future into a replay, and must not
let a missing fact quietly count as a pass.

## Decision

`tf-reference` turns fetched files into a `tf-universe` snapshot. It does no network I/O: one script
(`scripts/fetch_reference.sh`) fetches, one command (`tf reference build`) builds.

- **Source of the numbers.** Databento `ohlcv-1d` bars for the consolidated feed (EQUS.SUMMARY) and its
  symbology file, which says which symbol an instrument id was on each day. Prices stay raw integers;
  averages are integer divisions. No floating point.
- **Point in time.** Only bars dated on or before the requested date are read, and the snapshot's
  as-of date is the last session actually used. A symbol with no bar on that day has no price: a stale
  close is not the prior close.
- **Averages mean sessions.** Average volume divides by the sessions in the window, so a day without a
  bar is zero volume; a symbol that traded on fewer than `min_days` of them has no average (unknown),
  never a small one. True range uses the previous *session's* close and is left out where the symbol
  has none, so a new listing or a gap does not invent a range.
- **Strict inputs.** A bar line that does not parse, or whose high and low do not contain its open and
  close, is an error; a dropped bar would silently change an average.
- **Flags from the broker, facts we lack stay absent.** Alpaca's asset list gives tradable,
  shortable, easy-to-borrow and exchange for the symbols it lists; others stay unknown and fail
  conditions. No free source marks ETFs, so `etf` comes from a list the person keeps and is absent
  without it. Float and short interest are absent, so a universe that uses them refuses to run.
- **Dollar volume is approximate.** Each day's dollars are close times volume, not the day's VWAP,
  because daily bars carry no VWAP. For a universe floor that is a good enough bound, and it is said so
  rather than hidden.

## Consequences

- A snapshot's fingerprint, written into the stored member list, identifies exactly which bars and
  flags a day's universe was chosen from.
- Symbols with characters outside the name rule (warrants, rights and some preferreds, about 1% of the
  symbols) cannot be listed in a spec and are counted in the build report.
- A broker whose list was fetched later than the bars is a different snapshot: the as-of date covers
  the bars, and the flags are as of the fetch. Daily operation (E18-S09) fetches both together.
