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
  summary. A change beyond the thresholds is flagged with `!!` as a prompt to
  look, not a verdict. The thresholds are an argument: the defaults (10% on
  throughput, 25% on p99) suit a quiet machine, where repeated runs agree to
  about 5%; the CI workflow passes 60% and 150%.
- A p99 rise is flagged only if it is also at least 25 ns: a latency going from
  1 ns to 2 ns is +100% of timer noise.

## What shared runners actually do

Measured on the first runs of the workflow: the same code gave Tier 0 throughput
of 24, 25, 27, 31, 47 and 50 million events/s on six runs, and the empty loop
71 to 159 million. The pool evidently has fast and slow machines, so run-to-run
variation is about 2x, not the ~5% seen locally. Thresholds of 10% and 25% flagged
every scenario of a docs-only PR; 60% and 150% sit just beyond the spread seen in
those six runs. Normalising by the empty-loop floor did not help (Tier 0 is
memory-bound, the empty loop is not), so CI can only catch changes of roughly
2.5x or more. Revisit with more data.

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
