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
- `Enter`: select namespace in namespace view; otherwise open describe
- `d`: describe selected resource (toggles back to table)
- `v`: view YAML; `t`: back to table; `E`: events pane; `l`: logs
- `n`: cycle namespace; `s`: cycle sort column; `r`: reverse sort order
- `ctrl+d`: delete (guarded, table only); `ctrl+k`: kill (delete)
- `?`: toggle help (table) / search (detail panes)
- `esc`: clear filter (table) / close view (detail) / cancel pending action

All of the above are remappable via `keymap.toml`; the on-screen help line reflects your active bindings.

## Metrics

If the cluster has a metrics-server (`metrics.k8s.io`), krust shows live usage:
- the **Pods table** gains **CPU** and **MEM** columns (actual usage per pod, summed across containers)
- the **Cluster Pulse** panel shows a `[USE]` row: cluster cpu/mem used vs allocatable + util%

Without a metrics-server these degrade gracefully (columns show `-`, the table title notes `metrics-server n/a`, and the pulse falls back to request/limit-based numbers).

## Logs and Runtime Inspect

- `l`: open logs pane
- `s`: tail toggle
- `p`: pause/resume stream
- `S`: source selector
- `c`: container selector

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
- `:<resource>` browse any discovered resource/CRD (e.g. `:endpoints`, `:widgets`); `:<resource> <name>` describes one
- `:fmt yaml|json`
- `:edit [yaml|json]`

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
