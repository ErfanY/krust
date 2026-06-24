# Design: Phase 1.1 — Lean entity model

Status: **implemented** (2026-06-24). Owner: stable-release refactor. Related: ROADMAP.md Phase 1.1.
Result: RSS 482MB→85.5MB (5.6×), pulse 7.62ms→0.36ms (21×) on the kwok 10k-pod lab; 71 tests pass.
Decisions taken: `get_object` on the existing `ResourceProvider` trait; single-slot detail cache;
`to_value`-then-extract retained for 1.1 (per-event CPU deferred to 1.2).

## Problem

Every cached entity retains the **entire object as `serde_json::Value`**:

- `ResourceEntity.raw` (`src/model.rs:167`) holds the full serialized object.
- `to_entity` (`src/cluster/kube_provider.rs:793`) does `serde_json::to_value(obj)` per watch event and stores it.

Baseline (kwok, 10k pods, 1 context, release): **RSS 482 MB (~48 KB/pod)**. This is the largest
single cost and also inflates everything downstream (projection clones, pulse scans every pod's
JSON). It does not survive 20 contexts × 10k pods.

## Goal

Stop retaining full objects for list views. Keep a small, fixed-size per-row struct with the few
fields the hot paths actually need; fetch the full object **on demand** (one GET) only when the
user opens a detail/describe/decode/edit view.

Target after 1.1: RSS for 10k pods / 1 ctx from **482 MB → well under ~80 MB** (estimate ~30–60 MB),
and the pulse aggregate from ~7.6 ms → sub-millisecond (sum pre-extracted numbers, no JSON walk).

## What actually reads `raw` today (so we know what must be preserved)

| Consumer | File | Needs |
|---|---|---|
| Describe / Events render | `detail_pane.rs:135` (`detail_text`) | full object (YAML/JSON) |
| Secret decode render | `detail_pane.rs:154` | full Secret `data` |
| Edit (mutation, readonly-gated) | `detail_pane.rs:225` | full object as edit seed |
| Header "SEL-RES" | `render_loop.rs:77` | selected pod cpu/mem req+lim |
| Pulse pod totals | `app.rs:554` (`pod_resources_from_raw`) | **all pods** cpu/mem req+lim |
| Pulse node capacity | `app.rs:588` (`node_capacity_from_raw`) | **all nodes** ready/unsched/alloc |
| Log container names | `logs.rs:4` | selected pod container names |
| Log owner mapping | `app.rs:2124` (`owner_reference_matches`) | **all** RS/Pod ownerReferences |
| Projection filter/sort | `view/projector.rs` | name/ns/status/summary (NOT raw) ✓ |

Key insight: the **per-row aggregate** consumers (pulse over all pods/nodes, deploy→rs→pod owner
mapping) need their fields for *every* entity — so those fields must be **extracted at ingest**
and stored. The **single-object** consumers (describe/decode/edit/container-names-of-selected) can
be served by an **on-demand GET**.

## Proposed shape

Replace `raw` with a compact, mostly-empty `Extracted` payload:

```rust
// model.rs
pub struct ResourceEntity {
    pub key: ResourceKey,
    pub status: String,
    pub age: Option<DateTime<Utc>>,
    pub labels: Vec<(String, String)>,
    pub summary: String,
    pub extracted: Extracted,          // <-- replaces `raw`
}

#[derive(Clone, Debug, Default)]
pub struct Extracted {
    pub node_name: Option<String>,             // pods
    pub containers: Vec<String>,               // pod container names
    pub owners: Vec<OwnerRef>,                 // ownerReferences (pods->RS, RS->Deploy)
    pub pod_resources: Option<PodResources>,   // pods only
    pub node_capacity: Option<NodeCapacity>,   // nodes only
}
pub struct OwnerRef { pub kind: String, pub name: String }
pub struct PodResources { pub cpu_req_m: u64, pub cpu_lim_m: u64, pub mem_req_b: u64, pub mem_lim_b: u64 }
pub struct NodeCapacity { pub ready: bool, pub unschedulable: bool, pub cpu_alloc_m: u64, pub mem_alloc_b: u64, pub pod_alloc: u64 }
```

