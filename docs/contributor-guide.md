# krust Contributor Guide

This guide is for contributors and maintainers.

## Engineering Principles

All changes should preserve:

- low interactive latency
- bounded resource usage (CPU/memory/channels)
- controlled Kubernetes API pressure
- predictable keyboard UX
- explicit behavior under RBAC/auth/disconnect failures

If there is a tradeoff, prefer the simpler and faster path.

## Local Setup

Requirements:
- Rust stable
- macOS or Linux
- working kubeconfig context (for manual runtime checks)

Build:

```bash
cargo build --release
```

Run:

```bash
cargo run -- --context <context-name>
```

## Required Validation Before PR

```bash
make validate   # fmt-check -> check -> test -> release build (identical to CI)
```

`make validate` is exactly what GitHub Actions runs (`make ci`), so a green local run matches CI.
`make` (no args) lists all targets. Individual steps are also available: `make fmt`, `make check`,
`make test`, `make build-release`, `make clippy`.

## Code Review Focus

Review for:
- behavior regressions in keybindings/commands
- API watch/list pressure changes
- render-path complexity increases
- unbounded buffers/retries/backlogs
- RBAC failure behavior and retry handling

## Performance-Sensitive Areas

Treat changes in these areas as high-risk and document impact in PR description:

- watch lifecycle and activation scope
- state store indexing and delta fanout
- projection/filter/sort paths
- TUI render invalidation
- logs buffering and stream fan-in

Reference baseline:
- [Performance Guide](performance.md)
- [Architecture](architecture.md)

## Testing Guidance

When behavior changes, add/update tests for:

- parser/format helper logic
- projection and sorting behavior
- keybinding and pane semantics
- search/navigation behavior in detail/log panes

Use ignored tests for heavy perf benchmarks.

## Release Pipeline (Maintainers)

The `Krust / Release` workflow is the release path on `main`.

It performs:
- validation (`fmt`, `check`, `test`, `build`)
- semver bump inference
- manifest/lock update commit
- tag creation
- artifact packaging and GitHub Release publish
- release asset verification
- Homebrew tap formula sync

Release assets expected on each release:
- `krust-x86_64-unknown-linux-gnu.tar.gz`
- `krust-x86_64-apple-darwin.tar.gz`
- `krust-aarch64-apple-darwin.tar.gz`
- `SHA256SUMS`
- `krust.rb`

Homebrew tap sync requirement:
- secret `HOMEBREW_TAP_GITHUB_TOKEN` in `krust` repo Actions secrets
- token needs write access to `ErfanY/homebrew-krust`

Without this secret, release currently fails by design.

## Pull Request Checklist

- [ ] Validation commands pass locally
- [ ] user-facing docs updated (`README` or `docs/*`) when behavior changes
- [ ] tests updated for changed behavior
- [ ] performance impact called out for hot-path changes
- [ ] no secrets/credentials added

## Security

Never commit:
- kubeconfig files
- tokens/certs
- production identifiers that should stay private

## Related Docs

- [Contributing entrypoint](../CONTRIBUTING.md)
- [Operator Guide](operator-guide.md)
