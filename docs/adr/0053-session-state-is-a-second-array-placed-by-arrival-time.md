# 0053. Session state is a second array beside Tier 0, placed by arrival time

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-156

## Context

Tier 0's high, low, volume and VWAP count every trade of the day, extended hours included (its documented
convention; the scanner and the momentum strategy rely on it). The strategies in `docs/research` need the
regular session on its own (a VWAP anchored at 09:30, the day's high without the premarket spike), the premarket
(its high, low, volume), the first minute and five minutes after the open, the opening range, and after-hours.

## Decision

- **A second array, not new meaning for the old fields.** `tf_engine::Sessions` holds one `SessionState` (272
  bytes, `Copy`) per instrument, owned by `Tier0` and fed from its `on_event`. Tier 0's fields, the golden hashes
  and every consumer are untouched. `Tier0::set_day(times)` starts a day with the boundaries from `tf-calendar`;
  until then nothing is kept by session. `Host::start_day` calls it.
- **Placed by arrival time (`ts_recv`), not event time.** Databento's own one-minute bars place trades by arrival:
  on 2 October 2026, for 25 symbols, premarket, the first minute, 5 and 15 minutes, the regular session, after-hours
  and the open (275 comparisons) all agree with the bars by arrival time and 20 do not by event time (a trade
  whose event time is in the first minute and that arrived after it; the open as the earliest event time is a late
  facility print, not the opening cross). The history baselines of E19-S04 are built from those bars, so a live
  feature and its baseline must place trades the same way. Arrival time is also what a strategy knows when it decides,
  and a late report never changes a window that has closed.
- **The open is the first regular-session trade to arrive**, not the official opening cross (a statistics record).

## Consequences

- Cost: about 17 ns an event. Tier 0 over the real opening five minutes (2.4 million trades) ran at 23 million events
  a second with a day set against 37 million without; the busiest second ever seen is 337 thousand.
- Memory: about 3.5 MB for 13,000 symbols.
- **Zero-share prints.** The decoder drops prints of zero whole shares. Databento's bars count them in highs, lows and
  the open. In the 25-symbol sample 41% of the trades in the premarket and after-hours windows were such prints
  (about 4% at midday), and with them dropped 20 of 275 comparisons differ (every one a high or a low); with them kept,
  none does. A previous day's high or low taken from the bars (E19-S04) therefore includes them and a live one does not,
  until E19-S08 decides whether to keep them as flagged trades.
- Strategies that read `session(id)` need `start_day` called; the driver does it once operations (E18-S09) decide the date.
