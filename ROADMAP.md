# krust Stable-Release Roadmap (internal working tracker)

> Working doc for the principal-architect refactor toward the **first stable release**.
> Target: **20+ clusters, ~10k pods/cluster, millions of requests. Readonly-only.
> Every readonly function that works in k9s must work here. Never sacrifice perf/stability.**
> Current baseline: v0.2.0. Reload this file at the start of each work session.

Sequencing principle: **measure first**, then fix in dependency order — memory/data model
(the root cost) unblocks compute/render, which precedes stability hardening, then readonly
feature parity, then release hardening. Within a phase, items are ordered by impact at scale.

Status legend: `[ ]` todo · `[~]` in progress · `[x]` done · `[-]` dropped/out-of-scope

---

## Environment prerequisites (one-time)
- [x] `rustup` installed (Homebrew keg-only) — toolchain **stable 1.96.0** via `rustup default stable`.
  cargo/rustc are keg shims at `/opt/homebrew/opt/rustup/bin` (no `~/.cargo/bin`). PATH:
  `export PATH="/opt/homebrew/opt/rustup/bin:$PATH"` (added to ~/.zshrc by user).
- [x] `cargo fmt --all --check` clean · `cargo check --locked` ok (~55s cold) · `cargo test --locked` 69 passed / 3 ignored. release build (`cargo build --release --locked`) still to run.
- [x] `kwok`/`kwokctl` installed (v0.8.0) — fake-cluster scale lab
- [x] `samply` (0.13.1) + `hyperfine` (1.20.0) installed — profiling + timing
- already present: kubectl, k9s, docker, jq, yq

---

## Phase 0 — Measurement & scale harness (precondition for all optimization)
Goal: nothing gets "optimized" without a before/after number.

- [x] **0.1 Toolchain green** — fmt clean, check ~55s, 69 tests pass. (release build TBD)
- [x] **0.2 kwok scale lab** — `scripts/scale/` (up/load/contexts/churn/down + README, KRUST_-prefixed env):
  - kwok cluster via docker runtime (apiserver v1.36.1); 20 fake nodes (pods:1M each).
  - load.sh applies 20 ns × 5 deploy × 100 replicas = **10,000 pods** (+ rs/svc/cm/secrets), verified running.
  - contexts.sh writes `kubeconfig-scale.yaml` with 20 contexts (distinct default ns), gitignored (kwok certs).
  - churn.sh does rolling restarts at a configurable rate. Pod fill ~60/s (real scheduler), ~3min to 10k.
- [x] **0.3 Baseline metrics captured** — headless `--bench` mode (hidden CLI flag; harness in
  `src/ui/app/bench.rs`) boots the real data plane against the lab, syncs, and times the
  project/render/pulse hot paths + RSS. Run:
  `./target/release/krust --kubeconfig scripts/scale/kubeconfig-scale.yaml --bench`
- [ ] **0.4 Profiling workflow** — `samply record` flow against `--bench` to attribute CPU/allocs.
  (deferred to per-path optimization; baselines already localize the 3 hot paths)

### Baselines — 1 context, all-namespace Pods, 10,000 pods (kwok, release build, 2026-06-24)
```
store entities:            10,024 (10k pods + 24 ns)
initial sync:              2.04 s to stabilize, 4,915 deltas/s
RSS @10k pods 1 ctx:       482 MB   <-- ~48 KB/pod retained; full raw JSON per entity (Phase 1.1)
projection (recompute):    p50 2.03ms  p99 7.82ms  max 16.7ms   (10k rows cloned each pass; Phase 2.1)
pulse aggregate:           p50 7.62ms  mean 7.73ms              (walks 10k pods' raw JSON; Phase 2.2)
frame render (cached):     p50 5.91ms  p99 6.21ms               (builds a Row per pod; Phase 2.1)
```
**Read:** under churn (revision bumps invalidate caches each delta) a single frame ≈ pulse 7.6 +
projection 2.0 + render 5.9 ≈ **15.5ms for ONE context** — at the 16.6ms/60fps budget before input
or multi-context overhead. RSS 482MB/ctx is the headline memory risk. Confirms Phase 1→2 ordering.
RSS @20 warm ctx not yet measured (needs interactive driving; expected ~linear in retained ctx).

### After Phase 1.1 (lean entity model) — same conditions
```
RSS @10k pods 1 ctx:       85.5 MB  (was 482 MB; 5.6x smaller)
pulse aggregate:           p50 0.36ms (was 7.62ms; 21x) — now sums extracted u64s, no JSON walk
projection (recompute):    p50 1.92ms  (≈unchanged; clones rows -> Phase 2.1)
frame render (cached):     p50 5.92ms  (≈unchanged; Row per pod -> Phase 2.1)
initial sync:              2.10s       (≈unchanged; per-event to_value remains -> Phase 1.2)
```
**New per-frame-under-churn ≈ 0.36 + 1.92 + 5.92 ≈ 8.2ms** (was 15.5ms). Headroom restored; the
remaining frame cost is now projection+render (Phase 2.1) and the ingest `to_value` (Phase 1.2).

