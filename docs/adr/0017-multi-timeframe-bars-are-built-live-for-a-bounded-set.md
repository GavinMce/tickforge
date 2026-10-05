# 0017. Multi-timeframe bars are built live for a bounded set of symbols

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-108

## Context

Tier 0 keeps 60 one-second bars. Indicators on minute bars (an EMA of closes, an
opening range, ATR) need 1m to day bars, and the stored bars for research must
equal the ones a live strategy saw.

## Decision

`tf-engine::MtfBars` builds 1m, 5m, 15m, 1h and day bars from trades:

- **Bounded.** A symbol costs about 40 KB (120 closed bars on each of five
  timeframes, as `Copy` arrays in one box allocated when tracking starts), so the
  tracked set has a hard limit. A strategy asks for a symbol with
  `ctx.track_bars`; bars start with the next trade and are not back-filled.
- **Alignment.** 1m to 1h are aligned to the Unix epoch; day bars start at a
  configured offset into the UTC day. There is no time-zone database, so the
  caller must change the offset across a daylight-saving change.
- **Time.** Bars are placed by `ts_recv`. A stale timestamp counts in the latest
  second. A bar closes when a later interval's trade arrives or when time is
  advanced past its end, so a quiet symbol's bar closes on time and once.
- **Empty intervals** get no bar unless `fill_gaps`, which fills them with flat
  bars at the previous close (at most the last 120).
- **Corrections and cancels** do not change bars; storage can apply them later.
- **Delivery.** The `Host` owns the aggregator. Closes are delivered through
  `Strategy::on_bar`, in time order, before the `on_event` of the trade that
  closed them; `ctx.bars(id)` exposes closed bars and the forming one.
- The same aggregation code is meant to back the batch/stored bars (E15-S01), so
  live and stored bars cannot drift. That story now depends on this one.

## Consequences

- Every closed bar is an exact function of the trades: tests compare all five
  timeframes against a brute-force grouping of the raw trades over 80 randomised
  streams (with and without gap filling), and check that a stream and its
  encoded replay give identical bars and a pinned digest.
- Volume can differ slightly from a consolidated vendor feed where corrections
  apply; the stored bars should be built with corrections, the live ones cannot.
- Indicators (E07-S09) take bars from this module.
