# krust Operator Guide

This guide is for cluster operators and SREs using `krust` day-to-day.

## What krust is optimized for

- low keypress-to-render latency
- high resource counts and multi-context sessions
- keyboard-first Kubernetes workflows
- predictable behavior under RBAC/API/auth failures

## Run Modes

Default mode:

```bash
krust
```

Target a context:

```bash
krust --context <context-name>
```

Target a namespace:

```bash
krust --namespace <namespace>
```

Read-only mode:

```bash
krust --readonly
```

Use custom kubeconfig:

```bash
krust --kubeconfig <path>
```

Warm all contexts at startup (heavier):

```bash
krust --all-contexts
```

## Core Navigation

- `tab` / `shift+tab`: switch context tabs
- `j` / `k` (or arrows): move selection (table) / scroll (detail, logs)
- `g` / `G`: top / bottom (in detail panes use `gg` for top)
- `Enter`: drill down where it makes sense — namespace → its resources; Deployment/ReplicaSet/
  StatefulSet/DaemonSet → their pods; Node → pods scheduled on it. Other kinds open describe.
  In a drill-down the title shows `[DRILL] <owner> → Pods`; `esc` pops back to the owner list.
- `d`: describe selected resource (toggles back to table)
- `v`: view YAML; `t`: back to table; `E`: events pane (this resource's events); `l`: logs
- `n`: cycle namespace; `s`: cycle sort column; `r`: reverse sort order
- `H`: show/hide Helm release secrets (Secrets view; hidden by default)

The active sort column is marked in the table header with a direction arrow (`Name ↑` ascending,
`Status ↓` descending), and the top status bar shows `[SORT] <col><arrow>`. You can also set sort
explicitly with `:sort <name|namespace|status|age> [asc|desc]`.
- `ctrl+d`: delete (guarded, table only); `ctrl+k`: kill (delete)
- `?`: toggle help (table) / search (detail panes)
- `esc`: clear filter (table) / close view (detail) / cancel pending action

All of the above are remappable via `keymap.toml`; the on-screen help line reflects your active bindings.

## Events

Press `E` on any resource (pod, deployment, node, …) to see **its** events — the ones whose
`involvedObject` is that resource — newest first (LAST / TYPE / REASON / COUNT / SOURCE / MESSAGE),
like the Events section of `kubectl describe`. Selecting an Event resource itself shows its manifest.

## Resource Columns

Every table starts with the universal columns **Namespace · Name · Status · Age**. Beyond those,
each kind shows columns tailored to it (like `kubectl get`), e.g.:

- **Deployments** — `UP-TO-DATE`, `AVAILABLE`
- **ReplicaSets** — `DESIRED`, `CURRENT`, `READY`
- **StatefulSets** — `READY`
- **DaemonSets** — `DESIRED`, `READY`, `AVAIL`
- **Services** — `TYPE`, `CLUSTER-IP`, `PORTS`
- **Ingresses** — `CLASS`, `HOSTS`, `ADDRESS`
- **Jobs** — `COMPLETIONS`, `DURATION`
- **CronJobs** — `SCHEDULE`, `SUSPEND`, `ACTIVE`
- **ConfigMaps** — `DATA`; **Secrets** — `TYPE`, `DATA`
- **ServiceAccounts** — `SECRETS`, `PULL-SECRETS`
- **RoleBindings / ClusterRoleBindings** — `ROLE` (e.g. `ClusterRole/view`), `SUBJECTS` (e.g. `sa:build,u:alice`)
- **Nodes** — `ROLES`, `VERSION`
- **PVCs** — `VOLUME`, `CAPACITY`, `ACCESS`, `STORAGECLASS`; **PVs** add `RECLAIM`, `CLAIM`
- **HPAs** — `REFERENCE`, `MINPODS`, `MAXPODS`, `REPLICAS`
- **PodDisruptionBudgets** — `MIN-AVAIL`, `MAX-UNAVAIL`, `ALLOWED`

Kinds without distinctive columns show just the four universal ones. Pods are special — see Metrics.
Filtering and `:dump`/copy operate over all visible columns.

The **Pods table** adds, beyond the universal four:
`RST` (container restarts) · `CPU %CPU/R %CPU/L` · `MEM %MEM/R %MEM/L` · `IP` · `NODE`. `RST` turns
yellow at ≥3 restarts, red at ≥10 (crashloop signal); the metric columns are described below.

## Metrics

If the cluster has a metrics-server (`metrics.k8s.io`), krust shows live usage and right-sizing.
The **Pods table** splits this into six right-aligned columns — `CPU %CPU/R %CPU/L` then
`MEM %MEM/R %MEM/L`:
- **CPU** / **MEM** — actual usage (e.g. `1.50c`, `256Mi`).
- **%CPU/R** / **%MEM/R** — usage as a percentage of the CPU/memory **request** (the right-sizing
  signal: low = over-provisioned, ≥100 = under-requested). Turns yellow at ≥100%.
- **%CPU/L** / **%MEM/L** — usage as a percentage of the CPU/memory **limit** (the risk signal).
  Turns red at ≥90% (throttle/OOM risk).

So a pod showing CPU `1.50c`, %CPU/R `150%`, %CPU/L `75%` is using 1.5 cores, 150% of its CPU
request, and 75% of its CPU limit. Missing pieces (no request/limit, or no metrics) render as `-`.
The **Cluster Pulse** panel shows a `[USE]` row: cluster cpu/mem used vs allocatable + util%.

Without a metrics-server these degrade gracefully (columns show `-`, the table title notes
`metrics-server n/a`, and the pulse falls back to request/limit-based numbers).

## Logs and Runtime Inspect

- `l`: open logs pane
- `s`: tail toggle
- `p`: pause/resume stream
- `P`: toggle previous container instance (`kubectl logs -p`) — a one-shot fetch for crashloops
- `S`: source selector
- `c`: container selector

The logs status line shows `instance:current` or `instance:previous`. Previous-instance logs don't
follow or auto-reconnect (the container is gone); press `P` again to return to the live stream.

Pod behavior:
- multi-container pods can stream from all containers

Controller behavior:
- deployment/replicaset selections can stream from all selected replica pods

## Search and Command UX

- `/`: filter in list/overlay panes, search in detail/log panes

Table filter syntax (in the `/` prompt or via `:<kind> -l ...`):
- plain text — case-insensitive substring over name/namespace/status/summary
- `!term` — inverse (exclude matches)
- `key=value` / `key==value` / `key!=value` — label selector; comma-separate requirements (`app=api,tier=backend`), all must hold
- `?`: detail/search shortcut
- `n` / `N`: next/previous match in detail/log panes
- `:`: command mode
- `tab` in command mode: autocomplete
- `ctrl+w` in command/filter mode: delete previous word

Common command mode entries:
- `:ctx` contexts
- `:ns` namespaces
- `:po`, `:deploy`, `:svc`, `:ing`, `:cm`, `:sec`, etc.
- `:api [filter]` list all API resources/CRDs discovered on the context
- `:triage [namespace|all]` (alias `:issues`) board of pods needing attention, worst first
- `:xray [namespace|all]` open the namespace ownership graph (workloads → pods → containers)
- `:<resource>` browse any discovered resource/CRD (e.g. `:endpoints`, `:widgets`); `:<resource> <name>` describes one
- `:fmt yaml|json`
- `:edit [yaml|json]`
- `:sort <name|namespace|status|age> [asc|desc]` set the table sort column/direction
- `:helm [show|hide]` toggle (or set) visibility of Helm release secrets in the Secrets list

## Triage (what needs attention)

`:triage [namespace|all]` (alias `:issues`; defaults to the current namespace, `all` = whole cluster)
opens a live board of only the pods that need an operator's eye, worst severity first:

- **Critical** (`[XX]`): CrashLoopBackOff, OOMKilled, ImagePullBackOff, Error, Evicted, Failed, …
- **Warning** (`[!!]`): Pending/Unschedulable, NotReady (Running but failing its readiness probe —
  e.g. a slow JVM start), and restart hotspots (≥3 container restarts)

Columns: severity · namespace · pod · reason · restarts · ready · age. Healthy pods
(Running + Ready + few restarts) are omitted entirely, so the board is empty when nothing is wrong.
The title summarizes counts (`N critical · M warning`). It rebuilds from live cluster state each
frame; navigate with `j`/`k` (+ `g`/`G`), `esc` closes. Large incidents are capped at 500 rows
(worst-first) with the count shown.

## XRay (relationship graph)

`:xray [namespace]` opens a live ownership graph of a namespace — a single forest rooted at the
namespace, grouping every workload down to its pods and containers:

```
ns/default
├─ deploy/api → rs/api-7d9 → po/… → ctr/…
├─ sts/db → po/db-0 → ctr/…
├─ ds/agent → po/…
├─ cj/backup → job/… → po/…
├─ job/migrate → po/…
└─ po/debug                 (standalone — no controller)
```

`:xray` defaults to the current namespace; `:xray all` covers the whole cluster (grouped by
namespace). It renders as a real tree with box-drawing connectors (`├──`/`└──`/`│`) and `▾`/`▸`
expand markers; each node carries a severity-colored status. Navigate with `j`/`k` (or up/down),
`g`/`G` for top/bottom; `→`/`←` expand/collapse the node under the cursor (`Enter` toggles), `esc`
to close.

The graph rebuilds from live cluster state each frame and opening it watches the workload kinds it
draws, so it populates within a moment and tracks the cluster as it changes. Large graphs are
truncated with `… N more` nodes (collapse subtrees to keep it manageable).

## Detail Pane Behavior

- `w`: wrap toggle
- left/right: horizontal scroll when wrap is off
- `gg` / `G`: top/bottom
- `ctrl+u` / `ctrl+d`: half-page up/down
- `y`: copy visible/current detail content

Detail supports:
- syntax highlighting for YAML/JSON
- vim-style in-pane search navigation
- current-match highlighting separate from other matches

## Secrets

- `x`: toggle decoded secret view when on a Secret
- decoded view shows decoded values in YAML
- edit from decoded view re-encodes values to base64 on apply
- edit from raw describe expects base64 input

Helm release secrets (`type: helm.sh/release.v1`, named `sh.helm.release.v1.<release>.v<n>`) are
**hidden by default** to cut clutter. Press `H` (or run `:helm`) to toggle them; the Secrets table
title shows whether they're currently hidden or shown. `:helm show` / `:helm hide` set the state
explicitly. The toggle is per-context-tab.

## Configuration

Paths:
- `~/.config/krust/config.toml`
- `~/.config/krust/keymap.toml`

Example:

```toml
[runtime]
fps_limit = 60
delta_channel_capacity = 2048
warm_contexts = 1
warm_context_ttl_secs = 20

[ui]
theme = "default"
show_help = true
```

Defaults for initial context/namespace follow kubeconfig unless overridden by CLI flags.

## Safety Model

- `--readonly` blocks mutating actions
- delete/action flows are confirmation guarded
- RBAC/auth/network errors are surfaced in UI status messaging

## Troubleshooting

403 warnings:
- expected when RBAC does not allow a resource scope
- `krust` suppresses retry storms for forbidden watches

Slow startup:
- avoid `--all-contexts` unless needed
- reduce `warm_contexts`

Clipboard issues:
- tries `pbcopy`/`wl-copy`/`xclip`/`xsel`, then OSC52 fallback

Auth mismatch vs shell:
- run from shell session where `kubectl` is already working
- ensure exec auth dependencies are available in PATH

## Related Docs

- [Architecture](architecture.md)
- [Performance Guide](performance.md)
- [Contributor Guide](contributor-guide.md)
