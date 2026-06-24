#!/usr/bin/env bash
# Tear down the kwok scale cluster and generated kubeconfig.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require kwokctl

if kwokctl get clusters 2>/dev/null | grep -qx "$KRUST_SCALE_CLUSTER"; then
  echo "deleting cluster '$KRUST_SCALE_CLUSTER' ..."
  kwokctl delete cluster --name "$KRUST_SCALE_CLUSTER"
else
  echo "cluster '$KRUST_SCALE_CLUSTER' not found"
fi

rm -f "$KRUST_SCALE_KUBECONFIG" && echo "removed $KRUST_SCALE_KUBECONFIG" || true