### After Phase 2.1 (viewport windowing) — same conditions
```
RSS @10k pods 1 ctx:       64.8 MB  (was 85.5; cache holds keys not full ViewRows)
frame render (cached):     p50 0.47ms (was 5.92ms; 12.5x) — materializes only ~visible rows
projection (recompute):    p50 1.20ms (was 1.92ms; no per-row display-string build)
pulse aggregate:           p50 0.35ms (unchanged)
```
**Per-frame-under-churn ≈ 0.35 + 1.20 + 0.47 ≈ 2.0ms** (was 8.2ms, originally 15.5ms). Remaining
frame cost: projection recompute (sort+key-clone, O(n); could go incremental later) and the ingest
`to_value` on the watch path (Phase 1.2).

---

## Phase 1 — Memory & data model (root cost; unblocks Phases 2 & 3)
Root problem: full object JSON kept for every entity.

- [x] **1.1 Lean entity model** — DONE. Replaced `ResourceEntity.raw` with compact `Extracted`
  (model.rs); ingest extraction in `cluster/extract.rs`; full object fetched on demand via
  `ResourceProvider::get_object` into a single-slot `detail_cache` (app.rs). Design:
  `docs/design/phase-1.1-lean-entity-model.md`.
  **Measured (kwok 10k pods, 1 ctx): RSS 482MB -> 85.5MB (5.6x); pulse 7.62ms -> 0.36ms (21x).**
  71 tests pass. Residual per-event `to_value` remains (initial sync ~unchanged) -> Phase 1.2.
- [-] **1.2 Metadata/table-mode watches** — DEPRIORITIZED after measurement (2026-06-24).
  Micro-bench (`perf_to_value_vs_dynamic_parse`, kube_provider.rs) on a 2.6KB pod:
  `to_value` 5.4µs/obj, typed parse 5.0µs, dynamic parse 7.9µs. Current ingest serde
  (parse+to_value) ≈ 104ms/10k; dynamic ≈ 79ms/10k → net **~25ms saved over 10k**, and dynamic
  parse is *slower* than typed. Against a 2.1s initial sync, serde is ~5%. Sync is dominated by the
  initial LIST (network/pagination) + channel delivery + store.apply, not serialization. Not worth
  the watch-layer rewrite. Revisit only if a real cluster with fat objects (10-15KB) shifts the
  ratio, or fold a watcher list `page_size` bump into 3.x.
- [ ] **1.3 Warm-context memory bound** — entities for warm contexts (warm_contexts TTL,
  kube_provider.rs:321) can dominate RSS at 20 ctx. Cap/evict entity sets for non-active
  contexts; verify store shrinks when watchers stop (today `Reset` only fires on relist).

---

## Phase 2 — Compute & render hot paths
- [x] **2.1 Viewport-windowed projection/render** — DONE. `ViewModel` now holds ordered keys
  (`order: Vec<ResourceKey>`), not full rows; display rows materialize on demand via
  `view::materialize_row` only for the visible window in the table render (render_loop.rs) and for
  the selected row elsewhere. **Measured (10k pods): frame render 5.92ms -> 0.47ms (12.5x);
  projection 1.92ms -> 1.20ms; RSS 85.5MB -> 64.8MB.** 71 tests pass.
- [ ] **2.2 Incremental Pulse aggregates** — `pod_resource_totals(ctx,None)` (app.rs:554)
  walks every pod's raw JSON parsing CPU/mem strings; revision-keyed cache thrashes under
  churn. Maintain phase counts + resource sums incrementally in `StateStore` on delta apply,
  or make the pulse panel sampled/opt-in. (Depends on 1.1 deciding where resource data lives.)
- [ ] **2.3 Event-driven main loop** — replace `sleep(8ms)` spin + non-blocking poll
  (app.rs:235-293) with `tokio::select!` over (input, delta, log, render-tick) channels;
  coalesce delta bursts; keep FPS cap. Improves input latency under heavy relist.
- [ ] **2.4 Filter/sort cost** — precompute lowercased search keys per entity instead of
  per-keypress `format!`+`to_lowercase` over name/ns/status/summary (projector.rs:46).

---

## Phase 3 — Stability under churn/scale
- [x] **3.1 Reset/relist without blank window** — DONE. Replaced `StateDelta::Reset` (wiped the
  whole kind on every reconnect) with `RelistStart`/`RelistEnd` + a per-(ctx,kind) generation in the
  store: `Event::Init` bumps the generation (old set stays visible), each apply refreshes objects to
  the new generation, `Event::InitDone` sweeps only entities left at an older generation (truly gone
  while disconnected). No blank window during a 10k-pod relist. Unit test
  `relist_keeps_data_visible_and_sweeps_only_gone_objects`; bench confirms no perf/RSS regression
  (sync 2.03s, RSS 64.6MB). 72 tests pass.
