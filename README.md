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
```

`tf synth` prints event counts and a hash; identical inputs give an identical
hash on every platform. `crates/tf-synth/src/tests.rs` pins a golden hash, so any
change to the generator is a deliberate, reviewed change.

## Layout

| Crate | Role |
|---|---|
| `tf-core` | Canonical `Event` (Trade/Quote/Status), fixed-point `Px`, `Clock`/`SimClock`, stable binary encoding, FNV hash |
| `tf-provider` | `Provider` trait, `Capabilities`, `Subscription`, `ProviderError`, `Poll` contract |
| `tf-synth` | Deterministic synthetic provider: runner scenarios, connection/symbol limits, drops, dups, reordering, replay |
| `tf-replay` | `run()` loop on a simulated clock, plus sinks (hash, stats, dedupe) |
| `tf-cli` | `tf` binary |

## Rules of the road

- Engine and strategy code take time from a `Clock` or event timestamps, never
  `SystemTime`/`Instant`. Engine code does no I/O.
- Prices are fixed-point integers. Floats appear only at JSON adapter boundaries.
- No `unsafe` (forbidden workspace-wide) without an ADR.
- Determinism is a feature: do not use `rand`, floating-point transcendental
  functions or hash-map iteration order in anything that feeds a golden hash.

## Workflow (Jira)

- Epics hold larger bodies of work; stories are the actionable units.
- Jira project `TIC` ([board](https://arb-it-test.atlassian.net/jira/software/projects/TIC/boards/2/backlog)).
- Branch: `TIC-123-short-description`. Commit/PR titles start with `TIC-123:`.
- A story is done when its acceptance criteria are met and `task check` passes.
- New work goes into `docs/backlog.yaml` first, then Jira.
