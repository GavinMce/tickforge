#!/usr/bin/env bash
# Fetch what `tf reference minutes` (and `tf reference build --minutes`) reads: one-minute bars of the live feed (XNAS.BASIC) from
# Databento, with fixed-point prices and integer times.
#
#   scripts/fetch_minutes.sh OUTDIR START END [SYMBOLS]     # END is exclusive, YYYY-MM-DD
#
# SYMBOLS is a comma-separated list of up to 2000 (the bars then carry the symbol: minutes.csv with a `symbol` column), or
# ALL_SYMBOLS, the default. Databento does not map symbols for the whole market in one request, so the bars then carry instrument
# numbers and no symbol, and the names come beside them as symbology.json (`tf reference minutes --symbology`).
# Databento key: $DATABENTO_API_KEY or ~/.config/tickforge/databento.key. The cost is asked of the metadata service first (it is
# free) and printed; the script refuses to pull above $MAX_COST dollars (default 5.00), so a whole-market pull is a decision, not
# an accident. A whole session of the whole market is about 2.7 million bars, a quarter of a gigabyte of text.
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
# Databento's front end answers 504 now and then to a long request that works a minute later.
retry=(--retry 4 --retry-all-errors --retry-delay 10 --max-time 1800)
map=()
[ "$symbols" = ALL_SYMBOLS ] || map=(-d map_symbols=true)
rm -f "$out/minutes.csv.part"
curl -fsS "${retry[@]}" -u "$key:" "$host/timeseries.get_range" "${args[@]}" -d encoding=csv -d pretty_px=false \
  -d pretty_ts=false "${map[@]}" -o "$out/minutes.csv.part"
mv "$out/minutes.csv.part" "$out/minutes.csv"
echo "minutes: $(($(wc -l <"$out/minutes.csv") - 1)) rows in $(($(date +%s) - started)) s"
if [ "$symbols" = ALL_SYMBOLS ]; then
  curl -fsS "${retry[@]}" -u "$key:" "$host/symbology.resolve" -d dataset=XNAS.BASIC -d symbols=ALL_SYMBOLS \
    -d stype_in=raw_symbol -d stype_out=instrument_id -d start_date="$start" -d end_date="$end" -o "$out/symbology.json"
  echo "symbology: $(wc -c <"$out/symbology.json") bytes"
fi
