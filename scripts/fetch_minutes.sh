#!/usr/bin/env bash
# Fetch what `tf reference build --minutes` reads: one-minute bars of the live feed (XNAS.BASIC) from Databento, with
# fixed-point prices, integer times and the symbol mapped in.
#
#   scripts/fetch_minutes.sh OUTDIR START END [SYMBOLS]     # END is exclusive, YYYY-MM-DD
#
# SYMBOLS is a comma-separated list (default ALL_SYMBOLS). Databento key: $DATABENTO_API_KEY or
# ~/.config/tickforge/databento.key. The cost is asked of the metadata service first (it is free) and printed; the
# script refuses to pull above $MAX_COST dollars (default 5.00), so a whole-market pull is a decision, not an accident.
# Pull at least 60 sessions ending on the day the daily bars end: `tf reference build` refuses minute bars that
# end on another day, and a missing day in the middle.
set -euo pipefail
out=${1:?usage: fetch_minutes.sh OUTDIR START END [SYMBOLS]}
start=${2:?start date}
end=${3:?end date}
symbols=${4:-ALL_SYMBOLS}
max=${MAX_COST:-5.00}
key=${DATABENTO_API_KEY:-$(cat "$HOME/.config/tickforge/databento.key")}
host=https://hist.databento.com/v0
mkdir -p "$out"

args=(-d dataset=XNAS.BASIC -d schema=ohlcv-1m -d symbols="$symbols" -d stype_in=raw_symbol -d start="$start" -d end="$end")
cost=$(curl -fsS -u "$key:" "$host/metadata.get_cost" "${args[@]}")
echo "Databento cost for the minute bars: \$$cost (limit \$$max)"
if ! awk -v c="$cost" -v m="$max" 'BEGIN { exit !(c + 0 <= m + 0) }'; then
  echo "refusing: the cost is above the limit; raise MAX_COST if you mean it" >&2
  exit 1
fi
started=$(date +%s)
curl -fsS -u "$key:" "$host/timeseries.get_range" "${args[@]}" -d encoding=csv -d pretty_px=false \
  -d pretty_ts=false -d map_symbols=true -o "$out/minutes.csv"
echo "minutes: $(($(wc -l <"$out/minutes.csv") - 1)) rows in $(($(date +%s) - started)) s"
