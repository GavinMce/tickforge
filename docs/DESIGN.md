# tickforge design

Market data ingestion (~5,000 US equities) plus an agent-assisted trading
framework, built on Kubernetes. Providers: Databento (primary data), Alpaca
(execution, news, fallback data), and a synthetic provider modelled on both.

Status: design agreed 2026-10-04. Phase 1 scaffold in this repo. Work is tracked
in Jira; the source of truth for the backlog is [`backlog.yaml`](backlog.yaml).

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Hot-path language | Rust | Databento has an official Rust client; no GC pauses on the hot path |
| Latency budget | 50-300 ms tick-to-order is fine | Alpaca REST round trips dominate; microsecond tuning is wasted |
| Trading mode | Paper only, for now | Alpaca paper endpoint uses the same API |
| Data to start | Synthetic | Pipeline, determinism and strategy logic come before a vendor key |
| Agents | Tune parameters within hard bounds; never in the trade loop | LLM latency is seconds; see "Agent layer" |

## Principles

1. **One engine for live, replay and backtest.** They differ only in event
   source and clock. The engine core takes time from a `Clock`, does no I/O and
   keeps no globals, so a run is a pure function and thousands can run in parallel.
2. **Tiered attention.** 5,000 symbols do not all deserve tick-level state.
   A cheap scanner watches everything and promotes the few that matter.
3. **Agents on the slow loop.** The fast loop is deterministic code with risk
   limits no agent can override.
4. **Honest latency budget.** What matters is no GC pauses, no per-tick network
   hops and a good p99 during open-bell bursts.

## Architecture

```
Databento live ─┐                                    ┌─► Archiver ─► Parquet/MinIO ─► DuckDB
Alpaca WS ──────┼─► Ingestor (1 per provider)        │
Synthetic ──────┘   adapter → canonical Event ─► NATS JetStream (normalized events + raw tape)
                          │ (in-process channel)
                          ▼
              Engine (single writer, all in RAM)
              Tier 0: ~5,000 symbols, cheap aggregates + scanner
              Tier 1: <= ~50 promoted symbols, full ticks/quotes + features
              Strategy FSMs (reflex) ──► intents
                          │                                   ▲
                          ▼                          Agents via MCP (slow loop)
              Risk gateway / OMS ──► Alpaca trading API
                   └─ Postgres ledger
```

### Hot path

