# 0078. The premarket VWAP reclaim, and the engine additions it needed: session features to strategies, bars in the set, an exit on a signal

- Status: Accepted
- Date: 2026-10-10
- Jira: TIC-171, TIC-203, TIC-204

## Context

`docs/research/engine-for-the-library.md` listed what the strategies of the library need from the engine so that they can run together. Three of the
gaps stood in the way of T03, the first of them to be built after T04 and T25. The session features Tier 0 keeps (E19-S02: the premarket's high, low,
volume and VWAP, the regular session's VWAP anchored at the open, the open, the first minutes' volume, the 5 and 15 minute ranges) could not be read by any
strategy: the member view gave only Tier 0's own state. The shared bars (E19-S03) existed, but the host built from a strategy set had none, so no strategy run
by the research command or the live day could read a bar. And T03 exits on a bar closing under the VWAP, a decision about a bar and not a price a trade can touch,
which the exit book (stop, target, time) could not send.

## Decision

- **`MemberView::session(id)`** gives a member's session state (E19-S48): nothing else about the view changes, and a non-member gives none.
- **A strategy set can have a line `bars SYMBOLS`** (E19-S49): the most symbols the shared bars may track, aligned to the sessions. The host built from the set
  has them in research and in the live day. Without the line there are none, as before, so no set written before it changes; the host's configuration
  fingerprint already covers the bars, so a scenario made with them is not the same configuration as one made without.
- **`ExitBook::exit_now(ctx, id, px)`** sends the close of a held position on the strategy's own signal, with the book's one-at-a-time and retry rules and the
  reason code 0xE504 ("signal exit"). The book's stats count it.
- **T03, `VwapReclaim`, template `t03`, premarket variant** (docs/research 8a): at every review until `screen_by_minutes` after 04:00 a member with at least
  `min_dollars` of premarket dollars traded and a last price `gap_bp` over the prior close, in the price band, becomes a candidate and its one-minute bars are
  claimed (bounded; a refusal is counted, and the claims are let go when a name is finished and at a new day). From `start_minutes` a closed bar whose low is
  `dip_bp` under the premarket VWAP is a dip; a *later* bar that closes strictly above it is the reclaim and the entry (`dollars` at the ask, a collar, a day
  order, no protective order, a fair quote, room under `names`, not after `last_entry_minutes` before the open). Exits: the target `target_bp` over the VWAP at the
  entry, a bar closing `exit_below_bp` under the VWAP (the signal exit), a disaster stop `stop_permille` under the fill, and a time exit `flat_minutes` before the open.
- **The regular-session variants (first touch, wick) are not built.** They need the in-play baseline columns of E19-S52, which the daily snapshots do not carry yet.
  The strategy is the premarket variant and says so; the variants are the rest of E19-S17.

## Consequences

- Strategies can read the premarket high and the opening range (T01, T05, T09 need them); E19-S48 is done.
- A set that runs T03 needs a `bars` line, and without it a candidate is refused and counted and nothing is bought; the report says so.
- The bars are read when they close, at the review (every five seconds), so the VWAP a bar is compared with is the one at the review, not at the bar's last trade.
- The same one-share pair at two prices around the VWAP is how the tests keep a VWAP exact to the raw unit, which is what the boundary tests rest on.