Sizing: pod payload ≈ a handful of strings + 4 u64s ≈ ~200–400 B vs ~48 KB. 10k pods ≈ ~3–4 MB.

## On-demand full object for detail

Add to the provider trait (`cluster/mod.rs`):

```rust
async fn get_object(&self, key: &ResourceKey) -> anyhow::Result<serde_json::Value>;
```

Implemented in `KubeResourceProvider` via the same `DynamicObject` + `ApiResource` path already used
by `replace_resource` (`kube_provider.rs:378`), but a `GET`.

App side:
- New field `detail_cache: Option<DetailObject { key, value }>`.
- The describe/decode/events/edit **entry points are already `async`** (`handle_enter_key` is sync but
  called from async `handle_key`; `toggle_describe`, `toggle_secret_decode`, `edit_current_view`,
  `ToEvents`). On entry, `await get_object(key)`, store in `detail_cache`. RBAC 403 → show error text
  in the pane (consistent with existing error model).
- `detail_text` (sync, called from `draw`) reads `detail_cache` for the current key; if absent or key
  mismatched → render `"Loading <kind>/<name> …"`. In Describe, selection is stable while scrolling,
  so one fetch per open is enough.
- Brief reuse: keep the last object cached so format toggle (yaml/json) and edit reuse it without refetch.

## Extraction

Move the existing extractors (`pod_resources_from_raw`, `node_capacity_from_raw`,
`owner_reference_matches`, container-name read, `extract_status`, `extract_summary`) into a single
ingest path used by `to_entity`. For 1.1 they keep working off the transient `serde_json::Value`
(we still `to_value` once per event for extraction, then **drop** it). This fixes *retention* now.

> Residual (explicitly deferred to **Phase 1.2**): the per-event `to_value` CPU/alloc cost remains.
> 1.2 (metadata/typed-field watches) will extract directly from typed objects and eliminate it.

## Touch list

- `model.rs` — new `Extracted`/`OwnerRef`/`PodResources`/`NodeCapacity`; drop `raw`.
- `cluster/kube_provider.rs` — `to_entity` extracts instead of storing raw; new `get_object`.
- `cluster/mod.rs` — trait method `get_object`.
- `ui/app.rs` — pulse (`pod_resource_totals`, `node_capacity_totals`, `pod_phase_counts`) read
  `extracted`; header SEL-RES reads `extracted`; add `detail_cache` + fetch on pane entry.
- `ui/app/logs.rs` — container names + owner mapping read `extracted`.
- `ui/app/detail_pane.rs` — `detail_text`/`edit_current_view` read `detail_cache` not `entity.raw`.
- `ui/app/bench.rs` — pulse aggregate sums `extracted.pod_resources` (re-baseline after).
- Tests/mocks — `store.rs`, `projector.rs`, `app.rs` `mk_entity` build `extracted` not `raw`;
  `NoopProvider` implements `get_object`. (~6 test helpers.)

## Validation / done criteria

- `fmt / check / test / build` all green; report all four.
- Re-run `--bench`: record new RSS + pulse/projection/render. Done when **RSS drops ≥5×** and pulse
  aggregate is sub-ms; no functional regression in describe/decode/events/edit/logs against the lab.
- Manual: open describe/decode on a few pods/secrets; verify deploy→pod log fan-in still resolves.

## Risks & mitigations

1. **Describe latency** — one GET on open. Mitigate: "Loading…" + cache; sub-100 ms for one object.
2. **403 on GET** — surface in pane via existing error path; list view already works without it.
3. **Stale-vs-fresh** — describe now shows a fresh GET (more correct than last watch frame).
4. **Secrets** — secret `data` no longer retained for unopened secrets (security + memory win).
5. **Behavioral parity** — Events pane still renders the Event object itself (4.3 correlation is separate).

## Open questions for sign-off

1. OK to add `get_object` to the `ResourceProvider` trait (vs a separate `DetailFetcher` trait)?
2. Detail cache: single-slot (last opened) vs small LRU (say 8)? I lean single-slot for simplicity.
3. Keep `to_value`-then-extract for 1.1 (fix memory now), push typed-field extraction to 1.2? (recommended)
