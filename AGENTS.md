# AGENTS.md

## Project Intent
`krs` is a latency-first, power-user Kubernetes TUI for very large environments:
- many contexts/clusters
- many resources per cluster
- frequent churn

Agent behavior must prioritize snappiness and controlled API pressure over feature breadth.

## Mandatory Change Pipeline
After every code change, run validation in this order:
1. `cargo fmt --all`
2. `cargo check`
3. `cargo test`
4. `cargo build --release`

Rules:
- If tests fail, do not proceed to release build until fixed.
- Always report all four outcomes in the update.

## Commit Autonomy And Authorization
Agents are allowed to decide logical commit boundaries while working, especially when moving from one feature/fix area to another.

Hard rule:
- Before creating a commit, stop and request explicit user authorization.
- No commit without a clear user approval in the current thread.

Commit request format:
- Summary of what will be committed
- Files touched
- Validation status (`fmt/check/test/release`)
- Proposed commit title

## Performance Thinking Model (Non-Negotiable)
When designing or changing behavior, explicitly reason in this order.

### 1) Scope activation
- Default to lazy activation.
- Do not eagerly watch all contexts or all kinds unless explicitly requested.
- Only activate watches required by the visible pane/action.

### 2) API pressure control
- Prefer watch over poll where possible.
- Keep reconnect backoff bounded.
- Avoid fan-out explosions (context x kind x namespace) unless user-initiated.
- Treat RBAC denials and unavailable APIs as expected states, not fatal paths.

### 3) Compute budget
- Avoid full recompute on every event/keypress.
- Recompute only affected projections.
- Keep filtering/sorting scoped to active view.
- Favor stable O(visible) or O(active-view) paths over O(total-store) paths.

### 4) Render budget
- Render on invalidation, not continuous redraw.
- Cap frame rate.
- Avoid rebuilding large text buffers unless source changed.
- Clamp vertical/horizontal scrolling to content bounds.

### 5) Memory bounds
- Use bounded channels/ring buffers for high-volume streams (logs/events).
- Ensure no unbounded queue growth under churn.
- Prune or expire caches when no longer active.

### 6) Logs at scale
- Stream only what user asked for (pod/container/replica scope).
- Support multi-source aggregation without duplicate stream restarts.
- Prefix source identity clearly for merged streams.
- Reconnect with backoff; avoid replay storms.

### 7) UX under load
- Keep input responsive even when data updates are heavy.
- Never block UI navigation on slow API calls.
- Show partial data quickly; degrade gracefully with clear status.

## Implementation Guardrails
- Prefer minimal diffs that preserve current fast paths.
- Avoid introducing background work that scales with total cluster size by default.
- If adding a new feature, define:
  - activation trigger
  - steady-state CPU/memory impact
  - API call/watch impact

## Verification Expectations For Perf-Sensitive Changes
For changes touching watches, filtering/sorting, rendering, or logs, include:
- what hot path changed
- why the new path is cheaper or safer
- potential regressions to watch for

If measurement is available, include it. If not, provide complexity-level reasoning.
