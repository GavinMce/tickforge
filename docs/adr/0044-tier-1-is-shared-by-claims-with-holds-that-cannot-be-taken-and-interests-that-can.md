# 0044. Tier 1 is shared by claims, with holds that cannot be taken and interests that can

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-143

## Context

Tier 1 holds full history for a few dozen symbols and was sized for one strategy plus the scanner.
With ten to twenty strategies, three things were wrong: a hold was one flag, so one strategy letting
go released another's position; a strategy could not ask for a symbol, only wait for the scanner to
pick it; and when Tier 1 was full the only outcome was a silent counter on scanner hits.

## Decision

The promoter keeps a book of **claims**, per strategy and symbol, of two kinds.

- **Hold** (`pin`): the strategy has a position or a working order in the symbol. Never demoted, never
  evicted, while anyone holds it. Counted per strategy.
- **Interest** (`request`): the strategy wants to watch the symbol. The cool-down sweep leaves it alone
  while anyone is interested; when the last interest is released it leaves at the cool-down.

A request for a symbol not in Tier 1 is answered at once: promoted if there is room; otherwise by
evicting, under this rule: candidates are promoted symbols nobody holds that have dwelt the minimum
time; a candidate's *level* is 0 if only the scanner wanted it, else the highest priority among the
strategies interested; the request may evict only a candidate whose level is strictly below the
requester's priority; the victim is the lowest level, then the fewest interested strategies, then the
longest since it was hot, then the lowest id. Priority is a `u8` per strategy, default 1, so a strategy
outranks what the scanner alone wanted, and strategies at the same priority never evict each other.
Otherwise the request is denied.

Every promotion and eviction is a `TierChange` event with its own reason code, so the tape carries it
and a follower applies it. A follower never decides: a request on a symbol the tape has not promoted is
denied (`Following`). Every outcome is counted per strategy and exported as
`tier1_<name>{strategy="N"}` text lines. Interests lost to an eviction or demotion are listed for the
host to tell the strategies (`on_tier1_revoked`); holds are not revoked by a demotion that a tape forces.

## Consequences

- Capacity is a number to set (`max_tier1`) for the strategy count; the rule decides who gets it when
  it is not enough, and the denial counts say how often that happened and to whom.
- Holds protect positions, but only if strategies pin. The momentum strategy does; for cross-sectional
  strategies the multi-strategy host derives holds from the gateway's orders and positions (E18-S05).
- A strategy-caused eviction is applied inside the callback live but arrives before the event in a
  replay; code that inspects the evicted symbol in the same callback can see different state. The
  replay-equivalence work (E18-S06) probed it: a replay by recomputation has no such gap, and a follower holds the live promoter's symbols after every event (ADR 0047).
- Rejected: a scanner hit evicting an interest (the market's heat does not outrank a strategy's
  stated need), and priority by recency (it would let the busiest strategy crowd out the rest).
