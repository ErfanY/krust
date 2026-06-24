#!/usr/bin/env bash
# Shared config for the krust kwok scale lab.
# Source this from the other scripts: `source "$(dirname "$0")/lib.sh"`
# All tunables are KRUST_-prefixed env vars; override any of them inline, e.g.
#   KRUST_NAMESPACES=40 KRUST_REPLICAS=250 ./load.sh
set -euo pipefail

# Cluster identity (kwokctl prefixes the context with "kwok-").
export KRUST_SCALE_CLUSTER="${KRUST_SCALE_CLUSTER:-krust-scale}"
export KRUST_SCALE_CONTEXT="${KRUST_SCALE_CONTEXT:-kwok-${KRUST_SCALE_CLUSTER}}"
export KRUST_SCALE_RUNTIME="${KRUST_SCALE_RUNTIME:-docker}"

# Workload shape. Total pods = KRUST_NAMESPACES * KRUST_DEPLOYS_PER_NS * KRUST_REPLICAS.
# Defaults => 20 * 5 * 100 = 10,000 pods.
export KRUST_NODES="${KRUST_NODES:-20}"
export KRUST_NAMESPACES="${KRUST_NAMESPACES:-20}"
export KRUST_DEPLOYS_PER_NS="${KRUST_DEPLOYS_PER_NS:-5}"
export KRUST_REPLICAS="${KRUST_REPLICAS:-100}"
export KRUST_CONFIGMAPS_PER_NS="${KRUST_CONFIGMAPS_PER_NS:-3}"
export KRUST_SECRETS_PER_NS="${KRUST_SECRETS_PER_NS:-2}"

# Number of synthetic contexts to expose to krust (multi-context / warm-context test).
# All point at the same kwok apiserver but with distinct default namespaces.
export KRUST_SCALE_CONTEXTS="${KRUST_SCALE_CONTEXTS:-20}"

# Generated multi-context kubeconfig consumed by krust via --kubeconfig.
KRUST_SCALE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export KRUST_SCALE_DIR
export KRUST_SCALE_KUBECONFIG="${KRUST_SCALE_KUBECONFIG:-${KRUST_SCALE_DIR}/kubeconfig-scale.yaml}"

ns_name()   { printf 'ns-%02d' "$1"; }   # ns-00 .. ns-NN
node_name() { printf 'kwok-node-%03d' "$1"; }

kc() { kubectl --context "$KRUST_SCALE_CONTEXT" "$@"; }

require() { command -v "$1" >/dev/null 2>&1 || { echo "missing required tool: $1" >&2; exit 1; }; }
