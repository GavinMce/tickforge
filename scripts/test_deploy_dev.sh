#!/usr/bin/env bash
# Tests of scripts/deploy-dev.sh (and the guard it shares with load-research.sh) with a stand-in for kubectl: what the scripts ask
# of the cluster, and that they ask nothing of any cluster but the homelab's. No cluster is touched.
#
#   bash scripts/test_deploy_dev.sh
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
real_kubectl=$(command -v kubectl || true)
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; fail=1; }

mkdir -p "$work/bin"
# The stand-in: records every call; answers from STUB_CONTEXTS (the contexts that exist), STUB_NODE (the node is there), and
# STUB_SECRETS (secrets that exist already).
cat > "$work/bin/kubectl" <<STUB
#!/usr/bin/env bash
echo "\$*" >> "\$STUB_LOG"
case "\$*" in
  *"config get-contexts"*) printf '%s\n' \$STUB_CONTEXTS; exit 0 ;;
  *" get node "*) [ "\${STUB_NODE:-0}" = 1 ] && { echo node/talos-cp-1; exit 0; } || exit 1 ;;
  *" get secret "*) for s in \${STUB_SECRETS:-}; do case "\$*" in *" \$s "*|*" \$s") exit 0 ;; esac; done; exit 1 ;;
  *"kustomize"*) exec "$real_kubectl" kustomize "\${@: -1}" ;;
  *" exec -i "*) cat > /dev/null; exit 0 ;;
  *) exit 0 ;;
esac
STUB
chmod +x "$work/bin/kubectl"

run() { # name, expected exit, then env assignments and arguments
  local label=$1 want=$2; shift 2
  : > "$work/log"
  env PATH="$work/bin:$PATH" STUB_LOG="$work/log" HOME="$work/home" TF_DEV_TOKEN_FILE="$work/token" TF_DATABENTO_KEY_FILE="$work/key" "$@" > "$work/out" 2>&1
  local got=$?
  if [ "$got" = "$want" ]; then ok "$label"; else no "$label: wanted exit $want, got $got: $(head -c 300 "$work/out")"; fi
}
mkdir -p "$work/home"

# The guard: the context must exist on this machine and the cluster behind it must have the homelab's control-plane node.
run "refuses when there is no homelab context" 3 STUB_CONTEXTS="Platform" STUB_NODE=1 "$here/deploy-dev.sh" abc123
grep -q "no kubectl context named admin@homelab" "$work/out" && ok "says which context is missing" || no "says which context is missing"
if grep -qE "apply|create secret" "$work/log"; then no "applied nothing without the context"; else ok "applied nothing without the context"; fi

run "refuses when the cluster has no talos-cp-1" 3 STUB_CONTEXTS="Platform admin@homelab" STUB_NODE=0 "$here/deploy-dev.sh" abc123
grep -q "is not the homelab" "$work/out" && ok "says it is not the homelab" || no "says it is not the homelab"
if grep -qE "apply|create secret" "$work/log"; then no "applied nothing to another cluster"; else ok "applied nothing to another cluster"; fi

run "refuses a tag that is not a tag" 2 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 "$here/deploy-dev.sh" 'abc; rm -rf /'
run "refuses a missing tag" 1 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 "$here/deploy-dev.sh"

# A deployment: every call to the cluster names the homelab context and none changes the current one.
printf 'k' > "$work/key"
run "deploys" 0 STUB_CONTEXTS="Platform admin@homelab" STUB_NODE=1 TF_K8S_SERVER=https://10.0.30.20:6443 "$here/deploy-dev.sh" abc123
if grep -v "config get-contexts" "$work/log" | grep -v "kustomize" | grep -v -- "--context admin@homelab" | grep -q .; then no "every call names the context"; else ok "every call names the context"; fi
grep -q "use-context" "$work/log" && no "never changes the current context" || ok "never changes the current context"
grep -q -- "--server https://10.0.30.20:6443" "$work/log" && ok "uses the server override" || no "uses the server override"
grep -q "create secret generic tickforge-token" "$work/log" && ok "makes the token secret" || no "makes the token secret"
grep -q "create secret generic tickforge-databento" "$work/log" && ok "makes the key secret" || no "makes the key secret"
grep -q "rollout status deployment/tickforge-workspace" "$work/log" && ok "waits for the workspace" || no "waits for the workspace"
[ -s "$work/token" ] && [ "$(stat -c %a "$work/token")" = 600 ] && ok "keeps the token private (mode 600)" || no "keeps the token private"
if grep -qF "$(cat "$work/token")" "$work/out" "$work/log"; then no "never prints or passes the token"; else ok "never prints or passes the token"; fi

# Secrets that are there already are left alone, and a missing key file is said, not fatal.
rm -f "$work/token"; rm -f "$work/key"
run "leaves existing secrets alone" 0 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 STUB_SECRETS="tickforge-token tickforge-databento" "$here/deploy-dev.sh" abc123
grep -q "create secret" "$work/log" && no "created a secret that was there" || ok "created no secret that was there"
run "says when there is no Databento key" 0 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 STUB_SECRETS="tickforge-token" "$here/deploy-dev.sh" abc123
grep -q "no Databento key" "$work/out" && ok "says there is no Databento key" || no "says there is no Databento key"

# A dry run applies nothing and shows the manifests with the tag.
if [ -n "$real_kubectl" ]; then
  run "a dry run" 0 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 "$here/deploy-dev.sh" --dry-run abc123
  grep -q "ghcr.io/gavinmce/tickforge:abc123" "$work/out" && ok "shows the manifests at the tag" || no "shows the manifests at the tag"
  grep -q "tickforge:main" "$work/out" && no "no image is left at main" || ok "no image is left at main"
  grep -qE "apply|create secret" "$work/log" && no "a dry run applies nothing" || ok "a dry run applies nothing"
fi

# The loader and the check use the same guard.
mkdir -p "$work/scn/a"; : > "$work/scn/a/research.cfg"
run "the loader refuses without the context" 3 STUB_CONTEXTS="Platform" STUB_NODE=1 "$here/load-research.sh" "$work/scn"
run "the loader refuses a name that is not plain" 2 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 "$here/load-research.sh" "$work/scn" "../x"
run "the loader loads" 0 STUB_CONTEXTS="admin@homelab" STUB_NODE=1 "$here/load-research.sh" "$work/scn"
grep -q "exec -i deploy/tickforge-workspace" "$work/log" && ok "loads through the workspace pod" || no "loads through the workspace pod"
exit "$fail"
