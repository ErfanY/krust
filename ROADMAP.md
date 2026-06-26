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
RSS @20 warm ctx: see Phase 1.3 — bounded to active+warm (1,048 entities / 55 MB after visiting 20),
not linear in contexts visited.

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
- [x] **1.3 Warm-context memory bound** — DONE. Entities used to persist after a context's watchers
  stopped, so the store grew with every context visited. `replace_watch_plan` now emits
  `StateDelta::EvictContext` for contexts dropped from the active+warm set; the store drops their
  entities/indexes/generation. **Measured (lab, warm 20 contexts): store plateaus at 1,048 entities
  (≈ active + 1 warm × 524) instead of ~10,480; RSS 55 MB for all 20 visited.** Bench gained
  `--bench-contexts N`. Unit test `evict_context_drops_only_that_context`. 81 tests pass.

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
- [~] **4.2 Metrics integration** — DONE (node-level). `ResourceProvider::node_metrics` reads live
  usage from `metrics.k8s.io` (via `Api<DynamicObject>`); App caches it per active context (15s TTL,
  lazy) and the pulse shows a `[USE]` row: real cpu/mem used vs allocatable + util%, with severity.
  Degrades gracefully to the request/limit rows when no metrics-server (cache → None). Hidden
  `--metrics` probe for verification. Pulse `[USE]` row (cluster) **+ per-pod CPU/MEM columns in the
  Pods table** (`pod_metrics` fetch, cached per ctx+ns, looked up per visible row; title flags
  "metrics-server n/a" when absent). **Verified on a real cluster: 45 nodes / 821 pods reporting
  real usage; graceful 404 fallback on kwok.** Unit tests: real unit formats, pod columns render +
  unavailable note.
  - **Right-sizing:** Pods CPU/MEM cells show `‹used› R‹%req› L‹%limit›` (e.g. `1.50c R150 L75`) —
    actual usage plus utilization vs request and limit; cell yellow ≥100% request, red ≥90% limit.
    `pod_usage_cell` helper; unit-tested.
- [x] **4.3 Resource-correlated events** — DONE. The Events pane (`E`) on any non-Event resource now
  lists that resource's events via `ResourceProvider::events_for` (core/v1 Events, fieldSelector
  `involvedObject.name`), kubectl-describe-style (LAST/TYPE/REASON/CNT/SOURCE/MESSAGE), newest first;
  on-demand + cached per selected key (`events_cache`), like detail. Event-kind selections still show
  the Event's own manifest. Hidden `--events <ns>/<name>` probe. **Verified against a real cluster
  (correlated a live Warning/PolicyViolation event); unit-tested render.**
- [-] **4.4 Rendered describe** — SKIPPED. Prototype (kubectl-style human `describe` alongside
  YAML/JSON) was implemented and rejected by the user; reverted in full. Raw YAML/JSON describe on
  `d`/`v` stays the canonical detail view. Do not reintroduce without a new explicit ask.
- [~] **4.5 Filter parity** — table filter now parses k9s-style: `!term` inverse, and label
  selectors `key=value`/`key==value`/`key!=value` (comma-separated, all must hold) matched against
  entity labels — this also fixes `:<kind> -l app=foo`, which previously substring-matched the
  literal `app=foo` instead of labels. Plain text stays substring (predictable default).
  Compiled once per projection (`Filter`). Tests cover label eq/neq/absent, inverse, substring
  fallback. Deferred (dep-free for now): fuzzy + regex matching; detail/log search still substring.
- [ ] **4.6 Port-forward** — readonly-adjacent inspection; decide in/out of v1 scope.
- [~] **4.7 k9s readonly surface audit** — DONE (audit): `docs/k9s-parity-audit.md` enumerates the
  full k9s readonly surface vs krust (grounded in the k9s README keybindings + commands docs) and
  marks each ✅/🟡/❌/⛔. Mutations explicitly scoped out (readonly-first v1). Prioritized gaps to
  close next, highest value first:
  - [x] **Enter drill-down** — DONE (deploy/rs/sts/ds→pods via owner chain, node→pods via
    scheduled-node). Live filter on the Pods view (`DrillFilter` in `ViewRequest`), `[DRILL]` title,
    `esc` pops back to the owner list; cleared on any kind/namespace change. (svc→endpoints deferred
    — needs the service selector, not currently extracted.)
  - [x] **Previous logs** — DONE. `P` in the logs pane toggles the previous (terminated) container
    instance (`kubectl logs -p`) as a one-shot fetch (no follow/reconnect); status line shows
    `instance:current|previous`. Plumbing (`PodLogRequest.previous`) already existed.
  - [x] **xray** — DONE. `:xray [ns|all]` opens a live **cluster ownership graph** — a
    namespace-rooted forest (ns → deploy/rs, sts, ds, cj/job, bare rs, standalone pods → pods →
    containers), not a per-kind tree. Rebuilt from store state each frame; opening it watches the
    workload kinds it draws. Proper tree rendering: box-drawing connectors (`├──`/`└──`/`│`),
    `▾`/`▸` expand markers, severity-colored status, selectable cursor (`j`/`k`/`g`/`G`), `Enter`
    expand/collapse (state keyed by node id, survives rebuild). Truncates with `… N more` (no silent
    caps). Cross-cutting relations (node, PVC, service) intentionally left to Enter-drill/describe.
  - Medium: jump-to-owner (`Shift-J`), UsedBy/dependents (`U`), log timestamps, pod metric-column
    sort, inline `:pod /term`/`@ctx`.
  - Decide: popeye sanitizer (large, separable); pulses dashboard (likely redundant with the
    always-on pulse → lean scope-out); regex/fuzzy filter (4.5).

