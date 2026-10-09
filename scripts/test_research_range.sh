#!/usr/bin/env bash
# Tests of scripts/research-range.sh and scripts/backtest-dev.sh with stand-ins for `tf`, the pull, the names and the reference
# fetch: which days are pulled, what is left out, the order of the steps, and what is refused. Nothing touches the network or a cluster.
#
#   bash scripts/test_research_range.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; fail=1; }

new_world() {
  work=$(mktemp -d)
  mkdir -p "$work/bin" "$work/scripts" "$work/data"
  cp "$here/research-range.sh" "$work/scripts/"
  cat > "$work/bin/tf" <<'STUB'
#!/usr/bin/env bash
echo "tf $*" >> "$STUB_LOG"
[ "${STUB_RUN_FAILS:-0}" = 1 ] && [ "$1 $2" = "research run" ] && exit 1
exit 0
STUB
  cat > "$work/scripts/fetch_reference.sh" <<'STUB'
#!/usr/bin/env bash
echo "fetch_reference $*" >> "$STUB_LOG"
STUB
  # pull: stores the day unless it is in STUB_NO_TAPE (a space-separated list of dates).
  cat > "$work/scripts/pull_history.sh" <<'STUB'
#!/usr/bin/env bash
echo "pull_history $*" >> "$STUB_LOG"
case " ${STUB_NO_TAPE:-} " in *" $5 "*) exit 0;; esac
mkdir -p "$1/$2/$3"; echo x > "$1/$2/$3/$5.dbn.zst"
STUB
  cat > "$work/scripts/store_names.sh" <<'STUB'
#!/usr/bin/env bash
echo "store_names $*" >> "$STUB_LOG"
echo "0 SPY" > "$1/$2/$3/$4.names"
STUB
  chmod +x "$work/bin/tf" "$work/scripts/"*.sh
  : > "$work/log"
}

range() { # script name from to (extra env goes before the call: VAR=x range ...)
  env PATH="$work/bin:$PATH" STUB_LOG="$work/log" DATABENTO_API_KEY=k MAX_COST=1.00 \
    SET="$work/set" STORE="$work/data/store" REF="$work/data/ref" SNAPS="$work/data/snaps" RESEARCH="$work/data/research" \
    "$@" > "$work/out" 2>&1
}

# Thursday 8 October to Tuesday 13 October: Thu, Fri, Mon, Tue are weekdays; the 10th and 11th are not.
new_world
range "$work/scripts/research-range.sh" oct 2026-10-08 2026-10-13; code=$?
[ "$code" = 0 ] && ok "a range is run" || no "a range is run: exit $code: $(cat "$work/out")"
[ "$(grep -c '^pull_history' "$work/log")" = 4 ] && ok "pulls the four weekdays and no weekend" || no "pulls the four weekdays and no weekend: $(grep pull_history "$work/log")"
grep -q "pull_history $work/data/store EQUS.MINI tbbo ALL_SYMBOLS 2026-10-09 2026-10-10" "$work/log" && ok "pulls a day alone" || no "pulls a day alone"
[ "$(grep -c '^store_names' "$work/log")" = 4 ] && ok "names each day pulled" || no "names each day pulled"
grep -q "fetch_reference $work/data/ref 2026-07-25 2026-10-13" "$work/log" && ok "fetches the bars from 75 days before the range" || no "fetches the bars from 75 days before the range: $(grep fetch_ "$work/log")"
grep -q "tf research snapshots .*--from 2026-10-08 --to 2026-10-13 --out $work/data/snaps" "$work/log" && ok "makes the snapshots for the range" || no "makes the snapshots for the range"
grep -q "tf research run .*--out $work/data/research/oct --from 2026-10-08 --to 2026-10-13 --evidence" "$work/log" && ok "runs the set into the scenario" || no "runs the set into the scenario: $(grep 'research run' "$work/log")"
order=$(grep -n "pull_history\|store_names\|fetch_reference\|research snapshots\|history index\|research run\|research show" "$work/log" | sed -E 's/^[0-9]+://; s/ .*//' | uniq | tr '\n' ' ')
case "$order" in "pull_history store_names pull_history store_names pull_history store_names pull_history store_names fetch_reference tf ") ok "in the right order";; *) no "in the right order: $order";; esac

