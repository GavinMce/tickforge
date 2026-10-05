# 0023. The scanner scores volume against each symbol's own baseline

- Status: Accepted
- Date: 2026-10-06
- Jira: TIC-58

## Context

Tier 1 holds about fifty symbols out of thousands, so something cheap must watch
all of them and say which few deserve it. A fixed volume threshold is wrong for
almost every symbol: what is a surge for a name that trades 600 shares in ten
seconds is noise for one that trades 60,000.

## Decision

`tf_engine::Scanner` reports a **hit** for a symbol when, at once:

- its volume over the last 10 s has a z-score above `min_z_milli` (8 standard
  deviations by default) against that symbol's own exponentially weighted baseline
  of the same 10 s volume, and is at least `min_volume` shares;
- its price rose at least `min_change_permille` over `spike_secs`;
- it passes the universe filters: price range, spread (a quote is required) and
  float.

Details that matter, each covered by a test:

- The baseline takes a sample every 10 s but **not while the symbol already looks
  like a spike**, so a run does not teach it that runs are normal; a second spike
  minutes later scores about as high as the first.
- The deviation under the z-score has a **floor** (15% of the baseline mean), so a
  very steady symbol does not turn a 30% wobble into a signal.
- A symbol is **not scored until it has six baseline samples** (a minute of data).
- At most **one evaluation per symbol per second**, from Tier 0's windows, with no
  allocation.
- **Float** comes from a table the caller fills (`set_float`); the source is
  E14-S01. An unknown float passes unless `require_float`.
- The scanner only reports. Promotion and demotion, with hysteresis, is E07-S05.

## Measured

On the synthetic scenarios, with the defaults:

- every healthy and dangerous runner, halt-up and squeeze, over four lead-ins and
  ten seeds each, and every spike of a three-spike name, is detected, none before
  its catalyst and the first within three seconds of it;
- in 400-symbol universes with 5% runners (symbols trade every 0.2 to 5 s) every
  runner with a minute of baseline is hit and no quiet symbol is;
- 200 symbol-hours of quiet symbols give **zero** hits. With the volume gates nearly
  removed the same data gives dozens, so the zero is not an artefact.

The price-rise threshold ended up at 5 permille, not 3%: stocks near $20 in the
generator run hard on less than 3% in ten seconds, and 30 permille missed a quarter
of the runners. The volume gates do the discriminating.

## Limits

- A symbol that opens with a gap has no baseline to be compared with and cannot
  be a hit; premarket baselines (E07-S03) are the answer to that.
- Quiet and runner behaviour is the generator's; recorded data will need its own
  thresholds, and the false-positive rate must be remeasured on it.
- Nothing in the synthetic data has a float, so the float filter is tested with
  hand-set values only.
