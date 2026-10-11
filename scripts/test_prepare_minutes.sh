#!/usr/bin/env bash
# Tests of scripts/prepare_minutes.sh and scripts/fetch_minutes.sh with stand-ins for `tf` and curl: which days are fetched, which are
# kept, how a holiday and a failure are told apart, and the form of the request. Nothing touches the network.
#
#   bash scripts/test_prepare_minutes.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; fail=1; }

new_world() {
  work=$(mktemp -d)
  mkdir -p "$work/bin" "$work/scripts"
  cp "$here/prepare_minutes.sh" "$work/scripts/"
  # fetch: writes a day's bars and names into its OUTDIR (a header alone for a day in STUB_HOLIDAYS, a failure for one in STUB_FAILS).
  cat > "$work/scripts/fetch_minutes.sh" <<'STUB'
#!/usr/bin/env bash
echo "fetch_minutes $*" >> "$STUB_LOG"
case " ${STUB_FAILS:-} " in *" $2 "*) exit 1;; esac
mkdir -p "$1"
printf 'ts_event,rtype,publisher_id,instrument_id,open,high,low,close,volume\n' > "$1/minutes.csv"
case " ${STUB_HOLIDAYS:-} " in *" $2 "*) ;; *) echo "$2,33,1,1,1,1,1,1,1" >> "$1/minutes.csv";; esac
echo '{}' > "$1/symbology.json"
STUB
  # tf: `reference minutes` keeps the day named in the CSV's one row, as <date>.sessions.
  cat > "$work/bin/tf" <<'STUB'
#!/usr/bin/env bash
echo "tf $*" >> "$STUB_LOG"
csv=""; out=""
while [ $# -gt 0 ]; do case "$1" in --csv) csv=$2;; --out) out=$2;; esac; shift; done
[ -s "$csv" ] || exit 1
day=$(tail -n 1 "$csv" | cut -d, -f1)
echo "sessions" > "$out/$day.sessions"
STUB
  chmod +x "$work/bin/tf" "$work/scripts/"*.sh
  : > "$work/log"
}
prep() { env PATH="$work/bin:$PATH" STUB_LOG="$work/log" DATABENTO_API_KEY=k "$@" > "$work/out" 2>&1; }

# Thursday 8 October to Tuesday 13 October: Thu, Fri, Mon, Tue are weekdays.
new_world
prep "$work/scripts/prepare_minutes.sh" "$work/m" 2026-10-08 2026-10-13; code=$?
[ "$code" = 0 ] && ok "a range is prepared" || no "a range is prepared: exit $code: $(cat "$work/out")"
[ "$(grep -c '^fetch_minutes' "$work/log")" = 4 ] && ok "fetches the four weekdays and no weekend" || no "fetches the four weekdays and no weekend: $(grep fetch_minutes "$work/log")"
grep -q "fetch_minutes $work/m/.fetch 2026-10-09 2026-10-10 ALL_SYMBOLS" "$work/log" && ok "a day alone, for every symbol, in a scratch directory beside the files" || no "a day alone: $(grep fetch_minutes "$work/log")"
ls "$work/m" | tr '\n' ' ' | grep -q "^2026-10-08.sessions 2026-10-09.sessions 2026-10-12.sessions 2026-10-13.sessions $" && ok "one file for each day" || no "one file for each day: $(ls -a "$work/m")"
[ ! -e "$work/m/.fetch" ] && ok "leaves no bars behind" || no "leaves no bars behind"
grep -q "tf reference minutes --csv $work/m/.fetch/minutes.csv --symbology $work/m/.fetch/symbology.json --out $work/m" "$work/log" && ok "keeps a day with its names" || no "keeps a day with its names"
grep -q "4 fetched" "$work/out" && ok "says what it did" || no "says what it did: $(tail -1 "$work/out")"

# Days kept are not fetched again.
: > "$work/log"
prep "$work/scripts/prepare_minutes.sh" "$work/m" 2026-10-08 2026-10-13; code=$?
[ "$code" = 0 ] && [ ! -s "$work/log" ] && grep -q "4 kept, 0 fetched" "$work/out" && ok "kept days are not fetched again" || no "kept days are not fetched again: $(cat "$work/log")"

# A holiday leaves no file and is not an error; it is asked about again next time.
new_world
prep STUB_HOLIDAYS=2026-10-09 "$work/scripts/prepare_minutes.sh" "$work/m" 2026-10-08 2026-10-09; code=$?
[ "$code" = 0 ] && [ ! -e "$work/m/2026-10-09.sessions" ] && [ -e "$work/m/2026-10-08.sessions" ] && grep -q "no bars on 2026-10-09" "$work/out" && ok "a holiday is said and leaves no file" || no "a holiday is said and leaves no file: $(cat "$work/out")"
grep -c "tf reference minutes" "$work/log" | grep -q '^1$' && ok "keeps nothing for it" || no "keeps nothing for it"