- **Canonical event**: fixed-size, `Copy`, <= 64 bytes (`tf-core`). Prices are
  `i64` fixed-point at 1e-9 (Databento's scale); no strings or floats.
- **Engine state**: arrays indexed by dense `InstrumentId`; no hashmaps in the
  hot path. Tier 0 keeps a few hundred bytes per symbol (last, bid/ask, VWAP,
  cumulative volume, rolling windows, RVOL vs a premarket baseline). Tier 1 adds
  tick/quote rings, 1 s bars and pullback features for promoted symbols.
- **Redis is not on the hot path.** The engine is the single writer; in-process
  memory beats any network store. Redis is for 1 Hz snapshots, watchlists and config.
- **Bus**: NATS JetStream feeds the archiver and out-of-process consumers.
- A shared-memory ring is added only if measurement shows a second process
  needs raw ticks at microsecond latency.

### Storage

zstd Parquet on MinIO (partitioned by date / schema / symbol bucket) plus the
provider-native raw tape so any session replays bit-exactly. DuckDB serves
agents and backtests; ClickHouse only if outgrown. Flushing is a bus consumer,
so the engine never blocks on storage.

### Synthetic provider

Implements the same `Provider` trait, limits and failure modes as the real
vendors (connection caps, symbol caps, replay-on-reconnect, drops, duplicates,
reordering). Fully deterministic: same seed, same bytes, on every platform
(integer arithmetic only, own PRNG).

## Strategies

**Strategy 1 (low-float momentum).** Tier 0 scans for a volume z-score spike
plus a price spike, filtered by float, price and spread; a hit promotes the
symbol. Per symbol: `Spiked -> Watching -> HealthyPullback -> Long -> Riding`
or `DangerousPullback -> ShortCandidate -> Short`, then `Cooldown -> re-arm`
(many names spike several times). Pullback classification from depth as a
fraction of the impulse, volume on the pullback vs the impulse, higher-low
structure, L1 bid support, tape speed. Rule-based and parameterised first.

**Strategy 2 (swing: sweeps + news).** Slow loop, out of process, on 1m/5m/daily
bars plus a point-in-time news sentiment feature. Confirms tape strength by
promoting its candidates to Tier 1 before entry.

## Risk and execution

Strategies emit **intents**; a separate gateway process owns all order state.

- Per-symbol notional caps (% of ADV), daily loss limit, position and order-rate
  limits, kill switch.
- Short checks: shortable/ETB or locate, SSR active, halt/LULD state, spread and
  recent run-up.
- **Size shorts by worst-case gap, not stop distance.** Stops do not protect
  through halts or squeezes.
- Broker-side protective orders always; an external watchdog flattens positions
  if the engine heartbeat stops.
- Event-sourced Postgres ledger, reconciled with the broker every few seconds.

## Agent layer

An MCP server exposes read tools, research tools (`run_backtest` in a sandbox on
the replay engine) and action tools with permission tiers (read-only / paper /
live-with-approval).

**Live parameter tuning** is allowed, with guardrails:

1. A typed `ParamStore`: every parameter declares `min`, `max`, `max_step`,
   `cooldown` and `scope`. Out-of-bounds proposals are rejected.
2. Risk limits are not tunable by agents.
3. Prefer regime presets validated offline, or scalar multipliers around a
   baseline, over free-form numbers.
4. Changes apply to new entries only; open positions keep their parameters.
5. Every change is an event in the tape (with the agent's reason and evidence),
   so replay reproduces the exact session.
6. A fixed-parameter shadow engine runs on the same feed as the control. If the
   tuned instance does not beat it after costs, the agent adds nothing.
7. Auto-revert to baseline on a drawdown vs the shadow.

Per-tick adaptation (thresholds normalised by realised volatility, tape speed,
spread) belongs in deterministic engine code, not in an agent.

## Kubernetes and parallelism

Parallelism goes where work is independent; the live decision path is not.

| Workload | Parallel unit | Mechanism |
|---|---|---|
| Backtests / parameter sweeps | (strategy, params, date range) | Indexed Jobs, Kueue, PriorityClasses |
| Agent workers | queue item / hypothesis | KEDA on JetStream lag; Jobs |
| Archiver, compaction | date x symbol bucket | consumer group; CronJobs |
| Live engine sharding | `instrument_id % N` | StatefulSet; only if measurement demands it |
| Ingest sharding | symbol subset per session | one ingestor pod per session; only if needed |

Ingestors are singletons per connection (Lease-based leader election). Databento
allows several sessions per dataset (hot standby possible); Alpaca realistically
allows one. Live engine and ingestors run Guaranteed QoS on a dedicated tainted
node; sweeps are low-priority and preemptible. The live path (engine, gateway)
can move to a US-East node later without touching the rest.

## Provider facts (verified 2026-10-04; re-check before relying on them)

- **Databento**: 10 live sessions per dataset on Standard, 50 on Plus/Unlimited
  (limit change effective Feb 2025). Intraday replay for the last 24 h with a
  per-subscription start; subscriptions added mid-session get no replay.
  `EQUS.MINI` is a consolidated top-of-book dataset without exchange license fees.
- **Alpaca**: usually 1 websocket connection per endpoint (extra gets 406); 30
  symbols on basic plans, wildcard on paid; channels include trades, quotes,
  bars, statuses, lulds, corrections, cancelErrors; JSON or msgpack. Shorting was
  ETB-only; hard-to-borrow with a locates API is newer (check eligibility/fees).
- **Gaps**: neither provider supplies float or short interest; a reference-data
  source is needed for Strategy 1's universe. Halts and LULD must be ingested.
- PDT rules affect intraday strategies; verify current status.

Sources: Databento live connection limits
(https://databento.com/blog/changes-to-live-connection-limits), Databento live
API (https://databento.com/docs/api-reference-live), Alpaca WebSocket stream
(https://docs.alpaca.markets/docs/streaming-market-data), Alpaca HTB
(https://alpaca.markets/blog/htb-trading-api-locates/).

## Back-of-envelope (to be replaced by measurement; backlog E06-S01)

Order 10^4 trade msgs/s sustained and 10^5 quote msgs/s in bursts if L1 quotes
are subscribed for all 5,000 symbols. Hot state is low single-digit GB at worst.
