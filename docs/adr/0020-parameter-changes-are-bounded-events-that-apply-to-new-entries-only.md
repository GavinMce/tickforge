# 0020. Parameter changes are bounded events that apply to new entries only

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-86

## Context

DESIGN.md allows live parameter tuning by agents with guardrails: bounds, rate
limits, new entries only, every change on the tape, risk limits out of reach. Those
need to be code, not a convention.

## Decision

- **`tf-params::ParamStore`** holds typed parameters. Each declares `baseline`,
  `min`, `max`, `max_step` (zero = cannot change), `cooldown` and `scope` (global
  only, or overridable per instrument). A proposal is refused with a reason if the
  parameter is unknown, frozen, out of scope, out of bounds, a step too large from
  the current value, inside the cooldown of its last change at the same target, or
  no change at all.
- **A change is an event.** `ParamStore::check` validates and returns the
  `ParamChange` event (new in the encoding, schema v3, tag 7, 53 bytes: parameter
  index, scope, proposer, reason code, new value, evidence id) without changing
  anything. `ParamStore::apply` is the only mutation, and it re-checks everything,
  so the live path (check, put on the tape, apply) and a replay (apply each event
  from the tape) are the same code, and two proposals checked against the same
  state cannot both land if together they break a rule. The old value is not
  carried; it is what the previous change left. Free text for a reason belongs in a
  journal keyed by the event's `seq`.
- **Schema v3.** Events of v1 and v2 layouts are unchanged and decode as before; a
  v1 or v2 stream containing the new tag is corrupt. `ProviderId::Internal` marks
  events the system generates itself, and subscriptions never filter them out.
  Tier 0 and the statistics sink ignore them for per-instrument purposes.
- **New entries only.** Every applied change bumps `revision`. `MomentumLong` takes
  the parameters that govern a position after entry (trail, collar, maximum hold)
  when the entry is *decided*, not when it fills, and keeps them in the position;
  the stop buffer and size are already fixed in the order. Thresholds that decide
  whether to enter are read live, which affects only decisions not yet made.
- **What may be tuned** (`momentum::tunable_specs`): seventeen thresholds and
  execution knobs with bounds chosen so that no combination of allowed values is an
  invalid parameter set (a test draws 500 random corner combinations). The price
  filter, position and watch caps, cooldown and everything in the risk gateway are
  not in the store.
- **The host** applies change events to its store before the strategy sees the
  event; an event the store refuses (a tape that disagrees with the declarations)
  is counted in `Host::param_errors` and changes nothing.

## Limits

- The store does not stop a proposer from sending many valid proposals one cooldown
  apart; the bound is on rate and size, not on direction. The shadow baseline and
  auto-revert (E12-S02, S03) are what judge whether tuning helps.
- History is kept in memory and grows with changes.
- Nothing yet authenticates the proposer; that is the agent layer's job
  (E12-S06, E12-S09). The store only guarantees that whoever asks is held to the
  declared bounds.
