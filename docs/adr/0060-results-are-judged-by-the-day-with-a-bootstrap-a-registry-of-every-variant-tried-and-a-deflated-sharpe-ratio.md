# 0060. Results are judged by the day, with a block bootstrap, a registry of every variant tried, and a deflated Sharpe ratio

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-168

## Context

The research runner (ADR 0059) leaves one record per round trip. Twelve months of them for a dozen variants is where an
edge is looked for (`docs/research/data-and-protocol.md`, section 3), and three things make an honest answer harder than
an average: trades on one day share the market's move, so the number of trades overstates how sure an average is; the
best of many variants looks better than it is, by chance alone; and a refinement has to be judged against the plain
rule it refines, not against nothing. The protocol asks for day-level standard errors, a count of everything tried, the
deflated Sharpe ratio, a paired comparison for refinements and the null strategy's distribution to beat.

## Decision

- **A pure crate, `tf-stats`** (no dependencies, no clock): the same trades and the same configuration give the same
  numbers. The one source of randomness is a seeded SplitMix64 whose seed is part of the configuration, so a bootstrap is
  repeatable. `tf_host::research::stats` is the glue that reads a results directory.
- **The unit is the day.** A variant's result is a ratio of totals, the sum of its trades' results over the number of
  trades, taken over every day of the run, days with no trade included as nothing (a day on which the rule found nothing is
  still a day). Per trade it is in basis points of the money put in (always there) and in R (only for trades whose opening
  order stated a stop; the count of those is reported).
- **Standard error: a circular block bootstrap of the days** (Politis and Romano 1992): blocks of consecutive days, so
  that days that resemble each other stay together, default length the cube root of the number of days rounded up,
  2,000 resamples, drawn from the seed. The standard error is the standard deviation of the resampled estimates, `t` is the
  estimate over it, and a 95 percent percentile interval comes with it. The textbook cluster-robust error for a ratio of totals is
  reported beside it as a cross-check. A resample with no trade gives no estimate and is not counted. **A spread below
  the arithmetic's own rounding is no spread**: three trades of -10.29 basis points on three days gave an error of 2e-14 and a
  t-statistic of -4e14 until that was handled, and now give an error of nothing and no t-statistic.
- **Hit rate** is the share of trades with a net result above nothing; **payoff** the average win over the average loss;
  **maximum drawdown** the deepest fall of the running total of basis points below its highest, trades in the order they
  closed (day, exit time, entry time, symbol), starting from nothing.
- **The trial registry** (`trials.reg`, a text file with a checksum, written whole through a `.part` file) lists every
  variant ever run: the fingerprint of its rule (`StrategyDef::fingerprint`), its name, and the date it was first run.
  A fingerprint is entered once and keeps its first date. The caller supplies the date (the library has no clock). Strategies
  are entered **before** they are run (`register_defs`), so a variant that was tried and made no trade is still a trial, and
  a variant that is not in the registry **cannot be reported**: `report` and `paired` refuse it by name.
- **The deflated Sharpe ratio** (Bailey and Lopez de Prado 2014) of each variant's daily totals: the probability that its true
  Sharpe ratio exceeds what the best of N unskilled trials would show, with N the size of the registry, allowing for the
  number of days and the skew and kurtosis of the daily results. The Sharpe ratio is the daily mean over the daily standard
  deviation, not annualised; the unit of the day's result is its total in basis points, which the ratio does not depend on.
  The variance of the trials' Sharpe ratios, which the expected best needs, is estimated from the variants **in the report**
  (the registry keeps no results), so it is absent for a report of fewer than two variants.
- **A refinement is a paired difference** from its plain version: the days on which both traded are resampled together and the
  difference of the two per-trade means is taken, so the market's move on those days cancels, with its own bootstrap
  standard error, interval and t. How many of the refinement's trades are signals (day, symbol, entry time) the plain version
  also traded is counted, and so is how many are not: a refinement that trades what the plain rule never did is not one.
- **The null strategy's distribution** is used as a list of results from null runs: the share at or above the result, as
  `(k + 1) / (runs + 1)`, with the median and the 95th and 99th percentiles.
- **An independent check.** `scripts/check_stats.py` is a separate implementation from the formulas, standard library
  only (its own SplitMix64 and resampling rule, `math.erfc` against the Rust's own series), and prints the numbers the
  tests pin for a fixed data set; a few are also worked by hand in the tests (the mean 53/21, the cluster error 2 sqrt(3) of a
  three-day example, a paired difference of 95/9 - 67/17).

## Consequences

- Nothing here runs a strategy or chooses a result: the command that registers, runs and reports is E19-S31; the null
  strategy that produces the distribution is E19-S28; applying the gate to every variant is E19-S29.
- The spread of the trials' Sharpe ratios comes from the variants reported together, so it depends on which are: the gate
  report (E19-S29) reports every registered variant in one run, as the protocol intends.
- The block length is a rule of thumb, not an estimate of the days' dependence (Politis and White 2004); results state the
  block they used. Twelve months is about 250 days: a standard error from a bootstrap of that few days is itself uncertain,
  which is why the cluster-robust figure is printed beside it.
- The normal distribution function is the Rust's own (a series, and a continued fraction in the tail, to double precision;
  the inverse is bisection on it) because no numerical library is a dependency; the Python check uses the C library's.
- Floating-point results that use `exp` (the distribution function) can differ in the last digit between platforms; the
  tests that pin them allow 1e-9 and the ones that pin bootstrap numbers, which use only `+ - * /` and `sqrt`, 1e-9 too.
