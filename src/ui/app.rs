use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use chrono::Local;
use crossterm::{
    cursor::MoveTo,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table, Wrap},
};
use tokio::sync::mpsc;

use super::commands::{ResourceAlias, command_names, parse_resource_alias, resource_alias_names};
#[cfg(test)]
use super::detail::base64_decode;
use super::detail::{
    apply_decoded_secret_json, apply_decoded_secret_yaml, base64_encode, decoded_secret_json_text,
    decoded_secret_text, parse_json_to_json, parse_yaml_to_json, to_pretty_json, to_pretty_yaml,
};
use super::highlight::{
    ColorSupport, highlighted_json_text, highlighted_text, highlighted_yaml_text,
};
#[cfg(test)]
use super::highlight::{json_spans_for_line, yaml_spans_for_line};
use super::pulse::{
    activity_icon, ascii_meter, fixed_width_cell, format_pulse_cells, format_signed_count,
    ratio_percent_value, value_delta,
};
#[cfg(test)]
use super::render::detect_color_support_from_env;
use super::render::{
    Severity, UiTheme, classify_status_severity, color_support_label, detect_color_support,
    severity_style, severity_tag, status_style_for_line, ui_theme_for,
};
#[cfg(test)]
use super::search::search_match_lines_in_logs;
use super::search::{
    detail_viewer_title, resolved_active_match_line, search_match_lines, step_match_line,
};

use crate::{
    cluster::{
        ActionError, ActionExecutor, DiscoveredResource, PodLogEvent, PodLogRequest, PodLogStream,
        ResourceProvider, WatchTarget,
    },
    keymap::{Action, Keymap},
    model::{
        ConfirmationKind, Pane, PendingConfirmation, ResourceKey, ResourceKind, SortColumn,
        StateDelta,
    },
    state::StateStore,
    view::{
        DrillFilter, SimpleViewProjector, ViewModel, ViewProjector, ViewRequest, materialize_row,
    },
};

mod bench;
mod command_mode;
mod detail_pane;
mod logs;
mod overlay;
mod render_loop;
mod triage;
mod xray;

#[derive(Debug, Clone)]
struct ContextTabState {
    context: String,
    namespace: Option<String>,
    filter: String,
    detail_filter: String,
    detail_active_match_line: Option<usize>,
    selected: usize,
    table_offset: usize,
    detail_scroll: u16,
    detail_hscroll: u16,
    detail_wrap: bool,
    detail_format: DetailFormat,
    kind_idx: usize,
    last_non_namespace_kind_idx: usize,
    sort: SortColumn,
    descending: bool,
    /// Show Helm release secrets in the Secrets list (default false — they're hidden clutter).
    show_helm_secrets: bool,
    /// Active Enter drill-down (Pods view scoped to an owner). Cleared on kind/namespace change
    /// or Esc in the table.
    drill: Option<DrillFilter>,
    pane: Pane,
}

impl ContextTabState {
    fn kind(&self) -> ResourceKind {
        ResourceKind::ORDERED[self.kind_idx]
    }
}

#[derive(Debug, Clone, Copy)]
enum CommandMode {
    Command,
    Filter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailFormat {
    Yaml,
    Json,
}

impl DetailFormat {
    fn label(self) -> &'static str {
        match self {
            DetailFormat::Yaml => "yaml",
            DetailFormat::Json => "json",
        }
    }

    fn extension(self) -> &'static str {
        self.label()
    }

    fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "yaml" | "yml" => Some(DetailFormat::Yaml),
            "json" => Some(DetailFormat::Json),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct CommandInput {
    mode: CommandMode,
    value: String,
}

impl CommandInput {
    fn new(mode: CommandMode, initial_value: String) -> Self {
        Self {
            mode,
            value: initial_value,
        }
    }

    fn prefix(&self) -> &'static str {
        match self.mode {
            CommandMode::Command => ":",
            CommandMode::Filter => "/",
        }
    }
}

#[derive(Debug, Clone)]
enum Overlay {
    Text {
        title: String,
        lines: Vec<String>,
        scroll: u16,
        hscroll: u16,
        wrap: bool,
    },
    /// Live cluster relationship graph (`:xray`): a namespace-rooted ownership forest
    /// (ns → controllers → pods → containers). Holds the scope + view state (cursor, collapsed
    /// nodes), not the rows — rows are rebuilt from the store each frame so the tree tracks the
    /// cluster. `namespace` is None for the whole cluster. Collapsed nodes are keyed by stable id.
    Xray {
        namespace: Option<String>,
        selected: usize,
        collapsed: HashSet<String>,
    },
    /// Live triage board (`:triage`): pods needing attention (crashloop/OOM/pending/not-ready/
    /// restart-hot), worst first. Rows rebuilt from the store each frame. `namespace` None =
    /// whole cluster.
    Triage {
        namespace: Option<String>,
        selected: usize,
    },
    Contexts {
        contexts: Vec<String>,
        selected: usize,
        filter: String,
    },
    Containers {
        title: String,
        pod: ResourceKey,
        containers: Vec<String>,
        selected: usize,
        filter: String,
    },
    LogSources {
        sources: Vec<String>,
        selected: usize,
        filter: String,
    },
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    contexts: Vec<String>,
    initial_context: String,
    initial_namespace: Option<String>,
    context_default_namespaces: HashMap<String, Option<String>>,
    mut delta_rx: mpsc::Receiver<StateDelta>,
    action_executor: Arc<dyn ActionExecutor>,
    resource_provider: Arc<dyn ResourceProvider>,
    keymap: Keymap,
    readonly: bool,
    fps_limit: u16,
    show_help: bool,
) -> anyhow::Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;

    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;
    terminal.clear().context("failed to clear terminal")?;

    let mut app = App::new(
        contexts,
        initial_context,
        initial_namespace,
        context_default_namespaces,
        action_executor,
        resource_provider,
        keymap,
        readonly,
        show_help,
    );

    app.ensure_active_watch().await;

    let frame_budget = Duration::from_millis((1000 / fps_limit.max(1) as u64).max(1));
    let mut last_render = Instant::now() - frame_budget;
    let mut dirty = true;

    loop {
        if app.needs_terminal_reset {
            terminal
                .clear()
                .context("failed to clear terminal after editor exit")?;
            app.needs_terminal_reset = false;
            dirty = true;
            last_render = Instant::now() - frame_budget;
        }

        let mut drained = 0usize;
        while drained < DELTA_MAX_PER_CYCLE {
            match delta_rx.try_recv() {
                Ok(delta) => {
                    app.store.apply(delta);
                    dirty = true;
                    drained += 1;
                }
                Err(_) => break,
            }
        }
        if app.drain_log_events() {
            dirty = true;
        }

        while event::poll(Duration::from_millis(0)).context("event poll failed")? {
            match event::read().context("event read failed")? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    if app.handle_key(key).await? {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    if app.handle_mouse(mouse) {
                        dirty = true;
                    }
                }
                _ => {}
            }
        }

        if app.expire_confirmation() {
            dirty = true;
        }
        if app.reconcile_log_session().await {
            dirty = true;
        }
        if app.drain_log_events() {
            dirty = true;
        }

        if dirty && last_render.elapsed() >= frame_budget {
            terminal
                .draw(|frame| app.draw(frame))
                .context("render failed")?;
            last_render = Instant::now();
            dirty = false;
        }

        tokio::time::sleep(Duration::from_millis(8)).await;
    }
}

/// Headless performance benchmark entry point. Boots the same data plane as `run` (provider,
/// watchers, store) against the initial context scoped to all-namespace Pods, lets it sync, then
/// measures the real project/render/aggregate hot paths and prints a report. No terminal setup.
#[allow(clippy::too_many_arguments)]
pub async fn run_bench(
    contexts: Vec<String>,
    initial_context: String,
    context_default_namespaces: HashMap<String, Option<String>>,
    delta_rx: mpsc::Receiver<StateDelta>,
    action_executor: Arc<dyn ActionExecutor>,
    resource_provider: Arc<dyn ResourceProvider>,
    keymap: Keymap,
    readonly: bool,
    iters: usize,
    settle_secs: u64,
    context_count: usize,
    soak_secs: u64,
    soak_sample_secs: u64,
) -> anyhow::Result<()> {
    let mut app = App::new(
        contexts,
        initial_context,
        None,
        context_default_namespaces,
        action_executor,
        resource_provider,
        keymap,
        readonly,
        false,
    );
    app.run_bench(
        delta_rx,
        iters.max(1),
        settle_secs,
        context_count.max(1),
        soak_secs,
        soak_sample_secs.max(1),
    )
    .await;
    Ok(())
}

struct App {
    store: StateStore,
    tabs: Vec<ContextTabState>,
    active_tab: usize,
    projector: SimpleViewProjector,
    view_cache: Option<CachedViewModel>,
    command_input: Option<CommandInput>,
    command_history: Vec<String>,
    last_command: Option<String>,
    history_cursor: Option<usize>,
    overlay: Option<Overlay>,
    pod_util_cache: HashMap<UtilScopeKey, CachedPodTotals>,
    node_util_cache: HashMap<String, CachedNodeTotals>,
    logs: LogViewState,
    status_line: String,
    pending_confirmation: Option<PendingConfirmation>,
    action_executor: Arc<dyn ActionExecutor>,
    resource_provider: Arc<dyn ResourceProvider>,
    keymap: Keymap,
    readonly: bool,
    theme: UiTheme,
    color_support: ColorSupport,
    pulse_metrics_cache: Option<CachedPulseMetrics>,
    pulse_snapshot: Option<PulseSnapshot>,
    pulse_last_change_at: Instant,
    pulse_last_revision: u64,
    pulse_last_revision_at: Instant,
    show_help: bool,
    detail_page_step: u16,
    pending_detail_g: bool,
    needs_terminal_reset: bool,
    detail_cache: Option<DetailObject>,
    /// Events correlated to the selected resource (Phase 4.3), fetched on demand for the Events
    /// pane when the selection isn't itself an Event.
    events_cache: Option<(ResourceKey, Vec<crate::cluster::EventRow>)>,
    /// Per-context API discovery catalog (Phase 4.1), cached after first `:api`/dynamic browse.
    discovery_cache: HashMap<String, Vec<DiscoveredResource>>,
    /// Live cluster usage from metrics.k8s.io (Phase 4.2), refreshed lazily for the active context.
    /// `None` means metrics-server isn't available — the pulse falls back to request-based numbers.
    metrics: Option<ClusterMetrics>,
    /// Per-pod live usage `(namespace, name) -> (cpu_millicores, mem_bytes)` for the active scope,
    /// shown as CPU/MEM columns in the Pods table. Empty when no metrics-server.
    pod_metrics: HashMap<(String, String), (u64, u64)>,
    /// (context, namespace-scope, fetched-at) guarding the 15s metrics refresh.
    metrics_fetched: Option<(String, Option<String>, Instant)>,
}

/// Cluster-wide actual usage summed from node metrics (Phase 4.2).
#[derive(Debug, Clone, Copy)]
struct ClusterMetrics {
    cpu_used_m: u64,
    mem_used_b: u64,
    nodes_reporting: usize,
}

