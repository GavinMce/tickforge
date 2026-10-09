#!/usr/bin/env bash
# Copy research scenarios (results directories, each with its research.cfg) into the dev environment's volume (E19-S38).
#
#   scripts/load-research.sh <root-dir> [scenario ...]    a directory of scenarios; all of them if none are named
#
# Same guard as deploy-dev.sh: only the homelab context, named on every call, never changed.
set -euo pipefail

CONTEXT="admin@homelab"
NODE="talos-cp-1"
NAMESPACE="tickforge"

root="${1:?usage: load-research.sh <root-dir> [scenario ...]}"
shift
[ -d "$root" ] || { echo "refusing: $root is not a directory" >&2; exit 2; }
names=("$@")
if [ "${#names[@]}" -eq 0 ]; then
  for d in "$root"/*/; do
    [ -f "$d/research.cfg" ] && names+=("$(basename "$d")")
  done
fi
[ "${#names[@]}" -gt 0 ] || { echo "refusing: no scenarios (directories with a research.cfg) in $root" >&2; exit 2; }
for n in "${names[@]}"; do
  case "$n" in
    "" | .* | */* | *[!A-Za-z0-9._-]*) echo "refusing: $n is not a plain scenario name" >&2; exit 2 ;;
  esac
  [ -f "$root/$n/research.cfg" ] || { echo "refusing: $root/$n has no research.cfg" >&2; exit 2; }
done

kc() {
  local server=()
  if [ -n "${TF_K8S_SERVER:-}" ]; then server=(--server "$TF_K8S_SERVER"); fi
  kubectl --context "$CONTEXT" "${server[@]}" --request-timeout=60s "$@"
}
kubectl config get-contexts -o name | grep -qx "$CONTEXT" || { echo "refusing: no kubectl context named $CONTEXT" >&2; exit 3; }
kc get node "$NODE" -o name >/dev/null 2>&1 || { echo "refusing: the cluster behind $CONTEXT has no node $NODE" >&2; exit 3; }

for n in "${names[@]}"; do
  echo "loading $n"
  tar -C "$root" -cf - "$n" | kc -n "$NAMESPACE" exec -i deploy/tickforge-workspace -- tar -C /data/history/research -xf -
done
kc -n "$NAMESPACE" exec deploy/tickforge-workspace -- ls /data/history/research