# A failed day stops the run: the days after it are not fetched, so the files never have a gap behind it.
new_world
prep STUB_FAILS=2026-10-09 "$work/scripts/prepare_minutes.sh" "$work/m" 2026-10-08 2026-10-13; code=$?
[ "$code" != 0 ] && [ -e "$work/m/2026-10-08.sessions" ] && [ ! -e "$work/m/2026-10-12.sessions" ] && ok "the first failure stops the run, and what was kept stays" || no "the first failure stops the run: exit $code"
[ ! -e "$work/m/.fetch" ] && ok "and its scratch bars are removed" || no "and its scratch bars are removed"

# Refusals, before anything is fetched.
for args in "2026-10-32 2026-10-08" "2026-10-09 2026-10-08" "soon 2026-10-08"; do
  new_world
  prep "$work/scripts/prepare_minutes.sh" "$work/m" $args; code=$?
  [ "$code" = 2 ] && [ ! -s "$work/log" ] && ok "refuses: $args" || no "refuses: $args: exit $code"
done
new_world
env PATH="$work/bin:$PATH" STUB_LOG="$work/log" "$work/scripts/prepare_minutes.sh" "$work/m" 2026-10-08 2026-10-08 > "$work/out" 2>&1 </dev/null
[ $? != 0 ] && [ ! -s "$work/log" ] && ok "refuses without the Databento key" || no "refuses without the Databento key"

# fetch_minutes.sh: every symbol is asked for without mapping (Databento refuses it) and the names come beside; a list is mapped.
new_world
cp "$here/fetch_minutes.sh" "$work/scripts/"
cat > "$work/bin/curl" <<'STUB'
#!/usr/bin/env bash
echo "curl $*" >> "$STUB_LOG"
out=""; url=""
while [ $# -gt 0 ]; do case "$1" in -o) out=$2;; http*) url=$1;; esac; shift; done
case "$url" in
  */metadata.get_cost) echo "${STUB_COST:-0}" ;;
  */timeseries.get_range) printf 'ts_event,rtype\n1,33\n' > "$out" ;;
  */symbology.resolve) echo '{"result":{}}' > "$out" ;;
esac
exit 0
STUB
chmod +x "$work/bin/curl"
fetch() { env PATH="$work/bin:$PATH" STUB_LOG="$work/log" DATABENTO_API_KEY=k "$@" > "$work/out" 2>&1; }
fetch "$work/scripts/fetch_minutes.sh" "$work/o1" 2026-10-08 2026-10-09; code=$?
[ "$code" = 0 ] && [ -s "$work/o1/minutes.csv" ] && [ -s "$work/o1/symbology.json" ] && ok "every symbol: the bars and the names" || no "every symbol: the bars and the names: exit $code $(cat "$work/out")"
grep -q "map_symbols" "$work/log" && no "every symbol is not mapped" || ok "every symbol is not mapped"
grep -q "symbology.resolve" "$work/log" && grep "symbology.resolve" "$work/log" | grep -q "dataset=XNAS.BASIC" && ok "the names are the feed's own" || no "the names are the feed's own"
[ "$(grep -c -- "--retry 4 --retry-all-errors" "$work/log")" = 2 ] && ok "the bars and the names are retried" || no "the bars and the names are retried"
[ ! -e "$work/o1/minutes.csv.part" ] && ok "leaves no half-written file" || no "leaves no half-written file"
: > "$work/log"
fetch "$work/scripts/fetch_minutes.sh" "$work/o2" 2026-10-08 2026-10-09 AAPL,MSFT; code=$?
[ "$code" = 0 ] && grep -q "map_symbols=true" "$work/log" && ! grep -q "symbology.resolve" "$work/log" && [ ! -e "$work/o2/symbology.json" ] && ok "a list of symbols is mapped, with no names beside" || no "a list of symbols is mapped: exit $code"
: > "$work/log"
fetch STUB_COST=9.5 "$work/scripts/fetch_minutes.sh" "$work/o3" 2026-10-08 2026-10-09; code=$?
[ "$code" = 1 ] && ! grep -q "timeseries.get_range" "$work/log" && grep -q "refusing" "$work/out" && ok "a cost above the limit is refused before the pull" || no "a cost above the limit is refused: exit $code"
exit "$fail"
