# tickforge

Stock market data ingestion and an agent-assisted trading framework.
Rust hot path, Kubernetes deployment, paper trading first. Design:
[`docs/DESIGN.md`](docs/DESIGN.md). Backlog: [`docs/backlog.yaml`](docs/backlog.yaml)
(mirrored in Jira).

## Quick start

Rust is installed via rustup in `~/.cargo/bin` (not on PATH by default); the
Taskfile calls it by full path.

```sh
task check                                   # fmt + clippy -D warnings + tests
task synth ARGS="--symbols 5000 --secs 600"  # run a synthetic session through the run loop
task synth ARGS="--seed 7 --symbols 20 --secs 60 --dump 10"
task bench                                   # events/s and p50/p99/p99.9 through the run loop
task backtest                                # Strategy 1 through the risk gateway and simulated broker, with the report
task backtest ARGS="--higher-lows 0 --daily-loss 20"  # see the gateway refuse entries after a loss
task backtest ARGS="--strategy trend --healthy 2 --dangerous 2"  # the indicator example; it buys a fading runner too
task backtest ARGS="--propose min_higher_lows=2@1 --propose min_higher_lows=3@62"  # tuned vs fixed-parameter shadow, same feed
task backtest ARGS="--healthy 2 --dangerous 0 --quiet 0 --secs 700 --propose min_higher_lows=2@1 --propose min_higher_lows=3@62 --revert-drawdown 100"  # auto-revert
task synth ARGS="--store results"            # keep the result keyed by its manifest; a rerun is skipped
```

`tf synth` prints event counts and a hash; identical inputs give an identical
hash on every platform (CI checks x86_64 and aarch64). `crates/tf-synth/src/tests.rs`
pins a golden hash for the mixed universe and one per scenario, so any change to
the generator is a deliberate, reviewed change.

## Layout

| Crate | Role |
|---|---|
| `tf-core` | Canonical `Event` (Trade/Quote/Status, plus Correction/CancelError/News), fixed-point `Px`, `Clock`/`SimClock`, stable versioned binary encoding, FNV hash |
| `tf-provider` | `Provider` trait, `Capabilities`, `Subscription`, `ProviderError`, `Poll` contract |
| `tf-synth` | Deterministic synthetic provider: runner, halt-up, LULD, SSR, squeeze, gap-and-go and multi-spike scenarios, scripted news with a lead or lag, connection/symbol limits, drops, dups, reordering, replay |
| `tf-secmaster` | Persistent security master: stable `InstrumentId`s, dated tickers and provider keys, dense per-date `Session` lookups |
| `tf-tape` | Raw tape: zstd-compressed blocks of encoded events with a footer index; seek by `ts_recv`; `TapeProvider` replays a tape as a `Provider` at 1x..Nx or max speed |
| `tf-engine` | Hot-engine building blocks: `Tier0` per-symbol state arrays, allocation-free one-second bars and 1s/5s/60s rolling windows, EWMA baselines, and bounded `Tier1` rings with pullback features (depth, volume ratio, higher lows, tape speed, spread), `MtfBars` (1m/5m/15m/1h/day bars for a bounded set of symbols), and indicators (EMA, SMA, VWAP with bands, RSI, ATR, opening range, ...) |
| `tf-bench` | Throughput and p50/p99/p99.9 latency benchmarks for the run loop (`tf bench`), results as JSON lines per commit |
| `tf-manifest` | Run manifests (git sha, seed, config, params hash, data range) and results stored under the manifest's SHA-256, so reruns are skipped |
| `tf-strategy` | Strategy framework: `Intent` (side, size, limit/collar, protective orders, with validation) the order lifecycle (`OrderState`, `Order`, `Decision`, `RejectReason`), and the `Strategy` trait with its `Host` (timers, event-time `Ctx`; a clippy ban list keeps wall clocks and I/O out), `SimBroker` (backtest fills against recorded quotes with latency, slippage and borrow cost) `Report` (P&L, drawdown, slippage, hit rate, per-scenario breakdown) `MomentumLong` (Strategy 1, long side) and `TrendLong` (an example on the indicator and bar APIs) |
| `tf-risk` | Risk gateway: caps, daily loss, order rate, kill switch; explicit audited rejections; limits fixed at construction |
| `tf-params` | Bounded, rate-limited strategy parameters whose changes are tape events and apply to new entries only |
| `tf-backtest` | The backtest loop: strategy -> risk gateway -> simulated broker -> report, a synthetic demo session, and A/B runs of a tuned strategy against a fixed-parameter shadow |
| `tf-replay` | `run()` loop on a simulated clock, plus sinks (hash, stats, dedupe) |
| `tf-cli` | `tf` binary |

## Rules of the road

The reasoning is in [`docs/adr/`](docs/adr/README.md); the determinism rules are ADR 0003.

- Engine and strategy code take time from a `Clock` or event timestamps, never
  `SystemTime`/`Instant`. Engine code does no I/O.
- Prices are fixed-point integers. Floats appear only at JSON adapter boundaries.
- No `unsafe` (forbidden workspace-wide) without an ADR.
- Determinism is a feature: do not use `rand`, floating-point transcendental
  functions or hash-map iteration order in anything that feeds a golden hash.

## Workflow (Jira and GitHub)

- Epics hold larger bodies of work; stories are the actionable units. Jira
  project `TIC` ([board](https://arb-it-test.atlassian.net/jira/software/projects/TIC/boards/2/backlog)).
- New work goes into `docs/backlog.yaml` first, then Jira, then the issue key is
  written back to the yaml.
- Branch from `main` as `TIC-123-short-description`. Commit and PR titles start
  with `TIC-123:`, and the PR body links the Jira issue. PRs are squash-merged,
  so `main` has one `TIC-123: ... (#n)` commit per PR.
- Run `task check` before pushing. `main` is protected: the CI checks `check`,
  `determinism (x86_64)` and `determinism (aarch64)` must pass to merge.
- Jira status: In Progress when you start; Done when every acceptance criterion
  is met (repo settings included), which is after the merge.
- A story is done when its acceptance criteria are met and `task check` passes.
  Record that in `docs/backlog.yaml` (`status: done` plus `evidence:`), in the
  finishing PR or a small follow-up PR.
- Decisions that are expensive to reverse get an ADR in [`docs/adr/`](docs/adr/README.md).
