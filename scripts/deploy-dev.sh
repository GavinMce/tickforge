#!/usr/bin/env bash
# Deploy the workspace and the backtest view to the homelab Kubernetes cluster (E19-S38).
#
#   scripts/deploy-dev.sh [--dry-run] <image-tag>        e.g. the commit the image was built from
#
# The only cluster this touches is the homelab one. Every kubectl call names its context (`admin@homelab`) and nothing here
# ever runs `kubectl config use-context`, so the machine's current context (a work cluster is configured on it) is never used
# or changed. Before anything is applied the script checks that the context exists and that the cluster behind it has the
# homelab's control-plane node, and refuses otherwise.
#
#   TF_K8S_SERVER   optional API address for the context, for when the VIP in the kubeconfig does not answer
#                   (e.g. https://10.0.30.20:6443, the control-plane node itself)
#   TF_DEV_TOKEN_FILE  where the sign-in token is kept on this machine (default ~/.config/tickforge/dev-token); it is
#                   created if missing and put in the cluster as a secret; it is never printed
#   TF_DATABENTO_KEY_FILE  the Databento key (default ~/.config/tickforge/databento.key), put in the cluster as a secret if there
#                   is one there already it is left alone; never printed. Without the file the jobs that need it cannot run.
set -euo pipefail

CONTEXT="admin@homelab"
NODE="talos-cp-1"
NAMESPACE="tickforge"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOKEN_FILE="${TF_DEV_TOKEN_FILE:-$HOME/.config/tickforge/dev-token}"
KEY_FILE="${TF_DATABENTO_KEY_FILE:-$HOME/.config/tickforge/databento.key}"

dry=0
if [ "${1:-}" = "--dry-run" ]; then dry=1; shift; fi
tag="${1:?usage: deploy-dev.sh [--dry-run] <image-tag>}"
case "$tag" in
  *[!A-Za-z0-9._-]* | "") echo "refusing: the image tag must be letters, digits, . _ -" >&2; exit 2 ;;
esac

# Every call to the cluster goes through here, so none can leave out the context.
kc() {
  local server=()
  if [ -n "${TF_K8S_SERVER:-}" ]; then server=(--server "$TF_K8S_SERVER"); fi
  kubectl --context "$CONTEXT" "${server[@]}" --request-timeout=30s "$@"
}

if ! kubectl config get-contexts -o name | grep -qx "$CONTEXT"; then
  echo "refusing: no kubectl context named $CONTEXT on this machine" >&2
  exit 3
fi
if ! kc get node "$NODE" -o name >/dev/null 2>&1; then
  echo "refusing: the cluster behind $CONTEXT has no node $NODE, so it is not the homelab" >&2
  exit 3
fi

if [ "$dry" = 1 ]; then
  echo "dry run against $CONTEXT: would apply deploy/k8s with image tag $tag"
  (cd "$HERE/../deploy/k8s" && kubectl kustomize . | sed "s|ghcr.io/gavinmce/tickforge:main|ghcr.io/gavinmce/tickforge:$tag|")
  exit 0
fi

# The namespace first, so the token secret has somewhere to go.
kc apply -f "$HERE/../deploy/k8s/namespace.yaml" >/dev/null
if ! kc -n "$NAMESPACE" get secret tickforge-token >/dev/null 2>&1; then
  if [ ! -s "$TOKEN_FILE" ]; then
    mkdir -p "$(dirname "$TOKEN_FILE")"
    (umask 077; head -c 36 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 32 > "$TOKEN_FILE")
  fi
  kc -n "$NAMESPACE" create secret generic tickforge-token --from-file=token="$TOKEN_FILE" >/dev/null
  echo "created the sign-in token secret (the token is in $TOKEN_FILE)"
fi

if ! kc -n "$NAMESPACE" get secret tickforge-databento >/dev/null 2>&1; then
  if [ -s "$KEY_FILE" ]; then
    kc -n "$NAMESPACE" create secret generic tickforge-databento --from-file=api-key="$KEY_FILE" >/dev/null
    echo "created the Databento key secret (from $KEY_FILE)"
  else
    echo "no Databento key at $KEY_FILE: the prepare and live jobs cannot run until the secret tickforge-databento exists" >&2
  fi
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp -r "$HERE/../deploy/k8s/." "$work/"
(cd "$work" && kubectl kustomize . | sed "s|ghcr.io/gavinmce/tickforge:main|ghcr.io/gavinmce/tickforge:$tag|g" > rendered.yaml)
kc apply -f "$work/rendered.yaml"
kc -n "$NAMESPACE" rollout status deployment/tickforge-workspace --timeout=300s
echo "deployed $tag to $NAMESPACE on $CONTEXT: open it with scripts/smoke-dev.sh, or kubectl port-forward svc/tickforge-workspace 8787:80"
