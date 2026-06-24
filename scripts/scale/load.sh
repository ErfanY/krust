#!/usr/bin/env bash
# Generate scale load on the kwok cluster: fake nodes + namespaces + deployments
# (-> replicasets -> pods) + services + configmaps + secrets.
#
# Total pods = KRUST_NAMESPACES * KRUST_DEPLOYS_PER_NS * KRUST_REPLICAS  (default 10,000).
# Idempotent: re-applies the same object set. Tune via KRUST_* env vars (see lib.sh).
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require kubectl

total_pods=$(( KRUST_NAMESPACES * KRUST_DEPLOYS_PER_NS * KRUST_REPLICAS ))
echo "target: ${KRUST_NODES} nodes, ${KRUST_NAMESPACES} namespaces, ${total_pods} pods"
echo "        (${KRUST_DEPLOYS_PER_NS} deploys x ${KRUST_REPLICAS} replicas per ns)"

# ---- nodes -----------------------------------------------------------------
emit_nodes() {
  for ((n=0; n<KRUST_NODES; n++)); do
    name="$(node_name "$n")"
    cat <<EOF
---
apiVersion: v1
kind: Node
metadata:
  name: ${name}
  annotations:
    kwok.x-k8s.io/node: fake
    node.alpha.kubernetes.io/ttl: "0"
  labels:
    type: kwok
    kubernetes.io/hostname: ${name}
    kubernetes.io/role: agent
    kubernetes.io/os: linux
    kubernetes.io/arch: arm64
spec:
  taints:
    - effect: NoSchedule
      key: kwok.x-k8s.io/node
      value: fake
EOF
  done
}

# ---- per-namespace workloads ----------------------------------------------
emit_workloads() {
  for ((i=0; i<KRUST_NAMESPACES; i++)); do
    ns="$(ns_name "$i")"
    cat <<EOF
---
apiVersion: v1
kind: Namespace
metadata:
  name: ${ns}
  labels: { krust-scale: "true" }
EOF
    for ((c=0; c<KRUST_CONFIGMAPS_PER_NS; c++)); do
      cat <<EOF
---
apiVersion: v1
kind: ConfigMap
metadata: { name: cm-${c}, namespace: ${ns}, labels: { krust-scale: "true" } }
data: { key: "value-${c}", env: "scale" }
EOF
    done
    for ((s=0; s<KRUST_SECRETS_PER_NS; s++)); do
      cat <<EOF
---
apiVersion: v1
kind: Secret
metadata: { name: sec-${s}, namespace: ${ns}, labels: { krust-scale: "true" } }
type: Opaque
stringData: { username: "admin-${s}", password: "s3cr3t-${s}" }
EOF
    done
    for ((d=0; d<KRUST_DEPLOYS_PER_NS; d++)); do
      app="app-${d}"
      cat <<EOF
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: ${app}, namespace: ${ns}, labels: { app: ${app}, krust-scale: "true" } }
spec:
  replicas: ${KRUST_REPLICAS}
  selector: { matchLabels: { app: ${app} } }
  template:
    metadata: { labels: { app: ${app} } }
    spec:
      nodeSelector: { type: kwok }
      tolerations:
        - { key: kwok.x-k8s.io/node, operator: Exists, effect: NoSchedule }
      containers:
        - name: main
          image: registry.k8s.io/pause:3.9
          resources:
            requests: { cpu: 10m, memory: 16Mi }
            limits: { cpu: 50m, memory: 64Mi }
---
apiVersion: v1
kind: Service
metadata: { name: ${app}, namespace: ${ns}, labels: { app: ${app}, krust-scale: "true" } }
spec:
  selector: { app: ${app} }
  ports: [ { port: 80, targetPort: 80 } ]
EOF
    done
  done
}

echo "applying nodes ..."
emit_nodes | kc apply -f - >/dev/null

echo "applying namespaces + workloads ..."
emit_workloads | kc apply -f - >/dev/null

echo "waiting for pods to materialize (kwokctl runs a real scheduler, ~60 pods/s) ..."
prev=-1; stalls=0
for _ in $(seq 1 300); do
  running=$(kc get pods -A --no-headers 2>/dev/null | grep -c Running || true)
  echo "  running pods: ${running}/${total_pods}"
  [ "${running:-0}" -ge "$total_pods" ] && break
  # bail out if creation has clearly stalled (no progress over several ticks)
  if [ "${running:-0}" -le "$prev" ]; then stalls=$((stalls+1)); else stalls=0; fi
  [ "$stalls" -ge 10 ] && { echo "  (no progress for 10 ticks; stopping wait)"; break; }
  prev="${running:-0}"
  sleep 3
done

echo "done. nodes=$(kc get nodes --no-headers | wc -l | tr -d ' ')  pods=$(kc get pods -A --no-headers | wc -l | tr -d ' ')"