# Days already stored are not pulled again or named again.
range "$work/scripts/research-range.sh" oct 2026-10-08 2026-10-13; code=$?
[ "$code" = 0 ] && [ "$(grep -c '^pull_history' "$work/log")" = 4 ] && [ "$(grep -c '^store_names' "$work/log")" = 4 ] && ok "a stored day is not pulled or named again" || no "a stored day is not pulled or named again"

# A holiday (the pull stores nothing) is left out and said, and the rest runs.
new_world
STUB_NO_TAPE=2026-10-09 range "$work/scripts/research-range.sh" oct 2026-10-08 2026-10-09; code=$?
[ "$code" = 0 ] && grep -q "no tape for 2026-10-09" "$work/out" && [ "$(grep -c '^store_names' "$work/log")" = 1 ] && ok "a day with no tape is left out and said" || no "a day with no tape is left out and said: $(cat "$work/out")"

# No tape at all: nothing is run.
new_world
STUB_NO_TAPE=2026-10-08 range "$work/scripts/research-range.sh" oct 2026-10-08 2026-10-08; code=$?
[ "$code" = 1 ] && ! grep -q "research run" "$work/log" && ok "no stored day is an error and runs nothing" || no "no stored day is an error and runs nothing: exit $code"

# A failing run is a failing script.
new_world
STUB_RUN_FAILS=1 range "$work/scripts/research-range.sh" oct 2026-10-08 2026-10-08; code=$?
[ "$code" != 0 ] && ok "a failed run fails the script" || no "a failed run fails the script"

# Refusals, before anything is pulled.
refused() { # description, args...
  d=$1; shift
  new_world
  range "$work/scripts/research-range.sh" "$@"; code=$?
  [ "$code" = 2 ] && [ ! -s "$work/log" ] && ok "refuses $d" || no "refuses $d: exit $code, log: $(cat "$work/log")"
}
refused "a name with a slash" a/b 2026-10-08 2026-10-08
refused "a name that starts with a dot" .x 2026-10-08 2026-10-08
refused "a date that is not one" oct 2026-10-32 2026-10-08
refused "a range backwards" oct 2026-10-09 2026-10-08
refused "a weekend alone" oct 2026-10-10 2026-10-11
new_world
env PATH="$work/bin:$PATH" STUB_LOG="$work/log" "$work/scripts/research-range.sh" oct 2026-10-08 2026-10-08 > "$work/out" 2>&1 </dev/null
[ $? != 0 ] && [ ! -s "$work/log" ] && ok "refuses without the Databento key" || no "refuses without the Databento key"

# backtest-dev.sh: the Job it would start, and its refusals, with no cluster.
job=$("$here/backtest-dev.sh" --dry-run oct 2026-10-08 2026-10-13 2>&1); code=$?
[ "$code" = 0 ] && ok "a dry run prints the job" || no "a dry run prints the job: exit $code"
case "$job" in *'"/opt/tickforge/scripts/research-range.sh", "oct", "2026-10-08", "2026-10-13"'*) ok "the job runs the range script with the scenario and dates";; *) no "the job runs the range script with the scenario and dates";; esac
case "$job" in *"claimName: tickforge-history"*"readOnlyRootFilesystem: true"*|*"readOnlyRootFilesystem: true"*"claimName: tickforge-history"*) ok "the job has the history volume and a read-only root";; *) no "the job has the history volume and a read-only root";; esac
case "$job" in *"name: tickforge-live"*) no "the job does not touch the live volume";; *) ok "the job does not touch the live volume";; esac
case "$job" in *"name: backtest-oct-"*) ok "the job is named for the scenario";; *) no "the job is named for the scenario";; esac
[ "$(printf '%s' "$job" | grep -c 'kind: Job')" = 1 ] && ok "one job" || no "one job"
for bad in "a/b 2026-10-08 2026-10-08" ".x 2026-10-08 2026-10-08" "oct 2026-13-01 2026-10-08" "oct 2026-10-09 2026-10-08" "oct 2026-10-08"; do
  # shellcheck disable=SC2086
  "$here/backtest-dev.sh" --dry-run $bad > "$work/out" 2>&1; code=$?
  [ "$code" != 0 ] && ok "backtest-dev refuses: $bad" || no "backtest-dev refuses: $bad"
done

[ "$fail" = 0 ] && echo "all ok" || echo "FAILED"
exit "$fail"
