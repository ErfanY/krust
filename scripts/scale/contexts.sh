#!/usr/bin/env bash
# Generate a self-contained kubeconfig with KRUST_SCALE_CONTEXTS contexts, all pointing
# at the kwok apiserver but with distinct default namespaces. This exercises krust's
# multi-context tab handling, per-context client init, and warm-context retention without
# needing many real clusters.
#
# Point krust at it:  cargo run -- --kubeconfig scripts/scale/kubeconfig-scale.yaml
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require kubectl
require yq

base="$KRUST_SCALE_KUBECONFIG"
kubectl config view --raw --minify --context "$KRUST_SCALE_CONTEXT" > "$base"

cluster="$(yq '.clusters[0].name' "$base")"
user="$(yq '.users[0].name' "$base")"
[ -n "$cluster" ] && [ -n "$user" ] || { echo "could not read cluster/user from kwok config" >&2; exit 1; }

yq -i '.contexts = [] | .current-context = ""' "$base"
for ((i=0; i<KRUST_SCALE_CONTEXTS; i++)); do
  ns="$(ns_name "$(( i % KRUST_NAMESPACES ))")"
  name="$(printf 'scale-%02d' "$i")"
  cl="$cluster" us="$user" nm="$name" ns="$ns" \
    yq -i '.contexts += [{"name": strenv(nm), "context": {"cluster": strenv(cl), "user": strenv(us), "namespace": strenv(ns)}}]' "$base"
done
yq -i '.current-context = "scale-00"' "$base"

echo "wrote $base with $(yq '.contexts | length' "$base") contexts (cluster=$cluster)"
echo "run: cargo run -- --kubeconfig $base"
