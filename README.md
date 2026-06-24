# krust

[![CI](https://github.com/ErfanY/krust/actions/workflows/ci.yml/badge.svg)](https://github.com/ErfanY/krust/actions/workflows/ci.yml)
[![Release](https://github.com/ErfanY/krust/actions/workflows/release.yml/badge.svg)](https://github.com/ErfanY/krust/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Kubernetes 1.33+](https://img.shields.io/badge/Kubernetes-1.33%2B-326CE5?logo=kubernetes&logoColor=white)](https://kubernetes.io/)

`krust` is a latency-first Kubernetes terminal navigator for operators managing many clusters and high resource volume.

It focuses on fast navigation, inspect, logs, and safe actions, with high k9s command/keybinding compatibility for Kubernetes-native workflows.

## Goal and Approach

Goal:
- stay responsive for power users across many contexts and large object sets
- provide Kubernetes-native terminal workflows without plugin complexity

Approach:
- lazy scope activation instead of watch-everything startup
- bounded state/update pipelines to avoid UI jitter under churn
- keyboard-first interaction with high k9s compatibility
- explicit RBAC/auth error handling and guarded mutation paths

## Install

### Homebrew

```bash
brew tap ErfanY/krust
brew install krust
```

### Prebuilt binaries

```bash
curl -L -o krust.tar.gz \
  https://github.com/ErfanY/krust/releases/latest/download/krust-x86_64-unknown-linux-gnu.tar.gz

tar -xzf krust.tar.gz
sudo install krust-x86_64-unknown-linux-gnu/krust /usr/local/bin/krust
```

### From source

```bash
cargo install --git https://github.com/ErfanY/krust.git --locked krust
```

## Quick Start

```bash
krust
krust --context <context-name>
krust --namespace <namespace>
krust --readonly
```

## Compatibility

- OS: macOS, Linux
- Kubernetes: 1.33+
- Auth: kubeconfig + exec auth providers

## Documentation

- [Operator Guide](docs/operator-guide.md)
- [Contributor Guide](docs/contributor-guide.md)
- [Architecture](docs/architecture.md)
- [Performance Guide](docs/performance.md)
- [Contributing](CONTRIBUTING.md)

## Scope

Available now:
- Multi-context workflow with fast context switching and namespace scoping (`all` namespace supported)
- Kubernetes resource browser for curated kinds:
  pods, deployments, replicasets, statefulsets, daemonsets, services, ingresses, configmaps, secrets, jobs, cronjobs, pvc/pv, nodes, namespaces, events, serviceaccounts, roles/rolebindings/clusterroles/clusterrolebindings, networkpolicies, hpa, pdb
- Dynamic discovery: list and describe **any** other resource or CRD on the cluster (read-only) via `:api` and `:<resource>`
- Describe/detail pane with YAML/JSON toggle, syntax highlighting, vim-style search, and wrap/horizontal scroll modes
- Logs pane with stream, tail, pause, source filtering, container selection, and multi-pod fan-in for deployment/replicaset views
- Secret decode view in YAML, with edit/apply support and automatic re-encode on apply
- Guarded mutation flows (for example delete) plus read-only mode (`--readonly`)
- k9s-style command and keybinding compatibility for Kubernetes-native workflows (plugin commands excluded)
- Performance-oriented behavior for large clusters:
  lazy watch activation, bounded update pipelines, render invalidation, and RBAC-aware watch suppression

Explicitly out of scope:
- Plugin subsystem
- Non-Kubernetes plugin commands

## License

MIT. See [LICENSE](LICENSE).
