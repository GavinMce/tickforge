# 0041. A universe is a spec judged from a dated snapshot, and the list is kept with the run

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-140

## Context

A strategy that watches the market, not one symbol, needs a list of symbols that may be thousands
long, and the list has to be reproducible: a replay of a day must watch the very symbols the live
day watched, and a change to the rules for choosing them must be visible before it matters. The rule
layer (ADR 0026) already solved the same problem for rules: data with a text form, a canonical
rendering, a fingerprint and a diff.

## Decision

A universe is a `Spec` in a small text form, in `tf-universe`.

- **Two layers.** `static` conditions are judged once, before the session, from a reference
  `Snapshot` of prior-day data; `dynamic` keeps the top N of those by a live measurement, with a
  higher `keep` rank so a member is not dropped for a small move, re-ranked on *event time* so a
  replay re-ranks at the same moments.
- **Canonical and fingerprinted.** The same universe has one rendering and one FNV-1a fingerprint
  whatever the spelling or order of conditions. Thresholds may be `@param`s; a param must be declared,
  used, and always used as one kind of value.
- **A snapshot says what it lacks.** A column that is not in the snapshot is absent: a spec that uses
  it (or a live measure that needs it: gap needs the prior close, volume ratio the average volume)
  refuses to run. Float and short interest are absent until a source exists (E14), so a filter on them
  cannot silently pass everything. A cell left empty is unknown for that symbol and fails the condition.
- **The list is kept.** Selection yields a `Selection` text: the as-of date, the spec and snapshot
  fingerprints, the resolved params and the sorted symbols. The run stores it; a replay reads it back
  and does not recompute from whatever snapshot exists later. The snapshot's as-of date is the last
  day whose data it holds, which is how a replay can refuse a snapshot from a day it would not have
  been known.
- **Review is a diff with its effect.** `diff` lists changed parameters and conditions and, given a
  snapshot, the symbols admitted and dropped, and says when the universe *widens*, because anything
  sized against it (Tier 1 capacity, budgets) then needs another look.
- **Exact numbers.** Prices in specs and snapshots are decimal text read and written exactly
  (`Px::parse`, `Px::to_decimal`); no floating point touches a condition.

## Consequences

- Strategy sets can be compared and audited as text, and a universe's effect on a day is known before
  the day starts.
- Rejected: expressions with arithmetic or `or` between conditions. The rule layer's experience is
  that a small closed grammar is what lets a diff be trusted; a second `static` line is refused rather
  than silently combined. A universe that needs an alternative is two specs.
- Not built here: a backtest gate for a universe change, and ranking by a volume z-score. Both can be
  added as features or checks without changing the text form.
