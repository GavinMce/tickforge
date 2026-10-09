#!/usr/bin/env bash
# The names of a stored day's instruments. A history pull of every symbol carries no symbol mappings in its metadata, so a replay of
# the day knows instrument numbers and no symbols (and a strategy set naming symbols is refused: unknown symbols). This asks
# Databento's symbology for the day and keeps it beside the day's file as <date>.names (`tf history names`).
#
#   scripts/store_names.sh STORE DATASET SCHEMA DATE     # DATE is a stored day, YYYY-MM-DD
#
# Does nothing if the names are there. Databento key: $DATABENTO_API_KEY or ~/.config/tickforge/databento.key. Free of charge.
set -euo pipefail
store=${1:?usage: store_names.sh STORE DATASET SCHEMA DATE}
dataset=${2:?dataset}
schema=${3:?schema}
day=${4:?date}
file="$store/$dataset/$schema/$day.dbn.zst"
names="$store/$dataset/$schema/$day.names"
[ -s "$file" ] || { echo "no stored $dataset $schema for $day under $store" >&2; exit 1; }
if [ -s "$names" ]; then
  echo "names for $day: kept"
  exit 0
fi
key=${DATABENTO_API_KEY:-$(cat "$HOME/.config/tickforge/databento.key")}
next=$(date -u -d "$day + 1 day" +%F)
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -fsS --retry 4 --retry-all-errors --retry-delay 10 --max-time 600 -u "$key:" https://hist.databento.com/v0/symbology.resolve \
  -d dataset="$dataset" -d symbols=ALL_SYMBOLS -d stype_in=raw_symbol -d stype_out=instrument_id \
  -d start_date="$day" -d end_date="$next" -o "$tmp"
tf history names "$store" --dataset "$dataset" --schema "$schema" --date "$day" --symbology "$tmp"