/// On-demand full object for the active detail/describe/decode/edit view. Single-slot: holds the
/// last opened resource only (selection is stable while scrolling a detail pane).
#[derive(Debug, Clone)]
struct DetailObject {
    key: ResourceKey,
    value: serde_json::Value,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UtilScopeKey {
    context: String,
    namespace: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct PodResourceTotals {
    pods: usize,
    cpu_request_m: u64,
    cpu_limit_m: u64,
    mem_request_b: u64,
    mem_limit_b: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct NodeCapacityTotals {
    nodes_total: usize,
    nodes_ready: usize,
    nodes_unschedulable: usize,
    cpu_alloc_m: u64,
    mem_alloc_b: u64,
    pod_alloc: u64,
}

#[derive(Debug, Clone)]
struct PulseSnapshot {
    context: String,
    namespace: Option<String>,
    cluster_cpu_req_m: u64,
    cluster_mem_req_b: u64,
    cluster_pods: u64,
    running: usize,
    pending: usize,
    failed: usize,
}

#[derive(Debug, Clone, Copy)]
struct PulseMetrics {
    running: usize,
    pending: usize,
    failed: usize,
    other: usize,
    scope_pods: PodResourceTotals,
    cluster_pods: PodResourceTotals,
    node_caps: NodeCapacityTotals,
    deployments: usize,
    replicasets: usize,
    statefulsets: usize,
    daemonsets: usize,
    services: usize,
    ingresses: usize,
    jobs: usize,
    cronjobs: usize,
    pods: usize,
}

#[derive(Debug, Clone)]
struct CachedPulseMetrics {
    key: UtilScopeKey,
    revision: u64,
    metrics: PulseMetrics,
}

#[derive(Debug, Clone, Copy)]
struct CachedPodTotals {
    totals: PodResourceTotals,
    computed_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct CachedNodeTotals {
    totals: NodeCapacityTotals,
    computed_at: Instant,
}

const LOG_MAX_LINES: usize = 5_000;
const LOG_MAX_BYTES: usize = 8 * 1024 * 1024;
const LOG_DEFAULT_TAIL_LINES: i64 = 2_000;
const LOG_MAX_EVENTS_PER_DRAIN: usize = 1_024;
/// Max state deltas applied per UI cycle. Bounds per-cycle work so an initial-list flood
/// (10k+ upserts) can't starve input/render within one loop iteration; the rest drains next cycle
/// (the bounded channel back-pressures producers, so nothing is lost or grows unbounded).
const DELTA_MAX_PER_CYCLE: usize = 4_096;
/// Max concurrent pod log streams for a multi-pod selection (ReplicaSet/Deployment). A deployment
/// with hundreds of replicas would otherwise open hundreds of follow connections to the apiserver.
const LOG_MAX_STREAMS: usize = 50;
const PULSE_TAG_WIDTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogTarget {
    context: String,
    namespace: String,
    pod: String,
    container: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSelection {
    scope: String,
    targets: Vec<LogTarget>,
}

struct ActiveLogSession {
    rx: mpsc::Receiver<PodLogEvent>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct LogViewState {
    selection: Option<LogSelection>,
    session: Option<ActiveLogSession>,
    lines: VecDeque<String>,
    lines_version: u64,
    joined_cache: String,
    joined_cache_version: u64,
    total_bytes: usize,
    max_line_width: usize,
    dropped_lines: u64,
    last_error: Option<String>,
    stream_closed: bool,
    auto_scroll: bool,
    reconnect_attempt: u32,
    reconnect_after: Option<Instant>,
    reconnect_blocked: bool,
    paused: bool,
    /// Stream the previous (terminated) container instance's logs (`kubectl logs -p`) — a one-shot
    /// fetch (no follow/reconnect), used for crashloop debugging.
    previous: bool,
    paused_skipped_lines: u64,
    container_override_pod: Option<ResourceKey>,
    container_override: Option<String>,
    hidden_sources: HashSet<String>,
    source_filter_version: u64,
    search_cache_query: String,
    search_cache_lines_version: u64,
    search_cache_source_filter_version: u64,
    search_cache_matches: Vec<usize>,
}

#[derive(Debug, Clone)]
struct CachedViewModel {
    revision: u64,
    request: ViewRequest,
    model: Arc<ViewModel>,
}

impl Default for LogViewState {
    fn default() -> Self {
        Self {
            selection: None,
            session: None,
            lines: VecDeque::new(),
            lines_version: 0,
            joined_cache: String::new(),
            joined_cache_version: 0,
            total_bytes: 0,
            max_line_width: 0,
            dropped_lines: 0,
            last_error: None,
            stream_closed: false,
            auto_scroll: true,
            reconnect_attempt: 0,
            reconnect_after: None,
            reconnect_blocked: false,
            paused: false,
            previous: false,
            paused_skipped_lines: 0,
            container_override_pod: None,
            container_override: None,
            hidden_sources: HashSet::new(),
            source_filter_version: 0,
            search_cache_query: String::new(),
            search_cache_lines_version: 0,
            search_cache_source_filter_version: 0,
            search_cache_matches: Vec::new(),
        }
    }
}

impl App {
    fn projected_view(&mut self, request: &ViewRequest) -> Arc<ViewModel> {
        let revision = self.store.revision();
        let needs_recompute = self
            .view_cache
            .as_ref()
            .map(|cached| cached.revision != revision || cached.request != *request)
            .unwrap_or(true);

        if needs_recompute {
            let model = Arc::new(self.projector.project(&self.store, request));
            self.view_cache = Some(CachedViewModel {
                revision,
                request: request.clone(),
                model,
            });
        }

        self.view_cache
            .as_ref()
            .expect("view cache must be set")
            .model
            .clone()
    }

    fn view_request_for_tab(&self, tab: &ContextTabState) -> ViewRequest {
        ViewRequest {
            context: tab.context.clone(),
            kind: tab.kind(),
            namespace: if tab.kind().is_namespaced() {
                tab.namespace.clone()
            } else {
                None
            },
            filter: tab.filter.clone(),
            sort: tab.sort,
            descending: tab.descending,
            show_helm_secrets: tab.show_helm_secrets,
            // Drill-down only applies to the Pods view.
            drill: if tab.kind() == ResourceKind::Pods {
                tab.drill.clone()
            } else {
                None
            },
        }
    }

    fn selected_row(&mut self) -> Option<crate::view::ViewRow> {
        let active = self.current_tab().clone();
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let selected = active.selected.min(vm.len().saturating_sub(1));
        let key = vm.key(selected)?.clone();
        materialize_row(&self.store, &key)
    }

    fn count_kind_for_tab(&self, tab: &ContextTabState, kind: ResourceKind) -> usize {
        let ns_filter = if kind.is_namespaced() {
            tab.namespace.as_deref()
        } else {
            None
        };
        self.store.count(&tab.context, kind, ns_filter)
    }

    fn pod_resource_totals(&mut self, context: &str, namespace: Option<&str>) -> PodResourceTotals {
        let key = UtilScopeKey {
            context: context.to_string(),
            namespace: namespace.map(str::to_string),
        };
        if let Some(cached) = self.pod_util_cache.get(&key)
            && cached.computed_at.elapsed() < Duration::from_millis(750)
        {
            return cached.totals;
        }

        let pods = self.store.list(context, ResourceKind::Pods, namespace);
        let mut totals = PodResourceTotals {
            pods: pods.len(),
            ..PodResourceTotals::default()
        };
        for pod in pods {
            if let Some(res) = &pod.extracted.pod_resources {
                totals.cpu_request_m = totals.cpu_request_m.saturating_add(res.cpu_request_m);
                totals.cpu_limit_m = totals.cpu_limit_m.saturating_add(res.cpu_limit_m);
                totals.mem_request_b = totals.mem_request_b.saturating_add(res.mem_request_b);
                totals.mem_limit_b = totals.mem_limit_b.saturating_add(res.mem_limit_b);
            }
        }

        self.pod_util_cache.insert(
            key,
            CachedPodTotals {
                totals,
                computed_at: Instant::now(),
            },
        );
        totals
    }

    fn node_capacity_totals(&mut self, context: &str) -> NodeCapacityTotals {
        if let Some(cached) = self.node_util_cache.get(context)
            && cached.computed_at.elapsed() < Duration::from_secs(2)
        {
            return cached.totals;
        }

        let nodes = self.store.list(context, ResourceKind::Nodes, None);
        let mut totals = NodeCapacityTotals {
            nodes_total: nodes.len(),
            ..NodeCapacityTotals::default()
        };
        for node in nodes {
            let Some(cap) = &node.extracted.node_capacity else {
                continue;
            };
            if cap.ready {
                totals.nodes_ready += 1;
            }
            if cap.unschedulable {
                totals.nodes_unschedulable += 1;
            }
            totals.cpu_alloc_m = totals.cpu_alloc_m.saturating_add(cap.cpu_alloc_m);
            totals.mem_alloc_b = totals.mem_alloc_b.saturating_add(cap.mem_alloc_b);
            totals.pod_alloc = totals.pod_alloc.saturating_add(cap.pod_alloc);
        }

        self.node_util_cache.insert(
            context.to_string(),
            CachedNodeTotals {
                totals,
                computed_at: Instant::now(),
            },
        );
        totals
    }

    fn pod_phase_counts_for_tab(&self, tab: &ContextTabState) -> (usize, usize, usize, usize) {
        let pods = self
            .store
            .list(&tab.context, ResourceKind::Pods, tab.namespace.as_deref());
        let mut running = 0usize;
        let mut pending = 0usize;
        let mut failed = 0usize;
        let mut other = 0usize;

        for pod in pods {
            let phase = pod.status.to_ascii_lowercase();
            if phase.contains("running") {
                running += 1;
            } else if phase.contains("pending") {
                pending += 1;
            } else if phase.contains("failed") || phase.contains("error") {
                failed += 1;
            } else {
                other += 1;
            }
        }

        (running, pending, failed, other)
    }

    fn pulse_metrics_for_tab(&mut self, tab: &ContextTabState) -> PulseMetrics {
        let cache_key = UtilScopeKey {
            context: tab.context.clone(),
            namespace: tab.namespace.clone(),
        };
        let revision = self.store.revision();
        if let Some(cached) = &self.pulse_metrics_cache
            && cached.revision == revision
            && cached.key == cache_key
        {
            return cached.metrics;
        }

        let (running, pending, failed, other) = self.pod_phase_counts_for_tab(tab);
        let scope_pods = self.pod_resource_totals(&tab.context, tab.namespace.as_deref());
        let cluster_pods = self.pod_resource_totals(&tab.context, None);
        let node_caps = self.node_capacity_totals(&tab.context);
        let metrics = PulseMetrics {
            running,
            pending,
            failed,
            other,
            scope_pods,
            cluster_pods,
            node_caps,
            deployments: self.count_kind_for_tab(tab, ResourceKind::Deployments),
            replicasets: self.count_kind_for_tab(tab, ResourceKind::ReplicaSets),
            statefulsets: self.count_kind_for_tab(tab, ResourceKind::StatefulSets),
            daemonsets: self.count_kind_for_tab(tab, ResourceKind::DaemonSets),
            services: self.count_kind_for_tab(tab, ResourceKind::Services),
            ingresses: self.count_kind_for_tab(tab, ResourceKind::Ingresses),
            jobs: self.count_kind_for_tab(tab, ResourceKind::Jobs),
            cronjobs: self.count_kind_for_tab(tab, ResourceKind::CronJobs),
            pods: self.count_kind_for_tab(tab, ResourceKind::Pods),
        };
        self.pulse_metrics_cache = Some(CachedPulseMetrics {
            key: cache_key,
            revision,
            metrics,
        });
        metrics
    }

    fn active_filter_value(&self) -> String {
        if let Some(overlay) = &self.overlay {
            match overlay {
                Overlay::Contexts { filter, .. }
                | Overlay::Containers { filter, .. }
                | Overlay::LogSources { filter, .. } => {
                    return filter.clone();
                }
                Overlay::Text { .. } | Overlay::Xray { .. } | Overlay::Triage { .. } => {}
            }
        }
        let tab = self.current_tab();
        if tab.pane == Pane::Table {
            tab.filter.clone()
        } else {
            tab.detail_filter.clone()
        }
    }

    fn set_active_filter_value(&mut self, filter: String) {
        if let Some(overlay) = &mut self.overlay {
            match overlay {
                Overlay::Contexts {
                    filter: overlay_filter,
                    ..
                }
                | Overlay::Containers {
                    filter: overlay_filter,
                    ..
                }
                | Overlay::LogSources {
                    filter: overlay_filter,
                    ..
                } => {
                    *overlay_filter = filter;
                }
                Overlay::Text { .. } | Overlay::Xray { .. } | Overlay::Triage { .. } => {}
            }
            return;
        }
        let tab = self.current_tab_mut();
        if tab.pane == Pane::Table {
            tab.filter = filter;
        } else {
            let changed = tab.detail_filter != filter;
            tab.detail_filter = filter;
            if changed {
                tab.detail_active_match_line = None;
            }
        }
    }

    fn clear_active_filter_value(&mut self) -> bool {
        if self.active_filter_value().is_empty() {
            return false;
        }
        self.set_active_filter_value(String::new());
        true
    }

    fn active_filter_label(&self) -> &'static str {
        if let Some(overlay) = &self.overlay {
            return match overlay {
                Overlay::Contexts { .. }
                | Overlay::Containers { .. }
                | Overlay::LogSources { .. } => "Filter",
                Overlay::Text { .. } | Overlay::Xray { .. } | Overlay::Triage { .. } => "Search",
            };
        }
        if self.current_tab().pane == Pane::Table {
            "Filter"
        } else {
            "Search"
        }
    }

    fn filter_status_message(&self, value: &str) -> String {
        if value.is_empty() {
            format!("{} cleared", self.active_filter_label())
        } else {
            format!("{} set: {value}", self.active_filter_label())
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        contexts: Vec<String>,
        initial_context: String,
        initial_namespace: Option<String>,
        context_default_namespaces: HashMap<String, Option<String>>,
        action_executor: Arc<dyn ActionExecutor>,
        resource_provider: Arc<dyn ResourceProvider>,
        keymap: Keymap,
        readonly: bool,
        show_help: bool,
    ) -> Self {
        let tabs: Vec<ContextTabState> = contexts
            .iter()
            .map(|context| ContextTabState {
                context: context.clone(),
                namespace: if context == &initial_context {
                    initial_namespace
                        .clone()
                        .or_else(|| context_default_namespaces.get(context).cloned().flatten())
                } else {
                    context_default_namespaces.get(context).cloned().flatten()
                },
                filter: String::new(),
                detail_filter: String::new(),
                detail_active_match_line: None,
                selected: 0,
                table_offset: 0,
                detail_scroll: 0,
                detail_hscroll: 0,
                detail_wrap: true,
                detail_format: DetailFormat::Yaml,
                kind_idx: 0,
                last_non_namespace_kind_idx: 0,
                sort: SortColumn::Name,
                descending: false,
                show_helm_secrets: false,
                drill: None,
                pane: Pane::Table,
            })
            .collect();

        let active_tab = tabs
            .iter()
            .position(|tab| tab.context == initial_context)
            .unwrap_or(0);
        let color_support = detect_color_support();
        let now = Instant::now();

        Self {
            store: StateStore::default(),
            tabs,
            active_tab,
            projector: SimpleViewProjector,
            view_cache: None,
            command_input: None,
            command_history: Vec::new(),
            last_command: None,
            history_cursor: None,
            overlay: None,
            pod_util_cache: HashMap::new(),
            node_util_cache: HashMap::new(),
            logs: LogViewState::default(),
            status_line: "Press ':' for commands and '/' for filter".to_string(),
            pending_confirmation: None,
            action_executor,
            resource_provider,
            keymap,
            readonly,
            theme: ui_theme_for(color_support),
            color_support,
            pulse_metrics_cache: None,
            pulse_snapshot: None,
            pulse_last_change_at: now,
            pulse_last_revision: 0,
            pulse_last_revision_at: now,
            show_help,
            detail_page_step: 10,
            pending_detail_g: false,
            needs_terminal_reset: false,
            detail_cache: None,
            events_cache: None,
            discovery_cache: HashMap::new(),
            metrics: None,
            pod_metrics: HashMap::new(),
            metrics_fetched: None,
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        if self.command_input.is_some() {
            let should_quit = self.handle_command_key(key).await?;
            if !should_quit {
                self.ensure_active_watch().await;
            }
            return Ok(should_quit);
        }

        if self.overlay.is_some() {
            self.handle_overlay_key(key).await;
            return Ok(false);
        }

        if !self.in_detail_pane() {
            self.pending_detail_g = false;
        } else {
            if self.pending_detail_g {
                self.pending_detail_g = false;
                if key.modifiers.is_empty() && key.code == KeyCode::Char('g') {
                    self.current_tab_mut().detail_scroll = 0;
                    if self.current_tab().pane == Pane::Logs {
                        self.logs.auto_scroll = false;
                    }
                    self.status_line = "Top".to_string();
                    self.ensure_active_watch().await;
                    return Ok(false);
                }
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
                let step = self.detail_page_step.max(1);
                self.scroll_detail(step as isize);
                self.status_line = "Half-page down".to_string();
                self.ensure_active_watch().await;
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
                let step = self.detail_page_step.max(1);
                self.scroll_detail(-(step as isize));
                self.status_line = "Half-page up".to_string();
                self.ensure_active_watch().await;
                return Ok(false);
            }
            if key.modifiers.is_empty() && key.code == KeyCode::Char('g') {
                self.pending_detail_g = true;
                self.status_line = "g".to_string();
                return Ok(false);
            }
            if key.modifiers.is_empty() && key.code == KeyCode::Char('n') {
                self.jump_detail_match(true);
                self.ensure_active_watch().await;
                return Ok(false);
            }
            if (key.code == KeyCode::Char('N'))
                || (key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::SHIFT))
            {
                self.jump_detail_match(false);
                self.ensure_active_watch().await;
                return Ok(false);
            }
            if key.code == KeyCode::Char('?')
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                let existing_filter = self.active_filter_value();
                self.command_input = Some(CommandInput::new(CommandMode::Filter, existing_filter));
                self.status_line = format!("{} mode", self.active_filter_label());
                return Ok(false);
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }

        if key.code == KeyCode::Char(':') {
            self.command_input = Some(CommandInput::new(CommandMode::Command, String::new()));
            self.status_line = "Command mode".to_string();
            return Ok(false);
        }
        if key.code == KeyCode::Char('y')
            && key.modifiers.is_empty()
            && self.pending_confirmation.is_none()
            && self.in_detail_pane()
            && self.overlay.is_none()
        {
            self.copy_current_view_to_clipboard();
            return Ok(false);
        }
        if key.code == KeyCode::Enter {
            self.handle_enter_key();
            self.ensure_active_watch().await;
            return Ok(false);
        }

        if key.code == KeyCode::Char('-') {
            if let Some(last) = self.last_command.clone() {
                let should_quit = self.execute_colon_command(&last).await;
                if !should_quit {
                    self.ensure_active_watch().await;
                }
                return Ok(should_quit);
            }
            self.status_line = "No previous command".to_string();
            return Ok(false);
        }

        if key.code == KeyCode::Char('[') {
            if let Some(cmd) = self.history_step_back() {
                let should_quit = self.execute_colon_command(&cmd).await;
                if !should_quit {
                    self.ensure_active_watch().await;
                }
                return Ok(should_quit);
            }
            self.status_line = "No command history".to_string();
            return Ok(false);
        }

        if key.code == KeyCode::Char(']') {
            if let Some(cmd) = self.history_step_forward() {
                let should_quit = self.execute_colon_command(&cmd).await;
                if !should_quit {
                    self.ensure_active_watch().await;
                }
                return Ok(should_quit);
            }
            self.status_line = "No newer command history".to_string();
            return Ok(false);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.show_resource_aliases_overlay();
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            // k9s-compatible kill shortcut. For now we route to the guarded delete flow.
            self.prepare_delete_confirmation();
            self.status_line.push_str(" (ctrl+k mapped to delete)");
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('v') {
            // k9s "view yaml" compatibility; describe pane is the current read-only detail view.
            self.current_tab_mut().pane = Pane::Describe;
            self.current_tab_mut().detail_scroll = 0;
            self.current_tab_mut().detail_hscroll = 0;
            self.current_tab_mut().detail_format = DetailFormat::Yaml;
            self.overlay = None;
            self.status_line = "View opened (yaml)".to_string();
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('e') {
            self.edit_current_view(None).await;
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('x') {
            self.toggle_secret_decode();
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('c') {
            self.open_container_picker_from_selection();
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if key.modifiers.is_empty()
            && key.code == KeyCode::Char('w')
            && self.current_tab().pane != Pane::Table
        {
            self.toggle_detail_wrap();
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if self.current_tab().pane == Pane::Logs
            && key.modifiers.is_empty()
            && key.code == KeyCode::Char('p')
        {
            self.set_log_paused(!self.logs.paused);
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if self.current_tab().pane == Pane::Logs
            && ((key.code == KeyCode::Char('P'))
                || (key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::SHIFT)))
        {
            self.toggle_previous_logs().await;
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if self.current_tab().pane == Pane::Logs
            && ((key.code == KeyCode::Char('L'))
                || (key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::SHIFT)))
        {
            self.jump_logs_to_latest();
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if self.current_tab().pane == Pane::Logs
            && ((key.code == KeyCode::Char('S'))
                || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::SHIFT)))
        {
            self.show_log_sources_overlay();
            self.ensure_active_watch().await;
            return Ok(false);
        }

        if self.keymap.is(Action::Quit, &key) {
            return Ok(true);
        }
        if self.keymap.is(Action::NextContext, &key) {
            self.next_context();
        } else if self.keymap.is(Action::PrevContext, &key) {
            self.prev_context();
        } else if self.keymap.is(Action::NextKind, &key) {
            self.next_kind();
        } else if self.keymap.is(Action::PrevKind, &key) {
            self.prev_kind();
        } else if self.keymap.is(Action::MoveDown, &key) || key.code == KeyCode::Down {
            if self.current_tab().pane == Pane::Table {
                self.move_selection(1);
            } else {
                self.scroll_detail(1);
            }
        } else if self.keymap.is(Action::MoveUp, &key) || key.code == KeyCode::Up {
            if self.current_tab().pane == Pane::Table {
                self.move_selection(-1);
            } else {
                self.scroll_detail(-1);
            }
        } else if key.code == KeyCode::Left
            && self.current_tab().pane != Pane::Table
            && !self.current_tab().detail_wrap
        {
            self.scroll_detail_horizontal(-4);
        } else if key.code == KeyCode::Right
            && self.current_tab().pane != Pane::Table
            && !self.current_tab().detail_wrap
        {
            self.scroll_detail_horizontal(4);
        } else if self.keymap.is(Action::GotoTop, &key) {
            if self.current_tab().pane == Pane::Table {
                self.current_tab_mut().selected = 0;
            } else {
                self.current_tab_mut().detail_scroll = 0;
                if self.current_tab().pane == Pane::Logs {
                    self.logs.auto_scroll = false;
                }
            }
        } else if self.keymap.is(Action::GotoBottom, &key) {
            if self.current_tab().pane == Pane::Table {
                self.current_tab_mut().selected = usize::MAX;
            } else {
                self.current_tab_mut().detail_scroll = u16::MAX;
                if self.current_tab().pane == Pane::Logs {
                    self.logs.auto_scroll = true;
                }
            }
        } else if self.keymap.is(Action::FilterMode, &key) {
            let existing_filter = self.active_filter_value();
            self.command_input = Some(CommandInput::new(CommandMode::Filter, existing_filter));
            self.status_line = format!("{} mode", self.active_filter_label());
        } else if self.keymap.is(Action::CycleSort, &key) {
            if self.current_tab().pane == Pane::Logs {
                self.logs.auto_scroll = !self.logs.auto_scroll;
                if self.logs.auto_scroll {
                    self.logs.paused = false;
                    self.current_tab_mut().detail_scroll = u16::MAX;
                    self.status_line = "Log tailing: on".to_string();
                } else {
                    self.status_line = "Log tailing: off".to_string();
                }
            } else {
                self.cycle_sort();
            }
        } else if self.keymap.is(Action::ToggleDesc, &key) {
            let tab = self.current_tab_mut();
            tab.descending = !tab.descending;
        } else if self.keymap.is(Action::CycleNamespace, &key) {
            self.cycle_namespace();
        } else if self.keymap.is(Action::ToggleHelp, &key) {
            self.show_help = !self.show_help;
        } else if self.keymap.is(Action::ToggleHelmSecrets, &key) {
            self.toggle_helm_secrets();
        } else if self.keymap.is(Action::ToTable, &key) {
            let tab = self.current_tab_mut();
            tab.pane = Pane::Table;
            tab.detail_scroll = 0;
            tab.detail_hscroll = 0;
            self.overlay = None;
        } else if self.keymap.is(Action::ToggleDescribe, &key) {
            self.toggle_describe();
        } else if self.keymap.is(Action::ToEvents, &key) {
            self.current_tab_mut().pane = Pane::Events;
            self.current_tab_mut().detail_scroll = 0;
            self.current_tab_mut().detail_hscroll = 0;
            self.overlay = None;
        } else if self.keymap.is(Action::ToLogs, &key) {
            self.current_tab_mut().pane = Pane::Logs;
            self.current_tab_mut().detail_scroll = 0;
            self.current_tab_mut().detail_hscroll = 0;
            self.current_tab_mut().detail_wrap = false;
            self.logs.auto_scroll = true;
            self.overlay = None;
            self.status_line =
                "Logs pane (Pods: all containers, RS/Deploy: all replica pods)".to_string();
        } else if self.keymap.is(Action::Delete, &key) {
            self.prepare_delete_confirmation();
        } else if self.keymap.is(Action::Confirm, &key) {
            self.confirm_action().await;
        } else if self.keymap.is(Action::Cancel, &key) {
            if self.pending_confirmation.is_some() {
                self.pending_confirmation = None;
                self.current_tab_mut().pane = Pane::Table;
                self.current_tab_mut().detail_scroll = 0;
                self.current_tab_mut().detail_hscroll = 0;
                self.overlay = None;
                self.status_line = "Action canceled".to_string();
            } else if self.current_tab().pane != Pane::Table {
                self.current_tab_mut().pane = Pane::Table;
                self.current_tab_mut().detail_scroll = 0;
                self.current_tab_mut().detail_hscroll = 0;
                self.overlay = None;
                self.status_line = "Closed view".to_string();
            } else if self.clear_active_filter_value() {
                self.status_line = self.filter_status_message("");
            } else if let Some(owner_kind) = self.current_tab().drill.as_ref().map(|d| d.owner_kind)
            {
                // Pop the drill-down back to the owner's list (krust has no view stack).
                self.set_active_kind(owner_kind);
                self.status_line = format!("Drill-down cleared → {owner_kind}");
            } else {
                self.status_line = "Nothing to cancel".to_string();
            }
        }

        self.ensure_active_watch().await;

        Ok(false)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.overlay.is_some() {
                    self.scroll_overlay_or_select(-3);
                } else if self.current_tab().pane == Pane::Table {
                    self.move_selection(-3);
                } else {
                    self.scroll_detail(-3);
                }
                true
            }
            MouseEventKind::ScrollDown => {
                if self.overlay.is_some() {
                    self.scroll_overlay_or_select(3);
                } else if self.current_tab().pane == Pane::Table {
                    self.move_selection(3);
                } else {
                    self.scroll_detail(3);
                }
                true
            }
            _ => false,
        }
    }

    fn current_view_text(&mut self) -> Option<String> {
        if let Some(overlay) = &self.overlay {
            return match overlay {
                Overlay::Text { lines, .. } => Some(lines.join("\n")),
                Overlay::Xray {
                    namespace,
                    collapsed,
                    ..
                } => {
                    let rows = self.xray_rows(namespace.as_deref(), collapsed);
                    Some(
                        rows.iter()
                            .map(|r| {
                                let status = r
                                    .status
                                    .as_deref()
                                    .map(|s| format!("  {s}"))
                                    .unwrap_or_default();
                                format!(
                                    "{}{} {}/{}{status}",
                                    r.connectors, r.marker, r.kind, r.name
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                Overlay::Triage { namespace, .. } => {
                    let (items, _) = self.triage_items(namespace.as_deref());
                    Some(
                        items
                            .iter()
                            .map(|i| {
                                format!(
                                    "{} {}/{}  {}  restarts:{}  ready:{}  {}",
                                    severity_tag(i.severity),
                                    i.namespace,
                                    i.name,
                                    i.reason,
                                    i.restarts,
                                    if i.ready { "yes" } else { "no" },
                                    i.age
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                Overlay::Contexts {
                    contexts,
                    selected,
                    filter,
                    ..
                } => {
                    let filtered = context_filtered_indices(contexts, filter);
                    let mut out = vec!["context".to_string()];
                    for idx in filtered {
                        let marker = if idx == *selected { "*" } else { " " };
                        out.push(format!("{marker} {}", contexts[idx]));
                    }
                    Some(out.join("\n"))
                }
                Overlay::Containers {
                    containers,
                    selected,
                    filter,
                    ..
                } => {
                    let filtered = list_filtered_indices(containers, filter);
                    let mut out = vec!["container".to_string()];
                    for idx in filtered {
                        let marker = if idx == *selected { "*" } else { " " };
                        out.push(format!("{marker} {}", containers[idx]));
                    }
                    Some(out.join("\n"))
                }
                Overlay::LogSources {
                    sources,
                    selected,
                    filter,
                    ..
                } => {
                    let filtered = list_filtered_indices(sources, filter);
                    let mut out = vec!["use\tsource".to_string()];
                    for idx in filtered {
                        let marker = if self.logs.hidden_sources.contains(&sources[idx]) {
                            "off"
                        } else {
                            "on"
                        };
                        let active = if idx == *selected { "*" } else { " " };
                        out.push(format!("{active}{marker}\t{}", sources[idx]));
                    }
                    Some(out.join("\n"))
                }
            };
        }

        let active = self.current_tab().clone();
        match active.pane {
            Pane::Table => {
                let request = self.view_request_for_tab(&active);
                let vm = self.projected_view(&request);
                let mut lines = Vec::with_capacity(vm.len().saturating_add(1));
                let mut header = String::from("Namespace\tName\tStatus\tAge");
                for col in active.kind().extra_columns() {
                    header.push('\t');
                    header.push_str(col.header);
                }
                lines.push(header);
                // Copy/dump materializes the full list (user-initiated, not a hot path).
                for key in &vm.order {
                    if let Some(row) = materialize_row(&self.store, key) {
                        let mut line = format!(
                            "{}\t{}\t{}\t{}",
                            row.namespace, row.name, row.status, row.age
                        );
                        for cell in &row.columns {
                            line.push('\t');
                            line.push_str(cell);
                        }
                        lines.push(line);
                    }
                }
                Some(lines.join("\n"))
            }
            Pane::Describe | Pane::SecretDecode | Pane::Events => {
                let request = self.view_request_for_tab(&active);
                let vm = self.projected_view(&request);
                let selected = active.selected.min(vm.len().saturating_sub(1));
                let key = vm.key(selected).cloned();
                let raw = self.detail_text(key.as_ref(), active.pane, active.detail_format);
                Some(raw)
            }
            Pane::Logs => {
                let text = if self.logs.hidden_sources.is_empty() {
                    self.log_joined_text().to_string()
                } else {
                    self.filtered_log_body_text()
                };
                if text.is_empty() {
                    Some(self.log_body_text())
                } else {
                    Some(text)
                }
            }
        }
    }

    fn copy_current_view_to_clipboard(&mut self) {
        let Some(raw) = self.current_view_text() else {
            self.status_line = "Nothing to copy".to_string();
            return;
        };
        if raw.is_empty() {
            self.status_line = "Nothing to copy".to_string();
            return;
        }

        let (payload, truncated) = truncate_for_clipboard(&raw, 1_000_000);
        match copy_to_native_clipboard(&payload) {
            Ok(method) => {
                self.status_line = if truncated {
                    format!(
                        "Copied to clipboard via {method} (truncated to {} bytes)",
                        payload.len()
                    )
                } else {
                    format!("Copied to clipboard via {method}")
                };
            }
            Err(native_err) => {
                let encoded = base64_encode(payload.as_bytes());
                let osc52 = format!("\u{1b}]52;c;{encoded}\u{7}");
                match io::stdout()
                    .write_all(osc52.as_bytes())
                    .and_then(|_| io::stdout().flush())
                {
                    Ok(_) => {
                        self.status_line = if truncated {
                            format!(
                                "Copied via OSC52 (native unavailable: {native_err}; truncated to {} bytes)",
                                payload.len()
                            )
                        } else {
                            format!("Copied via OSC52 (native unavailable: {native_err})")
                        };
                    }
                    Err(err) => {
                        self.status_line =
                            format!("Clipboard copy failed: native={native_err}; osc52={err}");
                    }
                }
            }
        };
    }

    fn dump_current_view(&mut self, args: &[&str]) {
        let Some(path_raw) = args.first() else {
            self.status_line = "Usage: :dump <path>".to_string();
            return;
        };
        let Some(text) = self.current_view_text() else {
            self.status_line = "Nothing to dump".to_string();
            return;
        };
        let path = expand_user_path(path_raw);
        match fs::write(&path, text) {
            Ok(_) => {
                self.status_line = format!("Dumped view to {}", path.display());
            }
            Err(err) => {
                self.status_line = format!("Dump failed: {}", err);
            }
        }
    }

    fn expire_confirmation(&mut self) -> bool {
        let Some(pending) = &self.pending_confirmation else {
            return false;
        };
        if pending.created_at.elapsed() >= pending.ttl {
            self.pending_confirmation = None;
            self.status_line = "Pending action expired".to_string();
            return true;
        }
        false
    }

    fn move_selection(&mut self, delta: isize) {
        let tab = self.current_tab_mut();
        if delta < 0 {
            tab.selected = tab.selected.saturating_sub(delta.unsigned_abs());
        } else {
            tab.selected = tab.selected.saturating_add(delta as usize);
        }
    }

    fn next_context(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.tabs.len().max(1);
        self.status_line = format!("Context: {}", self.current_tab().context);
    }

    fn prev_context(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.active_tab = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.status_line = format!("Context: {}", self.current_tab().context);
    }

    fn next_kind(&mut self) {
        let tab = self.current_tab_mut();
        tab.kind_idx = (tab.kind_idx + 1) % ResourceKind::ORDERED.len();
        if tab.kind() != ResourceKind::Namespaces {
            tab.last_non_namespace_kind_idx = tab.kind_idx;
        }
        tab.selected = 0;
        tab.detail_scroll = 0;
        tab.detail_hscroll = 0;
        tab.drill = None;
        tab.pane = Pane::Table;
        self.overlay = None;
    }

    fn prev_kind(&mut self) {
        let tab = self.current_tab_mut();
        tab.kind_idx = if tab.kind_idx == 0 {
            ResourceKind::ORDERED.len() - 1
        } else {
            tab.kind_idx - 1
        };
        if tab.kind() != ResourceKind::Namespaces {
            tab.last_non_namespace_kind_idx = tab.kind_idx;
        }
        tab.selected = 0;
        tab.detail_scroll = 0;
        tab.detail_hscroll = 0;
        tab.drill = None;
        tab.pane = Pane::Table;
        self.overlay = None;
    }

    fn cycle_namespace(&mut self) {
        let context = self.current_tab().context.clone();
        let mut namespaces = self.store.namespaces(&context);
        namespaces.sort();

        if namespaces.is_empty() {
            let tab = self.current_tab_mut();
            tab.namespace = None;
            tab.selected = 0;
            tab.drill = None;
            tab.pane = Pane::Table;
            self.status_line = "Namespace filter cleared".to_string();
            return;
        }

        let ns_label = {
            let tab = self.current_tab_mut();
            match tab.namespace.as_deref() {
                None => tab.namespace = Some(namespaces[0].clone()),
                Some(current) => {
                    let pos = namespaces
                        .iter()
                        .position(|ns| ns == current)
                        .map(|idx| idx + 1)
                        .unwrap_or(0);
                    if pos >= namespaces.len() {
                        tab.namespace = None;
                    } else {
                        tab.namespace = Some(namespaces[pos].clone());
                    }
                }
            }
            tab.selected = 0;
            tab.drill = None;
            tab.pane = Pane::Table;
            tab.namespace.clone()
        };

        self.status_line = format!("Namespace filter: {}", ns_label.as_deref().unwrap_or("all"));
    }

    fn cycle_sort(&mut self) {
        let tab = self.current_tab_mut();
        // Advance to the next column in left-to-right table order (not an arbitrary jump).
        tab.sort = match tab.sort {
            SortColumn::Namespace => SortColumn::Name,
            SortColumn::Name => SortColumn::Status,
            SortColumn::Status => SortColumn::Age,
            SortColumn::Age => SortColumn::Namespace,
        };
    }

    fn toggle_helm_secrets(&mut self) {
        let tab = self.current_tab_mut();
        tab.show_helm_secrets = !tab.show_helm_secrets;
        // Selection offsets can point past the now-shorter/longer list; reset to the top.
        tab.selected = 0;
        tab.table_offset = 0;
        let showing = tab.show_helm_secrets;
        let on_secrets = tab.kind() == ResourceKind::Secrets;
        self.status_line = match (on_secrets, showing) {
            (true, true) => "Helm release secrets: shown".to_string(),
            (true, false) => "Helm release secrets: hidden".to_string(),
            (false, true) => "Helm release secrets: shown (applies to Secrets view)".to_string(),
            (false, false) => "Helm release secrets: hidden (applies to Secrets view)".to_string(),
        };
    }

    fn prepare_delete_confirmation(&mut self) {
        let active = self.current_tab().clone();
        let selected = active.selected;
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let Some(key) = vm.key(selected.min(vm.len().saturating_sub(1))).cloned() else {
            self.status_line = "No resource selected".to_string();
            return;
        };

        let name = key.name.clone();
        self.pending_confirmation = Some(PendingConfirmation {
            created_at: Instant::now(),
            ttl: Duration::from_secs(15),
            kind: ConfirmationKind::Delete(key),
        });
        self.status_line = format!("Delete {name}? press y to confirm");
    }

    async fn confirm_action(&mut self) {
        let Some(pending) = self.pending_confirmation.clone() else {
            return;
        };

        match pending.kind {
            ConfirmationKind::Delete(key) => {
                let result = self.action_executor.delete_resource(&key).await;
                self.pending_confirmation = None;
                self.status_line = match result {
                    Ok(outcome) => outcome.message,
                    Err(error) => render_action_error(error, &key),
                };
            }
        }
    }

    fn current_tab(&self) -> &ContextTabState {
        &self.tabs[self.active_tab]
    }

    fn current_tab_mut(&mut self) -> &mut ContextTabState {
        &mut self.tabs[self.active_tab]
    }

    fn set_active_kind(&mut self, kind: ResourceKind) {
        if let Some(idx) = ResourceKind::ORDERED.iter().position(|k| *k == kind) {
            let kind_label = {
                let tab = self.current_tab_mut();
                tab.kind_idx = idx;
                if kind != ResourceKind::Namespaces {
                    tab.last_non_namespace_kind_idx = idx;
                }
                tab.selected = 0;
                tab.detail_scroll = 0;
                tab.detail_hscroll = 0;
                tab.drill = None;
                tab.pane = Pane::Table;
                tab.kind().to_string()
            };
            self.overlay = None;
            self.status_line = format!("Kind: {kind_label}");
        }
    }

    fn active_watch_targets(tab: &ContextTabState) -> Vec<WatchTarget> {
        let mut set = HashSet::new();
        set.insert(WatchTarget {
            kind: ResourceKind::Namespaces,
            namespace: None,
        });
        set.insert(WatchTarget {
            kind: tab.kind(),
            namespace: if tab.kind().is_namespaced() {
                tab.namespace.clone()
            } else {
                None
            },
        });

        if tab.pane == Pane::Logs {
            match tab.kind() {
                ResourceKind::Deployments => {
                    set.insert(WatchTarget {
                        kind: ResourceKind::ReplicaSets,
                        namespace: tab.namespace.clone(),
                    });
                    set.insert(WatchTarget {
                        kind: ResourceKind::Pods,
                        namespace: tab.namespace.clone(),
                    });
                }
                ResourceKind::ReplicaSets | ResourceKind::Pods => {
                    set.insert(WatchTarget {
                        kind: ResourceKind::Pods,
                        namespace: tab.namespace.clone(),
                    });
                }
                _ => {}
            }
        }
        if tab.pane == Pane::Events {
            set.insert(WatchTarget {
                kind: ResourceKind::Events,
                namespace: tab.namespace.clone(),
            });
        }

        // A Deployment drill-down resolves pods transitively through their ReplicaSet, so the RS
        // for the drilled namespace must be watched too (otherwise the chain finds nothing).
        if tab.kind() == ResourceKind::Pods
            && let Some(drill) = &tab.drill
            && drill.owner_kind == ResourceKind::Deployments
        {
            set.insert(WatchTarget {
                kind: ResourceKind::ReplicaSets,
                namespace: tab.namespace.clone(),
            });
        }

        let mut out: Vec<WatchTarget> = set.into_iter().collect();
        out.sort_by(|a, b| {
            a.kind
                .short_name()
                .cmp(b.kind.short_name())
                .then_with(|| a.namespace.cmp(&b.namespace))
        });
        out
    }

    async fn ensure_active_watch(&mut self) {
        let tab = self.current_tab().clone();
        let mut targets = Self::active_watch_targets(&tab);
        // An open xray graph spans the whole ownership forest, so watch every kind it draws,
        // scoped to its namespace (None = whole cluster).
        if let Some(Overlay::Xray { namespace, .. }) = &self.overlay {
            for kind in XRAY_WATCH_KINDS {
                let target = WatchTarget {
                    kind,
                    namespace: namespace.clone(),
                };
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        // The triage board scans pods for problems, so it needs pods watched in its scope.
        if let Some(Overlay::Triage { namespace, .. }) = &self.overlay {
            let target = WatchTarget {
                kind: ResourceKind::Pods,
                namespace: namespace.clone(),
            };
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        if let Err(err) = self
            .resource_provider
            .replace_watch_plan(&tab.context, &targets)
            .await
        {
            self.status_line = format!("watch setup error: {err}");
        }
        self.maybe_load_detail().await;
        self.maybe_load_events().await;
        self.maybe_refresh_metrics().await;
    }

    /// Fetch events correlated to the selected resource for the Events pane (Phase 4.3), unless the
    /// selection is itself an Event (that case shows the Event's own manifest via detail_cache).
    async fn maybe_load_events(&mut self) {
        if self.current_tab().pane != Pane::Events {
            return;
        }
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.key.kind == ResourceKind::Events {
            return;
        }
        if self.events_cache.as_ref().map(|(k, _)| k) == Some(&row.key) {
            return;
        }
        let provider = self.resource_provider.clone();
        let key = row.key.clone();
        let rows = provider
            .events_for(&key.context, key.namespace.as_deref(), &key.name)
            .await
            .unwrap_or_default();
        self.events_cache = Some((key, rows));
    }

    /// Render correlated events for a resource (kubectl-describe-style), from `events_cache`.
    fn format_correlated_events(&self, key: &ResourceKey) -> String {
        let kind = key.kind.short_name();
        let Some((cached_key, rows)) = &self.events_cache else {
            return format!("Loading events for {kind} {} …", key.name);
        };
        if cached_key != key {
            return format!("Loading events for {kind} {} …", key.name);
        }
        if rows.is_empty() {
            return format!("No events for {kind} {}", key.name);
        }
        let mut out = vec![
            format!("Events for {kind} {} ({})", key.name, rows.len()),
            String::new(),
            format!(
                "{:<8}  {:<8}  {:<26}  {:>5}  {:<10}  Message",
                "Last", "Type", "Reason", "Count", "Source"
            ),
        ];
        for event in rows {
            out.push(format!(
                "{:<8}  {:<8}  {:<26}  {:>5}  {:<10}  {}",
                short_age(event.last),
                event.type_,
                event.reason,
                event.count,
                event.source,
                event.message,
            ));
        }
        out.join("\n")
    }

    /// Refresh live usage (metrics.k8s.io) for the active context+namespace, at most every 15s:
    /// cluster totals (node metrics) for the pulse and per-pod usage for the table columns. On any
    /// error/absence (no metrics-server) the caches clear and the UI degrades — never blocks.
    async fn maybe_refresh_metrics(&mut self) {
        let context = self.current_tab().context.clone();
        let namespace = self.current_tab().namespace.clone();
        let stale = match &self.metrics_fetched {
            Some((ctx, ns, at)) => {
                ctx != &context || ns != &namespace || at.elapsed() > Duration::from_secs(15)
            }
            None => true,
        };
        if !stale {
            return;
        }
        let provider = self.resource_provider.clone();

        self.metrics = match provider.node_metrics(&context).await {
            Ok(nodes) if !nodes.is_empty() => Some(ClusterMetrics {
                cpu_used_m: nodes.iter().map(|n| n.cpu_used_m).sum(),
                mem_used_b: nodes.iter().map(|n| n.mem_used_b).sum(),
                nodes_reporting: nodes.len(),
            }),
            _ => None,
        };

        self.pod_metrics = match provider.pod_metrics(&context, namespace.as_deref()).await {
            Ok(pods) => pods
                .into_iter()
                .map(|p| ((p.namespace, p.name), (p.cpu_used_m, p.mem_used_b)))
                .collect(),
            Err(_) => HashMap::new(),
        };

        self.metrics_fetched = Some((context, namespace, Instant::now()));
    }

    /// Live usage `(cpu_millicores, mem_bytes)` for a pod, if metrics are available.
    fn pod_usage(&self, namespace: Option<&str>, name: &str) -> Option<(u64, u64)> {
        self.pod_metrics
            .get(&(namespace.unwrap_or_default().to_string(), name.to_string()))
            .copied()
    }

    /// Fetch the full object for the selected row on demand when a detail pane is active and the
    /// cache does not already hold it. Single-slot; selection is stable while scrolling a detail
    /// pane, so this fires only on pane entry or selection change.
    async fn maybe_load_detail(&mut self) {
        let pane = self.current_tab().pane;
        if !matches!(pane, Pane::Describe | Pane::SecretDecode | Pane::Events) {
            return;
        }
        let Some(row) = self.selected_row() else {
            return;
        };
        // The Events pane on a non-Event resource shows correlated events (maybe_load_events),
        // not the resource's own manifest — skip the on-demand object fetch there.
        if pane == Pane::Events && row.key.kind != ResourceKind::Events {
            return;
        }
        if self.detail_cache.as_ref().map(|d| &d.key) == Some(&row.key) {
            return;
        }
        let provider = self.resource_provider.clone();
        let key = row.key.clone();
        self.detail_cache = Some(match provider.get_object(&key).await {
            Ok(value) => DetailObject {
                key,
                value,
                error: None,
            },
            Err(err) => DetailObject {
                key,
                value: serde_json::Value::Null,
                error: Some(err.to_string()),
            },
        });
    }
}

fn render_action_error(error: ActionError, key: &ResourceKey) -> String {
    match error {
        ActionError::ReadOnly => "Read-only mode enabled; action blocked".to_string(),
        ActionError::PermissionDenied(message) => format!(
            "RBAC denied for {} {}: {}",
            key.kind.short_name(),
            key.name,
            message
        ),
        ActionError::Unsupported(message) => message,
        ActionError::Failed(message) => message,
    }
}

fn common_prefix(values: &[String]) -> String {
    let Some(first) = values.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for value in values.iter().skip(1) {
        let mut shared = String::new();
        for (a, b) in prefix.chars().zip(value.chars()) {
            if a == b {
                shared.push(a);
            } else {
                break;
            }
        }
        prefix = shared;
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

fn expand_user_path(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(raw)
}

fn run_clipboard_command(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} exited with status {status}"
        )))
    }
}

fn copy_to_native_clipboard(text: &str) -> Result<&'static str, String> {
    #[cfg(target_os = "macos")]
    {
        run_clipboard_command("pbcopy", &[], text)
            .map(|_| "pbcopy")
            .map_err(|err| err.to_string())
    }

    #[cfg(target_os = "linux")]
    {
        let mut errors = Vec::new();
        if env::var_os("WAYLAND_DISPLAY").is_some() {
            match run_clipboard_command("wl-copy", &[], text) {
                Ok(()) => return Ok("wl-copy"),
                Err(err) => errors.push(format!("wl-copy: {err}")),
            }
        }
        match run_clipboard_command("xclip", &["-selection", "clipboard"], text) {
            Ok(()) => return Ok("xclip"),
            Err(err) => errors.push(format!("xclip: {err}")),
        }
        match run_clipboard_command("xsel", &["--clipboard", "--input"], text) {
            Ok(()) => return Ok("xsel"),
            Err(err) => errors.push(format!("xsel: {err}")),
        }
        return Err(errors.join("; "));
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = text;
        Err("native clipboard backend is not configured for this platform".to_string())
    }
}

fn truncate_for_clipboard(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (text[..end].to_string(), true)
}

fn run_external_editor(initial_text: &str, extension: &str) -> Result<Option<String>, String> {
    let editor = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut path = env::temp_dir();
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = format!("krust-edit-{}-{now_nanos}.{extension}", std::process::id());
    path.push(nonce);

    fs::write(&path, initial_text).map_err(|err| err.to_string())?;

    if let Err(err) = suspend_tui_for_editor() {
        let _ = fs::remove_file(&path);
        return Err(format!("failed to suspend terminal for editor: {err}"));
    }

    let status = Command::new(&editor).arg(&path).status();

    let restore_result = resume_tui_after_editor();
    drain_pending_input_events();

    let result = match status {
        Ok(status) if status.success() => fs::read_to_string(&path)
            .map(Some)
            .map_err(|err| err.to_string()),
        Ok(status) => Err(format!("editor exited with status {status}")),
        Err(err) => Err(err.to_string()),
    };

    let _ = fs::remove_file(&path);
    restore_result?;
    result
}

fn suspend_tui_for_editor() -> Result<(), String> {
    disable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, DisableMouseCapture, LeaveAlternateScreen).map_err(|err| err.to_string())
}

fn resume_tui_after_editor() -> Result<(), String> {
    enable_raw_mode().map_err(|err| err.to_string())?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        Clear(ClearType::All),
        MoveTo(0, 0)
    )
    .map_err(|err| err.to_string())?;
    stdout.flush().map_err(|err| err.to_string())
}

fn drain_pending_input_events() {
    while event::poll(Duration::from_millis(0)).unwrap_or(false) {
        let _ = event::read();
    }
}

fn list_filtered_indices(values: &[String], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return (0..values.len()).collect();
    }
    values
        .iter()
        .enumerate()
        .filter_map(|(idx, value)| {
            if value.to_ascii_lowercase().contains(&needle) {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

fn slice_chars(text: &str, start: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    text.chars().skip(start).take(width).collect()
}

fn is_retryable_log_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if lower.contains("forbidden")
        || lower.contains("insufficient rbac")
        || lower.contains("code: 403")
    {
        return false;
    }
    true
}

fn is_auth_refresh_log_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("code: 401")
        || lower.contains("token")
        || lower.contains("expired")
}

fn next_log_reconnect_backoff_ms(attempt: u32, last_error: Option<&str>) -> u64 {
    let base_ms = if last_error.is_some_and(is_auth_refresh_log_error) {
        1_000u64
    } else {
        500u64
    };
    base_ms.saturating_mul(1u64 << attempt.min(5)).min(30_000)
}

fn highlighted_structured_text(
    text: &str,
    query: &str,
    format: DetailFormat,
    support: ColorSupport,
    active_line: Option<usize>,
) -> Text<'static> {
    match format {
        DetailFormat::Yaml => highlighted_yaml_text(text, query, support, active_line),
        DetailFormat::Json => highlighted_json_text(text, query, support, active_line),
    }
}

fn format_signed_millicpu(delta: i64) -> String {
    if delta == 0 {
        return "0m".to_string();
    }
    let sign = if delta > 0 { "+" } else { "-" };
    format!("{sign}{}", format_millicpu(delta.unsigned_abs()))
}

fn format_signed_bytes(delta: i64) -> String {
    if delta == 0 {
        return "0B".to_string();
    }
    let sign = if delta > 0 { "+" } else { "-" };
    format!("{sign}{}", format_bytes(delta.unsigned_abs()))
}

fn pane_icon(pane: Pane) -> &'static str {
    match pane {
        Pane::Table => "[TB]",
        Pane::Describe => "[DS]",
        Pane::SecretDecode => "[SX]",
        Pane::Events => "[EV]",
        Pane::Logs => "[LG]",
    }
}

fn logs_state_icon(logs: &LogViewState) -> &'static str {
    if logs.paused {
        "||"
    } else if logs.session.is_some() {
        ">>>"
    } else if logs.reconnect_blocked {
        "!!"
    } else if logs.stream_closed {
        "--"
    } else {
        ".."
    }
}

fn health_icon(failed: usize, pending: usize) -> &'static str {
    if failed > 0 {
        "[XX]"
    } else if pending > 0 {
        "[!!]"
    } else {
        "[OK]"
    }
}

fn context_filtered_indices(contexts: &[String], filter: &str) -> Vec<usize> {
    list_filtered_indices(contexts, filter)
}

/// Kinds the xray ownership forest draws — watched while an xray overlay is open so it populates.
const XRAY_WATCH_KINDS: [ResourceKind; 7] = [
    ResourceKind::Deployments,
    ResourceKind::ReplicaSets,
    ResourceKind::StatefulSets,
    ResourceKind::DaemonSets,
    ResourceKind::CronJobs,
    ResourceKind::Jobs,
    ResourceKind::Pods,
];

fn format_millicpu(value: u64) -> String {
    if value >= 1000 {
        let cores = value as f64 / 1000.0;
        format!("{cores:.2}c")
    } else {
        format!("{value}m")
    }
}

fn format_bytes(value: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let v = value as f64;
    if v >= GIB {
        format!("{:.1}Gi", v / GIB)
    } else if v >= MIB {
        format!("{:.0}Mi", v / MIB)
    } else {
        format!("{value}B")
    }
}

fn percent(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        return "n/a".to_string();
    }
    let pct = (numerator as f64 / denominator as f64) * 100.0;
    format!("{pct:.0}%")
}

/// A pod's per-resource usage broken into three independently-rendered, right-aligned columns:
/// actual `used`, usage as a percentage of the request (`req_pct`), and of the limit (`lim_pct`).
/// Each percentage carries its own severity: `lim` turns red at ≥90% (throttle/OOM risk), `req`
/// turns yellow at ≥100% (under-requested); a low `req_pct` flags over-provisioning. Missing
/// pieces render as `-`.
struct UsageCells {
    used: String,
    req_pct: String,
    lim_pct: String,
    req_sev: Severity,
    lim_sev: Severity,
}

fn pod_usage_cells(
    used: Option<u64>,
    request: u64,
    limit: u64,
    render: fn(u64) -> String,
) -> UsageCells {
    let Some(used) = used else {
        return UsageCells {
            used: "-".to_string(),
            req_pct: "-".to_string(),
            lim_pct: "-".to_string(),
            req_sev: Severity::Ok,
            lim_sev: Severity::Ok,
        };
    };
    let pct = |denom: u64| (denom > 0).then(|| used.saturating_mul(100) / denom);
    let req_pct = pct(request);
    let lim_pct = pct(limit);
    UsageCells {
        used: render(used),
        req_pct: req_pct.map_or_else(|| "-".to_string(), |v| format!("{v}%")),
        lim_pct: lim_pct.map_or_else(|| "-".to_string(), |v| format!("{v}%")),
        req_sev: if req_pct.is_some_and(|v| v >= 100) {
            Severity::Warn
        } else {
            Severity::Ok
        },
        lim_sev: if lim_pct.is_some_and(|v| v >= 90) {
            Severity::Err
        } else {
            Severity::Ok
        },
    }
}

/// Compact age (e.g. "3d", "5h", "12m") for dynamic-resource rows.
fn short_age(when: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let Some(when) = when else {
        return "-".to_string();
    };
    let delta = chrono::Utc::now().signed_duration_since(when);
    if delta.num_days() > 0 {
        format!("{}d", delta.num_days())
    } else if delta.num_hours() > 0 {
        format!("{}h", delta.num_hours())
    } else if delta.num_minutes() > 0 {
        format!("{}m", delta.num_minutes())
    } else {
        format!("{}s", delta.num_seconds().max(0))
    }
}

fn log_target_name(target: &LogTarget) -> String {
    if let Some(container) = &target.container {
        format!("{}/{}/{}", target.namespace, target.pod, container)
    } else {
        format!("{}/{}", target.namespace, target.pod)
    }
}

fn parse_log_source(line: &str) -> Option<&str> {
    if !line.starts_with('[') {
        return None;
    }
    let end = line.find(']')?;
    if end <= 1 {
        return None;
    }
    Some(&line[1..end])
}

fn is_visible_log_line(line: &str, hidden_sources: &HashSet<String>) -> bool {
    if hidden_sources.is_empty() {
        return true;
    }
    let Some(source) = parse_log_source(line) else {
        return true;
    };
    !hidden_sources.contains(source)
}

fn normalize_log_targets(mut targets: Vec<LogTarget>) -> Vec<LogTarget> {
    targets.sort_by(|a, b| {
        a.namespace
            .cmp(&b.namespace)
            .then_with(|| a.pod.cmp(&b.pod))
            .then_with(|| a.container.cmp(&b.container))
    });
    targets.dedup();
    targets
}

fn delete_previous_word(value: &mut String) {
    while value.ends_with(char::is_whitespace) {
        value.pop();
    }
    while value.ends_with(|c: char| !c.is_whitespace()) {
        value.pop();
    }
}

fn table_viewport_rows(table_height: u16) -> usize {
    table_height.saturating_sub(3) as usize
}

fn sync_table_viewport(
    selected: usize,
    offset: usize,
    viewport_rows: usize,
    total_rows: usize,
) -> (usize, usize) {
    if total_rows == 0 || viewport_rows == 0 {
        return (0, 0);
    }

    let selected = selected.min(total_rows.saturating_sub(1));
    let max_offset = total_rows.saturating_sub(viewport_rows);
    let mut offset = offset.min(max_offset);

    if selected < offset {
        offset = selected;
    } else {
        let visible_end_exclusive = offset.saturating_add(viewport_rows);
        if selected >= visible_end_exclusive {
            offset = selected.saturating_add(1).saturating_sub(viewport_rows);
        }
    }

    (selected, offset.min(max_offset))
}

fn max_vertical_scroll_for_text(
    text: &str,
    viewport_width: u16,
    viewport_height: u16,
    wrap: bool,
) -> u16 {
    let viewport_h = viewport_height as usize;
    if viewport_h == 0 {
        return 0;
    }

    let line_count = if wrap {
        let width = (viewport_width.max(1)) as usize;
        text.lines()
            .map(|line| {
                let len = line.chars().count();
                len.max(1).div_ceil(width)
            })
            .sum::<usize>()
    } else {
        text.lines().count().max(1)
    };

    line_count.saturating_sub(viewport_h) as u16
}

fn max_horizontal_scroll_for_text(text: &str, viewport_width: u16, wrap: bool) -> u16 {
    if wrap {
        return 0;
    }
    let viewport_w = viewport_width as usize;
    if viewport_w == 0 {
        return 0;
    }
    let max_line = text
        .lines()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    max_line.saturating_sub(viewport_w) as u16
}

fn compact_context_name(context: &str) -> String {
    if context.starts_with("arn:") {
        return context
            .rsplit('/')
            .next()
            .map(str::to_string)
            .unwrap_or_else(|| context.to_string());
    }
    context.to_string()
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    }
}

#[cfg(test)]
impl App {
    /// Inject the on-demand detail object directly (tests have no live provider to fetch from).
    fn seed_detail_cache_for_test(&mut self, key: ResourceKey, value: serde_json::Value) {
        self.detail_cache = Some(DetailObject {
            key,
            value,
            error: None,
        });
    }

    /// Inject per-pod usage directly (tests don't run the async metrics refresh).
    fn seed_pod_metrics_for_test(&mut self, namespace: &str, name: &str, cpu_m: u64, mem_b: u64) {
        self.pod_metrics
            .insert((namespace.to_string(), name.to_string()), (cpu_m, mem_b));
    }

    /// Inject correlated events directly (tests don't run the async events fetch).
    fn seed_events_for_test(&mut self, key: ResourceKey, rows: Vec<crate::cluster::EventRow>) {
        self.events_cache = Some((key, rows));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap, HashSet, VecDeque},
        sync::Arc,
        time::Instant,
    };

    use async_trait::async_trait;
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend, style::Color};
    use tokio::sync::mpsc;

    use super::{
        App, ColorSupport, ResourceAlias, Severity, base64_decode, base64_encode,
        classify_status_severity, color_support_label, command_names, decoded_secret_text,
        detect_color_support_from_env, highlighted_json_text, highlighted_text,
        highlighted_yaml_text, is_auth_refresh_log_error, is_retryable_log_error,
        is_visible_log_line, json_spans_for_line, next_log_reconnect_backoff_ms, parse_log_source,
        parse_resource_alias, resolved_active_match_line, resource_alias_names,
        search_match_lines_in_logs, severity_tag, slice_chars, step_match_line,
        sync_table_viewport, table_viewport_rows, truncate_for_clipboard, ui_theme_for,
        yaml_spans_for_line,
    };
    use crate::{
        cluster::{
            ActionError, ActionExecutor, ActionResult, PodLogRequest, PodLogStream,
            ResourceProvider, WatchTarget,
        },
        keymap::Keymap,
        model::{Pane, ResourceEntity, ResourceKey, ResourceKind, SortColumn, StateDelta},
    };

    struct NoopProvider {
        contexts: Vec<String>,
    }

    #[async_trait]
    impl ResourceProvider for NoopProvider {
        fn context_names(&self) -> &[String] {
            &self.contexts
        }

        fn default_context(&self) -> Option<&str> {
            self.contexts.first().map(String::as_str)
        }

        async fn start(&self, _tx: mpsc::Sender<StateDelta>) -> anyhow::Result<()> {
            Ok(())
        }

        async fn replace_watch_plan(
            &self,
            _context: &str,
            _targets: &[WatchTarget],
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn stream_pod_logs(&self, _request: PodLogRequest) -> anyhow::Result<PodLogStream> {
            anyhow::bail!("noop log stream provider")
        }

        async fn get_object(&self, _key: &ResourceKey) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }

        async fn discover(
            &self,
            _context: &str,
        ) -> anyhow::Result<Vec<crate::cluster::DiscoveredResource>> {
            let mut ar = kube::core::ApiResource::from_gvk(&kube::core::GroupVersionKind::gvk(
                "demo.krust.io",
                "v1",
                "Widget",
            ));
            ar.plural = "widgets".to_string();
            Ok(vec![crate::cluster::DiscoveredResource {
                api_resource: ar,
                namespaced: true,
            }])
        }

        async fn list_dynamic(
            &self,
            _context: &str,
            _resource: &crate::cluster::DiscoveredResource,
            _namespace: Option<&str>,
        ) -> anyhow::Result<Vec<crate::cluster::DynamicRow>> {
            Ok(vec![
                crate::cluster::DynamicRow {
                    namespace: Some("ns-00".to_string()),
                    name: "widget-0".to_string(),
                    age: None,
                    status: "-".to_string(),
                },
                crate::cluster::DynamicRow {
                    namespace: Some("ns-00".to_string()),
                    name: "widget-1".to_string(),
                    age: None,
                    status: "-".to_string(),
                },
            ])
        }

        async fn get_dynamic(
            &self,
            _context: &str,
            _resource: &crate::cluster::DiscoveredResource,
            _namespace: Option<&str>,
            name: &str,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({
                "apiVersion": "demo.krust.io/v1",
                "kind": "Widget",
                "metadata": { "name": name, "namespace": "ns-00" }
            }))
        }

        async fn node_metrics(
            &self,
            _context: &str,
        ) -> anyhow::Result<Vec<crate::cluster::NodeUsage>> {
            Ok(Vec::new())
        }

        async fn pod_metrics(
            &self,
            _context: &str,
            _namespace: Option<&str>,
        ) -> anyhow::Result<Vec<crate::cluster::PodUsage>> {
            Ok(vec![crate::cluster::PodUsage {
                namespace: "default".to_string(),
                name: "pod-ok".to_string(),
                cpu_used_m: 250,
                mem_used_b: 64 * 1024 * 1024,
            }])
        }

        async fn events_for(
            &self,
            _context: &str,
            _namespace: Option<&str>,
            _name: &str,
        ) -> anyhow::Result<Vec<crate::cluster::EventRow>> {
            Ok(vec![crate::cluster::EventRow {
                type_: "Warning".to_string(),
                reason: "BackOff".to_string(),
                message: "Back-off restarting failed container".to_string(),
                count: 5,
                last: Some(chrono::Utc::now()),
                source: "kubelet".to_string(),
            }])
        }
    }

    struct NoopExecutor;

    #[async_trait]
    impl ActionExecutor for NoopExecutor {
        async fn delete_resource(&self, _key: &ResourceKey) -> Result<ActionResult, ActionError> {
            Err(ActionError::Unsupported("noop".to_string()))
        }

        async fn replace_resource(
            &self,
            _key: &ResourceKey,
            _manifest: serde_json::Value,
        ) -> Result<ActionResult, ActionError> {
            Err(ActionError::Unsupported("noop".to_string()))
        }
    }

    fn mk_entity(
        context: &str,
        kind: ResourceKind,
        namespace: Option<&str>,
        name: &str,
        status: &str,
    ) -> ResourceEntity {
        ResourceEntity {
            key: ResourceKey::new(context, kind, namespace.map(str::to_string), name),
            status: status.to_string(),
            age: Some(Utc::now()),
            labels: vec![("app".to_string(), "demo".to_string())],
            // Match the kind's column count so header/cell widths line up in render tests.
            columns: kind
                .extra_columns()
                .iter()
                .map(|_| "-".to_string())
                .collect(),
            extracted: Default::default(),
        }
    }

    fn test_app() -> App {
        let contexts = vec!["ctx-dev".to_string()];
        App::new(
            contexts,
            "ctx-dev".to_string(),
            Some("default".to_string()),
            HashMap::new(),
            Arc::new(NoopExecutor),
            Arc::new(NoopProvider {
                contexts: vec!["ctx-dev".to_string()],
            }),
            Keymap::default(),
            false,
            true,
        )
    }

    fn render_snapshot(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let frame = terminal.draw(|f| app.draw(f)).expect("draw");
        let mut out = Vec::new();
        for y in 0..frame.area.height {
            let mut line = String::new();
            for x in 0..frame.area.width {
                line.push_str(frame.buffer[(x, y)].symbol());
            }
            out.push(line.trim_end().to_string());
        }
        out.join("\n")
    }

    #[test]
    fn parses_supported_k9s_aliases() {
        assert!(matches!(
            parse_resource_alias("po"),
            ResourceAlias::Supported(ResourceKind::Pods)
        ));
        assert!(matches!(
            parse_resource_alias("svc"),
            ResourceAlias::Supported(ResourceKind::Services)
        ));
        assert!(matches!(
            parse_resource_alias("np"),
            ResourceAlias::Supported(ResourceKind::NetworkPolicies)
        ));
        assert!(matches!(
            parse_resource_alias("crb"),
            ResourceAlias::Supported(ResourceKind::ClusterRoleBindings)
        ));
    }

    #[test]
    fn parses_recognized_unimplemented_aliases() {
        assert!(matches!(
            parse_resource_alias("crd"),
            ResourceAlias::Unsupported(_)
        ));
        assert!(matches!(
            parse_resource_alias("endpoints"),
            ResourceAlias::Unsupported(_)
        ));
        assert!(matches!(
            parse_resource_alias("storageclasses"),
            ResourceAlias::Unsupported(_)
        ));
    }

    #[test]
    fn resource_alias_catalog_stays_supported() {
        for alias in resource_alias_names() {
            assert!(
                matches!(parse_resource_alias(alias), ResourceAlias::Supported(_)),
                "resource alias '{alias}' is listed for completion but is not supported"
            );
        }
    }

    #[test]
    fn command_catalog_covers_explicit_builtin_commands() {
        let known: HashSet<&str> = command_names().iter().copied().collect();
        let expected = [
            "q",
            "quit",
            "exit",
            "help",
            "?",
            "contexts",
            "ctxs",
            "ctx",
            "context",
            "ns",
            "namespace",
            "all",
            "0",
            "kind",
            "fmt",
            "format",
            "yaml",
            "yml",
            "json",
            "resources",
            "res",
            "aliases",
            "api",
            "apis",
            "clear",
            "clear-filter",
            "c",
            "container",
            "containers",
            "sources",
            "src",
            "pause",
            "resume",
            "edit",
            "tail",
            "copy",
            "yank",
            "dump",
            "pulse",
            "pulses",
            "pu",
            "xray",
            "popeye",
            "pop",
            "plugins",
            "plugin",
            "screendump",
            "sd",
        ];
        for cmd in expected {
            assert!(
                known.contains(cmd),
                "command '{cmd}' is handled by parser but missing from completion catalog"
            );
        }
    }

    #[test]
    fn command_catalog_has_no_duplicates() {
        let mut seen = HashSet::new();
        for cmd in command_names() {
            assert!(seen.insert(*cmd), "duplicate command '{cmd}'");
        }
    }

    #[test]
    fn classifies_non_retryable_rbac_log_errors() {
        assert!(!is_retryable_log_error(
            "code: 403 Forbidden: cannot get pods"
        ));
        assert!(!is_retryable_log_error("forbidden: insufficient RBAC"));
        assert!(is_retryable_log_error("connection refused"));
        assert!(is_retryable_log_error(
            "Unauthorized token refresh in progress"
        ));
    }

    #[test]
    fn classifies_auth_refresh_errors() {
        assert!(is_auth_refresh_log_error("Unauthorized"));
        assert!(is_auth_refresh_log_error("code: 401"));
        assert!(is_auth_refresh_log_error("token expired"));
        assert!(!is_auth_refresh_log_error("forbidden"));
    }

    #[test]
    fn reconnect_backoff_is_higher_for_auth_refresh_failures() {
        let normal = next_log_reconnect_backoff_ms(1, Some("connection reset by peer"));
        let auth = next_log_reconnect_backoff_ms(1, Some("Unauthorized token expired"));
        assert!(auth > normal);
    }

    #[test]
    fn reconnect_backoff_is_capped() {
        let non_auth = next_log_reconnect_backoff_ms(99, Some("connection reset by peer"));
        let auth = next_log_reconnect_backoff_ms(99, Some("token expired"));
        assert_eq!(non_auth, 16_000);
        assert_eq!(auth, 30_000);
    }

    #[test]
    fn parses_log_source_prefix() {
        assert_eq!(
            parse_log_source("[ns/pod/container] hello world"),
            Some("ns/pod/container")
        );
        assert_eq!(parse_log_source("plain line"), None);
    }

    #[test]
    fn hides_and_shows_log_lines_by_source() {
        let mut hidden = HashSet::new();
        hidden.insert("ns-a/pod-a/c1".to_string());

        assert!(!is_visible_log_line("[ns-a/pod-a/c1] hidden", &hidden));
        assert!(is_visible_log_line("[ns-b/pod-b/c1] visible", &hidden));
        assert!(is_visible_log_line("no prefix line", &hidden));
    }

    #[test]
    fn json_syntax_highlighter_marks_key_number_and_bool_tokens() {
        let spans = json_spans_for_line(r#"  "cpu": 123, "ready": true"#, ColorSupport::Basic);
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "\"cpu\"" && style.fg == Some(Color::Cyan))
        );
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "123" && style.fg == Some(Color::Magenta))
        );
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "true" && style.fg == Some(Color::Yellow))
        );
    }

    #[test]
    fn json_syntax_highlighter_keeps_search_highlight() {
        let rendered = highlighted_json_text(
            "{\n  \"name\": \"demo\"\n}",
            "demo",
            ColorSupport::Basic,
            Some(1),
        );
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.spans.iter().any(|span| {
                    span.content.contains("demo")
                        && (span.style.bg == Some(Color::Yellow)
                            || span.style.bg == Some(Color::Cyan))
                }))
        );
    }

    #[test]
    fn yaml_syntax_highlighter_marks_key_number_bool_and_comment_tokens() {
        let spans = yaml_spans_for_line("cpu: 123\nenabled: true # ok", ColorSupport::Basic);
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "cpu" && style.fg == Some(Color::Cyan))
        );
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "123" && style.fg == Some(Color::Magenta))
        );
        assert!(
            spans
                .iter()
                .any(|(text, style)| text == "true" && style.fg == Some(Color::Yellow))
        );
    }

    #[test]
    fn yaml_syntax_highlighter_keeps_search_highlight() {
        let rendered = highlighted_yaml_text(
            "kind: Pod\nname: demo # marker",
            "demo",
            ColorSupport::Basic,
            Some(1),
        );
        assert!(
            rendered
                .lines
                .iter()
                .any(|line| line.spans.iter().any(|span| {
                    span.content.contains("demo")
                        && (span.style.bg == Some(Color::Yellow)
                            || span.style.bg == Some(Color::Cyan))
                }))
        );
    }

    #[test]
    fn active_search_match_uses_distinct_highlight_color() {
        let rendered = highlighted_text("alpha\ndemo\nomega demo", "demo", Some(1));
        let mut saw_active = false;
        let mut saw_regular = false;
        for line in &rendered.lines {
            for span in &line.spans {
                if span.content.contains("demo") {
                    if span.style.bg == Some(Color::Cyan) {
                        saw_active = true;
                    }
                    if span.style.bg == Some(Color::Yellow) {
                        saw_regular = true;
                    }
                }
            }
        }
        assert!(saw_active);
        assert!(saw_regular);
    }

    #[test]
    fn base64_encode_matches_known_values() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_decode_handles_standard_and_urlsafe_forms() {
        assert_eq!(base64_decode("Zm9v").expect("decode"), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").expect("decode"), b"foob");
        assert!(base64_decode("%%%").is_err());
    }

    #[test]
    fn decoded_secret_text_decodes_data_entries() {
        let raw = serde_json::json!({
            "metadata": { "namespace": "default", "name": "demo" },
            "type": "Opaque",
            "data": {
                "username": "YWRtaW4=",
                "password": "czNjcjN0"
            }
        });
        let out = decoded_secret_text(&raw);
        let parsed: BTreeMap<String, String> =
            serde_yaml::from_str(&out).expect("decoded secret output yaml");
        assert_eq!(parsed.get("username"), Some(&"admin".to_string()));
        assert_eq!(parsed.get("password"), Some(&"s3cr3t".to_string()));
    }

    #[test]
    fn clipboard_truncate_preserves_utf8_boundary() {
        let (out, truncated) = truncate_for_clipboard("abc\u{1f680}def", 5);
        assert!(truncated);
        assert_eq!(out, "abc");
    }

    #[test]
    fn step_match_line_moves_cursor_and_wraps() {
        let matches = vec![3, 8, 13];
        assert_eq!(step_match_line(&matches, 3, true), Some((8, 2)));
        assert_eq!(step_match_line(&matches, 13, true), Some((3, 1)));
        assert_eq!(step_match_line(&matches, 8, false), Some((3, 1)));
        assert_eq!(step_match_line(&matches, 3, false), Some((13, 3)));
    }

    #[test]
    fn resolved_active_match_line_prefers_explicit_cursor_if_present() {
        let matches = vec![5, 15, 25];
        assert_eq!(resolved_active_match_line(0, &matches, Some(15)), Some(15));
        assert_eq!(resolved_active_match_line(14, &matches, None), Some(15));
    }

    #[test]
    fn classifies_status_tags() {
        assert_eq!(severity_tag(classify_status_severity("Running")), "[OK]");
        assert_eq!(severity_tag(classify_status_severity("Pending")), "[!!]");
        assert_eq!(
            severity_tag(classify_status_severity("CrashLoopBackOff")),
            "[XX]"
        );
    }

    #[test]
    fn detects_color_capability_levels() {
        assert_eq!(
            detect_color_support_from_env(None, Some("truecolor"), Some("xterm-256color")),
            ColorSupport::TrueColor
        );
        assert_eq!(
            detect_color_support_from_env(None, None, Some("xterm-256color")),
            ColorSupport::Ansi256
        );
        assert_eq!(
            detect_color_support_from_env(None, None, Some("xterm")),
            ColorSupport::Basic
        );
        assert_eq!(
            detect_color_support_from_env(Some("1"), Some("truecolor"), Some("xterm")),
            ColorSupport::NoColor
        );
    }

    #[test]
    fn color_labels_are_stable_for_ui_snapshot_headers() {
        assert_eq!(color_support_label(ColorSupport::NoColor), "mono");
        assert_eq!(color_support_label(ColorSupport::Basic), "basic");
        assert_eq!(color_support_label(ColorSupport::Ansi256), "256");
        assert_eq!(color_support_label(ColorSupport::TrueColor), "truecolor");
    }

    #[test]
    fn builds_theme_for_all_color_support_levels() {
        let _ = ui_theme_for(ColorSupport::NoColor);
        let _ = ui_theme_for(ColorSupport::Basic);
        let _ = ui_theme_for(ColorSupport::Ansi256);
        let _ = ui_theme_for(ColorSupport::TrueColor);
    }

    #[test]
    fn pulse_metrics_cache_reuses_revision_and_invalidates_on_store_change() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        )));
        let tab = app.current_tab().clone();
        let first = app.pulse_metrics_for_tab(&tab);
        assert_eq!(first.pods, 1);

        {
            let cache = app
                .pulse_metrics_cache
                .as_mut()
                .expect("pulse metrics cache after first compute");
            cache.metrics.pods = 777;
        }

        let cached = app.pulse_metrics_for_tab(&tab);
        assert_eq!(cached.pods, 777);

        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-b",
            "Running",
        )));
        let recomputed = app.pulse_metrics_for_tab(&tab);
        assert_eq!(recomputed.pods, 2);
    }

    #[test]
    fn pods_table_shows_usage_and_right_sizing() {
        let mut app = test_app();
        let mut entity = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        );
        entity.extracted.pod_resources = Some(crate::model::PodResources {
            cpu_request_m: 1000,
            cpu_limit_m: 2000,
            mem_request_b: 128 * 1024 * 1024,
            mem_limit_b: 512 * 1024 * 1024,
        });
        entity.extracted.restarts = 7;
        entity.extracted.pod_ip = Some("10.0.1.23".to_string());
        entity.extracted.node_name = Some("ip-10-0-1-9.ec2.internal".to_string());
        app.store.apply(StateDelta::Upsert(entity));
        app.seed_pod_metrics_for_test("default", "pod-a", 1500, 256 * 1024 * 1024);

        let snap = render_snapshot(&mut app, 200, 20);
        // actual usage
        assert!(snap.contains("1.50c"), "cpu used: {snap}");
        assert!(snap.contains("256Mi"), "mem used: {snap}");
        // right-sizing percentages vs request and limit, in their own columns
        assert!(
            snap.contains("150%") && snap.contains("75%"),
            "cpu %R/%L: {snap}"
        ); // 1500/1000, 1500/2000
        assert!(
            snap.contains("200%") && snap.contains("50%"),
            "mem %R/%L: {snap}"
        ); // 256/128, 256/512
        // restarts, IP, and node columns
        assert!(snap.contains("Restarts"), "restarts header: {snap}");
        assert!(snap.contains('7'), "restart count: {snap}");
        assert!(snap.contains("10.0.1.23"), "pod ip: {snap}");
        assert!(snap.contains("ip-10-0-1-9.ec2.internal"), "node: {snap}");
    }

    #[test]
    fn table_header_marks_active_sort_column_and_direction() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        )));

        // Default sort is Name ascending: header shows "Name ↑" and the top bar [SORT] name↑.
        app.current_tab_mut().sort = SortColumn::Name;
        app.current_tab_mut().descending = false;
        let snap = render_snapshot(&mut app, 160, 20);
        assert!(snap.contains("Name ↑"), "name asc header: {snap}");
        assert!(snap.contains("[SORT] name↑"), "sort top bar: {snap}");

        // Switch to Status descending: arrow flips and moves to the Status column.
        app.current_tab_mut().sort = SortColumn::Status;
        app.current_tab_mut().descending = true;
        let snap = render_snapshot(&mut app, 160, 20);
        assert!(snap.contains("Status ↓"), "status desc header: {snap}");
        assert!(!snap.contains("Name ↑"), "name no longer marked: {snap}");
        assert!(snap.contains("[SORT] status↓"), "sort top bar: {snap}");
    }

    #[test]
    fn non_pod_table_renders_kind_specific_columns() {
        let mut app = test_app();
        let kind_idx = ResourceKind::ORDERED
            .iter()
            .position(|k| *k == ResourceKind::Deployments)
            .expect("deployments in ORDERED");
        app.current_tab_mut().kind_idx = kind_idx;

        let mut entity = mk_entity(
            "ctx-dev",
            ResourceKind::Deployments,
            Some("default"),
            "api",
            "3/3 ready",
        );
        entity.columns = vec!["3".to_string(), "3".to_string()]; // UP-TO-DATE, AVAILABLE
        app.store.apply(StateDelta::Upsert(entity));

        let snap = render_snapshot(&mut app, 160, 20);
        // kind-specific headers replace the old generic "Summary"
        assert!(snap.contains("Up-to-date"), "header: {snap}");
        assert!(snap.contains("Available"), "header: {snap}");
        assert!(!snap.contains("Summary"), "no generic summary col: {snap}");
        assert!(snap.contains("api"), "row name: {snap}");
    }

    #[test]
    fn helm_release_secrets_hidden_by_default_and_toggle_shows_them() {
        let mut app = test_app();
        let kind_idx = ResourceKind::ORDERED
            .iter()
            .position(|k| *k == ResourceKind::Secrets)
            .expect("secrets in ORDERED");
        app.current_tab_mut().kind_idx = kind_idx;

        let mut plain = mk_entity(
            "ctx-dev",
            ResourceKind::Secrets,
            Some("default"),
            "app-config",
            "-",
        );
        plain.columns = vec!["Opaque".to_string(), "2".to_string()];
        app.store.apply(StateDelta::Upsert(plain));
        let mut helm = mk_entity(
            "ctx-dev",
            ResourceKind::Secrets,
            Some("default"),
            "sh.helm.release.v1.myapp.v3",
            "-",
        );
        helm.columns = vec!["helm.sh/release.v1".to_string(), "2".to_string()];
        app.store.apply(StateDelta::Upsert(helm));

        // Default: helm release hidden, title advertises the toggle.
        let snap = render_snapshot(&mut app, 140, 20);
        assert!(snap.contains("app-config"), "plain secret shown: {snap}");
        assert!(
            !snap.contains("sh.helm.release.v1"),
            "helm hidden by default: {snap}"
        );
        assert!(snap.contains("helm hidden"), "title hint: {snap}");

        // Toggle on: helm release becomes visible.
        app.toggle_helm_secrets();
        let snap = render_snapshot(&mut app, 140, 20);
        assert!(
            snap.contains("sh.helm.release.v1"),
            "helm shown after toggle: {snap}"
        );
        assert!(snap.contains("helm shown"), "title hint: {snap}");
    }

    #[test]
    fn xray_builds_owner_child_tree() {
        let mut app = test_app();

        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Deployments,
            Some("default"),
            "dep1",
            "1/1 ready",
        )));
        let mut rs = mk_entity(
            "ctx-dev",
            ResourceKind::ReplicaSets,
            Some("default"),
            "rs1",
            "-",
        );
        rs.extracted.owners = vec![crate::model::OwnerRef {
            kind: "Deployment".to_string(),
            name: "dep1".to_string(),
        }];
        app.store.apply(StateDelta::Upsert(rs));
        let mut pod = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "p1",
            "Running",
        );
        pod.extracted.owners = vec![crate::model::OwnerRef {
            kind: "ReplicaSet".to_string(),
            name: "rs1".to_string(),
        }];
        pod.extracted.containers = vec!["app".to_string()];
        app.store.apply(StateDelta::Upsert(pod));

        let empty = std::collections::HashSet::new();
        let rows = app.xray_rows(Some("default"), &empty);
        let labels: Vec<String> = rows
            .iter()
            .map(|r| format!("{}/{}", r.kind, r.name))
            .collect();
        // Namespace-rooted forest: ns → deploy → rs → pod → container.
        assert!(labels.contains(&"ns/default".to_string()), "{labels:?}");
        assert!(labels.contains(&"deploy/dep1".to_string()), "{labels:?}");
        assert!(labels.contains(&"rs/rs1".to_string()), "{labels:?}");
        assert!(labels.contains(&"po/p1".to_string()), "{labels:?}");
        assert!(labels.contains(&"ctr/app".to_string()), "{labels:?}");

        let ns = rows.iter().find(|r| r.name == "default").expect("ns row");
        assert!(ns.connectors.is_empty(), "ns root is flush-left");
        let dep = rows.iter().find(|r| r.name == "dep1").expect("deploy row");
        assert!(dep.has_children && dep.marker == '▾', "deploy expandable");
        assert!(
            dep.connectors.contains('─'),
            "deploy nested under ns: {:?}",
            dep.connectors
        );

        // Collapsing the namespace hides its whole subtree.
        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert(ns.id.clone());
        let rows = app.xray_rows(Some("default"), &collapsed);
        assert_eq!(rows.len(), 1, "only the collapsed namespace remains");
        assert_eq!(rows[0].marker, '▸', "collapsed marker");
    }

    #[test]
    fn triage_surfaces_unhealthy_pods_worst_first() {
        let mut app = test_app();
        let mut healthy = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "ok",
            "Running",
        );
        healthy.extracted.ready = true;
        app.store.apply(StateDelta::Upsert(healthy));

        let mut crash = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "crash",
            "CrashLoopBackOff",
        );
        crash.extracted.ready = false;
        crash.extracted.restarts = 9;
        app.store.apply(StateDelta::Upsert(crash));

        let mut pending = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pending",
            "Pending",
        );
        pending.extracted.ready = false;
        app.store.apply(StateDelta::Upsert(pending));

        let mut notready = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "slow",
            "Running",
        );
        notready.extracted.ready = false; // Running but failing readiness
        app.store.apply(StateDelta::Upsert(notready));

        let (items, total) = app.triage_items(Some("default"));
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(total, 3, "healthy pod excluded: {names:?}");
        assert!(!names.contains(&"ok"), "healthy excluded: {names:?}");
        // Worst-first: the crashloop (Err) ranks above the warnings.
        assert_eq!(items[0].name, "crash");
        assert_eq!(items[0].severity, Severity::Err);
        let (errs, warns) = App::triage_counts(&items);
        assert_eq!((errs, warns), (1, 2));
        let slow = items.iter().find(|i| i.name == "slow").expect("slow");
        assert_eq!(slow.reason, "NotReady");
    }

    #[test]
    fn xray_arrow_keys_collapse_and_expand() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Deployments,
            Some("default"),
            "dep1",
            "1/1 ready",
        )));
        app.show_xray_overlay(Some("default".to_string()));
        // Cursor starts on the ns root (selected = 0), which has the deployment as a child.

        let visible = |app: &App| -> usize {
            let Some(super::Overlay::Xray {
                namespace,
                collapsed,
                ..
            }) = &app.overlay
            else {
                panic!("xray overlay");
            };
            app.xray_rows(namespace.as_deref(), collapsed).len()
        };

        let expanded = visible(&app);
        assert!(expanded > 1, "ns starts expanded: {expanded}");

        // Left collapses the namespace → only the ns row remains.
        app.set_xray_collapsed(true);
        assert_eq!(visible(&app), 1, "collapsed to ns root only");

        // Right expands it again.
        app.set_xray_collapsed(false);
        assert_eq!(visible(&app), expanded, "re-expanded");
    }

    #[test]
    fn enter_on_deployment_drills_into_its_pods() {
        let mut app = test_app();
        let dep_idx = ResourceKind::ORDERED
            .iter()
            .position(|k| *k == ResourceKind::Deployments)
            .expect("deployments in ORDERED");
        app.current_tab_mut().kind_idx = dep_idx;

        // dep1 -> rs1 -> p1 ; unrelated p2
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Deployments,
            Some("default"),
            "dep1",
            "1/1 ready",
        )));
        let mut rs = mk_entity(
            "ctx-dev",
            ResourceKind::ReplicaSets,
            Some("default"),
            "rs1",
            "-",
        );
        rs.extracted.owners = vec![crate::model::OwnerRef {
            kind: "Deployment".to_string(),
            name: "dep1".to_string(),
        }];
        app.store.apply(StateDelta::Upsert(rs));
        let mut p1 = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "p1",
            "Running",
        );
        p1.extracted.owners = vec![crate::model::OwnerRef {
            kind: "ReplicaSet".to_string(),
            name: "rs1".to_string(),
        }];
        app.store.apply(StateDelta::Upsert(p1));
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "p2",
            "Running",
        )));

        app.handle_enter_key();

        // Switched to a Pods view scoped to dep1.
        assert_eq!(app.current_tab().kind(), ResourceKind::Pods);
        let drill = app.current_tab().drill.as_ref().expect("drill set");
        assert_eq!(drill.owner_kind, ResourceKind::Deployments);
        assert_eq!(drill.owner_name, "dep1");

        let snap = render_snapshot(&mut app, 140, 20);
        assert!(
            snap.contains("[DRILL] deploy/dep1"),
            "drill indicator: {snap}"
        );
        // p1 (owned by dep1 via rs1) shows; the unrelated p2 is filtered out.
        assert!(snap.contains("default"), "owned pod row present: {snap}");
        assert!(!snap.contains("p2"), "unrelated pod hidden: {snap}");
    }

    #[test]
    fn events_pane_shows_correlated_events_for_a_pod() {
        let mut app = test_app();
        let entity = mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        );
        let key = entity.key.clone();
        app.store.apply(StateDelta::Upsert(entity));
        app.current_tab_mut().pane = Pane::Events;
        app.seed_events_for_test(
            key,
            vec![crate::cluster::EventRow {
                type_: "Warning".to_string(),
                reason: "BackOff".to_string(),
                message: "Back-off restarting failed container".to_string(),
                count: 5,
                last: Some(Utc::now()),
                source: "kubelet".to_string(),
            }],
        );

        let body = app.current_view_text().expect("events body");
        assert!(body.contains("Events for po pod-a"), "{body}");
        assert!(
            body.contains("Warning") && body.contains("BackOff"),
            "{body}"
        );
        assert!(body.contains("Back-off restarting"), "{body}");
    }

    #[test]
    fn pods_table_notes_when_metrics_unavailable() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        )));
        // no seeded pod_metrics → CPU/MEM are "-" and the title flags it
        let snap = render_snapshot(&mut app, 140, 20);
        assert!(snap.contains("metrics-server n/a"), "{snap}");
    }

    #[test]
    fn snapshot_table_pane_has_light_icons_and_status_tags() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-ok",
            "Running",
        )));
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-warn",
            "Pending",
        )));
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-bad",
            "CrashLoopBackOff",
        )));

        let snap = render_snapshot(&mut app, 200, 32);
        assert!(snap.contains("[CTX]"));
        assert!(snap.contains("[PULSE] Cluster Pulse"));
        assert!(snap.contains("[LIVE]"));
        assert!(snap.contains("[OK] Running"));
        assert!(snap.contains("[!!] Pending"));
        assert!(snap.contains("pod-bad"));
        assert!(snap.contains("[XX]"));
    }

    #[tokio::test]
    async fn dynamic_browse_catalog_list_and_describe() {
        let mut app = test_app();

        // :api catalog lists the discovered CRD.
        app.show_api_catalog(None).await;
        let catalog = app.current_view_text().expect("catalog overlay");
        assert!(
            catalog.contains("widgets.demo.krust.io"),
            "catalog: {catalog}"
        );

        // :widgets lists objects.
        assert!(app.browse_dynamic("widgets", None).await);
        let list = app.current_view_text().expect("list overlay");
        assert!(
            list.contains("widget-0") && list.contains("widget-1"),
            "list: {list}"
        );

        // :Widget (by kind) resolves too.
        assert!(app.browse_dynamic("Widget", None).await);

        // :widgets widget-0 describes one object.
        assert!(app.browse_dynamic("widgets", Some("widget-0")).await);
        let described = app.current_view_text().expect("describe overlay");
        assert!(described.contains("kind: Widget"), "describe: {described}");

        // Unknown token does not resolve.
        assert!(!app.browse_dynamic("definitely-not-a-resource", None).await);
    }

    #[test]
    fn cycle_sort_follows_left_to_right_column_order() {
        let mut app = test_app();
        // Default is Name; `s` should advance in visual column order, not jump around.
        app.current_tab_mut().sort = SortColumn::Namespace;
        let order = [
            SortColumn::Name,
            SortColumn::Status,
            SortColumn::Age,
            SortColumn::Namespace,
        ];
        for expected in order {
            app.cycle_sort();
            assert_eq!(app.current_tab().sort, expected);
        }
    }

    #[tokio::test]
    async fn sort_command_sets_column_and_direction() {
        let mut app = test_app();

        // Column + explicit direction.
        app.execute_colon_command("sort age desc").await;
        assert_eq!(app.current_tab().sort, SortColumn::Age);
        assert!(app.current_tab().descending);
        assert_eq!(app.status_line, "Sort: age desc");

        // `ns` alias maps to Namespace; ascending.
        app.execute_colon_command("sort ns asc").await;
        assert_eq!(app.current_tab().sort, SortColumn::Namespace);
        assert!(!app.current_tab().descending);
        assert_eq!(app.status_line, "Sort: namespace asc");

        // No args reports current state without changing it.
        app.execute_colon_command("sort").await;
        assert_eq!(app.current_tab().sort, SortColumn::Namespace);
        assert_eq!(app.status_line, "Sort: namespace asc");

        // Unknown column is rejected and leaves sort unchanged.
        app.execute_colon_command("sort bogus").await;
        assert_eq!(app.current_tab().sort, SortColumn::Namespace);
        assert!(app.status_line.starts_with("Usage: :sort"));
    }

    #[test]
    fn table_help_matches_actual_table_bindings() {
        let app = test_app();
        let tab = app.current_tab().clone();
        let help = app.context_help_text(&tab);
        // present + correct
        assert!(help.contains("g/G top/bot"), "{help}");
        assert!(help.contains("n namespace"), "{help}");
        assert!(help.contains("s sort"), "{help}");
        assert!(help.contains("r reverse"), "{help}");
        assert!(help.contains("E events"), "{help}");
        assert!(help.contains("ctrl+d delete"), "{help}");
        // paging is detail-only; must NOT be advertised in the table (ctrl+d is delete here)
        assert!(!help.contains("half-page"), "{help}");
        assert!(!help.contains("gg/G"), "{help}");
    }

    #[test]
    fn detail_help_advertises_close_table_and_edit() {
        let mut app = test_app();
        app.current_tab_mut().pane = Pane::Describe;
        let tab = app.current_tab().clone();
        let help = app.context_help_text(&tab);
        assert!(help.contains("esc close"), "{help}");
        assert!(help.contains("t table"), "{help}");
        assert!(help.contains("e edit"), "{help}");
        assert!(help.contains("gg/G top/bottom"), "{help}");
    }

    #[test]
    fn snapshot_describe_pane_has_viewer_state_line() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        )));
        app.current_tab_mut().pane = Pane::Describe;
        app.current_tab_mut().detail_filter = "metadata".to_string();

        let snap = render_snapshot(&mut app, 120, 32);
        assert!(snap.contains("[DESCRIBE] (yaml)"));
        assert!(snap.contains("search: /metadata"));
    }

    #[test]
    fn detail_search_keeps_full_body_visible() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        )));
        app.current_tab_mut().pane = Pane::Describe;
        app.current_tab_mut().detail_filter = "metadata".to_string();
        let key = app.selected_row().expect("selected row").key;
        app.seed_detail_cache_for_test(
            key,
            serde_json::json!({
                "metadata": { "name": "pod-a", "namespace": "default" },
                "status": { "phase": "Running" }
            }),
        );

        let body = app.current_view_text().expect("detail body");
        assert!(body.contains("metadata"));
        assert!(body.contains("status"));
    }

    #[test]
    fn snapshot_logs_pane_has_state_and_icons() {
        let mut app = test_app();
        app.current_tab_mut().pane = Pane::Logs;
        app.logs.selection = Some(super::LogSelection {
            scope: "pod default/pod-a".to_string(),
            targets: vec![super::LogTarget {
                context: "ctx-dev".to_string(),
                namespace: "default".to_string(),
                pod: "pod-a".to_string(),
                container: Some("main".to_string()),
            }],
        });
        app.push_log_line("[default/pod-a/main] line-1".to_string());
        app.push_log_line("[default/pod-a/main] line-2".to_string());

        let snap = render_snapshot(&mut app, 120, 32);
        assert!(snap.contains("[LG]"));
        assert!(snap.contains("[LOGS] pod default/pod-a"));
        assert!(snap.contains("[default/pod-a/main] line-1"));
        // Defaults to the current container instance.
        assert!(snap.contains("instance: current"), "instance: {snap}");
    }

    #[tokio::test]
    async fn toggle_previous_logs_flips_instance_and_resets() {
        let mut app = test_app();
        app.current_tab_mut().pane = Pane::Logs;
        app.logs.selection = Some(super::LogSelection {
            scope: "pod default/pod-a".to_string(),
            targets: vec![super::LogTarget {
                context: "ctx-dev".to_string(),
                namespace: "default".to_string(),
                pod: "pod-a".to_string(),
                container: Some("main".to_string()),
            }],
        });
        app.push_log_line("[default/pod-a/main] old".to_string());

        assert!(!app.logs.previous);
        app.toggle_previous_logs().await;
        assert!(app.logs.previous, "toggled to previous");
        // Switching instances refetches: the prior buffer is cleared.
        assert!(
            !app.logs.lines.iter().any(|l| l.contains("old")),
            "buffer reset on toggle: {:?}",
            app.logs.lines
        );
        assert!(
            app.logs_title().contains("instance: previous"),
            "{}",
            app.logs_title()
        );

        app.toggle_previous_logs().await;
        assert!(!app.logs.previous, "toggled back to current");
        assert!(
            app.logs_title().contains("instance: current"),
            "{}",
            app.logs_title()
        );
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn perf_log_viewport_render_large_buffer() {
        let mut lines = VecDeque::new();
        for idx in 0..5_000 {
            lines.push_back(format!(
                "2026-03-07T00:00:{idx:02}Z [ns/pod/container] line-{idx} payload=abcdefghijklmnopqrstuvwxyz0123456789"
            ));
        }

        let start = Instant::now();
        let mut rendered = 0usize;
        for frame in 0..2_000 {
            let top = frame % 4_900;
            let hscroll = frame % 32;
            let matches = search_match_lines_in_logs(&lines, "payload");
            let mut visible = String::new();
            for line in lines.iter().skip(top).take(40) {
                visible.push_str(&slice_chars(line, hscroll, 140));
                visible.push('\n');
            }
            rendered = rendered.saturating_add(visible.len() + matches.len());
        }
        let elapsed = start.elapsed();
        eprintln!(
            "[perf] log_viewport_render rendered={} total={:?} avg/frame={:?}",
            rendered,
            elapsed,
            elapsed / 2_000
        );
    }

    #[test]
    fn table_viewport_rows_accounts_for_header_and_borders() {
        assert_eq!(table_viewport_rows(0), 0);
        assert_eq!(table_viewport_rows(2), 0);
        assert_eq!(table_viewport_rows(3), 0);
        assert_eq!(table_viewport_rows(6), 3);
    }

    #[test]
    fn table_viewport_sync_scrolls_down_when_selection_leaves_bottom() {
        let (selected, offset) = sync_table_viewport(15, 0, 10, 100);
        assert_eq!(selected, 15);
        assert_eq!(offset, 6);
    }

    #[test]
    fn table_viewport_sync_scrolls_up_when_selection_leaves_top() {
        let (selected, offset) = sync_table_viewport(89, 90, 10, 100);
        assert_eq!(selected, 89);
        assert_eq!(offset, 89);
    }

    #[test]
    fn table_viewport_sync_keeps_selection_moving_inside_viewport() {
        let (selected, offset) = sync_table_viewport(98, 90, 10, 100);
        assert_eq!(selected, 98);
        assert_eq!(offset, 90);
    }

    #[test]
    fn table_viewport_sync_handles_empty_data() {
        let (selected, offset) = sync_table_viewport(5, 3, 10, 0);
        assert_eq!(selected, 0);
        assert_eq!(offset, 0);
    }

    #[test]
    fn detail_scroll_clamps_to_content_bounds_after_draw() {
        let mut app = test_app();
        app.store.apply(StateDelta::Upsert(mk_entity(
            "ctx-dev",
            ResourceKind::Pods,
            Some("default"),
            "pod-a",
            "Running",
        )));
        app.current_tab_mut().pane = Pane::Describe;
        app.current_tab_mut().detail_wrap = false;
        app.current_tab_mut().detail_scroll = u16::MAX;
        app.current_tab_mut().detail_hscroll = u16::MAX;

        let _ = render_snapshot(&mut app, 120, 40);
        let tab = app.current_tab();
        assert_eq!(tab.detail_scroll, 0);
        assert_eq!(tab.detail_hscroll, 0);
    }

    #[test]
    fn logs_scroll_clamps_to_content_bounds_after_draw() {
        let mut app = test_app();
        app.current_tab_mut().pane = Pane::Logs;
        app.logs.auto_scroll = false;
        app.push_log_line("[default/pod-a/main] line-1".to_string());
        app.push_log_line("[default/pod-a/main] line-2".to_string());
        app.current_tab_mut().detail_wrap = false;
        app.current_tab_mut().detail_scroll = u16::MAX;
        app.current_tab_mut().detail_hscroll = u16::MAX;

        let _ = render_snapshot(&mut app, 120, 40);
        let tab = app.current_tab();
        assert_eq!(tab.detail_scroll, 0);
        assert_eq!(tab.detail_hscroll, 0);
    }

    #[test]
    fn table_scroll_offset_tracks_selection_when_moving_up() {
        let mut app = test_app();
        for idx in 0..150 {
            app.store.apply(StateDelta::Upsert(mk_entity(
                "ctx-dev",
                ResourceKind::Pods,
                Some("default"),
                &format!("pod-{idx:03}"),
                "Running",
            )));
        }
        app.current_tab_mut().pane = Pane::Table;
        app.current_tab_mut().selected = 120;
        app.current_tab_mut().table_offset = 118;
        let _ = render_snapshot(&mut app, 120, 20);

        app.move_selection(-110);
        let _ = render_snapshot(&mut app, 120, 20);
        let tab = app.current_tab();
        assert_eq!(tab.selected, 10);
        assert!(tab.table_offset <= tab.selected);
    }
}
