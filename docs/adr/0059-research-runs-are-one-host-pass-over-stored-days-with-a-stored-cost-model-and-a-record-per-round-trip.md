# 0059. Research runs are one host pass over stored days, with a stored cost model and a record per round trip

- Status: Accepted
- Date: 2026-10-07
- Jira: TIC-167

## Context

Every strategy of the plan is judged on twelve months of history, through the same engine and gateway it will run on
live, with the costs it will really pay (`docs/research/data-and-protocol.md`). A result is only worth anything if it can
be made again, if it says what it was made under, and if a day run for research decides what the live driver's own
replay check decides on the same day. There are twenty strategies and many variants of each: the data is read once, not
once per strategy.

## Decision

- **One host, one pass, per day** (`tf_host::research`). A day's files are read once through one `Host` that holds every
  definition, the simulated broker filling against the recorded quote after the latency. The host is built the way a replay
  builds it (`replay_host`), the definitions are installed as the replay installs them, the events pass the same
  de-duplication and the instruments are named by the same first pass (`learn_symbols`, which `replay_files` now uses
  too). The day's session boundaries come from the calendar and are given to the host as `HostConfig::day`, so a replay
  given the same configuration sees the same sessions: a test replays a research day's decision log through
  `replay_files` and gets `Equal`, and gets a difference with another latency.
- **A day is the unit.** A fresh host, an empty ledger, the day's own reference snapshot (given per day by a `DaySource`:
  what was known before that session). Nothing is carried overnight. A trip still open when the day's events end is closed at
  the last trade at the fees a sale of it would pay and carries `exit_reason` `0xFFFF` and the flag `open_at_end`, so a
  day's numbers never hide a position.
- **A round trip is flat to flat**, per strategy and instrument (`trips.rs`): the executions the host noted (`FillNote`:
  who, order, side, purpose, the intent's reason, quantity, price, time, reference price, stop) taken in order. Entry
  and exit are volume-weighted prices of the executions; gross, fees, borrow, net, slippage are exact integers in raw
  price units; basis points and R are in hundredths of a basis point and thousandths of R, truncated. A fill that goes through
  flat splits and the rest begins another trip. Slippage is, per execution, what was paid above or received below the
  price the intent was nearest to, times shares: positive is worse. **R** is net over shares times the distance from the
  entry to the stop of the opening intent's protective orders; a strategy that holds its stop itself (ADR 0056) states none, so
  its R is absent until the strategies report their initial risk (E19-S18).
- **The cost model is data** (`CostModel`): latency, borrow basis points a year, the Section 31 rate by effective date and
  the FINRA Trading Activity Fee rate and per-execution cap by effective date. It is rendered as text, fingerprinted and
  stored whole in the results. Rates as published: Section 31 $27.80 per million through 13 May 2025, $0.00 from 14 May
  2025 ([SEC advisory 2025-2](https://www.sec.gov/rules-regulations/fee-rate-advisories/2025-2)), $20.60 from 4 April 2026
  ([2026-2](https://www.sec.gov/rules-regulations/fee-rate-advisories/2026-2)); the Trading Activity Fee $0.000166 a
  share up to $8.30 in 2024 and 2025, $0.000195 up to $9.79 in 2026, $0.000232 up to $11.61 in 2027
  ([FINRA fee adjustment schedule](https://www.finra.org/rules-guidance/rule-filings/sr-finra-2024-019/fee-adjustment-schedule)).
  Only sales pay (a long's exit, a short's entry); fees are exact, not rounded to a cent per trade. **A date the table does
  not cover is refused, never given the nearest rate**: the table starts on 14 May 2025 for Section 31 and says it is known through
  30 September 2026 (the end of that fiscal year), so the rate from 1 October 2026 has to be looked up and added; before a
  day is run, both fees are looked up for it. Borrow is charged on a short of a name that is not easy to borrow, for the
  time held, and is zero by default (the broker charges nothing on easy-to-borrow names).
- **Results are a directory.** `research.cfg` holds what each definition is (number, fingerprint, name, parameters, universe)
  and the host's limits and engine settings (a fingerprint), then the cost model, whole. A file per day, `<date>.trips`:
  the configuration fingerprint and an identifier of the data it came from, the event count, the host's outcome hash, the
  counts of anomalies, gateway rejections and broker refusals, and a record per trip, tab separated, ending in a
  checksum. A day is written whole beside its name and renamed into place. **A result without its configuration is
  refused**: `Results::open` fails when `research.cfg` is missing or does not read; a run refuses a directory that has days
  and no configuration, or a configuration other than its own; reading refuses a day made under another configuration
  or damaged.
- **Resume at a day boundary.** A run asks the source for each day's data identifier (for a store, the checksums of its
  files), skips the days that are there, whole, under this configuration, for the same data, and runs the rest in order,
  so a run that stopped (a missing file, a crash) goes on from the first day it did not finish and leaves what an
  uninterrupted run leaves, byte for byte. A day that cannot be run stops the run with the days before it written.
- **Refusals are counted.** A strategy whose orders all end at the gateway or the broker makes no trips, which looks like
  no edge. Each day's file says how many intents the gateway rejected and how many orders the broker refused (the first
  real run of a placeholder strategy found its protective orders refused in the premarket this way).
- **Unreadable data is an error.** `CaptureReplay` ended its stream silently at a file it could not open or decode,
  which would have made a shorter day look like a day; it now says why (`failure()`), the runner turns that into an
  error for the day, and `replay_files` does the same.
- **The screening pass** (`tf_history::screen`, `tf history screen`): from the one-minute bars of a store and, where a quote
  schema is stored, the quoted spread over the mid at each two-sided quote, the symbols that meet every limit given (last
  price range, dollars traded, minutes traded, mean spread). It selects on nothing from a bar's high or low (ADR 0055).
  It is the first stage: it names the symbols for the full pass.

## Consequences

- Twenty definitions cost one read of the data. The cost of a run is the host's own: measured on one real day of two
  symbols (984,067 events from 607,775 stored records), about 620,000 events a second including the naming pass.
- The runner takes definitions and a `DaySource`; the command that builds definitions by name is waiting for the first
  strategies (E19-S18), and a store-backed source is in the example `research_real` until the command needs one.
- A store's day is a UTC day (ADR 0058). In winter New York's after-hours ends at 01:00 UTC, so a day file ends an hour
  before that session does and begins with the last hour of the one before; the runner processes the file as the day it
  is named for and does not drop that hour. Not yet looked at on a stored winter day.
- Holding no position overnight is a property of the research runner, not of the live host: a strategy that carries
  positions is not measured here.
- The regulatory fee table has to be kept up to date; the refusal makes forgetting loud.
