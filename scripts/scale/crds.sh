#!/usr/bin/env bash
# Install a sample CRD + a few custom resources on the kwok cluster, so the dynamic
# resource discovery path (Phase 4.1) has a real CRD to find and browse.
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

require kubectl

kc apply -f - <<'EOF'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: widgets.demo.krust.io
spec:
  group: demo.krust.io
  scope: Namespaced
  names:
    plural: widgets
    singular: widget
    kind: Widget
    shortNames: [wdg]
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
              properties:
                size: { type: string }
            status:
              type: object
              properties:
                phase: { type: string }
EOF

echo "waiting for CRD to register ..."
kc wait --for=condition=Established crd/widgets.demo.krust.io --timeout=30s || true

for i in 0 1 2; do
  kc apply -f - <<EOF
apiVersion: demo.krust.io/v1
kind: Widget
metadata: { name: widget-${i}, namespace: ns-00 }
spec: { size: "large" }
EOF
done

echo "created CRD widgets.demo.krust.io + 3 Widgets in ns-00"
echo "verify: krust --kubeconfig $KRUST_SCALE_KUBECONFIG --discover --discover-resource widgets"
