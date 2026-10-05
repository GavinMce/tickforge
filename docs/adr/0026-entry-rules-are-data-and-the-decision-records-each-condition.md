# 0026. Entry rules are data, and the decision records each condition

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-113

## Context

MomentumLong's entry decision was code: five tests spread over three `if` blocks, with a
separate re-implementation of them in the exporter to show the evidence. Changing which
features matter meant changing Rust, and the evidence could drift from the decision.

## Decision

`tf_strategy::rules::RuleSet` holds the decision as four stages of conditions (`too_old`,
`armed`, `dangerous`, `enter`), each `all` or `any` of `feature comparison threshold`. The
strategy asks them in that order once a second per watched symbol. `RuleSet::momentum()`
is the built-in set and reproduces the previous code exactly.

- **Thresholds** are literals, or `@name` of a tunable parameter. Named thresholds follow
  the parameter store, so bounds, step limits, audit and auto-revert apply unchanged.
  Literals are fixed for the run. Changing the structure of the rules is a new rule-set
  version, not a store change.
- **Missing data fails the condition**, for every comparison. An empty `all` is true, an
  empty `any` is false, and `enter` must have a condition.
- **Text form** is line based with a `rules v1` header; `render()` is canonical and
  `fingerprint()` (FNV-1a 64) identifies a version in records and in the backtest manifest.
- **Evidence** comes from the same conditions: every entry and decline stores
  every condition's value, resolved limit and verdict plus the rule fingerprint. The exporter
  prints that record instead of recomputing anything.
- `tf backtest --rules FILE` runs a rule set; the fingerprint is part of the manifest.

## Consequences

- Equivalence with the old code was checked by comparing 72 report outputs and 12 exports
  (25 trades, 60 declines) from before and after, byte for byte, plus mutation tests.
- The features, and which stages exist, are still code. A new feature is a code change.
- Rule edits by an agent are not yet proposed through the store: only named thresholds are.
  Editing structure needs its own review path (a later E16/E12 story).
- Rules apply to MomentumLong only. TrendLong keeps its parameters.
