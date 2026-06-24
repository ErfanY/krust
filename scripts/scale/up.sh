#!/usr/bin/env bash
# Create the kwok scale cluster (idempotent).
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require kwokctl
require kubectl

if kwokctl get clusters 2>/dev/null | grep -qx "$KRUST_SCALE_CLUSTER"; then
  echo "cluster '$KRUST_SCALE_CLUSTER' already exists"
else
  echo "creating cluster '$KRUST_SCALE_CLUSTER' (runtime: $KRUST_SCALE_RUNTIME) ..."
  kwokctl create cluster --name "$KRUST_SCALE_CLUSTER" --runtime "$KRUST_SCALE_RUNTIME"
fi

echo "apiserver health: $(kubectl --context "$KRUST_SCALE_CONTEXT" get --raw=/healthz 2>&1)"
echo "context: $KRUST_SCALE_CONTEXT"
