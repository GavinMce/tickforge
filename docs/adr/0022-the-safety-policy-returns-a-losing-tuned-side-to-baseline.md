# 0022. The safety policy returns a losing tuned side to baseline

- Status: Accepted
- Date: 2026-10-05
- Jira: TIC-88

## Context

Bounds and cooldowns limit how fast an agent can tune, not whether the tuning is
any good. The shadow (ADR 0021) shows whether it helped; something must act on
that without waiting for a person.

## Decision

- **The measure** is the *relative curve*: tuned equity minus shadow equity, which
  starts at zero and moves only when the two behave differently. The **drawdown
  versus the shadow** is the fall from the relative curve's highest point
  (`tf_params::RevertPolicy`). A shadow that is also losing is no reason to act.
  When the drawdown reaches the configured limit the policy trips and re-arms from
  the current relative value, so one loss is not counted twice.
- **The action** is `ParamStore::revert_events`: for every parameter whose value
  differs from its baseline, and every per-instrument override, a `ParamChange`
  event back to baseline. They are ordinary tape events, applied through the same
  `apply` as everything else, so a session with a revert replays exactly.
- **The policy has its own authority.** Its events carry the reserved proposer
  `PROPOSER_POLICY`. `check` refuses that id, so an agent cannot use it, and `apply`
  accepts it only for a return to baseline, exempt from the step size and
  cooldown (a revert must be able to undo several steps at once). A per-instrument
  revert clears the override, so the instrument follows the global value again.
- **Lockout.** After a revert the store refuses all other changes for a configured
  time, so the agent cannot immediately re-apply what just lost. The lockout is part
  of the store's configuration, so replaying the tape reproduces it.
- **Open positions** keep the parameters they were entered with (ADR 0020); a revert
  governs new entries. A trip when nothing is tuned is not a revert and is not
  recorded.
- `tf backtest --propose ... --revert-drawdown USD [--lockout SECS]` runs it; both
  are in the stored result's manifest. The exports gain `params.reverts` and
  `params.reverted`.

## Limits

- The trigger is a loss in P&L against one shadow on one feed; a few trades make it
  noisy. The limit is a safety net, not a statistical test.
- It reverts to the declared baseline, which is assumed good. It does not search for a
  better value.
- It sits in the backtest and simulation loop. The live equivalent needs the live
  engine's equity and the gateway process (E09), and the same events.
