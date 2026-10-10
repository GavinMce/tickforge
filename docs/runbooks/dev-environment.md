# The dev environment on the homelab cluster

What runs where, how to deploy it, how to see it and how to take it down (ADR 0073). The cluster is the Talos Kubernetes cluster
on Proxmox that `~/blockhost` builds; this repository adds one namespace to it and changes nothing else.

## What is there

Namespace `tickforge`:

| What | Kind | Does |
|---|---|---|
| `tickforge-workspace` | Deployment, Service, Ingress | `tf serve`: the overview, runs, replayed days and trade replay, read from the live ledger and the research results. Token sign-in, LAN only. |
| `tickforge-prepare` | CronJob 02:30 ET, Mon-Fri | `scripts/prepare-day.sh`: the day's reference snapshot, and certificates when the strategy set changed. |
| `tickforge-live` | CronJob 03:50 ET, Mon-Fri | `tf live run` from 03:55 to the 20:00 close, on the simulated broker. |
| `tickforge-live` and `tickforge-history` | PVCs (30 Gi, 60 Gi) | The ledger, each day's capture, log and report, snapshots and certificates; the history store and the research results. |
| `tickforge-config` | ConfigMap | `live.cfg`, `month.set` (the variants) and the universe, from `deploy/k8s/config/`. |
| `tickforge-token`, `tickforge-databento` | Secrets | The sign-in token and the Databento key. Made by the deploy script from files on this machine; never in the repository. |

Both jobs are kept on the workspace's node (it holds the volumes). The image is built by CI (`.github/workflows/image.yml`) and
published to `ghcr.io/gavinmce/tickforge`; a pull request's branch is tagged with its commit so it can be tried before it is merged.

## Deploy

```sh
# the tag is the commit the image was built from (the Image workflow's run, or `git rev-parse HEAD`)
export TF_K8S_SERVER=https://10.0.30.20:6443      # the control-plane node, while the API address 10.0.30.30 does not answer
scripts/deploy-dev.sh --dry-run <tag>             # what would be applied
scripts/deploy-dev.sh <tag>
scripts/smoke-dev.sh                              # signs in through a port-forward and opens a trade page
```

The script names its cluster (`admin@homelab`) on every call, refuses unless the cluster behind that context has the node
`talos-cp-1`, and never changes your current context (a work cluster is configured on this machine). It makes the two secrets
if they are not there: the token in `~/.config/tickforge/dev-token` (made if missing, mode 600) and the Databento key from
`~/.config/tickforge/databento.key`.

## See it

```sh
kubectl --context admin@homelab --server https://10.0.30.20:6443 -n tickforge port-forward svc/tickforge-workspace 8787:80
# then http://127.0.0.1:8787/ with the token from ~/.config/tickforge/dev-token
```

Through Traefik instead: add `10.0.30.41 tickforge.homelab.lan` to the hosts file of the machine that opens it and go to
https://tickforge.homelab.lan/ (the certificate is the namespace's own, so the browser will warn). It is not exposed beyond the LAN.

## Run things by hand

```sh
K="kubectl --context admin@homelab --server https://10.0.30.20:6443 -n tickforge"
$K create job --from=cronjob/tickforge-prepare prepare-now     # prepare today (the day, if it is a trading day)
$K create job --from=cronjob/tickforge-live live-now           # a live day now (it starts at once if past 03:55)
$K logs -f job/live-now
$K exec deploy/tickforge-workspace -- touch /data/live/run/STOP   # end a live day cleanly; remove it afterwards
scripts/load-research.sh DIR [scenario...]                      # copy research results in to be seen in the workspace
```

A one-off command in the image (the key from the secret):

```sh
$K run tf-check --rm -i --restart=Never --image=ghcr.io/gavinmce/tickforge:<tag> \
   --overrides='{"spec":{"securityContext":{"runAsNonRoot":true,"runAsUser":10001},"containers":[{"name":"tf-check","image":"ghcr.io/gavinmce/tickforge:<tag>","args":["live","check","--config","/config/live.cfg","--seconds","30"],"env":[{"name":"DATABENTO_API_KEY","valueFrom":{"secretKeyRef":{"name":"tickforge-databento","key":"api-key"}}}],"volumeMounts":[{"name":"config","mountPath":"/config"}]}],"volumes":[{"name":"config","configMap":{"name":"tickforge-config"}}]}}'
```

## Run a strategy over a range of days

```sh
export TF_K8S_SERVER=https://10.0.30.20:6443
scripts/backtest-dev.sh sept-reversal 2026-09-08 2026-10-08     # pulls what is missing, runs the set, follows the log
# then http://10.0.30.43:8787/ with the token from ~/.config/tickforge/dev-token: Backtests, scenario sept-reversal
```

The days come from `EQUS.MINI` `tbbo`, which the plan includes for the last twelve months (about 220 MB a day stored on the history
volume, 60 Gi in all); a pull that would cost over a dollar a day is refused. The set run is the deployed `month.set`, or the file of the config that `--set FILE` names (`--set premarket.set` runs the premarket
strategy's variants, which the live day does not read); a scenario is one configuration, so a changed set needs a new name. `--detach` starts it and returns; `--dry-run NAME FROM TO` prints the Job.
Closed days and weekends are left out and said. The workspace is on `10.0.30.43:8787` over plain HTTP (the sign-in token crosses the
LAN unencrypted) and on `https://tickforge.homelab.lan` through Traefik.

## Upgrade, roll back, take down

- **Upgrade:** deploy a newer tag. The workspace restarts (a few seconds); a day in progress is a Job and is not touched.
- **Roll back:** deploy the previous tag.
- **Change the strategy set:** edit `deploy/k8s/config/month.set`, deploy; the next prepare job notices and certifies again.
- **Take it down:** `kubectl delete ns tickforge` deletes the volumes with it (the storage class deletes a volume with its claim).
  Copy `/data/live/run` and the research results off first.

## What this is not

Not backed up. Not exposed beyond the LAN, and behind one shared token until an identity provider is chosen (E17-S17). No orders
reach a broker. No alerts: a failed job shows in `kubectl get jobs` and `kubectl logs`, nothing more (E18-S09).

## Checked on 2026-10-08

Deployed from a pull request's image, opened through a port-forward and through Traefik, and a one-off `tf live check` pod reached the
Databento gateway from inside the cluster and authenticated with the key from the secret. The gateway refused the session only because the
account had no live data license for `EQUS.MINI` yet. The prepare and live jobs had not run.

