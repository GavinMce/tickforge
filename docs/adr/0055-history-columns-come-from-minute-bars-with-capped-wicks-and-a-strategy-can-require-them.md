# 0055. History columns come from one-minute bars with capped wicks, and a strategy can require them

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-158

## Context

The intraday strategies of `docs/research` read things the snapshot of ADR 0042 does not have: the previous session's
high, low and close (levels to sweep), the ATR in price units (stop distances), baselines for the first minute, the first
five minutes, the premarket and the volume so far at a time of day (opening relative volume), and the state of an
EMA(100) over regular-session hourly closes (the 1a bias). They have to be point-in-time, and a missing history has
to read as unknown, not as a small number.

## Decision

- **Sixteen columns, appended.** `prev_high`, `prev_low`, `prev_close`, `atr14`, `ema100h_state`, `ema100h_count`,
  `vol_first1`, `vol_first5`, `vol_pre`, `cumvol_0935`, `cumvol_1000`, `cumvol_1030`, `cumvol_1100`, `cumvol_1200`,
  `cumvol_1400`, `cumvol_1530`. They follow the existing columns in the table, so a snapshot without them renders,
  fingerprints and selects exactly as before (the fingerprints and a stored selection from before are pinned in a
  test, taken from the code before the change). `vol_first5` and `cumvol_0935` are the same quantity; both exist
  because both were asked for.
- **From one-minute bars of the live feed** (Databento `ohlcv-1m`, XNAS.BASIC), streamed (`tf_reference::MinuteHistory`),
  so a whole market's history does not have to fit in memory: a few numbers per symbol and session are kept.
  `tf reference build --minutes FILE` adds them to the snapshot of the daily bars and refuses minute bars that end
  on another day than the daily ones. `scripts/fetch_minutes.sh` pulls them, asks for the cost first and refuses above
  a limit (default $5).
- **Sessions come from the calendar** (ADR 0052), so the open is 09:30 New York time on both sides of a daylight-saving
  change, an early close is recognised, and a date outside the table is an error. Only bars on or before the date
  are read; the as-of date is the last session with bars. Every trading day between the first and the last must have
  bars (a missing day is an error, not a smaller average); a symbol's missing session counts as zero volume; a volume
  average needs the symbol to have traded in 10 of the last 20 sessions, the ATR in all of the last 15, the EMA 100
  closes. Early closes are left out of the time-of-day (cumulative) averages only.
- **ATR14** is the mean of 14 true ranges, each against the session before it (the same number as Wilder's ATR(14)
  after its seed). **The EMA** is `tf_engine::Ema` (period 100, simple-average seed) over the last 60 sessions'
  hourly closes, the hours counted from 09:30 as the live session-aligned bars (ADR 0054) count them; its state is
  exported exactly (`Ema::state`, `Ema::resume`: a test shows an average resumed from its state continues
  identically to one run in a single pass), so the live average can carry on from the snapshot.
- **A bar's high and low are capped** at 0.5% of its body beyond the body (`--wick-clip`, permille; zero is the body
  alone, a very large number is the raw bars), and the build says how many bars that changed.
- **A strategy can require a column.** A universe spec may have `requires prev_high atr14 ...`: columns the strategy
  reads without filtering on them. `select` refuses a snapshot that lacks one, as it does for a column a condition uses,
  and the host refuses to admit the strategy. The reference row a host holds (`RefInfo`) now carries the history
  values, and `MemberView::reference(id)` gives them for members only.

## Why the cap (measured on a real day-range, 2 July to 2 October 2026)

The feed's own highs and lows are not the market's. For AAPL, NVDA, TSLA, F and SPY over 66 sessions (330
symbol-sessions, 5 symbols pulled for $0.19), compared with the consolidated daily bar (EQUS.SUMMARY): the feed's
regular-session high is more than 1% away in 27% of them and the low in 46%, up to 13.7%; AAPL's low on 1 October
is 263.88 in the feed and 325.81 consolidated. The minutes in question have normal opens and closes and large volume:
a single report far from the market. A 14-session ATR from the raw bars was 6.9% of AAPL's price (the consolidated
daily ATR says 2.1%), which would have made every stop three times too wide. With a wick cap, the error against the
consolidated high and low is: 0.5% cap: median 0.00%, 90th percentile 0.20% (high) and 0.30% (low); 0.25% cap: 0.11% and
0.15%; body only: 0.21% and 0.19%; raw: 5.3% and 8.0%. The largest remaining error is 3.1% on a high. **0.5% was
chosen after seeing these numbers, on this sample, and is a parameter**; the 0.25%, 0.5% and 0.1% results are close.
The real fix is to say which trades are off-market (trade flags, E19-S08); until then the cap is.

## Evidence

Real data, five symbols, 60 sessions to 2 October 2026 (303,753 minute bars, 15 s and $0.19 to pull, built in
0.1 s): all 80 column values (16 columns, 5 symbols) agree with an independent recomputation in Python from the raw CSV
(own time zone handling, own EMA in exact integers). 2,661 of about 117,000 regular-session bars (2.3%) had a high or
low capped. For the whole market for the same 65 sessions, the metadata service quotes $112.43 at pay-as-you-go
(included in the plan's 12 months of history).

## Consequences

- The columns are of the live feed (XNAS.BASIC, 65% of consolidated volume, ADR in `docs/DESIGN.md`), so volume
  baselines are comparable with what Tier 0 will count live; the daily columns of ADR 0042 are of the consolidated
  daily summary. They are not the same universe of trades and are not meant to equal each other.
- Zero-share prints (about 41% of premarket and after-hours trades, E19-S08) are counted in the bars' volumes' highs
  and lows; the cap limits their reach for highs and lows.
- The live Tier 0 high and low (and the session features of ADR 0053) carry the same off-market prints uncapped: a
  strategy comparing a live high with `prev_high` compares unlike things until E19-S08.
- Renamed tickers split a symbol's history (the minute file carries the symbol of each day).
