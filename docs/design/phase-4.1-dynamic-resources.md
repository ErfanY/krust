# Design: Phase 4.1 — Dynamic resources / CRDs

Status: **proposed** (awaiting sign-off). Related: ROADMAP.md Phase 4.1.

## Problem

krust supports a fixed set of 24 built-in kinds (`ResourceKind` enum, model.rs). Operators on real
clusters need to browse **any** resource — CRDs (the M3/Infor platform alone has many), plus core
kinds we don't enumerate (endpoints, leases, storageclasses, etc.). This is the biggest readonly
parity gap vs k9s. Today `:crd`, `:sc`, etc. report "recognized but not implemented".

## Constraint that shapes the design

The watch layer is **typed-generic**: `spawn_namespaced_watch::<Pod>`, `::<Deployment>`, … — 24
hard-coded dispatches (kube_provider.rs). Arbitrary kinds cannot be typed at compile time, so they
**must** be watched via `Api<DynamicObject>` + a runtime `ApiResource`. We already use exactly this
path for `get_object` (Phase 1.1) and proved `DynamicObject` watches parse fine (1.2 micro-bench:
~7.9µs/obj vs 5µs typed — acceptable).

## Approach: hybrid (curated built-ins + generic dynamic) — like k9s

Do **not** rip out `ResourceKind`. Keep the 24 curated kinds with their tailored columns/status
extraction (pod phase, deploy ready, node capacity, …). Add a **dynamic resource layer** that can
list/watch/describe **any** discovered GVK with generic columns. Known kinds render rich; everything
else renders generic (name / namespace / age / best-effort status) — exactly k9s's model.

### New core type

A resource is identified by a `ResourceRef` (runtime), superseding the enum *as the key* while the
enum remains an optional "curated profile":

```rust
struct GvkKey { group: String, version: String, kind: String }   // e.g. ("apps","v1","Deployment")
```

- `ResourceKey` changes `kind: ResourceKind` → a key that carries the GVK (plus a cheap interned id).
  This is the invasive part (ResourceKey is in every delta/index/cache). Mitigation below.
- A `ResourceCatalog` (per context, from discovery) maps GVK ↔ `ApiResource` (plural, namespaced,
  short names) and flags whether a curated profile exists.

### Migration that limits blast radius

`ResourceKey`/`StateStore`/projection are generic over "kind identity". To avoid editing ~290 sites
at once, introduce an interned `KindId` (u32) that both the enum and dynamic GVKs resolve to:

- `KindId` is the hash-keyed identity used in `ResourceKey`, indexes, watch keys (cheap Copy, no
  String churn in the hot path — important after we just made keys lean).
- A registry resolves `KindId → ResourceDescriptor { gvk, api_resource, curated: Option<ResourceKind>, namespaced, columns }`.
- Built-in kinds register deterministic `KindId`s at startup; discovered kinds register on demand.

This keeps the store/projection/key hot paths Copy-cheap and confines GVK strings to the registry.

## Incremental sub-phases (each shippable)

- **4.1a — discovery + dynamic list/describe (read-only, command-driven).** Add `kube::discovery`
  per context; a `:api`/`:<crd-plural>` command lists any discovered resource via `Api<DynamicObject>`
  (one-shot list, not yet watched) with generic columns; describe already works via `get_object`.
  Lowest risk, immediately closes "can I even see my CRDs". No change to the enum/key yet — dynamic
  results held in a parallel path.
- **4.1b — dynamic watches.** Replace the one-shot list with a `DynamicObject` watcher reusing
  `run_watch_loop` (generalized) + the relist generation logic; introduce `KindId` and fold the 24
  typed watches into the dynamic path (keeping curated extraction by GVK match). This is the
  ResourceKey refactor.
- **4.1c — navigation/UX.** Discovered kinds in the kind cycle / a resource picker (`:res`), short
  names from discovery, namespaced/cluster scoping from discovery.

We ship 4.1a first and reassess before the deeper 4.1b key refactor.

## Touch list (4.1b, the deep one)
`model.rs` (KindId, ResourceKey), `cluster/kube_provider.rs` (discovery, dynamic watch, drop 24
typed dispatches), `cluster/extract.rs` (extract by curated GVK match), `state/store.rs` (KindId
indexes), `view/projector.rs`, `ui/*` (kind nav, commands, columns). Curated column/status code
keyed by GVK instead of enum variant.

## Risks
1. **ResourceKey churn** — it's in every delta/index. Mitigated by interned Copy `KindId` (no perf
   regression vs today's enum, which is also Copy).
2. **Discovery cost/RBAC** — discovery is one call per context, cached; handle 403 gracefully
   (show only what's discoverable).
3. **Watch load** — arbitrary kinds still go through `replace_watch_plan` (lazy, scoped) — no change
   to the activation model; a CRD is only watched when viewed.
4. **Generic columns** — best-effort status for unknown kinds; acceptable and matches k9s.
5. **CRD scale** — clusters with hundreds of CRDs: discovery list is fine; we only watch what's open.

## Validation
- 4.1a: against the lab, `:` a core kind not in the 24 (e.g. endpoints) and a CRD → lists + describes.
- 4.1b: bench shows no regression on the 24 built-ins (KindId Copy vs enum Copy); relist still works.
- Tests: discovery parsing, KindId registry, generic column projection, curated-vs-generic dispatch.

## Open questions for sign-off
1. OK with the **hybrid** model (keep curated 24 + generic for the rest), not a full dynamic rewrite?
2. OK to **ship 4.1a first** (discovery + dynamic list/describe, command-driven) and reassess before
   the deeper `KindId`/ResourceKey refactor in 4.1b?
3. `KindId` interning approach for the key (vs carrying GVK strings in ResourceKey) — agree it's worth
   it to protect the hot path?
