# 0018. Indicators are small, `Copy`, integer types checked against exact references

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-109

## Context

Strategies need EMAs, VWAPs, RSI, ATR and ranges. Hand-rolling each per strategy
repeats the same edge cases (warm-up, seeding, rounding, overflow) and makes two
strategies disagree about what "RSI(14)" means.

## Decision

`tf-engine::indicators` provides `Sma`, `Ema`, `Extremes`, `RateOfChange`,
`Vwap` (anchored, with deviation bands), `RollingVwap`, `Rsi`, `Atr` and
`OpeningRange`.

- **`Copy`, fixed storage, integers.** Windows are const-generic arrays; results
  are raw price units or permille. Nothing allocates or uses floats, so results
  match on x86_64 and aarch64 (a pinned digest over a synthetic session runs on
  both in CI).
- **Textbook definitions, stated.** EMA `alpha = 2 / (n + 1)`, seeded by the
  simple average of the first `n` values (or by the first value, chosen
  explicitly); Wilder smoothing for RSI and ATR; RSI in permille with 500 for a
  series with no movement; ATR's first true range is high - low.
- **Warm-up is explicit.** `value()` is `None` until the definition is met and
  `is_ready()` says when.
- **Saturating, not panicking.** Inputs are clamped to +/- 2^40 raw units
  (about $1,100, the same bound as `Ewma`); extreme values saturate.
- **Checked against an independent computation.** EMA (both seeds), RSI(14) and
  ATR(14) are compared with vectors computed in Python with exact fractions
  (`tests/gen_indicator_vectors.py` regenerates them; a check confirmed the
  embedded constants match). They agree within one raw unit (one permille for
  RSI). SMA, extremes, rate of change, VWAP and rolling VWAP are compared with
  brute-force recomputation; the VWAP deviation with a floating-point reference.

## Consequences

- Bar-based indicators (`Atr`, `RollingVwap`, `OpeningRange`) take bars from
  `MtfBars` (ADR 0017) through `update_bar`.
- Indicators do not know about time except `OpeningRange` and `Vwap::anchor`,
  which take it as an argument. Session handling (when to reset a VWAP) is the
  strategy's job.
- Not included: MACD, Bollinger and other derived indicators; they compose from
  these. Warm-up from history before the session is a separate concern.
