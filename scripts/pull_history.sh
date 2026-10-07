#!/usr/bin/env bash
# Pull Databento history into the research store: one zstd DBN file per day, then `tf history index`.
#
#   scripts/pull_history.sh DIR DATASET SCHEMA SYMBOLS START END     # END is exclusive, YYYY-MM-DD
#
# SYMBOLS is a comma-separated list or ALL_SYMBOLS. Files land in DIR/DATASET/SCHEMA/<date>.dbn.zst, each with a
# <date>.cost beside it (what the metadata service quoted for that day, in dollars). Before anything is pulled the cost
# of the whole range is asked of the metadata service (it is free) and printed; the script refuses above $MAX_COST
# (default 5.00), so a whole-market pull is a decision, not an accident. Weekends are skipped; a day Databento has no
# data for (a holiday) is left out and said. A day already stored is not pulled again.
# Databento key: $DATABENTO_API_KEY or ~/.config/tickforge/databento.key.
# Then:  tf history index DIR --dataset DATASET --schema SCHEMA --symbols SYMBOLS && tf history verify DIR
set -euo pipefail
dir=${1:?usage: pull_history.sh DIR DATASET SCHEMA SYMBOLS START END}
dataset=${2:?dataset}
schema=${3:?schema}
symbols=${4:?symbols}
start=${5:?start date}
end=${6:?end date}
max=${MAX_COST:-5.00}
key=${DATABENTO_API_KEY:-$(cat "$HOME/.config/tickforge/databento.key")}
host=https://hist.databento.com/v0
out="$dir/$dataset/$schema"
mkdir -p "$out"
common=(-d dataset="$dataset" -d schema="$schema" -d symbols="$symbols" -d stype_in=raw_symbol)

cost=$(curl -fsS -u "$key:" "$host/metadata.get_cost" "${common[@]}" -d start="$start" -d end="$end")
echo "Databento cost for $dataset $schema $symbols from $start to $end: \$$cost (limit \$$max)"
if ! awk -v c="$cost" -v m="$max" 'BEGIN { exit !(c + 0 <= m + 0) }'; then
  echo "refusing: the cost is above the limit; raise MAX_COST if you mean it" >&2
  exit 1
fi

day=$start
pulled=0
skipped=0
while [[ "$day" < "$end" ]]; do
  next=$(date -u -d "$day + 1 day" +%F)
  dow=$(date -u -d "$day" +%u)
  file="$out/$day.dbn.zst"
  if [[ $dow -le 5 && ! -s "$file" ]]; then
    day_cost=$(curl -fsS -u "$key:" "$host/metadata.get_cost" "${common[@]}" -d start="$day" -d end="$next" || echo "")
    if curl -fsS -u "$key:" "$host/timeseries.get_range" "${common[@]}" -d start="$day" -d end="$next" \
        -d encoding=dbn -d compression=zstd -o "$file.part" 2>/dev/null && [[ -s "$file.part" ]]; then
      mv "$file.part" "$file"
      [[ -n "$day_cost" ]] && echo "$day_cost" >"$out/$day.cost"
      pulled=$((pulled + 1))
    else
      rm -f "$file.part"
      echo "no data for $day (a holiday, or the pull failed)"
      skipped=$((skipped + 1))
    fi
  fi
  day=$next
done
echo "pulled $pulled days into $out ($skipped left out)"
