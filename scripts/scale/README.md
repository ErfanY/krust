# krust scale lab (kwok)

Local high-scale test harness for the stable-release work (Phase 0.2). Uses
[`kwok`](https://kwok.sigs.k8s.io/) to simulate large clusters with near-zero compute:
fake nodes report huge capacity (pods: 1M) and fake pods go `Running` instantly.

## Requirements
`kwokctl`, `kubectl`, `yq`, a running Docker daemon. All via Homebrew.

## Usage

```bash
cd scripts/scale

./up.sh           # create the kwok cluster (idempotent)
./load.sh         # 20 nodes, 20 namespaces, 10,000 pods (+ svc/cm/secrets)
./contexts.sh     # write kubeconfig-scale.yaml with 20 synthetic contexts

# point krust at the lab:
cargo run -- --kubeconfig scripts/scale/kubeconfig-scale.yaml

./churn.sh        # (optional, separate terminal) continuous deployment restarts
./down.sh         # tear everything down
```

## Tuning (all `KRUST_`-prefixed, see `lib.sh`)
Total pods = `KRUST_NAMESPACES * KRUST_DEPLOYS_PER_NS * KRUST_REPLICAS`.

```bash
KRUST_NAMESPACES=40 KRUST_REPLICAS=250 ./load.sh   # 40 * 5 * 250 = 50,000 pods
KRUST_NODES=50 ./load.sh
KRUST_SCALE_CONTEXTS=30 ./contexts.sh
KRUST_CHURN_INTERVAL=1 KRUST_CHURN_BATCH=3 ./churn.sh
```

## Notes / limitations
- The 20 contexts target one kwok apiserver with distinct default namespaces. This faithfully
  exercises krust's per-context client init, watch plans, and warm-context retention (20 clients
  / 20 watcher sets), but not cross-cluster network variance. For true multi-cluster, create
  several kwok clusters (`KRUST_SCALE_CLUSTER=...`) and merge their kubeconfigs.
- kwok runs no real kubelet: logs are empty and exec/port-forward won't behave like a real pod.
  This lab is for list/watch/projection/render scale, not log-content fidelity.
