# krust Architecture

This document describes the current architecture and the design constraints behind it.

## Design Intent

`krust` is optimized for operators running large fleets:

- many kubeconfig contexts
- large object counts per context
- frequent state churn

The architecture is built to keep interaction latency stable under those conditions.

## Layered Model

`krust` is structured as four layers:

1. Cluster IO
2. State cache
3. View projection
4. TUI renderer

## 1) Cluster IO (`ResourceProvider`, `ActionExecutor`)

Primary responsibilities:

- initialize per-context Kubernetes clients lazily
- manage dynamic watch plans (`replace_watch_plan`)
- stream pod logs
- execute guarded mutation actions

Notable behavior:

- watchers are not started for every context/resource at startup
- watch plans are reconciled to active UI scope
- RBAC `403` watcher failures are treated as terminal for that resource/context
- exponential watch reconnect backoff is bounded and resets after healthy runs

## 2) State Cache (`StateStore`)

The cache stores normalized entities keyed by `(context, kind, namespace, name)` and keeps supporting indexes:

- context/kind index for fast listing
- namespace index by context
- context-scoped error state
- monotonic revision counter

The cache receives `StateDelta` messages (`Upsert`, `Remove`, `Reset`, `Error`) and applies them atomically.

## 3) View Projection (`ViewProjector`)

Projection transforms state cache data into active-view rows and detail content.

Key properties:

- projection is scoped to current `ViewRequest` (`context`, `kind`, `namespace`, `filter`, sort)
- cached projection is reused while request + state revision are unchanged
- avoids whole-state recompute on every keypress when inputs are stable

## 4) TUI Renderer (`ratatui` + `crossterm`)

Render strategy:

- invalidation-driven draws
- frame cap (`fps_limit`)
- coalesced state/input events
- bounded scroll behavior for table/detail/log panes

The UI can render table, describe, decode, events, and logs panes, plus selection overlays.

## Watch Scope Reconciliation

`replace_watch_plan(context, targets)` is the key boundary between UI intent and API load.

Inputs:

- active context
- active kind/pane requirements
- namespace scope
- warm-context retention policy

Outputs:

- start only missing required watchers
- stop watchers no longer needed
- retain watchers only for active and warm contexts

This prevents default "watch everything" fan-out.

## Warm Context Strategy

To make tab switches fast without broad eager loading:

- last-active contexts are tracked
- up to `warm_contexts` inactive contexts are retained
- retention expires after `warm_context_ttl_secs`

## Logs Pipeline

Logs are streamed per selected scope using pod log streams.

Current behavior supports:

- pod-level logs (all containers or selected container)
- deployment/replicaset fan-out to replica pod streams
- pause/resume, tail mode, latest jump
- source-level visibility filtering

For stability:

- bounded event drain per UI cycle
- bounded in-memory log buffer
- reconnect backoff for retryable stream failures

## Detail Content and Editing

Describe/decode/event detail views support:

- YAML/JSON render format toggle
- syntax highlighting + search highlighting
- external editor mutation flow

Mutation semantics:

- describe edit applies raw manifest as entered
- secret decode edit expects decoded values, then re-encodes to `Secret.data`

## Error Model

Error handling is explicit and user-visible:

- watcher errors are emitted as `StateDelta::Error`
- RBAC/action failures are surfaced in status line with context
- read-only mode blocks mutation actions early

## Internal Stable Interfaces

The code is intentionally organized around interface seams:

- `ResourceProvider`
- `ActionExecutor`
- `ViewProjector`
- `StateDelta`

These boundaries make it possible to evolve behavior while preserving operational semantics.
