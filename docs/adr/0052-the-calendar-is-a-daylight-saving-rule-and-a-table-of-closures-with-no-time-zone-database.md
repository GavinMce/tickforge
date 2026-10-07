# 0052. The calendar is a daylight-saving rule and a table of closures, with no time-zone database

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-155

## Context

Nothing in the system knew what day it was in New York. Bars start on multiples of their length since the epoch
(so an hourly bar begins at 09:00 New York time in summer and 08:00 in winter, never at 09:30), the day bar takes a
fixed offset that the caller must change twice a year (`MtfConfig::day_open_offset_secs`), and Tier 0 cannot tell
premarket from the regular session. The strategies in `docs/research` are all rules about the clock: the opening
range, 09:45, 15:30, the last 25 minutes, flat by 15:55, premarket from 04:00.

## Decision

New pure crate `tf-calendar`, depending only on `tf-core` for the time type.

- **New York time is arithmetic.** The US rule in force since 2007: daylight time from 02:00 on the second Sunday
  of March to 02:00 on the first Sunday of November. No time-zone database, no `chrono`, nothing that reads the
  machine's zone or its clock, so a replay on another machine gives the same answer (ADR 0003). Years before 2007
  are refused rather than answered with the wrong rule.
- **Closures are data.** A sorted table of the exchange's holidays, early closes (13:00) and unscheduled closures
  from 2024, the start of the dataset's history, to 2028, the end of the published calendars. A question about a date
  outside those years is an error, so a stale table fails on the first day it matters instead of trading through a
  holiday. Sources: NYSE Group press releases of 27 December 2021 (2024), 8 November 2024 (2025 to 2027) and
  23 December 2025 (2026 to 2028), and the NYSE notice that markets closed on 9 January 2025 for the national day of
  mourning. The table holds the NYSE's calendar; Nasdaq follows it.
- **Sessions.** Premarket 04:00 to 09:30, regular 09:30 to 16:00, after-hours to 20:00; on an early close the
  regular session ends at 13:00 and after-hours at 17:00 (the releases give 17:00 as the close of the late trading
  sessions). The start of a session belongs to it.
- **How it is checked.** The daylight-saving rule is compared with Python's zoneinfo (the system tz database) for the
  New York midnight of every day from 2024 to 2028 and the second of each of the ten changes
  (`tests/gen_ny_vectors.py` writes `tests/ny_midnights.txt`). The table is compared line by line with the
  releases. Boundaries are tested to the nanosecond, and the real 2 October 2026 records (the opening price at
  13:30:00.3 UTC, the closing price at 20:00:00.1 UTC) fall in the sessions they should. Mutation run: 47 mutants
  of the rule, the constants and the comparisons, all killed except two unreachable ones (`>=` for `>` where the
  equal case has already been handled).

## Consequences

- Extending the calendar past 2028 is a table edit with a new source and a test line; nothing else changes.
- The day-open offset in `MtfBars` and the hourly alignment still use the old fixed offset until E19-S03 moves
  them onto this crate; nothing else changes behaviour yet.
- Holidays and half days outside the NYSE's calendar (an exchange-specific closure) are not represented.
