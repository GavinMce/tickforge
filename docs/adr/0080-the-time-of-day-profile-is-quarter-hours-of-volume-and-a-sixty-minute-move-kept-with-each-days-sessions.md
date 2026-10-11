# 0080. The time-of-day profile is quarter hours of volume and a sixty-minute move, kept with each day's sessions

- Status: Accepted
- Date: 2026-10-11
- Jira: TIC-208

## Context

The snapshot carried volume at seven checkpoints (09:35, 10:00, 10:30, 11:00, 12:00, 14:00, 15:30) and nothing about how far a name normally moves. T06 (intraday
overreaction) and T07 (RSI exhaustion) compare volume and the size of a move against what is normal for the time of day, at the minute they trigger, which is not one of the
seven; T09 and the premarket strategies want the premarket's shape.

## Decision

- **Forty-eight quarter-hour volumes**, `qvol_0400` to `qvol_1545` (the premarket's 22 quarters from 04:00, then the regular session's 26 from 09:30; the open's is `qvol_0930`), each
  the average over the last 20 sessions, with the rules of the cumulative columns: the premarket over every session, the regular session over the normal ones (not early closes), a
  session without a trade counting as zero, and the symbol needing `min_days` sessions in which it traded.
- **Twenty-three sixty-minute moves**, `qmove_0930` to `qmove_1500`: the mean of `|P(t+60) - P(t)| / P(t)` over the normal sessions with a price at both ends, in
  hundred-thousandths of the price (0.1 basis points; one basis point is 10). `P(09:30)` is the first regular bar's open; `P(t)` is otherwise the close of the last bar that started
  before `t`, carried over a quarter hour with no bar. At least `min_days` sessions are needed.
- **A strategy reads a run of minutes** with `Profile::volume_between(from, to)` (minutes after midnight New York time): whole quarter hours are exact sums, part of a quarter hour is in
  proportion, taken as even within it, and the answer is unknown if any quarter hour it touches is. `Profile::move_from(minute)` reads a move. Both are on `HistInfo::profile`, reached
  through `MemberView::reference`.
- **A compact representation**: a `Profile` is 71 `u32` (284 bytes; `u32::MAX` for unknown, volumes saturating), so the reference the host holds for each instrument stays small, and
  `Tier0View` now borrows it instead of copying it on every read.
- **The kept day carries the data**: a day's file now holds each symbol's 48 quarter volumes, the closes of the 26 regular quarters and the open, still one line per symbol (about
  8 MB a day for the whole market, mostly zeros). That is a new version of the file (`# tf minute sessions 2`); `prepare_minutes.sh` fetches a day kept in another version again, and a
  history refuses to load one, naming the version, so a change to what is kept replaces the days by itself.

## Consequences

- A snapshot from `tf research snapshots --minutes-dir` has 71 more columns (about 0.5 MB a day more per 1,000 symbols); a universe can require them and filter on them like any other.
- The profile is of the Nasdaq feed's minute bars, like the other history columns (ADR 0079), and its volumes are that feed's.
- The days kept before this change (nine on the cluster) are fetched again by the next run that needs them.
