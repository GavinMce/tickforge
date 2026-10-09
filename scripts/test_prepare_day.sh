#!/usr/bin/env bash
# Tests of scripts/prepare-day.sh with stand-ins for `tf`, the fetch and the pull: which steps run, in what order, and when each is
# skipped. Nothing touches the network.
#
#   bash scripts/test_prepare_day.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; fail=1; }

new_world() {
  work=$(mktemp -d)
  mkdir -p "$work/bin" "$work/scripts" "$work/config" "$work/data"
  cp "$here/prepare-day.sh" "$work/scripts/"
  echo "strategy set v1" > "$work/config/month.set"
  echo "universe v1" > "$work/config/universe.txt"
  # tf: records itself; `research snapshots` writes the snapshot file unless STUB_HOLIDAY=1; `live certify` writes its --out.
  cat > "$work/bin/tf" <<'STUB'
#!/usr/bin/env bash
echo "tf $*" >> "$STUB_LOG"
case "$1 $2" in
  "research snapshots")
    out=""; from=""
    while [ $# -gt 0 ]; do case "$1" in --out) out=$2;; --from) from=$2;; esac; shift; done
    mkdir -p "$out"
    [ "${STUB_HOLIDAY:-0}" = 1 ] || echo "# as_of x" > "$out/$from.snapshot" ;;
  "live certify")
    out=""; while [ $# -gt 0 ]; do case "$1" in --out) out=$2;; esac; shift; done
    echo "certificates v1" > "$out" ;;
esac
exit 0
STUB
  cat > "$work/scripts/fetch_reference.sh" <<'STUB'
#!/usr/bin/env bash
echo "fetch_reference $*" >> "$STUB_LOG"
mkdir -p "$1"; : > "$1/bars.csv"; : > "$1/symbology.json"
STUB
  # pull: stores the day unless STUB_NO_TAPE=1.
  cat > "$work/scripts/pull_history.sh" <<'STUB'
#!/usr/bin/env bash
echo "pull_history $*" >> "$STUB_LOG"
[ "${STUB_NO_TAPE:-0}" = 1 ] && exit 0
mkdir -p "$1/$2/$3"; : > "$1/$2/$3/$5.dbn.zst"
STUB
  # names: stores the day's names unless STUB_NO_NAMES=1 (the symbology could not be had).
  cat > "$work/scripts/store_names.sh" <<'STUB'
#!/usr/bin/env bash
echo "store_names $*" >> "$STUB_LOG"
[ "${STUB_NO_NAMES:-0}" = 1 ] && exit 1
: > "$1/$2/$3/$4.names"
STUB
  chmod +x "$work/bin/tf" "$work/scripts/"*.sh
  : > "$work/log"
}

prep() { # day (extra env as arguments before --)
  env PATH="$work/bin:$PATH" STUB_LOG="$work/log" DATABENTO_API_KEY=k MAX_COST=9.00 \
    SET="$work/config/month.set" STORE="$work/data/store" REF="$work/data/ref" SNAPS="$work/data/snaps" CERTS="$work/data/certs.txt" \
    "$@" > "$work/out" 2>&1
}
steps() { sed -E 's/ .*//; s/^tf$//' "$work/log" | tr '\n' ' '; }
lines() { grep -c "$1" "$work/log"; }

# A Thursday: the reference, the snapshot, then certifying on the day before (pulled) and the certificates and their stamp.
new_world
prep "$work/scripts/prepare-day.sh" 2026-10-08; code=$?
[ "$code" = 0 ] && ok "a weekday is prepared" || no "a weekday is prepared: exit $code: $(cat "$work/out")"
grep -q "fetch_reference $work/data/ref 2026-07-25 2026-10-08" "$work/log" && ok "fetches 75 days of bars up to the day" || no "fetches 75 days of bars up to the day: $(cat "$work/log")"
grep -q "tf research snapshots .*--from 2026-10-08 --to 2026-10-08" "$work/log" && ok "makes the day's snapshot" || no "makes the day's snapshot"
grep -q "pull_history $work/data/store EQUS.MINI tbbo ALL_SYMBOLS 2026-10-07 2026-10-08" "$work/log" && ok "pulls the day before to certify on" || no "pulls the day before to certify on"
grep -q "tf research snapshots .*--from 2026-10-07 --to 2026-10-07" "$work/log" && ok "makes the tape's snapshot" || no "makes the tape's snapshot"
grep -q "tf live certify .*--schema tbbo --date 2026-10-07" "$work/log" && ok "certifies on the tape" || no "certifies on the tape"
[ -s "$work/data/certs.txt" ] && [ -s "$work/data/certs.txt.for" ] && ok "writes the certificates and their stamp" || no "writes the certificates and their stamp"
[ ! -e "$work/data/certs.txt.new" ] && ok "leaves no half-written file" || no "leaves no half-written file"
order=$(grep -n "fetch_reference\|snapshots.*2026-10-08\|pull_history\|store_names\|history index\|live certify" "$work/log" | sed -E 's/^[0-9]+://; s/ .*//' | tr '\n' ' ')
case "$order" in "fetch_reference tf pull_history store_names tf tf ") ok "in the right order";; *) no "in the right order: $order";; esac

