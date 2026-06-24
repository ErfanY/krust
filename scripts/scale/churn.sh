#!/usr/bin/env bash
# Generate steady state churn so krust's watch/delta/projection paths are exercised under
# continuous change (the realistic condition for latency regressions). Rolling-restarts a
# random deployment each tick, which deletes+recreates its pods (replicaset churn + pod
# add/remove deltas). Ctrl-C to stop.
#
# Tunables: KRUST_CHURN_INTERVAL (seconds, default 2), KRUST_CHURN_BATCH (deploys/tick, default 1)
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require kubectl

interval="${KRUST_CHURN_INTERVAL:-2}"
batch="${KRUST_CHURN_BATCH:-1}"

echo "churn: restarting ${batch} deployment(s) every ${interval}s (Ctrl-C to stop)"
trap 'echo; echo "churn stopped"; exit 0' INT

tick=0
while true; do
  for ((b=0; b<batch; b++)); do
    i=$(( RANDOM % KRUST_NAMESPACES ))
    d=$(( RANDOM % KRUST_DEPLOYS_PER_NS ))
    ns="$(ns_name "$i")"
    kc -n "$ns" rollout restart "deployment/app-${d}" >/dev/null 2>&1 \
      && echo "  [tick ${tick}] restarted ${ns}/app-${d}" \
      || echo "  [tick ${tick}] skip ${ns}/app-${d} (not found)"
  done
  tick=$(( tick + 1 ))
  sleep "$interval"
done
