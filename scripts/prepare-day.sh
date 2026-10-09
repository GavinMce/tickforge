#!/usr/bin/env bash
# What a live day needs on disk before the engine starts, made fresh each morning (run by the cluster's prepare job):
#
#   scripts/prepare-day.sh [YYYY-MM-DD]        # the day to prepare, default today in New York
#
#   1. the reference: Databento daily bars up to the day before, and one snapshot for the day (`tf research snapshots`)
#   2. the certificates, if they are missing or the strategy set (or a universe beside it) has changed since they were made: a
#      stored day of the dataset to certify on (the last one stored, or the last session before the day, pulled now) and
#      `tf live certify` over it
#
# Everything is a file under the volumes: SET, STORE (the history store), REF, SNAPS and CERTS. It does nothing on a weekend, and
# says so on a day with no session. Databento key: $DATABENTO_API_KEY. The cost of each pull is asked first and refused above
# $MAX_COST (default 8.00: a day of tbbo for the whole market is about $3.4 pay-as-you-go, and included in the plan's history).
set -euo pipefail

SET=${SET:-/config/month.set}
STORE=${STORE:-/data/history/store}
REF=${REF:-/data/live/ref}
SNAPS=${SNAPS:-/data/live/snapshots}
CERTS=${CERTS:-/data/live/certificates.txt}
DATASET=${DATASET:-EQUS.MINI}
SCHEMA=${SCHEMA:-tbbo}
export MAX_COST=${MAX_COST:-8.00}
: "${DATABENTO_API_KEY:?the Databento key must be in DATABENTO_API_KEY}"

day=${1:-$(TZ=America/New_York date +%F)}
date -d "$day" +%F >/dev/null 2>&1 || { echo "$day is not a date (YYYY-MM-DD)" >&2; exit 2; }
[ "$(date -d "$day" +%F)" = "$day" ] || { echo "$day is not a date (YYYY-MM-DD)" >&2; exit 2; }
if [ "$(date -d "$day" +%u)" -gt 5 ]; then
  echo "$day is a weekend: nothing to prepare"
  exit 0
fi
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# 1. The reference, as of the last session before the day.
start=$(date -d "$day - 75 days" +%F)
"$here/fetch_reference.sh" "$REF" "$start" "$day"
tf research snapshots --bars "$REF/bars.csv" --symbology "$REF/symbology.json" --from "$day" --to "$day" --out "$SNAPS"
if [ ! -f "$SNAPS/$day.snapshot" ]; then
  echo "no session on $day (a market holiday): nothing more to prepare"
  exit 0
fi

# 2. The certificates, when the set is not the one they were made for.
set_dir=$(dirname "$SET")
want=$(cat "$SET" "$set_dir"/*.txt 2>/dev/null | sha256sum | cut -c1-16)
have=$(cat "$CERTS.for" 2>/dev/null || true)
if [ -s "$CERTS" ] && [ "$want" = "$have" ]; then
  echo "the certificates are for this strategy set ($want): kept"
  exit 0
fi
echo "certifying: the set is $want and the certificates are for ${have:-nothing}"

tape=""
for back in 1 2 3 4 5 6 7; do
  cand=$(date -d "$day - $back days" +%F)
  [ "$(date -d "$cand" +%u)" -le 5 ] || continue
  if [ ! -f "$STORE/$DATASET/$SCHEMA/$cand.dbn.zst" ]; then
    "$here/pull_history.sh" "$STORE" "$DATASET" "$SCHEMA" ALL_SYMBOLS "$cand" "$(date -d "$cand + 1 day" +%F)" || true
  fi
  if [ -f "$STORE/$DATASET/$SCHEMA/$cand.dbn.zst" ]; then
    tape=$cand
    break
  fi
done
[ -n "$tape" ] || { echo "no stored or pullable day of $DATASET $SCHEMA in the week before $day to certify on" >&2; exit 1; }
echo "certifying on $tape"
tf research snapshots --bars "$REF/bars.csv" --symbology "$REF/symbology.json" --from "$tape" --to "$tape" --out "$SNAPS"
tf history index "$STORE" --dataset "$DATASET" --schema "$SCHEMA"
tf live certify --set "$SET" --store "$STORE" --dataset "$DATASET" --schema "$SCHEMA" --date "$tape" \
  --snapshots "$SNAPS" --out "$CERTS.new"
mv "$CERTS.new" "$CERTS"
echo "$want" > "$CERTS.for"
echo "certificates written for $want, on $tape"
