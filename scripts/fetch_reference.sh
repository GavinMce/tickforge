#!/usr/bin/env bash
# Fetch what `tf reference build` reads: daily bars and symbology from Databento, and (optionally)
# Alpaca's asset list. The only place in the project that touches the network for reference data.
#
#   scripts/fetch_reference.sh OUTDIR START END      # END is exclusive, YYYY-MM-DD
#   SYMBOLS=AAPL,NVDA scripts/fetch_reference.sh ... # a few symbols instead of the whole market
#
# Databento key: $DATABENTO_API_KEY or ~/.config/tickforge/databento.key.
# Alpaca (optional): $APCA_API_KEY_ID and $APCA_API_SECRET_KEY, and $APCA_API_BASE_URL
# (default https://paper-api.alpaca.markets).
# One month of all-symbol daily bars from EQUS.SUMMARY costs about $0.50 of Databento credit
# (checked with metadata.get_cost before this was written); the script prints the cost first.
set -euo pipefail
out=${1:?usage: fetch_reference.sh OUTDIR START END}
start=${2:?start date}
end=${3:?end date}
key=${DATABENTO_API_KEY:-$(cat "$HOME/.config/tickforge/databento.key")}
host=https://hist.databento.com/v0
symbols=${SYMBOLS:-ALL_SYMBOLS}
mkdir -p "$out"

cost=$(curl -fsS -u "$key:" "$host/metadata.get_cost" -d dataset=EQUS.SUMMARY -d schema=ohlcv-1d \
  -d symbols="$symbols" -d stype_in=raw_symbol -d start="$start" -d end="$end")
echo "Databento cost for the bars: \$$cost"

curl -fsS -u "$key:" "$host/timeseries.get_range" -d dataset=EQUS.SUMMARY -d schema=ohlcv-1d \
  -d symbols="$symbols" -d stype_in=raw_symbol -d start="$start" -d end="$end" -d encoding=csv \
  -o "$out/bars.csv"
curl -fsS -u "$key:" "$host/symbology.resolve" -d dataset=EQUS.SUMMARY -d symbols="$symbols" \
  -d stype_in=raw_symbol -d stype_out=instrument_id -d start_date="$start" -d end_date="$end" \
  -o "$out/symbology.json"
echo "bars: $(($(wc -l <"$out/bars.csv") - 1)) rows; symbology: $(wc -c <"$out/symbology.json") bytes"

if [[ -n ${APCA_API_KEY_ID:-} && -n ${APCA_API_SECRET_KEY:-} ]]; then
  curl -fsS "${APCA_API_BASE_URL:-https://paper-api.alpaca.markets}/v2/assets?status=active&asset_class=us_equity" \
    -H "APCA-API-KEY-ID: $APCA_API_KEY_ID" -H "APCA-API-SECRET-KEY: $APCA_API_SECRET_KEY" \
    -o "$out/assets.json"
  echo "assets: $(wc -c <"$out/assets.json") bytes"
else
  echo "no Alpaca credentials in the environment: assets.json not fetched"
fi
