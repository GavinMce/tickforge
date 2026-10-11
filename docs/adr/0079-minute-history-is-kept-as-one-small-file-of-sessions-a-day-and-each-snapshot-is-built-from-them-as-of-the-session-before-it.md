# 0079. Minute history is kept as one small file of sessions a day, and each snapshot is built from them as of the session before it

- Status: Accepted
- Date: 2026-10-11
- Jira: TIC-207

## Context

The columns the intraday strategies of the library read before the open (the previous session's high, low and close, the ATR over 14 sessions, the volume
baselines at the open, in the premarket and at seven times of the day, and the state of the hourly EMA) were built by `tf reference build --minutes` for one
snapshot (E19-S04). The research command and the daily prepare job make a snapshot for every day of a range with `tf research snapshots`, which read daily
bars only, so every run said "no minute history" and T01, T02, T05 to T10 could not select a single name.

The plan was one CSV of minute bars (`--minutes FILE`). Measured against Databento on 11 October 2026, 63 sessions of the whole market of XNAS.BASIC `ohlcv-1m` are
171 million bars and 9.6 GB (dbn), about 17 GB as CSV, at $0 on this plan. One session is 2.7 million bars and takes two minutes to fetch; reading it takes nine seconds.
Fetching 17 GB each morning, or for each range, is not a design. Also, Databento refuses `map_symbols` for every symbol in one request (422), so the bars
of the whole market carry instrument numbers and no symbol: the old `fetch_minutes.sh` could never have pulled the whole market.

## Decision

- **A day of minute bars is reduced once to its sessions**, a few numbers per symbol (the regular session's high, low and close, the premarket volume, the
  first minute and first five minutes, the cumulative volume at the seven checkpoints, the hourly closes), and kept as `DIR/DATE.sessions`: text, sorted by symbol,
  2 MB for the whole market (about 125 MB for 60 sessions) against about 270 MB of CSV a day. `tf reference minutes --csv FILE [--symbology FILE] --out DIR` makes the files,
  one for each trading day in the CSV, written whole and moved into place; a bar is read exactly as `tf reference build --minutes` reads it (same wick cap, same sessions from the
  calendar), so both make the same columns. A file says the wick cap it was made with and a history refuses a file made with another, so two rules are never mixed. `export_day` and
  `load_day` are tested to give equal columns after a round trip, an early close keeps its length, and a file that does not fit is refused with the reason.
- **Bars without symbols are named by the feed's own symbology** (`symbology.resolve` on XNAS.BASIC for the day), the instrument number read on the bar's own day. A bar whose number
  has no name is counted under the names not listable, as a bad symbol is.
- **`tf research snapshots --minutes-dir DIR`** loads the files from `--history-sessions` (60) sessions before the first day to the day before the last, once, and builds each day's
  columns with `MinuteHistory::build_as_of(the day before)`: the bars of the day itself and of any later day are never looked at, even when their files are in the directory. The
  last session must be the daily bars' own (the same check as `reference build`), a trading day missing in the middle is an error naming it, and a directory with nothing before the
  range is an error. A column stays unknown, never zero, where the history is short (E19-S04's rules are unchanged).
- **`scripts/prepare_minutes.sh DIR START END`** fetches each weekday that has no file, oldest first, through the scratch directory `DIR/.fetch` (on the volume, not in `/tmp`), keeps it, and
  deletes the bars. A weekday with no bars (a holiday) leaves no file and is asked about again; the first failure stops the run, so there is never a gap behind a failed day.
  The range runner runs it for the 90 days before the range up to the day before its last day, and **stops** if it cannot (snapshots without the columns would make the strategies that
  need them trade nothing, unsaid). The prepare job runs it for the 90 days before the day, and **goes on without the columns**, with a warning, if it cannot: today's other
  strategies are not held back. The prepare job's deadline is four hours, for the first morning, when it fetches the 60 days a hand-run job would otherwise fetch.
- **`scripts/check_minutes.py`** compares a kept day with Databento's consolidated daily bars. On 8 October 2026, for the 5,982 names with 100,000 shares or more: the close is within 0.5% for 93.2%
  and within 2% for 99.1% (median 4 basis points); the high 92.2% and 98.3%; the low 94.5% and 99.2%. The differences are the Nasdaq feed against the whole market, and the wick cap
  (ADR 0055), not the arithmetic.

## Consequences

- Every snapshot of a range has the minute columns, from the same files the prepare job keeps, so a backtest and a live day see the same numbers for the same session.
- What the volume baselines cover (S53 wants finer time-of-day profiles) is a change to what is kept in a file, not to how it is fetched.
- The files hold no more than the columns need: a new column that needs a different reduction means fetching the days again.
- The first fill of a history is about 60 × 2.3 minutes. After it a morning fetches one day.
