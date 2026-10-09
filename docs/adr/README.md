# Architecture decision records

An ADR records one decision that is expensive to reverse, and why it was made,
so the reasoning outlives the conversation. Write one when a change:

- picks a language, store, protocol or wire format;
- adds a rule other code must follow (or relaxes one, such as allowing `unsafe`);
- trades something away that a later reader would otherwise "fix".

How:

1. Copy [`0000-template.md`](0000-template.md) to `NNNN-short-title.md` with the next number.
2. Fill it in. Keep it to a page; link code and other ADRs instead of repeating them.
3. Open it as a normal PR (`TIC-n: ...`). The PR is the review.
4. Never edit an accepted ADR to change the decision. Write a new one that
   supersedes it, and set the old one's status to `Superseded by NNNN`.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-rust-for-the-hot-path.md) | Rust for the hot path | Accepted |
| [0002](0002-no-redis-on-the-hot-path.md) | No Redis on the hot path | Accepted |
| [0003](0003-determinism-rules.md) | Determinism rules | Accepted |
| [0004](0004-versioned-event-encoding.md) | Versioned event encoding and event identity | Accepted |
| [0005](0005-security-master-identity.md) | Security master: stable ids, dated tickers, dated provider keys | Accepted |
| [0006](0006-raw-tape-format.md) | Raw tape format and the zstd dependency | Accepted |
| [0007](0007-paced-replay-waits-inside-poll.md) | Paced replay waits inside `poll`, on an injected pacer | Accepted |
| [0008](0008-benchmarks-are-informational-and-tracked-per-commit.md) | Benchmarks are informational and tracked per commit | Accepted |
| [0009](0009-run-manifests-and-content-addressed-results.md) | Run manifests and results addressed by the manifest hash | Accepted |
| [0010](0010-strategies-are-event-driven-and-cannot-touch-the-outside-world.md) | Strategies are event-driven and cannot reach the clock or the outside world | Accepted |
| [0011](0011-simulated-broker-fills-against-recorded-quotes.md) | The simulated broker fills against recorded quotes, with latency | Accepted |
| [0012](0012-risk-gateway-decides-and-limits-are-fixed.md) | The risk gateway decides every order, and its limits are fixed | Accepted |
| [0013](0013-backtest-reports-are-integer-and-state-their-blind-spots.md) | Backtest reports are integer, comparable, and state their blind spots | Accepted |
| [0014](0014-tier-1-state-is-bounded-and-features-are-read-on-demand.md) | Tier 1 state is bounded, and pullback features are read on demand | Accepted |
| [0015](0015-strategy-1-long-side-waits-for-the-bounce.md) | Strategy 1, long side: classify on features, wait for the bounce, trail the high | Accepted |
| [0016](0016-the-backtest-loop-puts-the-gateway-between-strategy-and-broker.md) | The backtest loop puts the gateway between the strategy and the broker | Accepted |
| [0017](0017-multi-timeframe-bars-are-built-live-for-a-bounded-set.md) | Multi-timeframe bars are built live for a bounded set of symbols | Accepted |
| [0018](0018-indicators-are-small-copy-integer-types-checked-against-exact-references.md) | Indicators are small, Copy, integer types checked against exact references | Accepted |
| [0019](0019-the-example-indicator-strategy-and-what-it-shows.md) | The example indicator strategy, and what it shows | Accepted |
| [0020](0020-parameter-changes-are-bounded-events-that-apply-to-new-entries-only.md) | Parameter changes are bounded events that apply to new entries only | Accepted |
| [0021](0021-a-fixed-parameter-shadow-runs-beside-every-tuned-strategy.md) | A fixed-parameter shadow runs beside every tuned strategy | Accepted |
| [0022](0022-the-safety-policy-returns-a-losing-tuned-side-to-baseline.md) | The safety policy returns a losing tuned side to baseline | Accepted |
| [0023](0023-the-scanner-scores-volume-against-each-symbols-own-baseline.md) | The scanner scores volume against each symbol's own baseline | Accepted |
| [0024](0024-tier-moves-are-hysteretic-bounded-and-on-the-tape.md) | Tier moves are hysteretic, bounded, and on the tape | Accepted |
| [0025](0025-the-strategy-records-why-it-acted-and-the-viewer-only-reads.md) | The strategy records why it acted, and the viewer only reads | Accepted |
| [0026](0026-entry-rules-are-data-and-the-decision-records-each-condition.md) | Entry rules are data, and the decision records each condition | Accepted |
| [0027](0027-the-explorer-replays-stored-runs-and-checks-them-first.md) | The explorer replays stored runs and checks them first | Accepted |
| [0028](0028-strategies-take-tier-one-from-the-shared-promoter.md) | Strategies take Tier 1 from the shared promoter | Accepted |
| [0029](0029-rule-edits-are-reviewed-before-anything-uses-them.md) | Rule edits are reviewed before anything uses them | Accepted |
| [0030](0030-the-order-ledger-is-an-append-only-log-of-gateway-inputs.md) | The order ledger is an append-only log of gateway inputs | Accepted |
| [0031](0031-budgets-are-reserved-hierarchical-and-enforced-by-the-gateway.md) | Budgets are reserved, hierarchical, and enforced by the gateway | Accepted |
| [0032](0032-the-gateway-keeps-a-sub-account-per-strategy.md) | The gateway keeps a sub-account per strategy | Accepted |
| [0033](0033-the-rebalance-moves-realised-profit-into-the-strategy-that-made-it.md) | The rebalance moves realised profit into the strategy that made it | Accepted |
| [0034](0034-the-workspace-service-only-reads-and-reads-the-ledger-without-its-lock.md) | The workspace service only reads, and reads the ledger without its lock | Accepted |
| [0035](0035-budget-edits-are-requests-in-an-inbox-that-the-engine-records.md) | Budget edits are requests in an inbox that the engine records | Accepted |
| [0036](0036-agents-may-cut-risk-on-their-own-but-raising-it-needs-a-person.md) | Agents may cut risk on their own, but raising it needs a person | Accepted |
| [0037](0037-the-alpaca-adapter-is-a-translation-over-a-transport-and-never-retries-an-order-blind.md) | The Alpaca adapter is a translation over a transport and never retries an order blind | Accepted |
| [0038](0038-databento-records-are-decoded-by-the-dbn-crate-into-canonical-events.md) | Databento records are decoded by the `dbn` crate into canonical events | Accepted |
| [0039](0039-the-feed-never-waits-for-the-engine-and-what-is-given-up-is-decided-by-what-it-is-worth.md) | The feed never waits for the engine, and what is given up is decided by what it is worth | Accepted |
| [0040](0040-raw-capture-keeps-the-providers-records-in-files-that-are-complete-or-recoverable.md) | Raw capture keeps the provider's records in files that are complete or recoverable | Accepted |
| [0041](0041-a-universe-is-a-spec-judged-from-a-dated-snapshot-and-kept-with-the-run.md) | A universe is a spec judged from a dated snapshot, and the list is kept with the run | Accepted |
| [0042](0042-reference-rows-come-from-dated-daily-bars-and-an-asset-list-and-say-what-they-lack.md) | Reference rows come from dated daily bars and an asset list, and say what they lack | Accepted |
| [0043](0043-a-strategy-over-many-symbols-reviews-a-member-view-on-a-grid-of-event-time.md) | A strategy over many symbols reviews a member view on a grid of event time | Accepted |
| [0044](0044-tier-1-is-shared-by-claims-with-holds-that-cannot-be-taken-and-interests-that-can.md) | Tier 1 is shared by claims, with holds that cannot be taken and interests that can | Accepted |
| [0045](0045-brokers-speak-one-interface-and-their-events-are-checked-by-one-rule.md) | Brokers speak one interface, and their events are checked by one rule | Accepted |
| [0046](0046-the-host-runs-many-strategies-through-one-gateway-and-stops-one-without-stopping-the-rest.md) | The host runs many strategies through one gateway and stops one without stopping the rest | Accepted |
| [0047](0047-a-day-is-checked-by-replaying-its-capture-and-comparing-decision-logs.md) | A day is checked by replaying its capture and comparing decision logs | Accepted |
| [0048](0048-the-daily-report-is-a-plain-reading-that-says-what-was-not-measured.md) | The daily report is a plain reading that says what was not measured | Accepted |
| [0049](0049-the-live-feed-is-a-small-synchronous-client-not-the-official-async-one.md) | The live feed is a small synchronous client, not the official async one | Accepted |
| [0050](0050-one-engine-thread-takes-the-queue-and-the-feed-thread-keeps-the-bytes.md) | One engine thread takes the queue, and the feed thread keeps the bytes | Accepted |
| [0051](0051-strategy-ideas-are-researched-and-graded-first-and-a-refinement-is-tested-against-its-plain-version.md) | Strategy ideas are researched and graded first, and a refinement is tested against its plain version | Accepted |
| [0052](0052-the-calendar-is-a-daylight-saving-rule-and-a-table-of-closures-with-no-time-zone-database.md) | The calendar is a daylight-saving rule and a table of closures, with no time-zone database | Accepted |
| [0053](0053-session-state-is-a-second-array-placed-by-arrival-time.md) | Session state is a second array beside Tier 0, placed by arrival time | Accepted |
| [0054](0054-bars-are-shared-by-claims-aligned-to-the-session-by-the-calendar-and-read-through-the-strategys-own-claim.md) | Bars are shared by claims, aligned to the session by the calendar, and read through the strategy's own claim | Accepted |
| [0055](0055-history-columns-come-from-minute-bars-with-capped-wicks-and-a-strategy-can-require-them.md) | History columns come from one-minute bars with capped wicks, and a strategy can require them | Accepted |
| [0056](0056-protective-orders-in-the-simulator-extended-hours-rules-and-exits-the-strategy-holds.md) | Protective orders in the simulator, extended-hours rules, and exits the strategy holds | Accepted |
| [0057](0057-the-short-sale-restriction-comes-from-every-status-record-and-the-simulator-follows-the-brokers-short-rules.md) | The short-sale restriction comes from every status record, and the simulator follows the broker's short-sale rules | Accepted |
| [0058](0058-history-is-kept-as-the-providers-own-daily-files-with-a-manifest-and-replays-as-a-capture-does.md) | History is kept as the provider's own daily files with a manifest, and replays as a capture does | Accepted |
| [0059](0059-research-runs-are-one-host-pass-over-stored-days-with-a-stored-cost-model-and-a-record-per-round-trip.md) | Research runs are one host pass over stored days, with a stored cost model and a record per round trip | Accepted |
| [0060](0060-results-are-judged-by-the-day-with-a-bootstrap-a-registry-of-every-variant-tried-and-a-deflated-sharpe-ratio.md) | Results are judged by the day, with a block bootstrap, a registry of every variant tried, and a deflated Sharpe ratio | Accepted |
| [0061](0061-the-closing-reversal-buys-the-days-biggest-losers-at-half-past-three-with-every-time-from-the-calendar-and-a-disaster-stop-the-framework-requires.md) | The closing reversal buys the day's biggest losers at half past three, with every time from the calendar and a disaster stop the framework requires | Accepted |
| [0062](0062-the-null-strategy-is-random-names-at-random-times-with-the-same-exits-run-once-a-seed-in-one-pass-and-its-seeds-are-controls-not-trials.md) | The null strategy is random names at random times with the same exits, run once a seed in one pass, and its seeds are controls, not trials | Accepted |
| [0063](0063-a-cross-strategy-records-why-it-acted-as-a-table-the-host-collects-beside-its-decision-log.md) | A cross strategy records why it acted as a table the host collects beside its decision log | Accepted |
| [0064](0064-a-day-keeps-its-log-its-traces-and-the-market-around-its-trades-beside-its-trips.md) | A day keeps its log, its traces and the market around its trades beside its trips | Accepted |
| [0065](0065-the-backtest-view-reads-a-results-directory-through-the-program-that-hosts-the-service-and-only-reads.md) | The backtest view reads a results directory through the program that hosts the service, and only reads | Accepted |
| [0066](0066-a-trade-is-replayed-from-its-days-files-in-one-page-that-draws-only-what-has-happened-by-the-cursor.md) | A trade is replayed from its day's files in one page that draws only what has happened by the cursor | Accepted |
| [0067](0067-the-replay-page-shows-the-strategy-as-configured-and-what-each-fill-was-asked-for.md) | The replay page shows the strategy as configured and what each fill was asked for | Accepted |
| [0068](0068-a-replayed-day-is-a-live-day-the-host-over-a-file-ledger-in-the-days-own-directory-with-its-daily-report.md) | A replayed day is a live day: the host over a file ledger in the day's own directory, with its daily report | Accepted |
| [0069](0069-the-workspace-reads-a-replayed-days-ledger-as-it-reads-a-live-one.md) | The workspace reads a replayed day's ledger as it reads a live one | Accepted |
| [0070](0070-the-trade-page-shows-what-the-engine-did-with-tier-1-from-the-days-log.md) | The trade page shows what the engine did with Tier 1, from the day's log | Accepted |
| [0071](0071-research-and-the-live-day-read-one-strategy-set-and-research-runs-a-store-day-by-day.md) | Research and the live day read one strategy set, and research runs a store day by day | Accepted |
| [0072](0072-tf-live-certifies-checks-and-runs-a-day-from-one-config-and-refuses-before-it-touches-the-gateway.md) | `tf live` certifies, checks and runs a day from one config, and refuses before it touches the gateway | Accepted |
| [0073](0073-the-dev-environment-is-one-namespace-on-the-homelab-cluster-with-two-volumes-and-two-daily-jobs.md) | The dev environment is one namespace on the homelab cluster, with two volumes and two daily jobs | Accepted |
| [0074](0074-a-stored-days-instruments-are-named-from-the-vendors-symbology-kept-beside-the-days-file.md) | A stored day's instruments are named from the vendor's symbology, kept beside the day's file | Accepted |
| [0075](0075-a-range-of-days-is-run-by-one-job-that-pulls-what-is-missing-and-the-workspace-has-a-fixed-lan-address.md) | A range of days is run by one job that pulls what is missing, and the workspace has a fixed LAN address | Accepted |
| [0076](0076-a-premarket-volume-spike-and-the-first-small-pullback-in-its-run-is-bought-long-with-exits-the-strategy-holds.md) | A premarket volume spike, and the first small pullback in its run, is bought long with exits the strategy holds | Accepted |
