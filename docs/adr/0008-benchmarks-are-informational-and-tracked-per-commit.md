# 0008. Benchmarks are informational and tracked per commit

- Status: Accepted
- Date: 2026-10-04
- Jira: TIC-36

## Context

The design's latency claim is about p99 under bursts, so we need throughput and
tail-latency numbers for the run loop and a way to see them move as the code
changes. Timing on shared CI runners varies by tens of percent from one run to
the next, so a hard threshold would either flake or be set so loose it catches
nothing.

## Decision

- `tf bench` (crate `tf-bench`) measures four scenarios: the run loop with an
  empty sink, with Tier 0 as the sink, replaying a tape into Tier 0, and the
  synthetic generator into Tier 0. It checks that they processed the same events
  and reached the same Tier 0 state, and fails if not.
- Each scenario is measured in **two passes**: untimed for events per second,
  then timing every `on_event` call into a fixed-size log-linear histogram for
  p50, p99, p99.9 and max. Timing every event costs about as much as handling
  one, so the timed pass never feeds the throughput number. The timer's own
  median cost is measured and reported, and latencies are shown net of it.
- Results are JSON lines tagged with the commit; a file of them is a history.
  `--compare` shows the change against the last comparable row (same arch, OS,
  profile and workload).
- A **separate, non-required workflow** runs the benchmark on every PR and every
  push to `main`, uploads the results as an artifact (90 days), and puts the
  table, with deltas against the last successful `main` run, in the job
  summary. Changes beyond 10% on throughput or 25% on p99 are flagged with `!!`
  as a prompt to look, not a verdict.

## Consequences

- Regressions are visible per commit and per PR without a flaky gate. The cost
  is that nobody is forced to look; the flag in the PR's job summary is the
  nudge.
- The baseline is "the last successful run on main", so history lives in
  Actions artifacts and expires. For durable history, append `--out` files to
  somewhere permanent; nothing here does that automatically.
- Absolute numbers from CI runners are for trend only. For a real figure, run
  `task bench` on a quiet machine.
- Latency is time inside the sink per event, not end-to-end: it does not include
  the provider's work or queueing. The throughput figure does include the whole
  pipeline for the scenario.
