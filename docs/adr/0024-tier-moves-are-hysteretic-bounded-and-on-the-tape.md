# 0024. Tier moves are hysteretic, bounded, and on the tape

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-59

## Context

The scanner (ADR 0023) says which symbols are running. Something must turn that into
Tier 1 membership without flapping, without growing past the memory budget, and
without breaking replay: a strategy that sees a symbol promoted live must see it
promoted at the same moment when the tape is replayed.

## Decision

`tf_engine::Promoter` owns the scanner and Tier 1 and moves symbols between tiers.

- **Hysteresis.** The bar to get in (the scanner's z-score, 8) is above the bar to
  stay (`demote_z_milli`, 3, required to be lower). A promoted symbol is "hot" while its
  10 s volume stays above the lower bar and is demoted only after `cooldown_secs`
  without being hot, and only after `min_dwell_secs` in Tier 1. A demoted symbol cannot
  be promoted again for `repromote_after_secs`. `confirm_hits` hits within a window can
  be required to promote. Because the volume is measured over a sliding 10 s, any burst
  qualifies for about ten seconds, so confirming needs consecutive seconds of it.
- **Bounded.** At most `max_tier1` promoted. A hit that finds it full is counted
  (`refused_full`) and dropped; membership frees up only by demotion.
- **Pinning.** A strategy pins a symbol it holds a position in (`ctx.pin_tier1`); a
  pinned symbol is never demoted until released.
- **Demotion does not need trades.** A symbol that goes silent is still demoted, since
  its recent volume is read as of the current second (`RollingBars::volume_asof`), not
  as of its last trade.
- **On the tape.** Every move is a `TierChange` event (new in the encoding, schema v4,
  tag 8, 40 bytes: action, reason code, score). The promoter applies a decision through
  `apply`, the only thing that changes membership; a **follower** promoter has no
  scanner and applies the `TierChange` events in its stream, which is what replay does.
- **Ordering.** A decision is caused by an event and takes effect for it: the strategy
  that sees the event sees the symbol already promoted, and Tier 1 records the event.
  So the tape holds each `TierChange` *immediately before* the event that caused it, with
  the same timestamp. The first version put it after, and a test comparing a live run
  with its replay showed the strategy seeing a promotion one event late and Tier 1 missing
  the triggering event; the ordering and the order of the promoter's own steps were
  fixed together. A live run and its replay now agree at every step, on membership and
  on features.
- **Host.** `Host::with_promoter` runs it; strategies read promoted symbols through
  `ctx.tier1(id)`; `Host::drain_tier_events` returns the moves to write ahead of the
  event just fed. A decision-mode promoter ignores `TierChange` events in its input.

## Limits

- The momentum strategy still runs its own private Tier 1 and scan; moving it onto the
  shared promoter is a follow-up.
- Thresholds were set on the synthetic scenarios (ADR 0023) and need remeasuring on
  recorded data.
- A full Tier 1 refuses newcomers rather than evicting the weakest; with fifty slots and
  a scanner this selective that has not mattered, but a day with many simultaneous runners
  would favour whoever was first.
