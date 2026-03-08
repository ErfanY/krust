# krust Performance Guide

This document explains how `krust` keeps latency predictable and how to reason about performance-sensitive changes.

## Performance Priorities

In order:

1. Input responsiveness
2. Controlled API pressure
3. Bounded memory behavior
4. Visual stability (no redraw thrash)

`krust` intentionally does not optimize for maximum background data breadth by default.

## Core Performance Mechanisms

### Lazy activation by default

- clients are initialized lazily per context
- watchers are activated from the current watch plan
- inactive contexts are not fully watched unless retained via warm-context policy

This avoids `contexts x resources x namespaces` startup explosion.

### Watch lifecycle controls

- dynamic watch-plan reconciliation (`replace_watch_plan`)
- bounded exponential reconnect backoff
- terminal handling for RBAC `403` watcher failures

Result: fewer reconnect storms and less API/server noise.

### Projection and render controls

- state cache revision tracking
- cached projection reuse when request + revision unchanged
- invalidation-based render loop with FPS cap

Result: no unconditional full redraw at fixed ticks.

### Log-path controls

- bounded line/byte buffer
- bounded per-cycle drain
- per-source filtering without restarting all streams
- tail/pause behavior to reduce churn during operator inspection

Result: stable log UX under high stream volume.

## Configuration Knobs

`~/.config/krust/config.toml`

```toml
[runtime]
fps_limit = 60
delta_channel_capacity = 2048
warm_contexts = 1
warm_context_ttl_secs = 20
```

Tuning suggestions:

- many contexts: keep `warm_contexts` low (`0-2`)
- bursty clusters: raise `delta_channel_capacity` carefully
- low-power terminals: reduce `fps_limit` (for example `30`)

## Operational Recommendations

- default startup: use lazy mode (`krust`)
- only use `--all-contexts` when eager auth/client warmup is explicitly desired
- scope namespace when debugging dense clusters
- in logs, pause or source-filter when hunting specific signals

## Benchmark and Validation Hooks

`krust` includes perf-focused test hooks (some ignored by default) and should always pass full validation:

```bash
cargo fmt --all
cargo check
cargo test
cargo build --release
```

You can run ignored perf tests explicitly when profiling local changes:

```bash
cargo test -- --ignored --nocapture
```

## Change Review Checklist (Perf-Sensitive PRs)

When changing watches, rendering, projection, or logs, include:

- hot path changed
- expected complexity/cost change
- API pressure impact
- memory impact and bounds
- potential regressions and how to observe them

This keeps performance discussion concrete and reviewable.
