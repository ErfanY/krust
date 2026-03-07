# krs

High-performance Kubernetes terminal navigator in Rust.

Built against `k8s-openapi` `v1_33` (targeting Kubernetes `1.33+` clusters).

## Current Status

`krs` now includes a working foundation for the plan:

- Layered architecture: `cluster IO` -> `state cache` -> `view projector` -> `TUI renderer`
- Multi-context tabs in one session
- Lazy watch activation: starts with active context + active resource, then expands on navigation
- Event-driven state deltas over bounded channels
- Invalidation-based rendering with FPS cap
- Kubernetes watch streams for a broad set of core resource kinds
- k9s-style keyboard navigation, filter/sort, table + describe/events/logs panes
- Safe mutation workflow with confirmation (`delete` implemented for Pods)
- `config.toml` and `keymap.toml` loading

## CLI

```bash
krs
krs --context dev-cluster
krs --namespace payments
krs --readonly
krs --kubeconfig ~/.kube/config
krs --all-contexts   # optional eager auth/client warmup across contexts
```

## Config

`~/.config/krs/config.toml`

```toml
[runtime]
fps_limit = 60
delta_channel_capacity = 2048
namespace = "default"
default_context = "dev-cluster"
watch_buffer_size = 2048

[ui]
theme = "default"
show_help = true
```

## Keymap

`~/.config/krs/keymap.toml`

```toml
quit = "ctrl+c"
next_context = "tab"
prev_context = "shift+tab"
next_kind = "alt+right"
prev_kind = "alt+left"
move_down = "j"
move_up = "k"
goto_top = "g"
goto_bottom = "shift+g"
filter_mode = "/"
cycle_sort = "s"
toggle_desc = "r"
cycle_namespace = "n"
toggle_help = "?"
to_table = "t"
toggle_describe = "d"
to_events = "shift+e"
to_logs = "l"
delete = "ctrl+d"
confirm = "y"
cancel = "esc"
```

## Default Navigation

- `tab` / `shift+tab`: switch context tabs
- `j` / `k` (or arrows): move selection, or scroll when viewing boxed text panes
- `/`: open command field in filter mode (type pattern and press `Enter`)
- `:`: open command field in command mode
- `[` / `]`: command history back/forward
- `-`: rerun previous command
- `ctrl+a`: open resource alias list
- `d`: toggle describe pane
- `v`: k9s-style view (opens detail view)
- `ctrl+d`: delete selected resource (with confirmation)
- `ctrl+k`: k9s-style kill shortcut (mapped to guarded delete flow)
- `esc`: close command/overlay and return focus
- mouse wheel: scrolls inside overlay/detail/table box (terminal scrollback is not used)

## Command Field

Examples in the command field:

- `:ctx` show contexts list in a selectable box (`j/k`, arrows, mouse wheel, `Enter` to switch)
- `:ctx <context-name>` switch active context
- `:ns [namespace|all]` with arg sets namespace scope, without arg opens Namespaces
- `:all` (or `:0`) switch to all namespaces
- `:kind <po|deploy|svc|...>` switch resource kind
- `:po`, `:svc`, `:deploy`, `:ns`, ... k9s-style direct resource switches
- `:resources` show shorthand resource aliases
- `:clear` clear current filter
- `:po kube-system`, `:po /api`, `:po @my-context`, `:deploy -l app=my-api` are supported
- `:po --context <context> --namespace <ns>` and `:po -A` are supported
- `/pod-xxx` apply table filter

## Watched Resource Kinds

Pods, Deployments, ReplicaSets, StatefulSets, DaemonSets, Services, Ingresses, ConfigMaps,
Secrets, Jobs, CronJobs, PVCs, PVs, Nodes, Namespaces, Events, ServiceAccounts, Roles,
RoleBindings, ClusterRoles, ClusterRoleBindings, NetworkPolicies, HPAs, PDBs.

## Architecture Notes

- `ResourceProvider` and `ActionExecutor` are stable internal interfaces.
- `StateDelta` is the boundary from cluster runtime to UI state.
- `ViewProjector` isolates table/detail projections from storage and rendering.
- Watch loops reconnect with exponential backoff and emit contextual error deltas.
- Context list is loaded from kubeconfig, but clients/watches are activated lazily on demand.
- `--all-contexts` enables eager client warmup across contexts.
- `403 Forbidden` watchers are disabled per resource/context (no infinite retry flood).

## Next Milestones

1. Add live pod logs streaming and resource-correlated event pane.
2. Expand mutation actions (scale/restart/apply/edit/port-forward/exec) with RBAC-aware UX.
3. Add integration tests against disposable clusters and load/perf benchmarks.
4. Harden CRD discovery and generic resource browsing.
