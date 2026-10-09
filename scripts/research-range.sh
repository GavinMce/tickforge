#!/usr/bin/env bash
# Run the strategy set over a range of days and leave the result where the workspace's Backtests screen reads it (run by the
# cluster's backtest job, scripts/backtest-dev.sh):
#
#   scripts/research-range.sh NAME FROM TO        # NAME is the scenario; FROM and TO are dates, both included
#
#   1. each weekday of the range that is not stored is pulled (the cost of each pull is asked first and refused above $MAX_COST),
#      and given the names of its instruments from the vendor's symbology
#   2. the reference (daily bars from 75 days before the range) and one snapshot per trading day, as of the session before it
#   3. `tf history index`, then `tf research run` into $RESEARCH/NAME, which the workspace lists as a scenario
#
# Nothing here is the live day's: the reference and the snapshots are the history's own directories, so this can run while the
# prepare job does. A run that stopped goes on from the first day it did not finish; the same NAME with another strategy set is
# refused by the run (a scenario is one configuration). Databento key: $DATABENTO_API_KEY.
set -euo pipefail

SET=${SET:-/config/month.set}
STORE=${STORE:-/data/history/store}
REF=${REF:-/data/history/ref}
SNAPS=${SNAPS:-/data/history/snapshots}
RESEARCH=${RESEARCH:-/data/history/research}
DATASET=${DATASET:-EQUS.MINI}
SCHEMA=${SCHEMA:-tbbo}
# The plan includes the last year of this dataset's history, so a pull should cost nothing; a dollar a day is the line.
export MAX_COST=${MAX_COST:-1.00}
: "${DATABENTO_API_KEY:?the Databento key must be in DATABENTO_API_KEY}"

name=${1:?usage: research-range.sh NAME FROM TO}
from=${2:?usage: research-range.sh NAME FROM TO}
to=${3:?usage: research-range.sh NAME FROM TO}
case "$name" in
  "" | .* | *[!A-Za-z0-9._-]*) echo "$name is not a plain scenario name (letters, digits, . _ -)" >&2; exit 2 ;;
esac
for d in "$from" "$to"; do
  date -d "$d" +%F >/dev/null 2>&1 && [ "$(date -d "$d" +%F)" = "$d" ] || { echo "$d is not a date (YYYY-MM-DD)" >&2; exit 2; }
done
[[ ! "$from" > "$to" ]] || { echo "$from is after $to" >&2; exit 2; }
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# 1. The days.
days=()
day=$from
while [[ ! "$day" > "$to" ]]; do
  [ "$(date -d "$day" +%u)" -le 5 ] && days+=("$day")
  day=$(date -d "$day + 1 day" +%F)
done
[ "${#days[@]}" -gt 0 ] || { echo "no weekday between $from and $to" >&2; exit 2; }
echo "scenario $name: ${#days[@]} weekdays from $from to $to"
have=0
for day in "${days[@]}"; do
  file="$STORE/$DATASET/$SCHEMA/$day.dbn.zst"
  if [ ! -s "$file" ]; then
    "$here/pull_history.sh" "$STORE" "$DATASET" "$SCHEMA" ALL_SYMBOLS "$day" "$(date -d "$day + 1 day" +%F)" || true
  fi
  if [ ! -s "$file" ]; then
    echo "no tape for $day (a market holiday, or the pull failed or was over the limit): left out"
    continue
  fi
  have=$((have + 1))
  [ -s "$STORE/$DATASET/$SCHEMA/$day.names" ] || "$here/store_names.sh" "$STORE" "$DATASET" "$SCHEMA" "$day"
done
[ "$have" -gt 0 ] || { echo "no stored day in the range: nothing to run" >&2; exit 1; }

# 2. The reference and the snapshots. The bars end the day before the last day (a snapshot is as of the session before its day).
"$here/fetch_reference.sh" "$REF" "$(date -d "$from - 75 days" +%F)" "$to"
tf research snapshots --bars "$REF/bars.csv" --symbology "$REF/symbology.json" --from "$from" --to "$to" --out "$SNAPS"

# 3. The run.
tf history index "$STORE" --dataset "$DATASET" --schema "$SCHEMA"
tf research run --set "$SET" --store "$STORE" --dataset "$DATASET" --schema "$SCHEMA" --snapshots "$SNAPS" \
  --out "$RESEARCH/$name" --from "$from" --to "$to" --evidence
tf research show "$RESEARCH/$name"
echo "done: open the Backtests screen of the workspace and choose $name"
