#!/usr/bin/env python3
"""Check the cluster manifests (deploy/k8s) as `kubectl kustomize` renders them, against the rules they are written to.

    python3 -B scripts/check_manifests.py [DIR]       # default deploy/k8s; exit 1 and name every broken rule

Nothing is applied and no cluster is needed. The rules: everything in the namespace `tickforge`; one image of ours at one tag; every
pod unprivileged with a read-only root file system, no extra capabilities and no service account token, with resource requests and
limits; only the volumes and secrets the files name (no secret is defined in the repository, no host path); every cron job on New
York time, never running twice at once and with a deadline; the daily jobs on the workspace's node through affinity to its label.
"""
import subprocess
import sys

import yaml

DIR = sys.argv[1] if len(sys.argv) > 1 else "deploy/k8s"
NAMESPACE = "tickforge"
IMAGE = "ghcr.io/gavinmce/tickforge"
SECRETS_OUTSIDE = {"tickforge-token", "tickforge-databento", "tickforge-tls"}

text = subprocess.run(["kubectl", "kustomize", DIR], check=True, capture_output=True, text=True).stdout
docs = [d for d in yaml.safe_load_all(text) if d]
bad = []


def fail(what):
    bad.append(what)


def name(d):
    return f"{d['kind']}/{d['metadata']['name']}"


by_kind = {}
for d in docs:
    by_kind.setdefault(d["kind"], []).append(d)

for d in docs:
    ns = d["metadata"].get("namespace")
    if d["kind"] != "Namespace" and ns != NAMESPACE:
        fail(f"{name(d)} is in namespace {ns!r}, not {NAMESPACE!r}")
    if d["kind"] == "Secret":
        fail(f"{name(d)}: a secret is defined in the repository")

pvcs = {d["metadata"]["name"] for d in by_kind.get("PersistentVolumeClaim", [])}
configmaps = {d["metadata"]["name"] for d in by_kind.get("ConfigMap", [])}
workspace = [d for d in by_kind.get("Deployment", []) if d["metadata"]["name"] == "tickforge-workspace"]
if len(workspace) != 1:
    fail("there must be exactly one Deployment/tickforge-workspace")
workspace_labels = workspace[0]["spec"]["template"]["metadata"]["labels"] if workspace else {}

pods = []  # (owner, pod spec, metadata)
for d in docs:
    if d["kind"] == "Deployment":
        pods.append((name(d), d["spec"]["template"]["spec"], d["spec"]["template"]["metadata"]))
    elif d["kind"] == "CronJob":
        t = d["spec"]["jobTemplate"]["spec"]["template"]
        pods.append((name(d), t["spec"], t["metadata"]))

tags = set()
for owner, spec, meta in pods:
    sec = spec.get("securityContext", {})
    if sec.get("runAsNonRoot") is not True:
        fail(f"{owner}: the pod does not run as non-root")
    if spec.get("automountServiceAccountToken") is not False:
        fail(f"{owner}: the service account token is mounted")
    if spec.get("hostNetwork") or spec.get("hostPID") or spec.get("hostIPC"):
        fail(f"{owner}: uses the host's network or processes")
    for v in spec.get("volumes", []):
        if "hostPath" in v:
            fail(f"{owner}: volume {v['name']} is a host path")
        if "persistentVolumeClaim" in v and v["persistentVolumeClaim"]["claimName"] not in pvcs:
            fail(f"{owner}: volume {v['name']} names a claim that is not defined")
        if "configMap" in v and v["configMap"]["name"] not in configmaps:
            fail(f"{owner}: volume {v['name']} names a config map that is not defined")
        if "secret" in v and v["secret"]["secretName"] not in SECRETS_OUTSIDE:
            fail(f"{owner}: volume {v['name']} names an unexpected secret {v['secret']['secretName']}")
    for c in spec.get("initContainers", []) + spec["containers"]:
        img = c["image"]
        if not img.startswith(IMAGE + ":"):
            fail(f"{owner}/{c['name']}: image {img} is not ours")
        else:
            tags.add(img.split(":", 1)[1])
        s = c.get("securityContext", {})
        if s.get("allowPrivilegeEscalation") is not False or s.get("readOnlyRootFilesystem") is not True:
            fail(f"{owner}/{c['name']}: privilege escalation or a writable root file system")
        if s.get("capabilities", {}).get("drop") != ["ALL"] or s.get("privileged"):
            fail(f"{owner}/{c['name']}: capabilities are not all dropped")
        for env in c.get("env", []):
            ref = env.get("valueFrom", {}).get("secretKeyRef")
            if ref and ref["name"] not in SECRETS_OUTSIDE:
                fail(f"{owner}/{c['name']}: env {env['name']} reads an unexpected secret")
            if "value" in env and "KEY" in env["name"].upper():
                fail(f"{owner}/{c['name']}: env {env['name']} holds a key as a literal")
        r = c.get("resources", {})
        if not r.get("requests") or not r.get("limits"):
            fail(f"{owner}/{c['name']}: no resource requests and limits")
        # A volume mounted must be a volume of the pod.
        names = {v["name"] for v in spec.get("volumes", [])}
        for m in c.get("volumeMounts", []):
            if m["name"] not in names:
                fail(f"{owner}/{c['name']}: mounts {m['name']}, which the pod does not have")

if len(tags) != 1:
    fail(f"the images are not all at one tag: {sorted(tags)}")

for d in by_kind.get("CronJob", []):
    s = d["spec"]
    n = name(d)
    if s.get("timeZone") != "America/New_York":
        fail(f"{n}: not on New York time")
    if s.get("concurrencyPolicy") != "Forbid":
        fail(f"{n}: may run twice at once")
    if "activeDeadlineSeconds" not in s["jobTemplate"]["spec"]:
        fail(f"{n}: no deadline")
    if s["jobTemplate"]["spec"]["template"]["spec"].get("restartPolicy") != "Never":
        fail(f"{n}: restarts a container in place")
    aff = s["jobTemplate"]["spec"]["template"]["spec"].get("affinity", {}).get("podAffinity", {})
    terms = aff.get("requiredDuringSchedulingIgnoredDuringExecution", [])
    if not any(
        all(workspace_labels.get(k) == v for k, v in t["labelSelector"]["matchLabels"].items())
        and t["topologyKey"] == "kubernetes.io/hostname"
        for t in terms
    ):
        fail(f"{n}: not kept on the workspace's node")

for kind in ("Namespace", "PersistentVolumeClaim", "ConfigMap", "Service", "Ingress", "CronJob", "Deployment"):
    if kind not in by_kind:
        fail(f"no {kind} is rendered")
for d in by_kind.get("PersistentVolumeClaim", []):
    if d["spec"].get("storageClassName") != "proxmox-vmdata":
        fail(f"{name(d)}: not on the Proxmox storage class")
for d in by_kind.get("Ingress", []):
    for t in d["spec"].get("tls", []):
        if t.get("secretName") not in SECRETS_OUTSIDE:
            fail(f"{name(d)}: TLS secret {t.get('secretName')} is unexpected")

if bad:
    print("\n".join(f"- {b}" for b in bad))
    sys.exit(1)
print(f"{len(docs)} objects checked: all rules hold")
