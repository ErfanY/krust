# k9s Readonly Parity Audit (ROADMAP 4.7)

Goal of the first stable release: **every readonly function that works in k9s must work in krust.**
Mutating operations are explicitly out of scope for v1 (krust is readonly-first), so this audit
tracks the *readonly* surface only and marks mutations as scoped-out.

Source of truth for the k9s side: the k9s README key-bindings table and the k9s commands docs
(fetched 2026-06). krust side: `src/keymap.rs`, `src/ui/commands.rs`, `src/ui/app/command_mode.rs`,
and the pane/overlay handlers.

Legend: ✅ present · 🟡 partial / different mechanism · ❌ missing · ⛔ scoped-out (mutation or n/a)

## Global / navigation

| k9s | krust | Status | Notes |
|---|---|---|---|
| `?` help | `?` | ✅ | keymap-aware help |
| `Ctrl-A` aliases | `Ctrl-A` / `:aliases` | ✅ | |
| `:q` / `:quit` / `Ctrl-C` | same | ✅ | |
| `Esc` previous view | `Esc` | 🟡 | closes pane / cancels; no full breadcrumb stack (krust has `[`/`]`/`-` history) |
| `:pod` (singular/plural/short/alias) | `:po`/`:<kind>` | ✅ | 200+ aliases incl. discovered CRDs |
| `:pod ns-x` | `:<kind>` then `:ns` | 🟡 | no inline namespace arg in the resource command |
| `:pod /fred` (inline filter) | — | ❌ | krust filters via `/` or `:<kind> -l`; no inline `/term` in command |
| `:pod app=k,env=v` (inline labels) | `:<kind> -l app=k,env=v` | ✅ | |
| `:pod @ctx1` (inline context) | tabs / `:ctx` | ❌ | no inline `@ctx` jump |
| `/filter` (regex) | `/filter` | 🟡 | substring; **regex/fuzzy deferred** (4.5) |
| `/! filter` inverse | `/!term` | ✅ | |
| `/-l label` | `/key=value` | ✅ | |
| `/-f` fuzzy | — | ❌ | deferred |
| `-` / `[` / `]` history | same | ✅ | |
| `:ctx` / `:ctx name` | same | ✅ | |
| `:ns` | `:ns` / `n` cycle | ✅ | |
| `:screendump` / `:sd` | `:dump`/`:sd`/`:screendump` | 🟡 | krust dumps current view to clipboard; not a saved-file browser |

## Diagnostic / special views

| k9s | krust | Status | Notes |
|---|---|---|---|
| `:pulses` / `:pu` dashboard | always-on Cluster Pulse panel | 🟡 | krust shows live pulse continuously; dedicated dashboard view stubbed |
| `:xray RESOURCE` relationship tree | — (stubbed) | ❌ | **notable readonly gap** — owner/child tree view |
| `:popeye` / `:pop` sanitizer | — (stubbed) | ❌ | cluster linter/diagnostics; large, separable scope |

## Per-resource readonly actions

| k9s | krust | Status | Notes |
|---|---|---|---|
| `D` describe | `d` | ✅ | raw YAML/JSON describe |
| `Y` view YAML | `v` (+`:fmt yaml/json`) | ✅ | krust uses `v`; `y` is copy |
| `L` logs | `l` | ✅ | multi-container + owner→pod fan-in |
| `P` previous logs | — | ❌ | no `--previous` toggle (crashloop debugging) |
| `x` decode secret | `x` | ✅ | decode + re-encode-on-edit |
| `Enter` drill-down to children | `Enter` = describe / ns-select | ❌ | **major gap**: deploy→pods, node→pods, svc→endpoints/pods, rs/sts/ds→pods |
| `Shift-J` jump to owner | — | ❌ | pod→RS→deploy navigation |
| `U` UsedBy (dependents) | — | ❌ | SA/PVC/Secret/ConfigMap dependents |
| `Z` view ReplicaSets (deploy) | — | ❌ | subset of drill-down |
| `C` copy name / `N` copy namespace | `y` / `:copy` | 🟡 | copies detail content, not name/ns specifically |
| `E` events | `Shift-E` | ✅ | correlated events pane (4.3) |

## Sorting & columns

| k9s | krust | Status | Notes |
|---|---|---|---|
| `Shift-N/A/P/S` sort name/age/ns/status | `s` cycle + `:sort <col>`, `r` reverse | ✅ | sort indicator added; covers all four |
| `Shift-C` / `Shift-M` sort cpu/mem (pods) | — | ❌ | no metric-column sort |
| `Shift-O` sort by selected column | `:sort` | 🟡 | explicit column via command, not cursor column |
| per-kind columns | per-kind columns | ✅ | shipped (deploy/svc/pvc/hpa/… ) |
| `Ctrl-W` wide / `Ctrl-E` header / `Ctrl-Z` faults | — | ❌ | minor view toggles |
| `Shift-Left/Right` reorder columns | — | ❌ | cosmetic |
| `Ctrl-R` refresh | n/a | ⛔ | krust is always live-watch; no manual refresh needed |

## Logs pane

| k9s | krust | Status | Notes |
|---|---|---|---|
| `W` wrap | `w` | ✅ | |
| autoscroll / tail | `s` tail toggle | ✅ | |
| pause | `p` | ✅ | |
| container/source selectors | `c` / `S` | ✅ | |
| `T` timestamps | — | ❌ | no timestamp toggle |
| `F` fullscreen | — | ❌ | minor |

## Mutations — scoped out for the readonly-first v1

`E` edit (blocked under `--readonly`), `Ctrl-D` delete / `Ctrl-K` kill (guarded), `s` shell / `a`
attach, `Shift-F`/`F` port-forward (4.6), cordon/uncordon/drain/restart/rollback/trigger,
benchmark, and `Space`/`Ctrl-Space`/`Ctrl-\` marks (only useful for bulk mutation). These are
intentionally excluded from the readonly parity bar.

## Prioritized gaps to close for v1

**High value (true readonly-parity gaps):**
1. **Enter drill-down** — deploy/rs/sts/ds → pods, node → pods, svc → endpoints/pods. krust already
   has the owner→pod mapping (used by log fan-in), so the data path exists. Most impactful gap.
2. **Previous logs** — a `--previous` toggle in the logs pane for crashlooping containers.
3. **xray** view — owner/child relationship tree (`:xray`), replacing the current stub.

**Medium:**
4. **Jump to owner** (`Shift-J`) and **UsedBy / dependents** (`U`).
5. **Log timestamps** toggle.
6. **Metric-column sort** for pods (cpu/mem).
7. Inline command conveniences: `:pod /term`, `:pod @ctx`.

**Lower / decide:**
8. **popeye** sanitizer — large, separable; decide in-or-out for v1.
9. Pulses dashboard view (likely redundant with the always-on pulse — lean toward scope-out).
10. Regex/fuzzy filter (4.5 deferral), wide/header/fault toggles, column reorder, fullscreen,
    copy-name/copy-ns, screendump-to-file browser.

**Scoped-out / not applicable:** all mutations (above), `Ctrl-R` refresh (always live).
