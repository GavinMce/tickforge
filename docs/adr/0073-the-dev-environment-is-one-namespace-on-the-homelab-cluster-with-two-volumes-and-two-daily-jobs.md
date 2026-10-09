# 0073. The dev environment is one namespace on the homelab cluster, with two volumes and two daily jobs

- Status: Accepted
- Date: 2026-10-08
- Jira: TIC-192

## Context

The system is a workspace to look at, a day to run against the live feed, and research to run over history. The homelab Talos
cluster on Proxmox (`~/blockhost`, Flux, MetalLB, Traefik, cert-manager, Proxmox CSI) exists and is healthy. The design called for NATS,
MinIO, Postgres and Kueue; none is needed to run a day, whose path is in one process with a file ledger (ADR 0050, ADR 0072).

## Decision

- **One namespace, `tickforge`, added by this repository and applied from it** (`deploy/k8s`, Kustomize, `scripts/deploy-dev.sh`).
  Nothing in the cluster outside it is changed (no node labels, no Flux resources), and blockhost is not edited.
- **What runs:** the workspace (always on), a prepare job (02:30 New York time, weekdays) and a live job (03:50, weekdays), each a
  CronJob on New York time that never runs twice at once and has a deadline. The live job is `tf live run` to the 20:00 close; a
  crash is run again twice at most (the ledger continues, the day gets a numbered directory). No NATS, MinIO, Postgres or Kueue: none
  has a job to do yet.
- **Storage:** two Proxmox CSI volumes (ReadWriteOnce): `tickforge-live` (30 Gi: the ledger, each day's capture, log and report, snapshots,
  certificates) and `tickforge-history` (60 Gi: the history store and research results). Every pod that uses them is kept on the
  workspace's node by pod affinity to its label, so no node is labelled. The storage class deletes a volume with its claim and nothing
  is backed up: the runbook says so, and says to copy results off before taking the namespace down. Whether the Proxmox pool has the
  room for a year of history is not known; the claim is expandable.
- **The image** is built by CI and published to GitHub's registry (`ghcr.io/gavinmce/tickforge`), tagged with the commit (and `main`
  for main); a pull request's branch is tagged so it can be tried before it is merged. It holds `tf` and the scripts, runs as a fixed
  unprivileged user.
- **Every pod is unprivileged** with a read-only root file system, all capabilities dropped, no service account token, requests and
  limits; no host path; the only secrets are two made out of band. `scripts/check_manifests.py` enforces these rules on the rendered
  manifests and runs in CI, as do the stand-in tests of the deploy and prepare scripts.
- **The deploy script names its cluster** (`admin@homelab`) on every call, refuses unless the cluster has the node `talos-cp-1`, never
  changes the machine's current context (a work cluster is configured on it), and accepts an API address for the control-plane node
  while the VIP does not answer.
- **Access is LAN only:** Traefik terminates TLS with a certificate the namespace's own issuer signs for `tickforge.homelab.lan`
  (not in public DNS), and the workspace is behind one shared token. Real sign-in with an identity provider is E17-S17 and
  comes before anything is exposed beyond the LAN.
- **The strategy set, the live config and the universe are a ConfigMap** made from `deploy/k8s/config/`, so a change is a commit and a
  deploy; the prepare job certifies again when it notices the set changed.

## Consequences

- One command deploys; one more opens a trade page through the deployed service.
- A day on the cluster is a Job and survives the workspace being redeployed.
- The first real session will exercise the cluster path and the gateway together; the first thing to run is `tf live check` as a one-off
  pod, which needs nothing but the key.
