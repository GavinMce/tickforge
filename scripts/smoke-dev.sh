#!/usr/bin/env bash
# Open a stored scenario through the deployed service (E19-S38): sign-in is required, the scenario list answers, and a trade page
# opens with the policy that lets it run its own script and nothing else.
#
#   scripts/smoke-dev.sh [--via-ingress]
#
# By default it goes through a port-forward to the service; --via-ingress goes to https://tickforge.homelab.lan (the hosts file must
# name it), which also tests Traefik and the certificate (the certificate is self-signed, so it is not verified).
set -euo pipefail

CONTEXT="admin@homelab"
NAMESPACE="tickforge"
TOKEN_FILE="${TF_DEV_TOKEN_FILE:-$HOME/.config/tickforge/dev-token}"
[ -s "$TOKEN_FILE" ] || { echo "no token file at $TOKEN_FILE (deploy first)" >&2; exit 2; }
token="$(cat "$TOKEN_FILE")"

kc() {
  local server=()
  if [ -n "${TF_K8S_SERVER:-}" ]; then server=(--server "$TF_K8S_SERVER"); fi
  kubectl --context "$CONTEXT" "${server[@]}" "$@"
}

pf=""
cleanup() { [ -n "$pf" ] && kill "$pf" 2>/dev/null || true; }
trap cleanup EXIT
if [ "${1:-}" = "--via-ingress" ]; then
  base="https://tickforge.homelab.lan"
  curlopt=(-k --resolve "tickforge.homelab.lan:443:10.0.30.41")
else
  kc -n "$NAMESPACE" port-forward svc/tickforge-workspace 18787:80 >/dev/null 2>&1 &
  pf=$!
  base="http://127.0.0.1:18787"
  curlopt=()
  for _ in $(seq 1 30); do curl -fs "$base/health" >/dev/null 2>&1 && break; sleep 1; done
fi

fail=0
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: wanted $2, got $3"; fail=1; fi
}
code() { curl -s "${curlopt[@]}" -o /dev/null -w '%{http_code}' "$@"; }

check "health answers without signing in" 200 "$(code "$base/health")"
check "the scenario list needs the token" 401 "$(code "$base/api/research")"
check "the scenario list with the token" 200 "$(code -H "Authorization: Bearer $token" "$base/api/research")"
list="$(curl -s "${curlopt[@]}" -H "Authorization: Bearer $token" "$base/api/research")"
scenario="$(printf '%s' "$list" | python3 -c 'import sys,json; d=json.load(sys.stdin)["scenarios"]; print(d[0]["name"] if d else "")')"
if [ -z "$scenario" ]; then
  echo "FAIL no scenario is stored (scripts/load-research.sh puts them there)"; exit 1
fi
read -r day strategy < <(printf '%s' "$list" | python3 -c 'import sys,json; s=json.load(sys.stdin)["scenarios"][0]; print(s["days"][0], s["strategies"][0]["id"])')
q="scenario=$scenario&day=$day&strategy=$strategy"
check "the trades of a day" 200 "$(code -H "Authorization: Bearer $token" "$base/api/research/trades?$q")"
check "a trade page needs the token" 401 "$(code "$base/research/trade?$q&n=0")"
page="$(curl -s "${curlopt[@]}" -D - -H "Authorization: Bearer $token" "$base/research/trade?$q&n=0")"
check "a trade page opens" 200 "$(printf '%s' "$page" | head -1 | awk '{print $2}')"
printf '%s' "$page" | grep -qi "content-security-policy:.*connect-src 'none'" && echo "ok   the trade page's policy sends and loads nothing" || { echo "FAIL the trade page's policy"; fail=1; }
printf '%s' "$page" | grep -q 'id="data"' && echo "ok   the trade page has its data" || { echo "FAIL the trade page has no data"; fail=1; }
check "nothing but reading is accepted" 405 "$(code -X POST -H "Authorization: Bearer $token" "$base/api/research")"
exit "$fail"