---

## Phase 5 — Hardening & release
- [~] **5.1 Soak test** — reusable soak harness (`--soak-secs N [--soak-sample-secs S]`,
  `src/ui/app/bench.rs`) runs the real watch→store pipeline under live churn, sampling
  RSS / entity-count / fd-count for leaks. Representative run (210s, 3 rollout-restarts/s on
  100-replica deploys): **RSS 64→62 MB (−3.6%, stable), fds flat at 16, no leak**; entity count
  tracks the churning workload without affecting RSS (lean entities). Multi-hour run still TODO for
  the final gate, but the harness + this run validate the bounds hold under sustained churn.
- [~] **5.2 Reconnect-storm & mixed-RBAC soak** — partial: the 5.1 soak exercised periodic watcher
  relists (every ~20s) under load with flat fds + RSS (no reconnect/fd leak). Still TODO: explicit
  forced-reconnect storm and a mixed-RBAC (some 403) context.
  **Per-context auth resilience** (DONE): a context that can't authenticate (expired SSO / exec-auth
  failure) no longer aborts startup — provider client warmup is best-effort, and `replace_watch_plan`
  records a per-context `StateDelta::Error` (shown as `[XX]` with the message) instead of failing the
  call. Other contexts stay usable; the broken one recovers on a later watch tick once creds refresh.
  Test: bad-context watch plan records an error delta and returns Ok.
- [x] **5.8 Configurable metrics refresh interval** — `runtime.metrics_interval_secs` (default **5s**,
  down from a hardcoded 15s; k9s polls every 2s). Lower = more live, bounded by metrics-server's
  ~15s sample resolution; raise on large fleets to cut API load.
- [~] **5.0 UX consistency (labels/keybindings/help)** — ongoing pass to keep each view's help,
  labels, and key hints accurate. Done so far: per-pane help corrected (table no longer claims
  detail-only `ctrl+d/u` paging or `gg`; added namespace/sort/reverse/events/help/close hints);
  help is now **keymap-aware** (`Keymap::hint`, reflects remapped bindings); operator-guide +
  README reconciled (incl. `:api`/dynamic browse). Tests: keymap hint + per-pane help accuracy.
  **Sort indicator**: the active sort column is now marked in the table header with a direction
  arrow (`Name ↑` / `Status ↓`), the top bar carries a `[SORT] <col><arrow>` field, and `:sort
  <name|namespace|status|age> [asc|desc]` sets it explicitly (added to command autocomplete). Tests:
  header-arrow render snapshot + `:sort` column/direction parsing.
  **Per-kind columns**: non-pod views no longer collapse to a loose `Summary` string — each kind
  renders meaningful, fixed-width columns after the universal Namespace/Name/Status/Age (deploy
  UP-TO-DATE/AVAILABLE, svc TYPE/CLUSTER-IP/PORTS, pvc VOLUME/CAPACITY/ACCESS/STORAGECLASS, hpa,
  cronjob, node ROLES/VERSION, etc.). Single source of truth `ResourceKind::extra_columns()` drives
  both header and `extract_columns()` row values; a contract test asserts they stay in lockstep for
  all 24 kinds. Entity model: `summary: String` → `columns: Vec<String>` (pods now carry an empty
  vec — drops the unused per-pod `node=` string on the 10k-pod hot path). Filter + `:dump`/copy span
  all columns. ServiceAccounts (SECRETS/PULL-SECRETS) and (Cluster)RoleBindings (ROLE/SUBJECTS)
  enriched beyond the initial pass.
  **Hide Helm secrets**: Helm release secrets (`type: helm.sh/release.v1`) are hidden from the
  Secrets list by default (clutter) — toggle with `H` or `:helm [show|hide]`, per-tab, with the
  Secrets title advertising current state. Detection via `ResourceEntity::is_helm_release()` (type
  or `sh.helm.release.v1.*` name); filtered at the projection layer. Tests: model detection,
  projector hide/show, render+toggle integration.
  Remaining: revisit when new views/commands land (e.g. interactive dynamic-list overlay).
- [x] **5.6 Triage board** — operator daily-driver "what needs attention" view (`:triage`/`:issues
  [ns|all]`): a live, worst-first board of only the pods needing an eye — CrashLoopBackOff/OOMKilled/
  ImagePullBackOff/Error (critical), Pending/NotReady/restart-hot ≥3 (warning). Reuses existing
  severity + adds `restarts`/`ready` to `Extracted` (crashloop + readiness signals the status string
  hides). Title summarizes `N critical · M warning`; healthy pods omitted; capped at 500 rows
  worst-first. Also fixed `classify_status_severity` to flag `ImagePullBackOff`/`*BackOff` (was Ok)
  — improves table coloring too. The pods table also gained `RST` (restarts, colored ≥3/≥10),
  `IP`, and `NODE` columns (pod IP added to `Extracted`); metric headers are now the explicit
  `%CPU/R`/`%CPU/L`/`%MEM/R`/`%MEM/L`. Right-sizing % columns now graduate green→yellow→red
  (%R: ≥100 warn, ≥200 err; %L: ≥75 warn, ≥90 err). Next: per-tenant (namespace) health rollup;
  workload rollout health (ready≠desired).
- [ ] **5.7 Per-pod usage history (p95/max)** — keep a short rolling window of per-pod usage so
  right-sizing reads from p95/max, not an instantaneous snapshot. Unblocks a reliable
  **over-provisioned** (low-%R) signal — deliberately deferred from the high-side colors because a
  snapshot-based "you're wasting resources" flag cries wolf on momentary idle dips. Also steadies
  the high-side colors.
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
