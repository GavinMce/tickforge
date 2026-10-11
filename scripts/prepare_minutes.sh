#!/usr/bin/env bash
# Keep one small file of sessions for each trading day of XNAS.BASIC one-minute bars, the history `tf research snapshots
# --minutes-dir` builds the snapshots' minute columns from (E19-S52: the previous session's high, low and close, the ATR, the volume
# baselines and the hourly EMA):
#
#   scripts/prepare_minutes.sh DIR START END        # START and END are dates, both included
#
# Each weekday of the range with no DIR/DATE.sessions is fetched (scripts/fetch_minutes.sh: the cost is asked first and refused
# above $MAX_COST; a day is 2.7 million bars, a quarter of a gigabyte of text), turned into its file by `tf reference minutes`, and
# the bars thrown away: what is kept is a few hundred kilobytes a day, so 60 sessions are a few tens of megabytes and the daily
# prepare job fetches one day. A weekday with no bars (a market holiday) leaves no file and is asked about again next time. Days
# go oldest first and the first failure stops the run, so the files kept never have a gap behind a failed day.
# Databento key: $DATABENTO_API_KEY.
set -euo pipefail

dir=${1:?usage: prepare_minutes.sh DIR START END}
from=${2:?usage: prepare_minutes.sh DIR START END}
to=${3:?usage: prepare_minutes.sh DIR START END}
export MAX_COST=${MAX_COST:-5.00}
: "${DATABENTO_API_KEY:?the Databento key must be in DATABENTO_API_KEY}"
for d in "$from" "$to"; do
  date -d "$d" +%F >/dev/null 2>&1 && [ "$(date -d "$d" +%F)" = "$d" ] || { echo "$d is not a date (YYYY-MM-DD)" >&2; exit 2; }
done
[[ ! "$from" > "$to" ]] || { echo "$from is after $to" >&2; exit 2; }
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
mkdir -p "$dir"
# On the same volume as the files, not in /tmp: a day of text is a quarter of a gigabyte.
work="$dir/.fetch"
trap 'rm -rf "$work"' EXIT

kept=0 fetched=0 holidays=0
day=$from
while [[ ! "$day" > "$to" ]]; do
  if [ "$(date -d "$day" +%u)" -le 5 ]; then
    if [ -s "$dir/$day.sessions" ]; then
      kept=$((kept + 1))
    else
      rm -rf "$work"
      "$here/fetch_minutes.sh" "$work" "$day" "$(date -d "$day + 1 day" +%F)" ALL_SYMBOLS
      if [ "$(wc -l < "$work/minutes.csv")" -le 1 ]; then
        echo "no bars on $day (a market holiday)"
        holidays=$((holidays + 1))
      else
        tf reference minutes --csv "$work/minutes.csv" --symbology "$work/symbology.json" --out "$dir"
        fetched=$((fetched + 1))
      fi
      rm -rf "$work"
    fi
  fi
  day=$(date -d "$day + 1 day" +%F)
done
echo "minute sessions in $dir for $from to $to: $kept kept, $fetched fetched, $holidays holidays"
