# 0062. The null strategy is random names at random times with the same exits, run once a seed in one pass, and its seeds are controls, not trials

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-182

## Context

Every result of the harness is held against doing nothing clever (`docs/research/data-and-protocol.md`, section 3, item 3): entries
at random times on random names of the same universe, with the same exits and the same costs, repeated many times. It shows what
the exits and the costs alone do, and gives a result a distribution to beat. The statistics of ADR 0060 take a list of null results
(`against_null`); this story makes them.

## Decision

- **A strategy, `RandomEntries`** (`tf-strategy`), built like the closing reversal (ADR 0061) on the same pieces. At its first review
  of a day it draws, from a seeded generator, `names` distinct members and, for each, an entry time uniform to the second between
  `window_start_minutes` and `window_end_minutes` before the close; one timer each. The generator is SplitMix64 (`tf_stats::rng`)
  started from the seed and the day's close, so a seed gives a day the same draw every time and another seed or another day another.
  The draw is the first `k` of a shuffle of the members, so every member is as likely as any.
- **A window of one instant is a null with no random time.** The null of a rule that buys at one time (T04 at 15:30) is a strategy that
  buys *random names at that time*, and that is the default: twenty names at 15:30, held until 15:59:30. A wide window is the null of a
  rule that can enter at any time of the afternoon.
- **What is not random is what the reference has.** A name is bought only if it could be (not halted or paused, not restricted, a two-sided
  quote, a price that buys a share and a stop above nothing: the closing reversal's checks), so the null is not charged for names nobody
  could trade; skipped draws are counted. The order is the same marketable collar around the ask for the same dollars with the same
  disaster stop (the framework requires a stop on an open in the regular session). The exits are the `ExitBook`'s: a time exit `hold_seconds`
  after the fill but never later than 30 seconds before the close, and, for a reference that has them, a stop and a target as permille from
  the average fill price (0 for none). The host flattens what is left. Costs are the cost model's, as for any variant.
  The parameters have to be set to the reference's for the comparison to be fair; nothing checks that they are.
- **One pass for all seeds.** `research::null::null_defs` makes one definition per seed (numbered from a given number, named `{prefix}{seed}`,
  a seed twice refused), and the runner reads each day once for all of them (ADR 0059), so a hundred seeds are a hundred small strategies
  over one read of the data.
- **The seeds are controls, not trials.** They are **not** entered in the trial registry (ADR 0060): a hundred of them would raise the number
  of variants tried and deflate the Sharpe ratio of every real strategy for nothing. `null_distribution` takes the figures of the seeds
  through a registry made for the call and thrown away, so the trial registry is unchanged by asking.
- **The distribution** (`NullDistribution`): the per-trade mean in basis points of each seed that traded, their average (what random
  entries with these exits earn after costs), the same weighted by trades, and the spread of the seeds' means; `against(observed)`
  places a result among them by `tf_stats::against_null` (`(k + 1) / (seeds + 1)`, so eight seeds cannot give a p-value below 1/9). A
  directory in which no seed traded gives no distribution, not a distribution of nothing.
- **The cost of the exits and the spread alone, by hand.** In a market with nothing in it every null trade buys at the ask and sells at the
  bid: at a quote of 19.99 / 20.01, 99 shares for $2,000, a gross of -$1.98, fees of $0.060072606 (Section 31 at $20.60 a million on
  $1,979.01 and 99 shares at $0.000195) and a net of -$2.040072606 on $1,980.99, which is **-10.29 basis points** a trade, exactly
  (a test), whatever the seed. In a market that moves, the seeds differ by which names they drew.

## Consequences

- The null says what the exits and costs do; it does not say a reference is good. A strategy with a mean above the null's by the null's own
  spread is a candidate (the gate of E19-S29 applies the rest); one with a mean inside it has shown nothing.
- Per-trade means from a few trades are noisy: a seed with three names a day over twelve months is about 750 trades, a standard error
  of a few basis points on a 68 basis point spread of outcomes. The number of seeds sets the resolution of the p-value.
- A null that matches a reference with a stop or a target relies on the stop and target being given as distances from the fill, which
  is how the `ExitBook` takes them; a reference whose exits depend on something else (a signal, a trailing level) has no null here.
- `tf-strategy` now depends on `tf-stats` for the generator (a pure crate with no dependencies).
- Tests: 11 of the strategy from scripted events (the draw repeats by seed and day, distinct members only, every member as likely, the
  window used to both ends, skips, the order, exits from the fill with the margin, stop and target, parameters, day roll) and 5 through
  the host (the flat market above, a moving one varying with the seed, repeating with the same seed, asking for a subset, definitions,
  replay equal).