- [x] **3.2 Delta backpressure/coalescing** — DONE (bounded drain). The main loop drained *all*
  available deltas per cycle; a 10k initial-list flood could run before input/render was checked.
  Now capped at `DELTA_MAX_PER_CYCLE` (4096) per cycle; the bounded channel back-pressures producers
  so nothing is lost and the rest drains next cycle, keeping input responsive during floods.
  (Per-context lanes/coalescing not needed — the bound + back-pressure is sufficient at measured scale.)
- [x] **3.3 Log fan-out cap** — DONE. Multi-pod log selections (ReplicaSet/Deployment) cap at
  `LOG_MAX_STREAMS` (50) concurrent streams via `capped_multi_pod_selection`; scope label notes
  "N of M streams, capped". Tests `caps_concurrent_streams_and_marks_scope`, `does_not_cap_under_limit`.
- [x] **3.4 Watcher ceiling + auth refresh** — ASSESSED, no change needed. Watcher count is already
  bounded by the warm-context policy (active context + `warm_contexts` inactive only; default 1 →
  ~2 contexts × few kinds). 401/token-expiry on watch streams is handled by kube's auth refresh on
  reconnect + the existing bounded backoff (`run_watch_loop`). A redundant hard cap would risk
  dropping the active context's essential watchers for no gain.

---

## Phase 4 — Readonly parity vs k9s (HARD REQUIREMENT: all readonly k9s functions)
- [~] **4.1 Dynamic resource discovery + CRDs** — biggest functional gap. Hybrid design (curated 24
  + generic dynamic), `docs/design/phase-4.1-dynamic-resources.md`. Shipped incrementally:
  - [x] **4.1a-1 provider layer** — `ResourceProvider::{discover,list_dynamic,get_dynamic}` via
    `kube::discovery` + `Api<DynamicObject>` (cluster/mod.rs, kube_provider.rs); `DiscoveredResource`
    /`DynamicRow` types; hidden `--discover[ --discover-resource X]` headless check; `scripts/scale/crds.sh`
    adds a sample CRD. Verified against lab: 68 resources incl. CRD `widgets.demo.krust.io`, listed +
    described 3 Widgets. 75 tests pass.
  - [x] **4.1a-2 UI** — `:api [filter]` shows the discovery catalog; `:<resource>` lists any
    discovered resource/CRD's objects (generic columns) and `:<resource> <name>` describes one — all
    via the proven `Overlay::Text` (command-driven, no new interactive overlay). Per-context
    discovery cached. Unit test `dynamic_browse_catalog_list_and_describe`. 76 tests pass.
    (Interactive selectable dynamic-list overlay deferred to 4.1c polish.)
  - [ ] **4.1b dynamic watches + KindId/ResourceKey refactor** (fold 24 typed watches into the dynamic path).
  - [ ] **4.1c navigation/UX** (discovered kinds in kind-cycle / resource picker, short names from discovery).
- [ ] **4.2 Metrics integration** — `metrics.k8s.io` for real pod/node CPU+mem (top-style).
  Current numbers are from spec requests/limits, not usage.
- [ ] **4.3 Resource-correlated events** — selecting a pod shows its events; today Events pane
  only renders Event objects directly (detail_pane.rs:170 "planned").
- [ ] **4.4 Rendered describe** — kubectl-style human `describe` output alongside YAML/JSON.
- [ ] **4.5 Filter parity** — fuzzy + regex + label-selector matching (k9s `/`, `-l`), beyond
  plain substring (projector.rs:46).
- [ ] **4.6 Port-forward** — readonly-adjacent inspection; decide in/out of v1 scope.
- [ ] **4.7 k9s readonly surface audit** — enumerate k9s readonly commands/keybindings; close
  remaining gaps or explicitly mark out-of-scope (pulses/xray/popeye are stubbed,
  command_mode.rs:286).

---

## Phase 5 — Hardening & release
- [ ] **5.1 Soak test** — 20 ctx × 10k pods × churn for hours; watch RSS / fd / tokio-task growth.
- [ ] **5.2 Reconnect-storm & mixed-RBAC soak** — verify bounded backoff, no leaks, 403 stays terminal.
- [ ] **5.3 Config defaults for large fleets** — review fps_limit / delta_channel_capacity /
  warm_contexts / TTL defaults; document tuning.
- [ ] **5.4 Docs refresh** — architecture/performance/operator guides updated to new model.
- [ ] **5.5 Release checklist** — version bump, changelog, artifact + Homebrew verification.

---

## Working agreement
- One item at a time; each PR-sized change runs `fmt → check → test → build` and reports all four (AGENTS.md).
- Perf-sensitive changes include before/after numbers from the scale lab (or complexity reasoning if not measurable).
- No commits without explicit approval (AGENTS.md).
- Readonly-only: do not add/extend mutation paths; keep `--readonly` guards intact.
