# Contributing to krust

Thanks for contributing to `krust`.

This project is built for operators running large Kubernetes estates, so performance and operational clarity are non-negotiable.

## Principles

When proposing changes, optimize for these outcomes:

- low input-to-render latency under churn
- controlled Kubernetes API pressure
- bounded memory growth
- predictable, keyboard-first UX
- clear behavior under RBAC/auth/network failures

If a feature conflicts with these principles, prefer the simpler and faster path.

## Development Setup

### Prerequisites

- Rust stable toolchain
- kubeconfig with at least one working context
- macOS or Linux environment

### Clone and build

```bash
git clone git@github.com:ErfanY/krust.git
cd krust
cargo build --release
```

Run locally:

```bash
cargo run -- --context <context-name>
```

## Required Validation Pipeline

Run this sequence before opening or updating a PR:

```bash
cargo fmt --all
cargo check
cargo test
cargo build --release
```

The CI pipeline enforces the same flow.

## Code and Design Expectations

## Performance-sensitive changes

If your change touches watchers, projection, rendering, or logs, include in your PR description:

- what hot path changed
- expected complexity/cost impact
- API watch/list pressure impact
- memory impact and bounds
- failure-mode behavior (RBAC/auth/disconnect)

Use [`docs/performance.md`](docs/performance.md) as the baseline.

## API/watch behavior

- avoid eager all-context/all-kind watchers by default
- prefer active-scope/lazy activation
- keep reconnect logic bounded
- treat RBAC denials as expected operational state

## UX behavior

- preserve existing keybindings unless explicitly changing compatibility behavior
- avoid introducing mode ambiguities
- keep detail/log scrolling and search behavior predictable

## Testing Guidance

Add or update tests when changing behavior:

- unit tests for parsing, formatting, and helper logic
- state/projection tests when list or sorting behavior changes
- UI behavior tests for keybinding and pane semantics

Performance benchmark tests can be added as ignored tests.

## Pull Request Checklist

Before requesting review, verify:

- [ ] `cargo fmt --all`
- [ ] `cargo check`
- [ ] `cargo test`
- [ ] `cargo build --release`
- [ ] docs updated (`README`/`docs/*`) for user-visible changes
- [ ] performance notes included for hot-path changes

## Commit Style

Use imperative commit titles and keep scope tight.

Good examples:

- `ui: clamp detail scroll bounds for wrapped views`
- `cluster: disable forbidden watchers per context/kind`
- `docs: add performance tuning guide`

## Security and Sensitive Data

- never commit kubeconfigs, tokens, certificates, or customer data
- redact cluster/account identifiers from logs/screenshots in PRs where possible
- for sensitive reports, contact maintainers privately

## Need help?

Open an issue with:

- expected behavior
- current behavior
- reproducible steps
- environment (`krust` version, OS, Kubernetes version)
