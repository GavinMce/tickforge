#!/usr/bin/env bash
# Tests of scripts/fetch_reference.sh with a stand-in for curl: the bars of a long range are asked for in pieces of at most 28
# days that meet end to start, joined under one header, and a piece that fails is left to curl's retries (the stand-in checks the
# flags). Nothing touches the network.
#
#   bash scripts/test_fetch_reference.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; fail=1; }

work=$(mktemp -d)
mkdir -p "$work/bin"
# curl: records its arguments; get_range answers a header and one row per request, named by its start; the cost is a number.
cat > "$work/bin/curl" <<'STUB'
#!/usr/bin/env bash
echo "curl $*" >> "$STUB_LOG"
out=""; start=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in -o) out=$2;; http*) url=$1;; esac
  if [ "$1" = -d ]; then case "$2" in start=*) start=${2#start=};; esac; fi
  shift
done
case "$url" in
  */metadata.get_cost) echo 0.5 ;;
  */timeseries.get_range) printf 'ts_event,close\n%s,1\n' "$start" > "$out" ;;
  */symbology.resolve) echo '{}' > "$out" ;;
esac
exit 0
STUB
chmod +x "$work/bin/curl"
run() { env PATH="$work/bin:$PATH" STUB_LOG="$work/log" DATABENTO_API_KEY=k "$here/fetch_reference.sh" "$@" > "$work/out" 2>&1; }

# 70 days: pieces of 28, 28 and 14 days, and one header.
: > "$work/log"
run "$work/o1" 2026-07-25 2026-10-03; code=$?
[ "$code" = 0 ] && ok "a long range succeeds" || no "a long range succeeds: $code $(cat "$work/out")"
[ "$(grep -c timeseries.get_range "$work/log")" = 3 ] && ok "asks in three pieces" || no "asks in three pieces: $(grep -c timeseries.get_range "$work/log")"
[ "$(cat "$work/o1/bars.csv")" = "$(printf 'ts_event,close\n2026-07-25,1\n2026-08-22,1\n2026-09-19,1')" ] \
  && ok "pieces meet end to start under one header" || no "pieces meet end to start under one header: $(cat "$work/o1/bars.csv")"
grep -q -- "end=2026-10-03" "$work/log" && ok "the last piece ends where the range does" || no "the last piece ends where the range does"
[ ! -e "$work/o1/piece.csv" ] && [ ! -e "$work/o1/bars.csv.part" ] && ok "leaves no scratch files" || no "leaves no scratch files"

# A short range is one piece; every request that can fail carries the retry flags.
: > "$work/log"
run "$work/o2" 2026-10-01 2026-10-09; code=$?
[ "$code" = 0 ] && [ "$(grep -c timeseries.get_range "$work/log")" = 1 ] && ok "a short range is one piece" || no "a short range is one piece"
[ "$(grep -c -- "--retry 4 --retry-all-errors" "$work/log")" = 2 ] && ok "bars and symbology are retried" || no "bars and symbology are retried: $(grep -c -- '--retry' "$work/log")"
exit "$fail"
