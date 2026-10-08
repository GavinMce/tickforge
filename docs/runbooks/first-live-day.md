# The first live day

What to do, in order, to run a day against the Databento live gateway with `tf live` (ADR 0072). Nothing here sends an order to a
broker: the strategies trade on the simulated broker, filled against the recorded quotes. What the first day proves is the pipe, not
an edge.

All times are New York time unless they say CDT. The gateway should be open before 04:00 (03:00 CDT).

## Once

1. A Databento plan that includes live data for the dataset in `live.cfg` (the Standard plan: `EQUS.MINI`), and a key. Keep the key in
   `~/.config/tickforge/databento.key` (mode 600) or in `$DATABENTO_API_KEY`; never in a file in the repository.
2. `cargo build --release -p tf-cli`, then `export PATH=$PWD/target/release:$PATH`.
3. Copy `docs/examples/live/` to a working directory and edit `month.set` (the variants) and `live.cfg`. Everything is relative to
   that directory.

## The evening before (or the morning, before 03:30 CDT)

```sh
export DATABENTO_API_KEY=$(cat ~/.config/tickforge/databento.key)

# 0. Free: what the plan gives. On 2026-10-08 EQUS.MINI listed mbp-1, tbbo, trades, bbo-1s, bbo-1m, ohlcv-1s/1m/1h/1d and
#    definition (no tcbbo, cmbp-1 or status), with history from 2023-03-28.
curl -fsS -u "$DATABENTO_API_KEY:" https://hist.databento.com/v0/metadata.list_schemas -d dataset=EQUS.MINI

# 1. A real day of the same feed, to certify the strategies on (a day of tbbo for the whole market was quoted at $3.37 on 2026-10-08; the cost is asked first and refused above $MAX_COST).
MAX_COST=8 scripts/pull_history.sh store EQUS.MINI tbbo ALL_SYMBOLS 2026-10-07 2026-10-08
tf history index store --dataset EQUS.MINI --schema tbbo && tf history verify store

# 2. The reference for the tape's day and for the live day: daily bars to 2026-10-07, then one snapshot per trading day.
scripts/fetch_reference.sh ref 2026-08-01 2026-10-08
tf research snapshots --bars ref/bars.csv --symbology ref/symbology.json --from 2026-10-07 --to 2026-10-08 --out snapshots

# 3. Certify every strategy on the stored day. Nothing is written if one fails; do it again after any change to the set.
tf live certify --set month.set --store store --dataset EQUS.MINI --schema tbbo --date 2026-10-07 \
    --snapshots snapshots --out certificates.txt

# 4. Log in and read for a minute without placing anything. Between 04:00 and 20:00 it should say events were read.
tf live check --config live.cfg --seconds 60
```

## The day

```sh
tf live run --config live.cfg --date 2026-10-08 --start-at 03:55 2>&1 | tee run/live-2026-10-08.out
```

- It checks everything first (the set, the snapshot, the certificates, the ledger) and stops before the gateway if anything is
  wrong. It then waits for 03:55, connects, waits until the gateway has been quiet about instrument names, builds the day and runs
  to the close (20:00) or until it is stopped.
- To stop it cleanly: `touch run/STOP`. The capture is finished and the report written. (Ctrl-C also works but leaves the capture's
  last segment for the next start to repair, and no report.) Remove `run/STOP` before the next day.
- A restart in the same day makes a new directory (`run/2026-10-08-2`); the ledger continues.
- Files: `run/ledger/` (the ledger, one for all the days), `run/2026-10-08/capture/` (the raw records), `decisions.log`, `report.txt`.
  The report says how the day ended, how many reconnects, what each strategy did and whether the capture replayed to the same
  decisions.

## Seeing it

```sh
tf serve --token-file ~/.config/tickforge/dev-token --ledger run/ledger --kind live
```

## What can go wrong the first time

Nothing in `tf live` has run against the real gateway. The first session exercises the subscription, the stream, heartbeats and
reconnects for the first time. If `check` says nothing arrived in market hours, the plan's entitlement does not cover the dataset or
schema: ask Databento. If the day ends with "THE GATEWAY WAS LOST", read the report: it names what was open.

This machine must stay awake and connected for the whole day; if it sleeps, the day ends and is reported as a lost gateway.
