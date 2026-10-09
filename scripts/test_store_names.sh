#!/usr/bin/env bash
# Tests of scripts/store_names.sh with stand-ins for curl and tf: what it asks the vendor for, that it keeps what is there, and that
# a day that is not stored, or a symbology that cannot be had, leaves no names file. Nothing touches the network.
#
#   bash scripts/test_store_names.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; fail=1; }

work=$(mktemp -d)
mkdir -p "$work/bin" "$work/store/EQUS.MINI/tbbo"
cat > "$work/bin/curl" <<'STUB'
#!/usr/bin/env bash
echo "curl $*" >> "$STUB_LOG"
[ "${STUB_CURL_FAILS:-0}" = 1 ] && exit 22
out=""; while [ $# -gt 0 ]; do case "$1" in -o) out=$2;; esac; shift; done
echo '{"result":{}}' > "$out"
STUB
cat > "$work/bin/tf" <<'STUB'
#!/usr/bin/env bash
echo "tf $*" >> "$STUB_LOG"
store=$3; ds=""; sc=""; date=""
while [ $# -gt 0 ]; do case "$1" in --dataset) ds=$2;; --schema) sc=$2;; --date) date=$2;; esac; shift; done
echo "5 AAA" > "$store/$ds/$sc/$date.names"
STUB
chmod +x "$work/bin/"*
run() { env PATH="$work/bin:$PATH" STUB_LOG="$work/log" DATABENTO_API_KEY=k "$@" "$here/store_names.sh" "$work/store" EQUS.MINI tbbo "${DAY:-2026-10-08}" > "$work/out" 2>&1; }
f="$work/store/EQUS.MINI/tbbo"

: > "$work/log"; DAY=2026-10-08 run env; code=$?
[ "$code" = 1 ] && grep -q "no stored" "$work/out" && [ ! -s "$work/log" ] && ok "a day that is not stored is an error and asks nothing" || no "a day that is not stored: $code $(cat "$work/out")"

echo x > "$f/2026-10-08.dbn.zst"
: > "$work/log"; run env STUB_CURL_FAILS=1; code=$?
[ "$code" != 0 ] && [ ! -e "$f/2026-10-08.names" ] && ok "a symbology that cannot be had leaves no names file" || no "a symbology that cannot be had: $code"

: > "$work/log"; run env; code=$?
[ "$code" = 0 ] && [ -s "$f/2026-10-08.names" ] && ok "names the day" || no "names the day: $code $(cat "$work/out")"
grep -q "symbology.resolve" "$work/log" && grep -q "dataset=EQUS.MINI" "$work/log" && grep -q "start_date=2026-10-08" "$work/log" && grep -q "end_date=2026-10-09" "$work/log" \
  && grep -q "stype_out=instrument_id" "$work/log" && grep -q -- "--retry 4 --retry-all-errors" "$work/log" && ok "asks the vendor for that day, with retries" || no "asks the vendor: $(cat "$work/log")"
grep -q "tf history names $work/store --dataset EQUS.MINI --schema tbbo --date 2026-10-08 --symbology" "$work/log" && ok "turns it into the day's names" || no "turns it into the day's names"

: > "$work/log"; run env; code=$?
[ "$code" = 0 ] && [ ! -s "$work/log" ] && grep -q kept "$work/out" && ok "keeps names that are there" || no "keeps names that are there: $(cat "$work/log")"

# A month end: the day after is in the next month.
echo x > "$f/2026-09-30.dbn.zst"; : > "$work/log"; DAY=2026-09-30 run env
grep -q "end_date=2026-10-01" "$work/log" && ok "the day after a month end" || no "the day after a month end"
exit "$fail"
