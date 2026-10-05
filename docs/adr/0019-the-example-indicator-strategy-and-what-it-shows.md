# 0019. The example indicator strategy, and what it shows

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-110

## Context

The indicator library (ADR 0018) and the bar aggregator (ADR 0017) need a
strategy built on them, to prove the API is usable end to end and to give the
next author a pattern to copy. It must go through the same gateway and
simulator as the momentum strategy (ADR 0016).

## Decision

`tf-strategy::trend::TrendLong`, long only, with every threshold in
`TrendParams` (18 fields, validated, recorded in the manifest by an exhaustive
`pairs()`):

- asks for one-minute bars of each symbol it sees (`ctx.track_bars`), owns two
  `Ema`s, an `Atr`, a volume `Ema` and a session `Vwap` per symbol, and decides
  only when a bar closes (`Strategy::on_bar`);
- enters on a volume surge (a multiple of the recent average bar volume) with the
  fast EMA rising, via any enabled trigger: a fresh EMA cross with the close above
  the VWAP, a VWAP reclaim, or the trend state on the surge bar; protects with a
  stop a multiple of ATR under the entry; exits when the EMAs cross down or the
  close falls below the VWAP; sits out a cooldown in whole bars.
- `tf backtest --strategy trend` runs it through the gateway, simulator and report.

`Strategy::on_bar` receives the closed bar. A first version read the newest bar
in the series, which is wrong when a gap is filled and several bars close at
once; `BarClose` now carries its bar.

## What it shows, and does not

On the synthetic scenarios it stays out of quiet noise (once volume is gated
against the recent average; without that gate it trades noise) and buys the
surge of a runner. **It does not tell a healthy pullback from a dangerous one.**
It decides on minute bars, so whether it buys a runner that later fades depends
on where the impulse falls against the minute boundaries; in the default demo it
makes about $830 on two healthy runners and loses about $750 on one dangerous
one. That is the point of keeping the pullback logic in the momentum strategy and
a reason to treat any single-scenario profit here as the generator's, not an edge.

## Consequences

- New strategies should copy the shape: bars from `ctx`, indicators owned per
  symbol, decisions in `on_bar`, state from final order updates.
- Indicators warm up from the start of tracking, so this strategy needs a long
  quiet lead-in in a session (the CLI defaults to 420 s and 1800 s for it).
