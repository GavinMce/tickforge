#!/usr/bin/env bash
# Run the strategy set over a range of days on the homelab cluster and say where to look at the result.
#
#   scripts/backtest-dev.sh [--dry-run] [--detach] [--set FILE] NAME FROM TO       # FROM and TO are dates, both included
#
# Starts a Job from the image the workspace runs (scripts/research-range.sh: pull the days that are not stored, make the
# snapshots, run the set) and follows its log; the result is the scenario NAME in the workspace's Backtests screen. --detach
# starts it and returns. --dry-run prints the Job and touches nothing. The set that is run is a file of the deployed config
# (deploy/k8s/config/): month.set, or the one --set names (e.g. premarket.set); change it and deploy first. A scenario is one configuration: use a new NAME for another set.
#
# Same guard as deploy-dev.sh: only the homelab context, named on every call, never changed.
#   TF_K8S_SERVER   optional API address for the context (e.g. https://10.0.30.20:6443)
#   TF_IMAGE        the image to run (default: the one the workspace deployment runs)
#   TF_WORKSPACE_URL  what is printed at the end (default http://10.0.30.43:8787/)
set -euo pipefail

CONTEXT="admin@homelab"
NODE="talos-cp-1"
NAMESPACE="tickforge"
URL="${TF_WORKSPACE_URL:-http://10.0.30.43:8787/}"

dry=0
detach=0
setfile="month.set"
while [ "${1:-}" = "--dry-run" ] || [ "${1:-}" = "--detach" ] || [ "${1:-}" = "--set" ]; do
  case "$1" in
    --dry-run) dry=1 ;;
    --detach) detach=1 ;;
    --set) shift; setfile="${1:?--set needs a file name}" ;;
  esac
  shift
done
case "$setfile" in
  "" | .* | *[!A-Za-z0-9._-]*) echo "refusing: $setfile is not a plain file name of the config" >&2; exit 2 ;;
esac
name="${1:?usage: backtest-dev.sh [--dry-run] [--detach] NAME FROM TO}"
from="${2:?usage: backtest-dev.sh [--dry-run] [--detach] NAME FROM TO}"
to="${3:?usage: backtest-dev.sh [--dry-run] [--detach] NAME FROM TO}"
case "$name" in
  "" | .* | *[!A-Za-z0-9._-]*) echo "refusing: $name is not a plain scenario name (letters, digits, . _ -)" >&2; exit 2 ;;
esac
[ "${#name}" -le 40 ] || { echo "refusing: the scenario name is longer than 40" >&2; exit 2; }
for d in "$from" "$to"; do
  date -d "$d" +%F >/dev/null 2>&1 && [ "$(date -d "$d" +%F)" = "$d" ] || { echo "refusing: $d is not a date (YYYY-MM-DD)" >&2; exit 2; }
done
[[ ! "$from" > "$to" ]] || { echo "refusing: $from is after $to" >&2; exit 2; }

kc() {
  local server=()
  if [ -n "${TF_K8S_SERVER:-}" ]; then server=(--server "$TF_K8S_SERVER"); fi
  kubectl --context "$CONTEXT" "${server[@]}" --request-timeout=60s "$@"
}

job="backtest-$(printf '%s' "$name" | tr 'A-Z._' 'a-z--')-$(date +%H%M%S)"

image="${TF_IMAGE:-}"
if [ "$dry" = 0 ]; then
  kubectl config get-contexts -o name | grep -qx "$CONTEXT" || { echo "refusing: no kubectl context named $CONTEXT" >&2; exit 3; }
  kc get node "$NODE" -o name >/dev/null 2>&1 || { echo "refusing: the cluster behind $CONTEXT has no node $NODE" >&2; exit 3; }
  [ -n "$image" ] || image="$(kc -n "$NAMESPACE" get deploy tickforge-workspace -o jsonpath='{.spec.template.spec.containers[0].image}')"
fi
image="${image:-ghcr.io/gavinmce/tickforge:main}"

manifest() {
  cat <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: $job
  namespace: $NAMESPACE
  labels: {app.kubernetes.io/name: tickforge-backtest, app.kubernetes.io/part-of: tickforge}
spec:
  backoffLimit: 0
  activeDeadlineSeconds: 21600
  ttlSecondsAfterFinished: 604800
  template:
    metadata:
      labels: {app.kubernetes.io/name: tickforge-backtest, app.kubernetes.io/part-of: tickforge}
    spec:
      restartPolicy: Never
      automountServiceAccountToken: false
      securityContext:
        runAsNonRoot: true
        runAsUser: 10001
        runAsGroup: 10001
        fsGroup: 10001
        seccompProfile: {type: RuntimeDefault}
      affinity:
        podAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            - labelSelector:
                matchLabels: {app.kubernetes.io/name: tickforge-workspace}
              topologyKey: kubernetes.io/hostname
      containers:
        - name: backtest
          image: $image
          command: ["/opt/tickforge/scripts/research-range.sh", "$name", "$from", "$to"]
          env:
            - name: SET
              value: /config/$setfile
            - name: DATABENTO_API_KEY
              valueFrom: {secretKeyRef: {name: tickforge-databento, key: api-key}}
          resources:
            requests: {cpu: 500m, memory: 1Gi}
            limits: {cpu: "3", memory: 4Gi}
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities: {drop: [ALL]}
          volumeMounts:
            - {name: config, mountPath: /config, readOnly: true}
            - {name: history, mountPath: /data/history}
            - {name: tmp, mountPath: /tmp}
      volumes:
        - name: config
          configMap: {name: tickforge-config}
        - name: history
          persistentVolumeClaim: {claimName: tickforge-history}
        - name: tmp
          emptyDir: {}
EOF
}

if [ "$dry" = 1 ]; then
  manifest
  exit 0
fi

manifest | kc apply -f - >/dev/null
echo "started job $job: scenario $name, $from to $to, set $setfile"
if [ "$detach" = 1 ]; then
  echo "follow it:  kubectl --context $CONTEXT -n $NAMESPACE logs -f job/$job"
  echo "then open:  $URL (Backtests)"
  exit 0
fi
kc -n "$NAMESPACE" wait --for=condition=ready pod -l job-name="$job" --timeout=300s >/dev/null 2>&1 || true
kc -n "$NAMESPACE" logs -f "job/$job" || true
state="$(kc -n "$NAMESPACE" get job "$job" -o jsonpath='{.status.succeeded}/{.status.failed}')"
case "$state" in
  1/*) echo "finished: open $URL and choose Backtests, scenario $name" ;;
  *) echo "the job did not finish well (succeeded/failed: $state): kubectl --context $CONTEXT -n $NAMESPACE logs job/$job" >&2; exit 1 ;;
esac