grep -q "store_names $work/data/store EQUS.MINI tbbo 2026-10-07" "$work/log" && ok "names the tape's instruments" || no "names the tape's instruments: $(cat "$work/log")"

# The certificates are kept while the set is the same, and made again when it, or a universe beside it, changes.
: > "$work/log"
prep "$work/scripts/prepare-day.sh" 2026-10-08
[ "$(lines 'live certify')" = 0 ] && [ "$(lines pull_history)" = 0 ] && ok "keeps certificates for the same set" || no "keeps certificates for the same set"
grep -q "kept" "$work/out" && ok "says they are kept" || no "says they are kept"
echo "strategy 1 more" >> "$work/config/month.set"; : > "$work/log"
prep "$work/scripts/prepare-day.sh" 2026-10-08
[ "$(lines 'live certify')" = 1 ] && ok "certifies again when the set changes" || no "certifies again when the set changes"
[ "$(lines pull_history)" = 0 ] && ok "on the day already stored" || no "on the day already stored"
echo "static more" >> "$work/config/universe.txt"; : > "$work/log"
prep "$work/scripts/prepare-day.sh" 2026-10-08
[ "$(lines 'live certify')" = 1 ] && ok "certifies again when a universe changes" || no "certifies again when a universe changes"
rm -f "$work/data/certs.txt.for"; : > "$work/log"
prep "$work/scripts/prepare-day.sh" 2026-10-08
[ "$(lines 'live certify')" = 1 ] && ok "certifies again without the stamp" || no "certifies again without the stamp"

# Names that cannot be had stop the certifying: a replay without symbols would refuse every strategy anyway.
new_world
prep STUB_NO_NAMES=1 "$work/scripts/prepare-day.sh" 2026-10-08; code=$?
[ "$code" != 0 ] && [ "$(lines 'live certify')" = 0 ] && [ ! -e "$work/data/certs.txt" ] && ok "no names, no certificates" || no "no names, no certificates: exit $code"

# A Monday: the last session before it is the Friday, not the weekend.
new_world
prep "$work/scripts/prepare-day.sh" 2026-10-12
grep -q "pull_history .* 2026-10-09 2026-10-10" "$work/log" && ok "certifies on the Friday before a Monday" || no "certifies on the Friday before a Monday: $(cat "$work/log")"

# No tape to be had (a pull that brings nothing): an error, and no certificates.
new_world
prep STUB_NO_TAPE=1 "$work/scripts/prepare-day.sh" 2026-10-08; code=$?
[ "$code" = 1 ] && ok "no tape is an error" || no "no tape is an error: exit $code"
grep -q "no stored or pullable day" "$work/out" && ok "says so" || no "says so"
[ ! -e "$work/data/certs.txt" ] && ok "writes no certificates without a tape" || no "writes no certificates without a tape"
[ "$(lines store_names)" = 0 ] && ok "names nothing then" || no "names nothing then"
[ "$(lines pull_history)" = 7 ] || [ "$(lines pull_history)" = 5 ] && ok "tries the week before" || no "tries the week before: $(lines pull_history)"

# A holiday: no snapshot is made, so nothing more is done, and that is not an error.
new_world
prep STUB_HOLIDAY=1 "$work/scripts/prepare-day.sh" 2026-10-08; code=$?
[ "$code" = 0 ] && grep -q "no session" "$work/out" && ok "a day with no session says so and stops" || no "a day with no session says so and stops: $code $(cat "$work/out")"
[ "$(lines 'live certify')" = 0 ] && ok "certifies nothing then" || no "certifies nothing then"

# A weekend, a date that is not one, and a missing key.
new_world
prep "$work/scripts/prepare-day.sh" 2026-10-10; code=$?
[ "$code" = 0 ] && grep -q "weekend" "$work/out" && [ ! -s "$work/log" ] && ok "a weekend does nothing" || no "a weekend does nothing"
for bad in 2026-02-30 soon 2026-13-01 ""; do
  prep "$work/scripts/prepare-day.sh" "$bad"; code=$?
  if [ -z "$bad" ]; then continue; fi
  [ "$code" = 2 ] && ok "refuses the date '$bad'" || no "refuses the date '$bad': exit $code"
done
env PATH="$work/bin:$PATH" STUB_LOG="$work/log" SET="$work/config/month.set" "$work/scripts/prepare-day.sh" 2026-10-08 > "$work/out" 2>&1 < /dev/null
grep -q "DATABENTO_API_KEY" "$work/out" && ok "says when the key is missing" || no "says when the key is missing: $(cat "$work/out")"
exit "$fail"
