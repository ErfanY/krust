use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use chrono::Local;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table, Wrap},
};
use tokio::sync::mpsc;

use crate::{
    cluster::{
        ActionError, ActionExecutor, PodLogEvent, PodLogRequest, PodLogStream, ResourceProvider,
        WatchTarget,
    },
    keymap::{Action, Keymap},
    model::{
        ConfirmationKind, Pane, PendingConfirmation, ResourceKey, ResourceKind, SortColumn,
        StateDelta,
    },
    state::StateStore,
    view::{SimpleViewProjector, ViewModel, ViewProjector, ViewRequest},
};

#[derive(Debug, Clone)]
struct ContextTabState {
    context: String,
    namespace: Option<String>,
    filter: String,
    detail_filter: String,
    selected: usize,
    detail_scroll: u16,
    detail_hscroll: u16,
    detail_wrap: bool,
    kind_idx: usize,
    last_non_namespace_kind_idx: usize,
    sort: SortColumn,
    descending: bool,
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
    Contexts {
        title: String,
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
        title: String,
        sources: Vec<String>,
        selected: usize,
        filter: String,
    },
}

pub async fn run(
    contexts: Vec<String>,
    initial_context: String,
    initial_namespace: Option<String>,
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
        while let Ok(delta) = delta_rx.try_recv() {
            app.store.apply(delta);
            dirty = true;
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
    show_help: bool,
    detail_page_step: u16,
    pending_detail_g: bool,
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

#[derive(Clone, Copy)]
struct UiTheme {
    header: Style,
    block: Style,
    table_header: Style,
    row_highlight: Style,
    status_ok: Style,
    status_warn: Style,
    status_err: Style,
    help: Style,
    command_active: Style,
    command_idle: Style,
}

fn ui_theme() -> UiTheme {
    UiTheme {
        header: Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 242, 204))
            .add_modifier(Modifier::BOLD),
        block: Style::default().fg(Color::Rgb(238, 244, 255)),
        table_header: Style::default()
            .fg(Color::Rgb(255, 247, 214))
            .add_modifier(Modifier::BOLD),
        row_highlight: Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(186, 223, 255))
            .add_modifier(Modifier::BOLD),
        status_ok: Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(214, 245, 214)),
        status_warn: Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 235, 179)),
        status_err: Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 204, 204))
            .add_modifier(Modifier::BOLD),
        help: Style::default().fg(Color::Rgb(192, 208, 235)),
        command_active: Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(229, 242, 255))
            .add_modifier(Modifier::BOLD),
        command_idle: Style::default().fg(Color::Rgb(161, 180, 214)),
    }
}

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
        }
    }

    fn selected_row(&mut self) -> Option<crate::view::ViewRow> {
        let active = self.current_tab().clone();
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let selected = active.selected.min(vm.rows.len().saturating_sub(1));
        vm.rows.get(selected).cloned()
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
            let (cpu_req, cpu_lim, mem_req, mem_lim) = pod_resources_from_raw(&pod.raw);
            totals.cpu_request_m = totals.cpu_request_m.saturating_add(cpu_req);
            totals.cpu_limit_m = totals.cpu_limit_m.saturating_add(cpu_lim);
            totals.mem_request_b = totals.mem_request_b.saturating_add(mem_req);
            totals.mem_limit_b = totals.mem_limit_b.saturating_add(mem_lim);
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
            let (ready, unschedulable, cpu_alloc_m, mem_alloc_b, pod_alloc) =
                node_capacity_from_raw(&node.raw);
            if ready {
                totals.nodes_ready += 1;
            }
            if unschedulable {
                totals.nodes_unschedulable += 1;
            }
            totals.cpu_alloc_m = totals.cpu_alloc_m.saturating_add(cpu_alloc_m);
            totals.mem_alloc_b = totals.mem_alloc_b.saturating_add(mem_alloc_b);
            totals.pod_alloc = totals.pod_alloc.saturating_add(pod_alloc);
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

    fn active_filter_value(&self) -> String {
        if let Some(overlay) = &self.overlay {
            match overlay {
                Overlay::Contexts { filter, .. }
                | Overlay::Containers { filter, .. }
                | Overlay::LogSources { filter, .. } => {
                    return filter.clone();
                }
                Overlay::Text { .. } => {}
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
                Overlay::Text { .. } => {}
            }
            return;
        }
        let tab = self.current_tab_mut();
        if tab.pane == Pane::Table {
            tab.filter = filter;
        } else {
            tab.detail_filter = filter;
        }
    }

    fn clear_active_filter_value(&mut self) -> bool {
        if self.active_filter_value().is_empty() {
            return false;
        }
        self.set_active_filter_value(String::new());
        true
    }

    fn pod_container_names_for_key(&self, pod_key: &ResourceKey) -> Vec<String> {
        let Some(entity) = self.store.get(pod_key) else {
            return Vec::new();
        };
        entity
            .raw
            .pointer("/spec/containers")
            .and_then(serde_json::Value::as_array)
            .map(|containers| {
                containers
                    .iter()
                    .filter_map(|container| {
                        container
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    fn pod_log_targets(
        &self,
        pod_key: &ResourceKey,
        container_override: Option<&str>,
    ) -> Vec<LogTarget> {
        let namespace = pod_key
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let mut containers = self.pod_container_names_for_key(pod_key);
        containers.sort();
        containers.dedup();

        if let Some(target_container) = container_override {
            if containers.iter().any(|name| name == target_container) {
                return vec![LogTarget {
                    context: pod_key.context.clone(),
                    namespace,
                    pod: pod_key.name.clone(),
                    container: Some(target_container.to_string()),
                }];
            }
        }

        if containers.is_empty() {
            return vec![LogTarget {
                context: pod_key.context.clone(),
                namespace,
                pod: pod_key.name.clone(),
                container: None,
            }];
        }

        containers
            .into_iter()
            .map(|container| LogTarget {
                context: pod_key.context.clone(),
                namespace: namespace.clone(),
                pod: pod_key.name.clone(),
                container: Some(container),
            })
            .collect()
    }

    fn replica_set_pod_keys(&self, rs_key: &ResourceKey) -> Vec<ResourceKey> {
        let namespace = rs_key.namespace.as_deref();
        let mut pods: Vec<ResourceKey> = self
            .store
            .list(&rs_key.context, ResourceKind::Pods, namespace)
            .into_iter()
            .filter(|pod| owner_reference_matches(&pod.raw, "ReplicaSet", &rs_key.name))
            .map(|pod| pod.key.clone())
            .collect();
        pods.sort_by(|a, b| a.name.cmp(&b.name));
        pods.dedup();
        pods
    }

    fn deployment_replica_set_keys(&self, dep_key: &ResourceKey) -> Vec<ResourceKey> {
        let namespace = dep_key.namespace.as_deref();
        let mut replica_sets: Vec<ResourceKey> = self
            .store
            .list(&dep_key.context, ResourceKind::ReplicaSets, namespace)
            .into_iter()
            .filter(|rs| owner_reference_matches(&rs.raw, "Deployment", &dep_key.name))
            .map(|rs| rs.key.clone())
            .collect();
        replica_sets.sort_by(|a, b| a.name.cmp(&b.name));
        replica_sets.dedup();
        replica_sets
    }

    fn deployment_pod_keys(&self, dep_key: &ResourceKey) -> Vec<ResourceKey> {
        let mut pods = Vec::new();
        for rs_key in self.deployment_replica_set_keys(dep_key) {
            pods.extend(self.replica_set_pod_keys(&rs_key));
        }
        pods.sort_by(|a, b| a.name.cmp(&b.name));
        pods.dedup();
        pods
    }

    fn desired_log_selection(&mut self) -> Option<LogSelection> {
        if self.current_tab().pane != Pane::Logs {
            return None;
        }
        let row = self.selected_row()?;
        match row.key.kind {
            ResourceKind::Pods => {
                let container_override =
                    if self.logs.container_override_pod.as_ref() == Some(&row.key) {
                        self.logs.container_override.as_deref()
                    } else {
                        None
                    };
                let targets =
                    normalize_log_targets(self.pod_log_targets(&row.key, container_override));
                if targets.is_empty() {
                    return None;
                }
                let scope = if let Some(container) = container_override {
                    format!(
                        "pod {}/{}/{}",
                        row.key
                            .namespace
                            .clone()
                            .unwrap_or_else(|| "default".to_string()),
                        row.key.name,
                        container
                    )
                } else {
                    format!(
                        "pod {}/{} (all containers)",
                        row.key
                            .namespace
                            .clone()
                            .unwrap_or_else(|| "default".to_string()),
                        row.key.name
                    )
                };
                Some(LogSelection { scope, targets })
            }
            ResourceKind::ReplicaSets => {
                let mut targets = Vec::new();
                for pod_key in self.replica_set_pod_keys(&row.key) {
                    targets.extend(self.pod_log_targets(&pod_key, None));
                }
                targets = normalize_log_targets(targets);
                if targets.is_empty() {
                    return None;
                }
                Some(LogSelection {
                    scope: format!(
                        "rs {}/{} ({} streams)",
                        row.key.namespace.unwrap_or_else(|| "default".to_string()),
                        row.key.name,
                        targets.len()
                    ),
                    targets,
                })
            }
            ResourceKind::Deployments => {
                let mut targets = Vec::new();
                for pod_key in self.deployment_pod_keys(&row.key) {
                    targets.extend(self.pod_log_targets(&pod_key, None));
                }
                targets = normalize_log_targets(targets);
                if targets.is_empty() {
                    return None;
                }
                Some(LogSelection {
                    scope: format!(
                        "deploy {}/{} ({} streams)",
                        row.key.namespace.unwrap_or_else(|| "default".to_string()),
                        row.key.name,
                        targets.len()
                    ),
                    targets,
                })
            }
            _ => None,
        }
    }

    fn stop_log_session(&mut self) {
        if let Some(session) = self.logs.session.take() {
            for task in session.tasks {
                task.abort();
            }
        }
    }

    fn reset_log_buffer(&mut self) {
        self.logs.lines.clear();
        self.logs.lines_version = self.logs.lines_version.wrapping_add(1);
        self.logs.joined_cache.clear();
        self.logs.joined_cache_version = 0;
        self.logs.total_bytes = 0;
        self.logs.max_line_width = 0;
        self.logs.dropped_lines = 0;
        self.logs.paused_skipped_lines = 0;
        self.logs.last_error = None;
        self.logs.stream_closed = false;
        self.logs.auto_scroll = true;
        self.logs.reconnect_attempt = 0;
        self.logs.reconnect_after = None;
        self.logs.reconnect_blocked = false;
        self.logs.paused = false;
        self.logs.hidden_sources.clear();
        self.logs.source_filter_version = self.logs.source_filter_version.wrapping_add(1);
    }

    async fn start_log_session(&mut self, selection: LogSelection, initial: bool) -> bool {
        let (tx, rx) = mpsc::channel(4096);
        let mut tasks = Vec::new();
        let mut opened = 0usize;

        for target in &selection.targets {
            let request = PodLogRequest {
                context: target.context.clone(),
                namespace: target.namespace.clone(),
                pod: target.pod.clone(),
                container: target.container.clone(),
                follow: true,
                tail_lines: if initial {
                    Some(LOG_DEFAULT_TAIL_LINES)
                } else {
                    None
                },
                since_seconds: if initial { None } else { Some(15) },
                previous: false,
                timestamps: true,
            };
            match self.resource_provider.stream_pod_logs(request).await {
                Ok(PodLogStream {
                    rx: source_rx,
                    task,
                }) => {
                    opened = opened.saturating_add(1);
                    tasks.push(task);
                    let mut source_rx = source_rx;
                    let tx = tx.clone();
                    let stream_name = log_target_name(target);
                    let forward_task = tokio::spawn(async move {
                        while let Some(event) = source_rx.recv().await {
                            match event {
                                PodLogEvent::Line(line) => {
                                    if tx
                                        .send(PodLogEvent::Line(format!("[{stream_name}] {line}")))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                PodLogEvent::End => return,
                                PodLogEvent::Error(error) => {
                                    let _ = tx
                                        .send(PodLogEvent::Error(format!(
                                            "[{stream_name}][error] {error}"
                                        )))
                                        .await;
                                    return;
                                }
                            }
                        }
                    });
                    tasks.push(forward_task);
                }
                Err(err) => {
                    let message = err.to_string();
                    self.logs.last_error = Some(message.clone());
                    if !is_retryable_log_error(&message) {
                        self.logs.reconnect_blocked = true;
                    }
                    self.push_log_line(format!(
                        "[error] failed to open {}: {}",
                        log_target_name(target),
                        message
                    ));
                }
            }
        }
        drop(tx);

        if opened > 0 {
            self.logs.session = Some(ActiveLogSession { rx, tasks });
            self.logs.reconnect_attempt = 0;
            self.logs.reconnect_after = None;
            self.logs.stream_closed = false;
            self.logs.reconnect_blocked = false;
            self.status_line = format!("Streaming logs: {}", selection.scope);
        } else {
            self.logs.session = None;
            if self.logs.reconnect_blocked {
                self.logs.stream_closed = true;
                self.logs.reconnect_after = None;
                self.status_line = "Log stream blocked by non-retryable error".to_string();
            } else {
                self.schedule_log_reconnect();
                self.status_line = "Failed to start log stream".to_string();
            }
        }
        true
    }

    fn schedule_log_reconnect(&mut self) {
        if self.logs.reconnect_blocked {
            self.logs.stream_closed = true;
            self.logs.reconnect_after = None;
            return;
        }
        self.logs.stream_closed = true;
        self.logs.reconnect_attempt = self.logs.reconnect_attempt.saturating_add(1);
        let backoff_ms = next_log_reconnect_backoff_ms(
            self.logs.reconnect_attempt,
            self.logs.last_error.as_deref(),
        );
        self.logs.reconnect_after = Some(Instant::now() + Duration::from_millis(backoff_ms));
    }

    async fn reconcile_log_session(&mut self) -> bool {
        let desired = self.desired_log_selection();
        if desired != self.logs.selection {
            self.stop_log_session();
            self.reset_log_buffer();
            self.logs.selection = desired.clone();
            let Some(selection) = desired else {
                return true;
            };
            return self.start_log_session(selection, true).await;
        }

        if self.logs.selection.is_none()
            || self.logs.session.is_some()
            || self.logs.reconnect_blocked
        {
            return false;
        }
        if let Some(reconnect_after) = self.logs.reconnect_after
            && Instant::now() < reconnect_after
        {
            return false;
        }

        let selection = self.logs.selection.clone().expect("checked is_some");
        self.start_log_session(selection, false).await
    }

    fn push_log_line(&mut self, line: String) {
        self.logs.lines_version = self.logs.lines_version.wrapping_add(1);
        let new_width = line.chars().count();
        self.logs.total_bytes = self.logs.total_bytes.saturating_add(line.len());
        self.logs.max_line_width = self.logs.max_line_width.max(new_width);
        self.logs.lines.push_back(line);

        let mut must_recompute_width = false;
        while self.logs.lines.len() > LOG_MAX_LINES || self.logs.total_bytes > LOG_MAX_BYTES {
            let Some(oldest) = self.logs.lines.pop_front() else {
                break;
            };
            if oldest.chars().count() >= self.logs.max_line_width {
                must_recompute_width = true;
            }
            self.logs.total_bytes = self.logs.total_bytes.saturating_sub(oldest.len());
            self.logs.dropped_lines = self.logs.dropped_lines.saturating_add(1);
        }

        if must_recompute_width {
            self.logs.max_line_width = self
                .logs
                .lines
                .iter()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0);
        }
    }

    fn drain_log_events(&mut self) -> bool {
        let mut changed = false;
        let mut should_close_session = false;
        let mut processed = 0usize;
        let mut events = Vec::new();
        if let Some(session) = &mut self.logs.session {
            loop {
                if processed >= LOG_MAX_EVENTS_PER_DRAIN {
                    break;
                }
                match session.rx.try_recv() {
                    Ok(event) => {
                        processed = processed.saturating_add(1);
                        events.push(event);
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        if self
                            .logs
                            .last_error
                            .as_deref()
                            .is_some_and(|err| !is_retryable_log_error(err))
                        {
                            self.logs.reconnect_blocked = true;
                            self.logs.stream_closed = true;
                            self.logs.reconnect_after = None;
                            self.status_line =
                                "Log reconnect blocked: non-retryable RBAC error".to_string();
                        } else {
                            self.schedule_log_reconnect();
                        }
                        should_close_session = true;
                        changed = true;
                        break;
                    }
                }
            }
        }

        for event in events {
            match event {
                PodLogEvent::Line(line) => {
                    if self.logs.paused {
                        self.logs.paused_skipped_lines =
                            self.logs.paused_skipped_lines.saturating_add(1);
                    } else {
                        self.push_log_line(line);
                        changed = true;
                    }
                }
                PodLogEvent::End => {}
                PodLogEvent::Error(error) => {
                    self.logs.last_error = Some(error.clone());
                    if !is_retryable_log_error(&error) {
                        self.logs.reconnect_blocked = true;
                    }
                    self.push_log_line(format!("[error] {error}"));
                    changed = true;
                }
            }
        }

        if should_close_session {
            self.stop_log_session();
        }

        if processed >= LOG_MAX_EVENTS_PER_DRAIN {
            changed = true;
        }

        if changed && self.logs.auto_scroll && self.current_tab().pane == Pane::Logs {
            self.current_tab_mut().detail_scroll = u16::MAX;
        }

        changed
    }

    fn ensure_log_joined_cache(&mut self) {
        if self.logs.joined_cache_version == self.logs.lines_version {
            return;
        }
        self.logs.joined_cache = self
            .logs
            .lines
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        self.logs.joined_cache_version = self.logs.lines_version;
    }

    fn log_joined_text(&mut self) -> &str {
        self.ensure_log_joined_cache();
        self.logs.joined_cache.as_str()
    }

    fn log_body_text(&mut self) -> String {
        if !self.logs.lines.is_empty() {
            return self.log_joined_text().to_string();
        }
        if let Some(selection) = &self.logs.selection {
            return format!("Waiting for log lines from {} ...", selection.scope);
        }
        if self.current_tab().pane == Pane::Logs {
            if let Some(row) = self.selected_row()
                && !matches!(
                    row.key.kind,
                    ResourceKind::Pods | ResourceKind::ReplicaSets | ResourceKind::Deployments
                )
            {
                return "Logs are available for Pods, ReplicaSets, and Deployments.".to_string();
            }
            return "No logs yet.".to_string();
        }
        "No logs available.".to_string()
    }

    fn logs_title(&self) -> String {
        let state = if self.logs.paused {
            "paused"
        } else if self.logs.session.is_some() {
            "streaming"
        } else if self.logs.reconnect_blocked {
            "blocked"
        } else if self.logs.stream_closed {
            "closed"
        } else {
            "idle"
        };
        let target = self
            .logs
            .selection
            .as_ref()
            .map(|selection| selection.scope.clone())
            .unwrap_or_else(|| "-".to_string());
        let streams = self
            .logs
            .selection
            .as_ref()
            .map(|selection| selection.targets.len())
            .unwrap_or(0);
        let err = self
            .logs
            .last_error
            .as_ref()
            .map(|_| " | err".to_string())
            .unwrap_or_default();
        let source_filters = if self.logs.hidden_sources.is_empty() {
            "all".to_string()
        } else {
            format!("{} hidden", self.logs.hidden_sources.len())
        };

        format!(
            "Logs | target:{} | streams:{} | state:{} | src:{} | lines:{} | dropped:{} | paused-drop:{}{} | wrap:{}",
            target,
            streams,
            state,
            source_filters,
            self.logs.lines.len(),
            self.logs.dropped_lines,
            self.logs.paused_skipped_lines,
            err,
            if self.current_tab().detail_wrap {
                "on"
            } else {
                "off"
            }
        )
    }

    fn set_log_paused(&mut self, paused: bool) {
        self.logs.paused = paused;
        if paused {
            self.logs.auto_scroll = false;
            self.status_line = "Log stream paused".to_string();
        } else {
            self.status_line = "Log stream resumed".to_string();
        }
    }

    fn jump_logs_to_latest(&mut self) {
        self.logs.paused = false;
        self.logs.auto_scroll = true;
        self.current_tab_mut().detail_scroll = u16::MAX;
        self.status_line = "Logs: jumped to latest and resumed tailing".to_string();
    }

    fn available_log_sources(&self) -> Vec<String> {
        let mut sources = HashSet::new();
        for line in &self.logs.lines {
            if let Some(source) = parse_log_source(line) {
                sources.insert(source.to_string());
            }
        }
        let mut out: Vec<String> = sources.into_iter().collect();
        out.sort();
        out
    }

    fn toggle_log_source(&mut self, source: &str) -> bool {
        let changed = if self.logs.hidden_sources.contains(source) {
            self.logs.hidden_sources.remove(source)
        } else {
            self.logs.hidden_sources.insert(source.to_string())
        };
        if changed {
            self.logs.source_filter_version = self.logs.source_filter_version.wrapping_add(1);
            self.current_tab_mut().detail_scroll = 0;
        }
        changed
    }

    fn filtered_log_line_count_and_width(&self) -> (usize, usize) {
        if self.logs.hidden_sources.is_empty() {
            return (self.logs.lines.len(), self.logs.max_line_width);
        }
        let mut count = 0usize;
        let mut max_width = 0usize;
        for line in &self.logs.lines {
            if is_visible_log_line(line, &self.logs.hidden_sources) {
                count = count.saturating_add(1);
                max_width = max_width.max(line.chars().count());
            }
        }
        (count, max_width)
    }

    fn filtered_log_body_text(&mut self) -> String {
        if self.logs.hidden_sources.is_empty() {
            return self.log_joined_text().to_string();
        }
        self.logs
            .lines
            .iter()
            .filter(|line| is_visible_log_line(line, &self.logs.hidden_sources))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn log_search_match_lines(&mut self, query: &str) -> Vec<usize> {
        let needle = query.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }

        if self.logs.search_cache_query == needle
            && self.logs.search_cache_lines_version == self.logs.lines_version
            && self.logs.search_cache_source_filter_version == self.logs.source_filter_version
        {
            return self.logs.search_cache_matches.clone();
        }

        let mut matches = Vec::new();
        let mut visible_idx = 0usize;
        for line in &self.logs.lines {
            if !is_visible_log_line(line, &self.logs.hidden_sources) {
                continue;
            }
            if line.to_ascii_lowercase().contains(&needle) {
                matches.push(visible_idx);
            }
            visible_idx = visible_idx.saturating_add(1);
        }

        self.logs.search_cache_query = needle;
        self.logs.search_cache_lines_version = self.logs.lines_version;
        self.logs.search_cache_source_filter_version = self.logs.source_filter_version;
        self.logs.search_cache_matches = matches.clone();
        matches
    }

    fn new(
        contexts: Vec<String>,
        initial_context: String,
        initial_namespace: Option<String>,
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
                    initial_namespace.clone()
                } else {
                    None
                },
                filter: String::new(),
                detail_filter: String::new(),
                selected: 0,
                detail_scroll: 0,
                detail_hscroll: 0,
                detail_wrap: true,
                kind_idx: 0,
                last_non_namespace_kind_idx: 0,
                sort: SortColumn::Name,
                descending: false,
                pane: Pane::Table,
            })
            .collect();

        let active_tab = tabs
            .iter()
            .position(|tab| tab.context == initial_context)
            .unwrap_or(0);

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
            show_help,
            detail_page_step: 10,
            pending_detail_g: false,
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
                self.status_line = "Filter mode".to_string();
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
            self.overlay = None;
            self.status_line = "View opened".to_string();
            self.ensure_active_watch().await;
            return Ok(false);
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Char('e') {
            self.status_line = "Edit action is not implemented yet".to_string();
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
            self.status_line = "Filter mode".to_string();
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
                self.status_line = "Filter cleared".to_string();
            } else {
                self.status_line = "Nothing to cancel".to_string();
            }
        }

        self.ensure_active_watch().await;

        Ok(false)
    }

    fn handle_enter_key(&mut self) {
        if self.current_tab().pane != Pane::Table {
            return;
        }

        let Some(row) = self.selected_row() else {
            self.status_line = "No resource selected".to_string();
            return;
        };

        if self.current_tab().kind() == ResourceKind::Namespaces {
            let pods_idx = ResourceKind::ORDERED
                .iter()
                .position(|kind| *kind == ResourceKind::Pods)
                .unwrap_or(0);
            let target_kind_idx = {
                let tab = self.current_tab_mut();
                tab.namespace = Some(row.name.clone());
                let idx = tab.last_non_namespace_kind_idx;
                if ResourceKind::ORDERED.get(idx) == Some(&ResourceKind::Namespaces) {
                    pods_idx
                } else {
                    idx
                }
            };
            let (kind_label, ns_label) = {
                let tab = self.current_tab_mut();
                tab.kind_idx = target_kind_idx;
                tab.selected = 0;
                tab.detail_scroll = 0;
                tab.detail_hscroll = 0;
                tab.pane = Pane::Table;
                tab.last_non_namespace_kind_idx = target_kind_idx;
                (tab.kind().to_string(), row.name.clone())
            };
            self.overlay = None;
            self.status_line = format!("Namespace selected: {ns_label} | kind: {kind_label}");
            return;
        }

        self.current_tab_mut().pane = Pane::Describe;
        self.current_tab_mut().detail_scroll = 0;
        self.current_tab_mut().detail_hscroll = 0;
        self.status_line = format!("Describe: {} {}", row.key.kind.short_name(), row.key.name);
    }

    fn in_detail_pane(&self) -> bool {
        self.current_tab().pane != Pane::Table
    }

    fn current_detail_body_for_search(&mut self) -> Option<String> {
        let tab = self.current_tab().clone();
        match tab.pane {
            Pane::Table => None,
            Pane::Logs => Some(self.log_body_text()),
            Pane::Describe | Pane::Events => {
                let request = self.view_request_for_tab(&tab);
                let vm = self.projected_view(&request);
                let selected = tab.selected.min(vm.rows.len().saturating_sub(1));
                let raw = self.detail_text(&vm.rows, selected, tab.pane);
                if tab.detail_filter.trim().is_empty() {
                    Some(raw)
                } else {
                    Some(filter_text_lines(&raw, &tab.detail_filter))
                }
            }
        }
    }

    fn jump_detail_match(&mut self, forward: bool) {
        if !self.in_detail_pane() {
            return;
        }
        let needle = self.current_tab().detail_filter.trim().to_string();
        if needle.is_empty() {
            self.status_line = "No active detail search. Press '/' to search.".to_string();
            return;
        }
        let matches: Vec<usize> = if self.current_tab().pane == Pane::Logs {
            self.log_search_match_lines(&needle)
        } else {
            let Some(body) = self.current_detail_body_for_search() else {
                self.status_line = "No detail content".to_string();
                return;
            };
            let lower = needle.to_ascii_lowercase();
            body.lines()
                .enumerate()
                .filter_map(|(idx, line)| {
                    if line.to_ascii_lowercase().contains(&lower) {
                        Some(idx)
                    } else {
                        None
                    }
                })
                .collect()
        };
        if matches.is_empty() {
            self.status_line = format!("No matches for '{}'", needle.trim());
            return;
        }

        let current = self.current_tab().detail_scroll as usize;
        let target = if forward {
            matches
                .iter()
                .copied()
                .find(|idx| *idx > current)
                .unwrap_or(matches[0])
        } else {
            matches
                .iter()
                .copied()
                .rev()
                .find(|idx| *idx < current)
                .unwrap_or(*matches.last().unwrap_or(&matches[0]))
        };
        let match_pos = matches.iter().position(|idx| *idx == target).unwrap_or(0) + 1;
        self.current_tab_mut().detail_scroll = target.min(u16::MAX as usize) as u16;
        if self.current_tab().pane == Pane::Logs {
            self.logs.auto_scroll = false;
        }
        self.status_line = format!("Match {match_pos}/{}", matches.len());
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

    async fn handle_command_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        match key.code {
            KeyCode::Esc => {
                if let Some(input) = self.command_input.take() {
                    if matches!(input.mode, CommandMode::Filter) {
                        if self.clear_active_filter_value() {
                            self.status_line = "Filter cleared".to_string();
                        } else {
                            self.status_line = "Filter canceled".to_string();
                        }
                    } else {
                        self.status_line = "Command canceled".to_string();
                    }
                }
                return Ok(false);
            }
            KeyCode::Backspace => {
                if let Some(input) = &mut self.command_input {
                    input.value.pop();
                    if matches!(input.mode, CommandMode::Filter) {
                        let filter = input.value.trim().to_string();
                        self.set_active_filter_value(filter.clone());
                        self.status_line = if filter.is_empty() {
                            "Filter cleared".to_string()
                        } else {
                            format!("Filter set: {filter}")
                        };
                    }
                }
                return Ok(false);
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = &mut self.command_input {
                    delete_previous_word(&mut input.value);
                    if matches!(input.mode, CommandMode::Filter) {
                        let filter = input.value.trim().to_string();
                        self.set_active_filter_value(filter.clone());
                        self.status_line = if filter.is_empty() {
                            "Filter cleared".to_string()
                        } else {
                            format!("Filter set: {filter}")
                        };
                    }
                }
                return Ok(false);
            }
            KeyCode::Tab => {
                self.autocomplete_command_input();
                return Ok(false);
            }
            KeyCode::Enter => {
                let Some(input) = self.command_input.take() else {
                    return Ok(false);
                };
                return Ok(self.execute_command_input(input).await);
            }
            KeyCode::Char(ch) => {
                if let Some(input) = &mut self.command_input {
                    input.value.push(ch);
                    if matches!(input.mode, CommandMode::Filter) {
                        let filter = input.value.trim().to_string();
                        self.set_active_filter_value(filter.clone());
                        self.status_line = if filter.is_empty() {
                            "Filter cleared".to_string()
                        } else {
                            format!("Filter set: {filter}")
                        };
                    }
                }
                return Ok(false);
            }
            _ => return Ok(false),
        }
    }

    async fn execute_command_input(&mut self, input: CommandInput) -> bool {
        match input.mode {
            CommandMode::Filter => {
                let filter = input.value.trim().to_string();
                self.set_active_filter_value(filter.clone());
                self.status_line = if filter.is_empty() {
                    "Filter cleared".to_string()
                } else {
                    format!("Filter set: {filter}")
                };
                false
            }
            CommandMode::Command => self.execute_colon_command(input.value.trim()).await,
        }
    }

    async fn execute_colon_command(&mut self, raw: &str) -> bool {
        if raw.is_empty() {
            self.status_line = "No command entered".to_string();
            return false;
        }
        self.record_command_history(raw.to_string());
        self.overlay = None;

        let parts: Vec<&str> = raw.split_whitespace().collect();
        let cmd = parts[0].trim_start_matches(':').to_ascii_lowercase();
        let args = &parts[1..];

        match cmd.as_str() {
            "q" | "quit" | "exit" => true,
            "help" | "?" => {
                self.status_line = "Commands: :ctx [name] | :ns [name|all] | :kind <kind> | :c | :sources | :pause | :resume | :tail | :resources | :clear | :quit | :all".to_string();
                false
            }
            "contexts" | "ctxs" => {
                self.show_contexts_overlay();
                false
            }
            "ctx" | "context" => {
                if args.is_empty() {
                    self.show_contexts_overlay();
                    return false;
                }
                let target = args[0];
                if let Some(idx) = self.tabs.iter().position(|tab| tab.context == target) {
                    self.active_tab = idx;
                    self.status_line = format!("Context: {}", self.current_tab().context);
                    self.overlay = None;
                    self.ensure_active_watch().await;
                } else {
                    self.status_line = format!("Unknown context: {target}");
                }
                false
            }
            "ns" | "namespace" => {
                if args.is_empty() {
                    self.set_active_kind(ResourceKind::Namespaces);
                    return false;
                }
                let target = args[0];
                let tab = self.current_tab_mut();
                if target.eq_ignore_ascii_case("all") {
                    tab.namespace = None;
                } else {
                    tab.namespace = Some(target.to_string());
                }
                self.status_line =
                    format!("Namespace: {}", tab.namespace.as_deref().unwrap_or("all"));
                false
            }
            "all" | "0" => {
                self.current_tab_mut().namespace = None;
                self.status_line = "Namespace: all".to_string();
                false
            }
            "kind" => {
                let Some(token) = args.first() else {
                    self.status_line = "Usage: :kind <pods|deploy|svc|...>".to_string();
                    return false;
                };
                match parse_resource_alias(token) {
                    ResourceAlias::Supported(kind) => self.execute_resource_command(kind, &[]),
                    ResourceAlias::Unsupported(resource) => {
                        self.status_line =
                            format!("Resource '{resource}' is recognized but not implemented yet");
                    }
                    ResourceAlias::Unknown => {
                        self.status_line = format!("Unknown kind: {token}");
                    }
                }
                false
            }
            "resources" | "res" | "aliases" => {
                self.show_resource_aliases_overlay();
                false
            }
            "clear" | "clear-filter" => {
                if self.current_tab().pane == Pane::Table {
                    self.current_tab_mut().filter.clear();
                } else {
                    self.current_tab_mut().detail_filter.clear();
                }
                self.status_line = "Filter cleared".to_string();
                false
            }
            "c" | "container" | "containers" => {
                self.open_container_picker_from_selection();
                false
            }
            "sources" | "src" => {
                self.show_log_sources_overlay();
                false
            }
            "pause" => {
                self.set_log_paused(true);
                false
            }
            "resume" => {
                self.set_log_paused(false);
                false
            }
            "tail" => {
                self.jump_logs_to_latest();
                false
            }
            "pulse" | "pulses" | "pu" | "xray" | "popeye" | "pop" | "plugins" | "plugin"
            | "screendump" | "sd" => {
                self.status_line =
                    format!("Command ':{cmd}' is recognized but not implemented yet");
                false
            }
            _ => match parse_resource_alias(cmd.as_str()) {
                ResourceAlias::Supported(kind) => {
                    self.execute_resource_command(kind, args);
                    false
                }
                ResourceAlias::Unsupported(resource) => {
                    self.status_line =
                        format!("Resource '{resource}' is recognized but not implemented yet");
                    false
                }
                ResourceAlias::Unknown => {
                    self.status_line = format!("Unknown command: :{raw}");
                    false
                }
            },
        }
    }

    fn autocomplete_command_input(&mut self) {
        let Some(input) = self.command_input.as_ref() else {
            return;
        };
        if !matches!(input.mode, CommandMode::Command) {
            return;
        }
        let current_value = input.value.clone();

        let trimmed = current_value.trim();
        if trimmed.is_empty() {
            if let Some(input) = &mut self.command_input {
                input.value = "ctx".to_string();
            }
            self.status_line = "Autocomplete: ctx".to_string();
            return;
        }

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.len() == 1 && !current_value.ends_with(' ') {
            let prefix = tokens[0].to_ascii_lowercase();
            let mut candidates: Vec<String> = command_names()
                .iter()
                .chain(resource_alias_names().iter())
                .filter(|candidate| candidate.starts_with(&prefix))
                .map(|candidate| (*candidate).to_string())
                .collect();
            candidates.sort();
            candidates.dedup();
            self.apply_completion_for_first_token(candidates, &prefix);
            return;
        }

        let command = tokens[0].to_ascii_lowercase();
        let arg_prefix = if current_value.ends_with(' ') {
            ""
        } else {
            tokens.last().copied().unwrap_or("")
        };

        match command.as_str() {
            "ctx" | "context" => {
                let prefix = arg_prefix.to_ascii_lowercase();
                let candidates: Vec<String> = self
                    .tabs
                    .iter()
                    .map(|tab| tab.context.clone())
                    .filter(|ctx| ctx.to_ascii_lowercase().starts_with(&prefix))
                    .collect();
                self.apply_completion_for_argument(&command, candidates, arg_prefix);
            }
            "kind" => {
                let prefix = arg_prefix.to_ascii_lowercase();
                let candidates: Vec<String> = resource_alias_names()
                    .iter()
                    .filter(|alias| alias.starts_with(&prefix))
                    .map(|alias| (*alias).to_string())
                    .collect();
                self.apply_completion_for_argument(&command, candidates, &prefix);
            }
            _ => {
                self.status_line = "No autocomplete candidates".to_string();
            }
        }
    }

    fn apply_completion_for_first_token(&mut self, candidates: Vec<String>, prefix: &str) {
        let Some(input) = &mut self.command_input else {
            return;
        };
        if candidates.is_empty() {
            self.status_line = "No autocomplete candidates".to_string();
            return;
        }

        if candidates.len() == 1 {
            input.value = candidates[0].clone();
            self.status_line = format!("Autocomplete: {}", candidates[0]);
            return;
        }

        let common = common_prefix(&candidates);
        if common.len() > prefix.len() {
            input.value = common.clone();
            self.status_line = format!("Autocomplete: {common}");
        } else {
            self.status_line = format!(
                "Matches: {}",
                candidates
                    .iter()
                    .take(8)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    fn apply_completion_for_argument(
        &mut self,
        command: &str,
        mut candidates: Vec<String>,
        prefix: &str,
    ) {
        let Some(input) = &mut self.command_input else {
            return;
        };
        if candidates.is_empty() {
            self.status_line = "No autocomplete candidates".to_string();
            return;
        }
        candidates.sort();
        candidates.dedup();

        if candidates.len() == 1 {
            input.value = format!("{command} {}", candidates[0]);
            self.status_line = format!("Autocomplete: {}", candidates[0]);
            return;
        }

        let common = common_prefix(&candidates);
        if common.len() > prefix.len() {
            input.value = format!("{command} {common}");
            self.status_line = format!("Autocomplete: {common}");
        } else {
            self.status_line = format!(
                "Matches: {}",
                candidates
                    .iter()
                    .take(8)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    fn record_command_history(&mut self, command: String) {
        if self.command_history.last() != Some(&command) {
            self.command_history.push(command.clone());
        }
        self.last_command = Some(command);
        self.history_cursor = None;
    }

    fn history_step_back(&mut self) -> Option<String> {
        if self.command_history.is_empty() {
            return None;
        }
        let next = match self.history_cursor {
            None => self.command_history.len().saturating_sub(1),
            Some(0) => 0,
            Some(idx) => idx.saturating_sub(1),
        };
        self.history_cursor = Some(next);
        self.command_history.get(next).cloned()
    }

    fn history_step_forward(&mut self) -> Option<String> {
        let Some(current) = self.history_cursor else {
            return None;
        };
        if current + 1 >= self.command_history.len() {
            self.history_cursor = None;
            return None;
        }
        let next = current + 1;
        self.history_cursor = Some(next);
        self.command_history.get(next).cloned()
    }

    fn execute_resource_command(&mut self, kind: ResourceKind, args: &[&str]) {
        self.set_active_kind(kind);

        let mut idx = 0usize;
        while idx < args.len() {
            let arg = args[idx];

            if let Some(ctx) = arg.strip_prefix('@') {
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.context == ctx) {
                    self.active_tab = tab_idx;
                } else {
                    self.status_line = format!("Unknown context: {ctx}");
                }
            } else if let Some(filter) = arg.strip_prefix('/') {
                self.current_tab_mut().filter = filter.to_string();
            } else if let Some(ctx) = arg.strip_prefix("--context=") {
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.context == ctx) {
                    self.active_tab = tab_idx;
                } else {
                    self.status_line = format!("Unknown context: {ctx}");
                }
            } else if arg == "--context" && idx + 1 < args.len() {
                idx += 1;
                let ctx = args[idx];
                if let Some(tab_idx) = self.tabs.iter().position(|tab| tab.context == ctx) {
                    self.active_tab = tab_idx;
                } else {
                    self.status_line = format!("Unknown context: {ctx}");
                }
            } else if arg == "-A" || arg == "--all-namespaces" {
                self.current_tab_mut().namespace = None;
            } else if let Some(ns) = arg.strip_prefix("--namespace=") {
                self.current_tab_mut().namespace = if ns.eq_ignore_ascii_case("all") {
                    None
                } else {
                    Some(ns.to_string())
                };
            } else if let Some(ns) = arg.strip_prefix("-n=") {
                self.current_tab_mut().namespace = if ns.eq_ignore_ascii_case("all") {
                    None
                } else {
                    Some(ns.to_string())
                };
            } else if arg == "-n" && idx + 1 < args.len() {
                idx += 1;
                let ns = args[idx];
                self.current_tab_mut().namespace = if ns.eq_ignore_ascii_case("all") {
                    None
                } else {
                    Some(ns.to_string())
                };
            } else if arg == "--namespace" && idx + 1 < args.len() {
                idx += 1;
                let ns = args[idx];
                self.current_tab_mut().namespace = if ns.eq_ignore_ascii_case("all") {
                    None
                } else {
                    Some(ns.to_string())
                };
            } else if arg == "-l" && idx + 1 < args.len() {
                idx += 1;
                self.current_tab_mut().filter = args[idx].to_string();
            } else if let Some(selector) = arg.strip_prefix("-l=") {
                self.current_tab_mut().filter = selector.to_string();
            } else if arg.contains('=') || arg.contains(',') {
                self.current_tab_mut().filter = arg.to_string();
            } else if !arg.starts_with('-') {
                self.current_tab_mut().namespace = if arg.eq_ignore_ascii_case("all") {
                    None
                } else {
                    Some(arg.to_string())
                };
            }

            idx += 1;
        }

        let tab = self.current_tab();
        self.status_line = format!(
            "Kind: {} | ctx: {} | ns: {} | filter: {}",
            tab.kind(),
            tab.context,
            tab.namespace.as_deref().unwrap_or("all"),
            if tab.filter.is_empty() {
                "-"
            } else {
                tab.filter.as_str()
            }
        );
    }

    fn show_contexts_overlay(&mut self) {
        let contexts = self.tabs.iter().map(|tab| tab.context.clone()).collect();
        self.overlay = Some(Overlay::Contexts {
            title: "Contexts".to_string(),
            contexts,
            selected: self.active_tab,
            filter: String::new(),
        });
        self.status_line = "Context list opened".to_string();
    }

    fn show_log_sources_overlay(&mut self) {
        let sources = self.available_log_sources();
        self.overlay = Some(Overlay::LogSources {
            title: "Log Sources".to_string(),
            sources,
            selected: 0,
            filter: String::new(),
        });
        self.status_line = "Log source filter opened".to_string();
    }

    fn open_container_picker_from_selection(&mut self) {
        let Some(row) = self.selected_row() else {
            self.status_line = "No resource selected".to_string();
            return;
        };
        if row.key.kind != ResourceKind::Pods {
            self.status_line = "Container picker requires a selected pod".to_string();
            return;
        }
        let mut containers = self.pod_container_names_for_key(&row.key);
        containers.sort();
        containers.dedup();
        if containers.is_empty() {
            self.status_line = "Selected pod has no containers".to_string();
            return;
        }
        containers.insert(0, "* all containers".to_string());
        self.overlay = Some(Overlay::Containers {
            title: format!(
                "Containers {}/{}",
                row.key
                    .namespace
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
                row.key.name
            ),
            pod: row.key.clone(),
            containers,
            selected: 0,
            filter: String::new(),
        });
        self.status_line = "Container picker opened".to_string();
    }

    fn show_resource_aliases_overlay(&mut self) {
        let lines = vec![
            "Supported resource aliases:".to_string(),
            "po|pod, deploy|dp, rs, sts, ds, svc, ing, cm, sec|secret, job, cj".to_string(),
            "pvc|claim, pv, no|node, ns|namespace, ev|event, sa, role, rb".to_string(),
            "crole, crb, netpol|np, hpa, pdb".to_string(),
            String::new(),
            "Commands: :ctx :ns :kind :c :resources :clear :quit".to_string(),
            String::new(),
            "Examples:".to_string(),
            ":po".to_string(),
            ":po kube-system".to_string(),
            ":po /api".to_string(),
            ":po @my-context".to_string(),
            ":svc -A".to_string(),
            ":deploy -l app=my-api".to_string(),
            ":po --context arn:aws:eks:... --namespace kube-system".to_string(),
            String::new(),
            "Recognized but not implemented yet (examples):".to_string(),
            "crd, cr, ep, eps, rc, csr, sc, ingclass, quota, limits, lease".to_string(),
        ];
        self.overlay = Some(Overlay::Text {
            title: "Resource Aliases".to_string(),
            lines,
            scroll: 0,
            hscroll: 0,
            wrap: true,
        });
        self.status_line = "Resource aliases opened".to_string();
    }

    async fn handle_overlay_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                self.status_line = "Closed view".to_string();
            }
            KeyCode::Char('/') => {
                let existing_filter = self.active_filter_value();
                self.command_input = Some(CommandInput::new(CommandMode::Filter, existing_filter));
                self.status_line = "Filter mode".to_string();
            }
            KeyCode::Char('w') => {
                if let Some(Overlay::Text { wrap, hscroll, .. }) = &mut self.overlay {
                    *wrap = !*wrap;
                    if *wrap {
                        *hscroll = 0;
                        self.status_line = "Wrap: on".to_string();
                    } else {
                        self.status_line =
                            "Wrap: off (use left/right to scroll horizontally)".to_string();
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(Overlay::Contexts {
                    selected, contexts, ..
                }) = &self.overlay
                {
                    if *selected >= contexts.len() {
                        self.status_line = "No context selected".to_string();
                        return;
                    }
                    self.active_tab = *selected;
                    self.overlay = None;
                    self.status_line = format!("Context: {}", self.current_tab().context);
                    self.ensure_active_watch().await;
                    return;
                }

                if let Some(Overlay::Containers {
                    selected,
                    containers,
                    pod,
                    ..
                }) = &self.overlay
                {
                    if *selected >= containers.len() {
                        self.status_line = "No container selected".to_string();
                        return;
                    }
                    let selected_name = containers[*selected].clone();
                    let pod_key = pod.clone();
                    self.overlay = None;
                    self.current_tab_mut().pane = Pane::Logs;
                    self.current_tab_mut().detail_scroll = 0;
                    self.current_tab_mut().detail_hscroll = 0;
                    self.current_tab_mut().detail_wrap = false;
                    self.logs.auto_scroll = true;
                    if selected_name == "* all containers" {
                        self.logs.container_override = None;
                        self.logs.container_override_pod = Some(pod_key.clone());
                        self.status_line = format!(
                            "Container selection: all ({}/{})",
                            pod_key.namespace.as_deref().unwrap_or("default"),
                            pod_key.name
                        );
                    } else {
                        self.logs.container_override = Some(selected_name.clone());
                        self.logs.container_override_pod = Some(pod_key.clone());
                        self.status_line = format!(
                            "Container selected: {}/{}/{}",
                            pod_key.namespace.as_deref().unwrap_or("default"),
                            pod_key.name,
                            selected_name
                        );
                    }
                    self.ensure_active_watch().await;
                    return;
                }

                if let Some(Overlay::LogSources {
                    selected, sources, ..
                }) = &self.overlay
                {
                    if *selected >= sources.len() {
                        self.status_line = "No log source selected".to_string();
                        return;
                    }
                    let source = sources[*selected].clone();
                    if self.toggle_log_source(&source) {
                        let hidden = self.logs.hidden_sources.contains(&source);
                        self.status_line = if hidden {
                            format!("Log source hidden: {source}")
                        } else {
                            format!("Log source shown: {source}")
                        };
                    }
                }
            }
            KeyCode::Char(' ') => {
                if let Some(Overlay::LogSources {
                    selected, sources, ..
                }) = &self.overlay
                {
                    if *selected < sources.len() {
                        let source = sources[*selected].clone();
                        if self.toggle_log_source(&source) {
                            let hidden = self.logs.hidden_sources.contains(&source);
                            self.status_line = if hidden {
                                format!("Log source hidden: {source}")
                            } else {
                                format!("Log source shown: {source}")
                            };
                        }
                    }
                }
            }
            KeyCode::Char('a') => {
                if matches!(self.overlay, Some(Overlay::LogSources { .. })) {
                    if self.logs.hidden_sources.is_empty() {
                        self.status_line = "All log sources already visible".to_string();
                    } else {
                        self.logs.hidden_sources.clear();
                        self.logs.source_filter_version =
                            self.logs.source_filter_version.wrapping_add(1);
                        self.status_line = "All log sources enabled".to_string();
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_overlay_or_select(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_overlay_or_select(1);
            }
            KeyCode::Left => {
                self.scroll_overlay_horizontal(-4);
            }
            KeyCode::Right => {
                self.scroll_overlay_horizontal(4);
            }
            KeyCode::PageUp => {
                self.scroll_overlay_or_select(-10);
            }
            KeyCode::PageDown => {
                self.scroll_overlay_or_select(10);
            }
            KeyCode::Home => {
                self.overlay_home();
            }
            KeyCode::End => {
                self.overlay_end();
            }
            _ => {}
        }
    }

    fn scroll_overlay_or_select(&mut self, delta: isize) {
        match &mut self.overlay {
            Some(Overlay::Text { scroll, .. }) => {
                if delta < 0 {
                    *scroll = scroll.saturating_sub(delta.unsigned_abs() as u16);
                } else {
                    *scroll = scroll.saturating_add(delta as u16);
                }
            }
            Some(Overlay::Contexts {
                contexts,
                selected,
                filter,
                ..
            }) => {
                let filtered = context_filtered_indices(contexts, filter);
                if filtered.is_empty() {
                    *selected = 0;
                    return;
                }
                let current_pos = filtered
                    .iter()
                    .position(|idx| *idx == *selected)
                    .unwrap_or(0);
                let new_pos = if delta < 0 {
                    current_pos.saturating_sub(delta.unsigned_abs())
                } else {
                    (current_pos + delta as usize).min(filtered.len() - 1)
                };
                *selected = filtered[new_pos];
            }
            Some(Overlay::Containers {
                containers,
                selected,
                filter,
                ..
            }) => {
                let filtered = list_filtered_indices(containers, filter);
                if filtered.is_empty() {
                    *selected = 0;
                    return;
                }
                let current_pos = filtered
                    .iter()
                    .position(|idx| *idx == *selected)
                    .unwrap_or(0);
                let new_pos = if delta < 0 {
                    current_pos.saturating_sub(delta.unsigned_abs())
                } else {
                    (current_pos + delta as usize).min(filtered.len() - 1)
                };
                *selected = filtered[new_pos];
            }
            Some(Overlay::LogSources {
                sources,
                selected,
                filter,
                ..
            }) => {
                let filtered = list_filtered_indices(sources, filter);
                if filtered.is_empty() {
                    *selected = 0;
                    return;
                }
                let current_pos = filtered
                    .iter()
                    .position(|idx| *idx == *selected)
                    .unwrap_or(0);
                let new_pos = if delta < 0 {
                    current_pos.saturating_sub(delta.unsigned_abs())
                } else {
                    (current_pos + delta as usize).min(filtered.len() - 1)
                };
                *selected = filtered[new_pos];
            }
            None => {}
        }
    }

    fn scroll_overlay_horizontal(&mut self, delta: isize) {
        if let Some(Overlay::Text { hscroll, wrap, .. }) = &mut self.overlay {
            if *wrap {
                return;
            }
            if delta < 0 {
                *hscroll = hscroll.saturating_sub(delta.unsigned_abs() as u16);
            } else {
                *hscroll = hscroll.saturating_add(delta as u16);
            }
        }
    }

    fn overlay_home(&mut self) {
        match &mut self.overlay {
            Some(Overlay::Text { scroll, .. }) => *scroll = 0,
            Some(Overlay::Contexts {
                contexts,
                selected,
                filter,
                ..
            }) => {
                let filtered = context_filtered_indices(contexts, filter);
                *selected = filtered.first().copied().unwrap_or(0);
            }
            Some(Overlay::Containers {
                containers,
                selected,
                filter,
                ..
            }) => {
                let filtered = list_filtered_indices(containers, filter);
                *selected = filtered.first().copied().unwrap_or(0);
            }
            Some(Overlay::LogSources {
                sources,
                selected,
                filter,
                ..
            }) => {
                let filtered = list_filtered_indices(sources, filter);
                *selected = filtered.first().copied().unwrap_or(0);
            }
            None => {}
        }
    }

    fn overlay_end(&mut self) {
        match &mut self.overlay {
            Some(Overlay::Text { scroll, .. }) => *scroll = u16::MAX,
            Some(Overlay::Contexts {
                contexts,
                selected,
                filter,
                ..
            }) => {
                let filtered = context_filtered_indices(contexts, filter);
                *selected = filtered.last().copied().unwrap_or(0);
            }
            Some(Overlay::Containers {
                containers,
                selected,
                filter,
                ..
            }) => {
                let filtered = list_filtered_indices(containers, filter);
                *selected = filtered.last().copied().unwrap_or(0);
            }
            Some(Overlay::LogSources {
                sources,
                selected,
                filter,
                ..
            }) => {
                let filtered = list_filtered_indices(sources, filter);
                *selected = filtered.last().copied().unwrap_or(0);
            }
            None => {}
        }
    }

    fn scroll_detail(&mut self, delta: isize) {
        if self.current_tab().pane == Pane::Logs && delta != 0 {
            self.logs.auto_scroll = false;
        }
        let tab = self.current_tab_mut();
        if delta < 0 {
            tab.detail_scroll = tab
                .detail_scroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            tab.detail_scroll = tab.detail_scroll.saturating_add(delta as u16);
        }
    }

    fn scroll_detail_horizontal(&mut self, delta: isize) {
        let tab = self.current_tab_mut();
        if delta < 0 {
            tab.detail_hscroll = tab
                .detail_hscroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            tab.detail_hscroll = tab.detail_hscroll.saturating_add(delta as u16);
        }
    }

    fn toggle_detail_wrap(&mut self) {
        let tab = self.current_tab_mut();
        tab.detail_wrap = !tab.detail_wrap;
        if tab.detail_wrap {
            tab.detail_hscroll = 0;
            self.status_line = "Wrap: on".to_string();
        } else {
            self.status_line = "Wrap: off (use left/right to scroll horizontally)".to_string();
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

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let theme = ui_theme();
        let active = self.current_tab().clone();
        let active_tab_idx = self.active_tab;
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let visible_rows = vm.rows.len();
        let max_selection = visible_rows.saturating_sub(1);
        let selected = active.selected.min(max_selection);
        self.current_tab_mut().selected = selected;

        let pane_label = match active.pane {
            Pane::Table => "table",
            Pane::Describe => "describe",
            Pane::Events => "events",
            Pane::Logs => "logs",
        };
        let ns_label = if active.kind().is_namespaced() {
            active.namespace.as_deref().unwrap_or("all")
        } else {
            "cluster"
        };
        let now = Local::now().format("%Y-%m-%d %H:%M:%S");
        let selected_human = if visible_rows == 0 { 0 } else { selected + 1 };
        let hb = heartbeat_icon();
        let ctx_short = compact_context_name(&active.context);
        let mut top_line = format!(
            "{} {}  [CTX] {} ({}/{})  [NS] {}  [K] {}  [P] {} {}  [SEL] {selected_human}/{}  [VIS] {}  [CACHE] {}  [ERR] {}",
            hb,
            now,
            ctx_short,
            self.active_tab + 1,
            self.tabs.len(),
            ns_label,
            active.kind().short_name(),
            pane_label,
            pane_icon(active.pane),
            visible_rows,
            visible_rows,
            self.store.entity_count(),
            self.store.error_count(),
        );
        let active_filter = if active.pane == Pane::Table {
            active.filter.as_str()
        } else {
            active.detail_filter.as_str()
        };
        if !active_filter.is_empty() {
            top_line.push_str("  [F]");
            top_line.push_str(active_filter);
        }
        if active.pane == Pane::Logs {
            top_line.push_str("  [LOG]");
            top_line.push_str(logs_state_icon(&self.logs));
            top_line.push_str(" tail:");
            top_line.push_str(if self.logs.auto_scroll { "on" } else { "off" });
        }
        if active.kind() == ResourceKind::Pods
            && let Some(row) = vm.rows.get(selected)
            && let Some(entity) = self.store.get(&row.key)
        {
            let (cpu_req, cpu_lim, mem_req, mem_lim) = pod_resources_from_raw(&entity.raw);
            top_line.push_str(&format!(
                "  [SEL-RES] cpu {}/{} mem {}/{}",
                format_millicpu(cpu_req),
                format_millicpu(cpu_lim),
                format_bytes(mem_req),
                format_bytes(mem_lim)
            ));
        }

        let (running, pending, failed, other) = self.pod_phase_counts_for_tab(&active);
        let scope_pods = self.pod_resource_totals(&active.context, active.namespace.as_deref());
        let cluster_pods = self.pod_resource_totals(&active.context, None);
        let node_caps = self.node_capacity_totals(&active.context);
        let cpu_pct = percent(cluster_pods.cpu_request_m, node_caps.cpu_alloc_m);
        let mem_pct = percent(cluster_pods.mem_request_b, node_caps.mem_alloc_b);
        let pod_pct = percent(cluster_pods.pods as u64, node_caps.pod_alloc);
        let deployments = self.count_kind_for_tab(&active, ResourceKind::Deployments);
        let statefulsets = self.count_kind_for_tab(&active, ResourceKind::StatefulSets);
        let daemonsets = self.count_kind_for_tab(&active, ResourceKind::DaemonSets);
        let services = self.count_kind_for_tab(&active, ResourceKind::Services);
        let ingresses = self.count_kind_for_tab(&active, ResourceKind::Ingresses);
        let jobs = self.count_kind_for_tab(&active, ResourceKind::Jobs);
        let cronjobs = self.count_kind_for_tab(&active, ResourceKind::CronJobs);

        let scope_label = active.namespace.as_deref().unwrap_or("all namespaces");
        let watch_health = if self.store.error_for_context(&active.context).is_some() {
            "[XX]"
        } else {
            "[OK]"
        };
        let pod_health = health_icon(failed, pending);
        let mut pulse_sections = vec![
            format!(
                "[SYS] {}  [WATCH] {}  [POD] {}  [LOG] {}  Context:{} ({}/{})  Scope:{}",
                hb,
                watch_health,
                pod_health,
                logs_state_icon(&self.logs),
                ctx_short,
                self.active_tab + 1,
                self.tabs.len(),
                scope_label
            ),
            format!(
                "[CLUSTER] CPU req/alloc {}/{} ({})  MEM req/alloc {}/{} ({})  PODS {}/{} ({})",
                format_millicpu(cluster_pods.cpu_request_m),
                format_millicpu(node_caps.cpu_alloc_m),
                cpu_pct,
                format_bytes(cluster_pods.mem_request_b),
                format_bytes(node_caps.mem_alloc_b),
                mem_pct,
                cluster_pods.pods,
                node_caps.pod_alloc,
                pod_pct
            ),
            format!(
                "[SCOPE] CPU req/lim {}/{}  MEM req/lim {}/{}",
                format_millicpu(scope_pods.cpu_request_m),
                format_millicpu(scope_pods.cpu_limit_m),
                format_bytes(scope_pods.mem_request_b),
                format_bytes(scope_pods.mem_limit_b),
            ),
            format!(
                "[HEALTH] Pods run:{} pend:{} fail:{} other:{}  Nodes ready:{}/{} unsched:{}",
                running,
                pending,
                failed,
                other,
                node_caps.nodes_ready,
                node_caps.nodes_total,
                node_caps.nodes_unschedulable
            ),
            format!(
                "[WORKLOADS] dp:{}  sts:{}  ds:{}  svc:{}  ing:{}  job:{}  cj:{}",
                deployments, statefulsets, daemonsets, services, ingresses, jobs, cronjobs
            ),
        ];
        if let Some(err) = self.store.error_for_context(&active.context) {
            pulse_sections.push(format!("[ALERT] API/watch error: {}", err));
        }
        let pulse_text = pulse_sections.join("\n");
        let help_text = "ctrl+c quit | : command | / ? filter/search | n/N next/prev | gg/G top/bottom | ctrl+d/u half-page | [ ] history | - repeat | ctrl+a aliases | tab switch-ctx | j/k move/scroll | left/right h-scroll (wrap off) | w wrap toggle | d describe | l logs(stream) | s tail on/off | p pause/resume logs | S sources | L latest | c container picker | ctrl+d delete(table) | ctrl+k kill";
        let help_height = if self.show_help {
            max_vertical_scroll_for_text(help_text, frame.area().width.max(1), 1, true) + 1
        } else {
            0
        };
        // 2 columns removed for borders + 2 columns for left/right block padding.
        let pulse_width = frame.area().width.saturating_sub(4).max(1);
        let pulse_lines = max_vertical_scroll_for_text(&pulse_text, pulse_width, 1, true) + 1;
        let pulse_min_height = 3;
        let content_min_height = 6;
        let reserved_height = 1 + content_min_height + 1 + help_height + 1;
        let max_pulse_height = frame
            .area()
            .height
            .saturating_sub(reserved_height)
            .max(pulse_min_height);
        let pulse_height = pulse_lines
            .saturating_add(2)
            .max(pulse_min_height)
            .min(max_pulse_height);

        let mut constraints = vec![
            Constraint::Length(1),
            Constraint::Length(pulse_height),
            Constraint::Min(content_min_height),
            Constraint::Length(1),
        ];
        if self.show_help {
            constraints.push(Constraint::Length(help_height));
        }
        constraints.push(Constraint::Length(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        let top_status = Paragraph::new(Line::from(top_line)).style(theme.header);
        frame.render_widget(top_status, chunks[0]);

        let pulse_widget = Paragraph::new(pulse_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("[PULSE] Resource Pulse")
                    .padding(Padding::new(1, 1, 0, 0)),
            )
            .wrap(Wrap { trim: false })
            .style(theme.block);
        frame.render_widget(pulse_widget, chunks[1]);

        if let Some(overlay) = &mut self.overlay {
            match overlay {
                Overlay::Text {
                    title,
                    lines,
                    scroll,
                    hscroll,
                    wrap,
                } => {
                    let body = lines.join("\n");
                    let content_width = chunks[2].width.saturating_sub(2);
                    let content_height = chunks[2].height.saturating_sub(2);
                    let max_v =
                        max_vertical_scroll_for_text(&body, content_width, content_height, *wrap);
                    let max_h = max_horizontal_scroll_for_text(&body, content_width, *wrap);
                    *scroll = (*scroll).min(max_v);
                    *hscroll = if *wrap { 0 } else { (*hscroll).min(max_h) };

                    let mut paragraph = Paragraph::new(body)
                        .block(Block::default().borders(Borders::ALL).title(format!(
                            "[TXT] {} | wrap:{}",
                            title,
                            if *wrap { "on" } else { "off" }
                        )))
                        .scroll((*scroll, *hscroll))
                        .style(theme.block);
                    if *wrap {
                        paragraph = paragraph.wrap(Wrap { trim: false });
                    }
                    frame.render_widget(paragraph, chunks[2]);
                }
                Overlay::Contexts {
                    title,
                    contexts,
                    selected,
                    filter,
                } => {
                    let filtered = context_filtered_indices(contexts, filter);
                    if filtered.is_empty() {
                        *selected = 0;
                    } else if !filtered.contains(selected) {
                        *selected = filtered[0];
                    }
                    let rows: Vec<Row<'_>> = if filtered.is_empty() {
                        vec![Row::new(vec![
                            Cell::from(" "),
                            Cell::from(format!("No contexts match '{}'", filter)),
                        ])]
                    } else {
                        filtered
                            .iter()
                            .map(|idx| {
                                let context = &contexts[*idx];
                                let marker = if *idx == active_tab_idx { "*" } else { " " };
                                Row::new(vec![Cell::from(marker), Cell::from(context.clone())])
                            })
                            .collect()
                    };

                    let table = Table::new(rows, [Constraint::Length(2), Constraint::Min(10)])
                        .header(Row::new(vec!["", "Context"]).style(theme.table_header))
                        .block(Block::default().borders(Borders::ALL).title(format!(
                            "[CTX] {title} (Enter switch, '/' filter, Esc close) | filter:{}",
                            if filter.is_empty() {
                                "-"
                            } else {
                                filter.as_str()
                            }
                        )))
                        .row_highlight_style(theme.row_highlight);

                    let selected_visible = if filtered.is_empty() {
                        None
                    } else {
                        filtered.iter().position(|idx| idx == selected)
                    };
                    let mut state =
                        ratatui::widgets::TableState::default().with_selected(selected_visible);
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
                Overlay::Containers {
                    title,
                    containers,
                    selected,
                    filter,
                    ..
                } => {
                    let filtered = list_filtered_indices(containers, filter);
                    if filtered.is_empty() {
                        *selected = 0;
                    } else if !filtered.contains(selected) {
                        *selected = filtered[0];
                    }
                    let rows: Vec<Row<'_>> = if filtered.is_empty() {
                        vec![Row::new(vec![Cell::from(format!(
                            "No containers match '{}'",
                            filter
                        ))])]
                    } else {
                        filtered
                            .iter()
                            .map(|idx| Row::new(vec![Cell::from(containers[*idx].clone())]))
                            .collect()
                    };

                    let table = Table::new(rows, [Constraint::Min(10)])
                        .header(Row::new(vec!["Container"]).style(theme.table_header))
                        .block(Block::default().borders(Borders::ALL).title(format!(
                            "[CTR] {title} (Enter select, '/' filter, Esc close) | filter:{}",
                            if filter.is_empty() {
                                "-"
                            } else {
                                filter.as_str()
                            }
                        )))
                        .row_highlight_style(theme.row_highlight);

                    let selected_visible = if filtered.is_empty() {
                        None
                    } else {
                        filtered.iter().position(|idx| idx == selected)
                    };
                    let mut state =
                        ratatui::widgets::TableState::default().with_selected(selected_visible);
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
                Overlay::LogSources {
                    title,
                    sources,
                    selected,
                    filter,
                } => {
                    let filtered = list_filtered_indices(sources, filter);
                    if filtered.is_empty() {
                        *selected = 0;
                    } else if !filtered.contains(selected) {
                        *selected = filtered[0];
                    }
                    let rows: Vec<Row<'_>> = if filtered.is_empty() {
                        vec![Row::new(vec![
                            Cell::from(" "),
                            Cell::from(format!("No sources match '{}'", filter)),
                        ])]
                    } else {
                        filtered
                            .iter()
                            .map(|idx| {
                                let source = &sources[*idx];
                                let marker = if self.logs.hidden_sources.contains(source) {
                                    "off"
                                } else {
                                    "on"
                                };
                                Row::new(vec![Cell::from(marker), Cell::from(source.clone())])
                            })
                            .collect()
                    };

                    let table = Table::new(rows, [Constraint::Length(4), Constraint::Min(10)])
                        .header(Row::new(vec!["Use", "Source"]).style(theme.table_header))
                        .block(Block::default().borders(Borders::ALL).title(format!(
                            "[SRC] {title} (Enter/Space toggle, 'a' show all, '/' filter, Esc close) | filter:{}",
                            if filter.is_empty() {
                                "-"
                            } else {
                                filter.as_str()
                            }
                        )))
                        .row_highlight_style(theme.row_highlight);

                    let selected_visible = if filtered.is_empty() {
                        None
                    } else {
                        filtered.iter().position(|idx| idx == selected)
                    };
                    let mut state =
                        ratatui::widgets::TableState::default().with_selected(selected_visible);
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
            }
        } else {
            match active.pane {
                Pane::Table => {
                    let rows: Vec<Row<'_>> = vm
                        .rows
                        .iter()
                        .map(|row| {
                            Row::new(vec![
                                Cell::from(row.namespace.clone()),
                                Cell::from(row.name.clone()),
                                Cell::from(row.status.clone()),
                                Cell::from(row.age.clone()),
                                Cell::from(row.summary.clone()),
                            ])
                        })
                        .collect();

                    let table = Table::new(
                        rows,
                        [
                            Constraint::Length(18),
                            Constraint::Length(38),
                            Constraint::Length(20),
                            Constraint::Length(10),
                            Constraint::Min(10),
                        ],
                    )
                    .header(
                        Row::new(vec!["Namespace", "Name", "Status", "Age", "Summary"])
                            .style(theme.table_header),
                    )
                    .block(Block::default().borders(Borders::ALL).title(format!(
                        "[KIND] {} ({})",
                        active.kind(),
                        vm.rows.len()
                    )))
                    .row_highlight_style(theme.row_highlight);

                    let mut state =
                        ratatui::widgets::TableState::default().with_selected(Some(selected));
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
                Pane::Describe | Pane::Events => {
                    let raw_body = self.detail_text(&vm.rows, selected, active.pane);
                    let body = if active.detail_filter.trim().is_empty() {
                        raw_body
                    } else {
                        filter_text_lines(&raw_body, &active.detail_filter)
                    };
                    let content_width = chunks[2].width.saturating_sub(2);
                    let content_height = chunks[2].height.saturating_sub(2);
                    self.detail_page_step = (content_height / 2).max(1);
                    let detail_wrap = active.detail_wrap;
                    let max_v = max_vertical_scroll_for_text(
                        &body,
                        content_width,
                        content_height,
                        detail_wrap,
                    );
                    let max_h = max_horizontal_scroll_for_text(&body, content_width, detail_wrap);
                    let (detail_scroll, detail_hscroll, detail_wrap) = {
                        let tab = self.current_tab_mut();
                        tab.detail_scroll = tab.detail_scroll.min(max_v);
                        tab.detail_hscroll = if tab.detail_wrap {
                            0
                        } else {
                            tab.detail_hscroll.min(max_h)
                        };
                        (tab.detail_scroll, tab.detail_hscroll, tab.detail_wrap)
                    };
                    let pane_title = match active.pane {
                        Pane::Describe => "Describe",
                        Pane::Events => "Events",
                        Pane::Logs => "Logs",
                        Pane::Table => "Table",
                    };
                    let search_query = active.detail_filter.trim();
                    let match_lines = search_match_lines(&body, search_query);
                    let total_lines = body.lines().count();
                    let title = detail_viewer_title(
                        pane_title,
                        detail_wrap,
                        search_query,
                        &match_lines,
                        detail_scroll,
                        total_lines,
                    );
                    let mut paragraph = Paragraph::new(highlighted_text(&body, search_query))
                        .block(Block::default().borders(Borders::ALL).title(title))
                        .scroll((detail_scroll, detail_hscroll))
                        .style(theme.block);
                    if detail_wrap {
                        paragraph = paragraph.wrap(Wrap { trim: false });
                    }
                    frame.render_widget(paragraph, chunks[2]);
                }
                Pane::Logs => {
                    let (logs_line_count, logs_max_line_width) =
                        self.filtered_log_line_count_and_width();
                    let content_width = chunks[2].width.saturating_sub(2);
                    let content_height = chunks[2].height.saturating_sub(2);
                    self.detail_page_step = (content_height / 2).max(1);
                    let detail_wrap = active.detail_wrap;
                    let (max_v, max_h) = if detail_wrap {
                        let body = if logs_line_count > 0 {
                            if self.logs.hidden_sources.is_empty() {
                                self.log_joined_text().to_string()
                            } else {
                                self.filtered_log_body_text()
                            }
                        } else if !self.logs.lines.is_empty()
                            && !self.logs.hidden_sources.is_empty()
                        {
                            "No log lines match the current source filter.".to_string()
                        } else {
                            self.log_body_text()
                        };
                        (
                            max_vertical_scroll_for_text(
                                body.as_str(),
                                content_width,
                                content_height,
                                detail_wrap,
                            ),
                            0,
                        )
                    } else {
                        let viewport_h = content_height as usize;
                        let viewport_w = content_width as usize;
                        (
                            logs_line_count
                                .saturating_sub(viewport_h)
                                .min(u16::MAX as usize) as u16,
                            logs_max_line_width
                                .saturating_sub(viewport_w)
                                .min(u16::MAX as usize) as u16,
                        )
                    };
                    let detail_scroll = active.detail_scroll.min(max_v);
                    let detail_hscroll = if detail_wrap {
                        0
                    } else {
                        active.detail_hscroll.min(max_h)
                    };
                    {
                        let tab = self.current_tab_mut();
                        tab.detail_scroll = detail_scroll;
                        tab.detail_hscroll = detail_hscroll;
                    }
                    let search_query = active.detail_filter.trim();
                    let match_lines = self.log_search_match_lines(search_query);
                    let total_lines = logs_line_count.max(1);
                    let title = format!(
                        "{} | {}",
                        self.logs_title(),
                        detail_viewer_title(
                            "VIEWER",
                            detail_wrap,
                            search_query,
                            match_lines.as_slice(),
                            detail_scroll,
                            total_lines
                        )
                    );
                    if detail_wrap {
                        let body = if logs_line_count > 0 {
                            if self.logs.hidden_sources.is_empty() {
                                self.log_joined_text().to_string()
                            } else {
                                self.filtered_log_body_text()
                            }
                        } else if !self.logs.lines.is_empty()
                            && !self.logs.hidden_sources.is_empty()
                        {
                            "No log lines match the current source filter.".to_string()
                        } else {
                            self.log_body_text()
                        };
                        let paragraph =
                            Paragraph::new(highlighted_text(body.as_str(), search_query))
                                .block(Block::default().borders(Borders::ALL).title(title))
                                .scroll((detail_scroll, detail_hscroll))
                                .style(theme.block)
                                .wrap(Wrap { trim: false });
                        frame.render_widget(paragraph, chunks[2]);
                    } else {
                        let viewport_h = content_height.max(1) as usize;
                        let viewport_w = content_width.max(1) as usize;
                        let start = detail_scroll as usize;
                        let mut visible = Vec::with_capacity(viewport_h.max(1));
                        let mut visible_idx = 0usize;
                        for line in &self.logs.lines {
                            if !is_visible_log_line(line, &self.logs.hidden_sources) {
                                continue;
                            }
                            if visible_idx < start {
                                visible_idx = visible_idx.saturating_add(1);
                                continue;
                            }
                            if visible.len() >= viewport_h {
                                break;
                            }
                            visible.push(slice_chars(line, detail_hscroll as usize, viewport_w));
                            visible_idx = visible_idx.saturating_add(1);
                        }
                        if visible.is_empty() {
                            if !self.logs.lines.is_empty() && !self.logs.hidden_sources.is_empty() {
                                visible.push(
                                    "No log lines match the current source filter.".to_string(),
                                );
                            } else {
                                visible.push(self.log_body_text());
                            }
                        }
                        let body = visible.join("\n");
                        let paragraph = Paragraph::new(highlighted_text(&body, search_query))
                            .block(Block::default().borders(Borders::ALL).title(title))
                            .style(theme.block);
                        frame.render_widget(paragraph, chunks[2]);
                    }
                }
            }
        }

        let mut status = self.status_line.clone();
        if self.readonly {
            status.push_str(" | READONLY");
        }
        if self.pending_confirmation.is_some() {
            status.push_str(" | Confirm with 'y'");
        }
        let status_widget =
            Paragraph::new(status).style(status_style_for_line(&theme, &self.status_line));
        frame.render_widget(status_widget, chunks[3]);

        if self.show_help {
            let help = Paragraph::new(help_text)
                .style(theme.help)
                .wrap(Wrap { trim: false });
            frame.render_widget(help, chunks[4]);
        }

        let command_idx = if self.show_help { 5 } else { 4 };
        let command_line = if let Some(input) = &self.command_input {
            format!("{}{}", input.prefix(), input.value)
        } else {
            "Command: ':' for commands, '/' for filter".to_string()
        };
        let command_style = if self.command_input.is_some() {
            theme.command_active
        } else {
            theme.command_idle
        };
        frame.render_widget(
            Paragraph::new(command_line).style(command_style),
            chunks[command_idx],
        );
    }

    fn detail_text(&self, rows: &[crate::view::ViewRow], selected: usize, pane: Pane) -> String {
        let Some(row) = rows.get(selected) else {
            return "No resource selected".to_string();
        };
        let Some(entity) = self.store.get(&row.key) else {
            return "Resource details unavailable".to_string();
        };

        match pane {
            Pane::Describe => {
                serde_json::to_string_pretty(&entity.raw).unwrap_or_else(|_| "{}".to_string())
            }
            Pane::Events => {
                if row.key.kind == ResourceKind::Events {
                    serde_json::to_string_pretty(&entity.raw).unwrap_or_else(|_| "{}".to_string())
                } else {
                    "Events pane currently supports Event resources directly; resource-scoped event correlation is planned in next milestone.".to_string()
                }
            }
            Pane::Logs => {
                "Log streaming is planned; this pane is wired for future pod log tailing."
                    .to_string()
            }
            Pane::Table => "".to_string(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let tab = self.current_tab_mut();
        if delta < 0 {
            tab.selected = tab.selected.saturating_sub(delta.unsigned_abs());
        } else {
            tab.selected = tab.selected.saturating_add(delta as usize);
        }
    }

    fn toggle_describe(&mut self) {
        let tab = self.current_tab_mut();
        tab.pane = if tab.pane == Pane::Describe {
            Pane::Table
        } else {
            Pane::Describe
        };
        tab.detail_scroll = 0;
        tab.detail_hscroll = 0;
        self.overlay = None;
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
            tab.pane = Pane::Table;
            tab.namespace.clone()
        };

        self.status_line = format!("Namespace filter: {}", ns_label.as_deref().unwrap_or("all"));
    }

    fn cycle_sort(&mut self) {
        let tab = self.current_tab_mut();
        tab.sort = match tab.sort {
            SortColumn::Name => SortColumn::Namespace,
            SortColumn::Namespace => SortColumn::Status,
            SortColumn::Status => SortColumn::Age,
            SortColumn::Age => SortColumn::Name,
        };
    }

    fn prepare_delete_confirmation(&mut self) {
        let active = self.current_tab().clone();
        let selected = active.selected;
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let Some(row) = vm.rows.get(selected.min(vm.rows.len().saturating_sub(1))) else {
            self.status_line = "No resource selected".to_string();
            return;
        };

        self.pending_confirmation = Some(PendingConfirmation {
            created_at: Instant::now(),
            ttl: Duration::from_secs(15),
            kind: ConfirmationKind::Delete(row.key.clone()),
        });
        self.status_line = format!("Delete {}? press y to confirm", row.key.name);
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
        let targets = Self::active_watch_targets(&tab);
        if let Err(err) = self
            .resource_provider
            .replace_watch_plan(&tab.context, &targets)
            .await
        {
            self.status_line = format!("watch setup error: {err}");
        }
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

enum ResourceAlias {
    Supported(ResourceKind),
    Unsupported(&'static str),
    Unknown,
}

fn command_names() -> &'static [&'static str] {
    &[
        "ctx",
        "context",
        "contexts",
        "ctxs",
        "ns",
        "namespace",
        "all",
        "0",
        "kind",
        "resources",
        "res",
        "aliases",
        "clear",
        "clear-filter",
        "c",
        "container",
        "containers",
        "sources",
        "src",
        "pause",
        "resume",
        "tail",
        "help",
        "?",
        "quit",
        "exit",
        "q",
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
    ]
}

fn resource_alias_names() -> &'static [&'static str] {
    &[
        "po",
        "pod",
        "pods",
        "deploy",
        "dp",
        "deployment",
        "deployments",
        "rs",
        "replicaset",
        "replicasets",
        "sts",
        "statefulset",
        "statefulsets",
        "ds",
        "daemonset",
        "daemonsets",
        "svc",
        "service",
        "services",
        "ing",
        "ingress",
        "ingresses",
        "cm",
        "configmap",
        "configmaps",
        "sec",
        "secret",
        "secrets",
        "job",
        "jobs",
        "cj",
        "cronjob",
        "cronjobs",
        "pvc",
        "pvcs",
        "claim",
        "claims",
        "pv",
        "pvs",
        "no",
        "node",
        "nodes",
        "ns",
        "namespace",
        "namespaces",
        "ev",
        "event",
        "events",
        "sa",
        "serviceaccount",
        "serviceaccounts",
        "role",
        "roles",
        "rb",
        "rolebinding",
        "rolebindings",
        "crole",
        "clusterrole",
        "clusterroles",
        "crb",
        "clusterrolebinding",
        "clusterrolebindings",
        "netpol",
        "np",
        "networkpolicy",
        "networkpolicies",
        "hpa",
        "hpas",
        "pdb",
        "pdbs",
    ]
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

fn filter_text_lines(text: &str, filter: &str) -> String {
    let needle = filter.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return text.to_string();
    }

    let matched: Vec<&str> = text
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains(&needle))
        .collect();
    if matched.is_empty() {
        return format!("No matches for '{}'", filter.trim());
    }
    matched.join("\n")
}

fn search_match_lines(text: &str, query: &str) -> Vec<usize> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    text.lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            if line.to_ascii_lowercase().contains(&needle) {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
fn search_match_lines_in_logs(lines: &VecDeque<String>, query: &str) -> Vec<usize> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            if line.to_ascii_lowercase().contains(&needle) {
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

fn highlighted_text(text: &str, query: &str) -> Text<'static> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Text::from(text.to_string());
    }

    let mut out = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let mut spans = Vec::new();
        let mut cursor = 0usize;
        while let Some(found) = lower[cursor..].find(&needle) {
            let start = cursor + found;
            let end = start + needle.len();
            if start > cursor {
                spans.push(Span::raw(line.get(cursor..start).unwrap_or("").to_string()));
            }
            spans.push(Span::styled(
                line.get(start..end).unwrap_or("").to_string(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            cursor = end;
        }
        if cursor < line.len() {
            spans.push(Span::raw(line.get(cursor..).unwrap_or("").to_string()));
        }
        if spans.is_empty() {
            spans.push(Span::raw(line.to_string()));
        }
        out.push(Line::from(spans));
    }
    Text::from(out)
}

fn detail_viewer_title(
    pane_title: &str,
    wrap: bool,
    search_query: &str,
    match_lines: &[usize],
    scroll: u16,
    total_lines: usize,
) -> String {
    let search = if search_query.trim().is_empty() {
        "search:-".to_string()
    } else if match_lines.is_empty() {
        format!("search:/{} 0/0", search_query.trim())
    } else {
        let current_line = scroll as usize;
        let current_idx = match_lines
            .iter()
            .position(|line| *line >= current_line)
            .unwrap_or(match_lines.len() - 1)
            + 1;
        format!(
            "search:/{} {}/{}",
            search_query.trim(),
            current_idx,
            match_lines.len()
        )
    };
    let total = total_lines.max(1);
    let line_pos = ((scroll as usize) + 1).min(total);
    format!(
        "{} | NORMAL | {} | ln:{}/{} | wrap:{}",
        pane_title,
        search,
        line_pos,
        total,
        if wrap { "on" } else { "off" }
    )
}

fn heartbeat_icon() -> &'static str {
    const FRAMES: [&str; 4] = ["-", "\\", "|", "/"];
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 250;
    FRAMES[(frame % FRAMES.len() as u128) as usize]
}

fn pane_icon(pane: Pane) -> &'static str {
    match pane {
        Pane::Table => "[TB]",
        Pane::Describe => "[DS]",
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

fn status_style_for_line(theme: &UiTheme, status: &str) -> Style {
    let lower = status.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("denied")
        || lower.contains("forbidden")
        || lower.contains("blocked")
    {
        return theme.status_err;
    }
    if lower.contains("warn")
        || lower.contains("retry")
        || lower.contains("pending")
        || lower.contains("paused")
    {
        return theme.status_warn;
    }
    theme.status_ok
}

fn context_filtered_indices(contexts: &[String], filter: &str) -> Vec<usize> {
    list_filtered_indices(contexts, filter)
}

fn pod_resources_from_raw(raw: &serde_json::Value) -> (u64, u64, u64, u64) {
    let spec = raw.get("spec").and_then(serde_json::Value::as_object);

    let mut cpu_req_sum = 0u64;
    let mut cpu_lim_sum = 0u64;
    let mut mem_req_sum = 0u64;
    let mut mem_lim_sum = 0u64;
    let mut init_cpu_req_max = 0u64;
    let mut init_cpu_lim_max = 0u64;
    let mut init_mem_req_max = 0u64;
    let mut init_mem_lim_max = 0u64;

    if let Some(spec) = spec {
        if let Some(containers) = spec.get("containers").and_then(serde_json::Value::as_array) {
            for c in containers {
                let resources = c.get("resources").and_then(serde_json::Value::as_object);
                if let Some(resources) = resources {
                    cpu_req_sum = cpu_req_sum.saturating_add(extract_cpu(resources, "requests"));
                    cpu_lim_sum = cpu_lim_sum.saturating_add(extract_cpu(resources, "limits"));
                    mem_req_sum = mem_req_sum.saturating_add(extract_mem(resources, "requests"));
                    mem_lim_sum = mem_lim_sum.saturating_add(extract_mem(resources, "limits"));
                }
            }
        }

        if let Some(containers) = spec
            .get("initContainers")
            .and_then(serde_json::Value::as_array)
        {
            for c in containers {
                let resources = c.get("resources").and_then(serde_json::Value::as_object);
                if let Some(resources) = resources {
                    init_cpu_req_max = init_cpu_req_max.max(extract_cpu(resources, "requests"));
                    init_cpu_lim_max = init_cpu_lim_max.max(extract_cpu(resources, "limits"));
                    init_mem_req_max = init_mem_req_max.max(extract_mem(resources, "requests"));
                    init_mem_lim_max = init_mem_lim_max.max(extract_mem(resources, "limits"));
                }
            }
        }

        if let Some(overhead) = spec.get("overhead").and_then(serde_json::Value::as_object) {
            if let Some(cpu) = overhead.get("cpu").and_then(serde_json::Value::as_str) {
                let v = parse_cpu_millicores(cpu);
                cpu_req_sum = cpu_req_sum.saturating_add(v);
                cpu_lim_sum = cpu_lim_sum.saturating_add(v);
            }
            if let Some(mem) = overhead.get("memory").and_then(serde_json::Value::as_str) {
                let v = parse_bytes_quantity(mem);
                mem_req_sum = mem_req_sum.saturating_add(v);
                mem_lim_sum = mem_lim_sum.saturating_add(v);
            }
        }
    }

    (
        cpu_req_sum.saturating_add(init_cpu_req_max),
        cpu_lim_sum.saturating_add(init_cpu_lim_max),
        mem_req_sum.saturating_add(init_mem_req_max),
        mem_lim_sum.saturating_add(init_mem_lim_max),
    )
}

fn node_capacity_from_raw(raw: &serde_json::Value) -> (bool, bool, u64, u64, u64) {
    let unschedulable = raw
        .pointer("/spec/unschedulable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let ready = raw
        .pointer("/status/conditions")
        .and_then(serde_json::Value::as_array)
        .and_then(|conditions| {
            conditions.iter().find_map(|cond| {
                let cond_type = cond.get("type").and_then(serde_json::Value::as_str)?;
                if cond_type != "Ready" {
                    return None;
                }
                cond.get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(|status| status.eq_ignore_ascii_case("true"))
            })
        })
        .unwrap_or(false);

    let cpu_alloc_m = raw
        .pointer("/status/allocatable/cpu")
        .and_then(serde_json::Value::as_str)
        .map(parse_cpu_millicores)
        .unwrap_or(0);
    let mem_alloc_b = raw
        .pointer("/status/allocatable/memory")
        .and_then(serde_json::Value::as_str)
        .map(parse_bytes_quantity)
        .unwrap_or(0);
    let pod_alloc = raw
        .pointer("/status/allocatable/pods")
        .and_then(serde_json::Value::as_str)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    (ready, unschedulable, cpu_alloc_m, mem_alloc_b, pod_alloc)
}

fn extract_cpu(resources: &serde_json::Map<String, serde_json::Value>, field: &str) -> u64 {
    resources
        .get(field)
        .and_then(serde_json::Value::as_object)
        .and_then(|m| m.get("cpu"))
        .and_then(serde_json::Value::as_str)
        .map(parse_cpu_millicores)
        .unwrap_or(0)
}

fn extract_mem(resources: &serde_json::Map<String, serde_json::Value>, field: &str) -> u64 {
    resources
        .get(field)
        .and_then(serde_json::Value::as_object)
        .and_then(|m| m.get("memory"))
        .and_then(serde_json::Value::as_str)
        .map(parse_bytes_quantity)
        .unwrap_or(0)
}

fn parse_cpu_millicores(value: &str) -> u64 {
    let value = value.trim();
    if value.is_empty() {
        return 0;
    }
    if let Some(v) = value.strip_suffix('n') {
        return parse_decimal_to_u64(v, 1.0 / 1_000_000.0);
    }
    if let Some(v) = value.strip_suffix('u') {
        return parse_decimal_to_u64(v, 1.0 / 1000.0);
    }
    if let Some(v) = value.strip_suffix('m') {
        return parse_decimal_to_u64(v, 1.0);
    }
    parse_decimal_to_u64(value, 1000.0)
}

fn parse_bytes_quantity(value: &str) -> u64 {
    let value = value.trim();
    if value.is_empty() {
        return 0;
    }
    let split = value
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit() && *ch != '.' && *ch != '-' && *ch != '+')
        .map(|(idx, _)| idx)
        .unwrap_or(value.len());
    let (num, suffix) = value.split_at(split);

    let multiplier = match suffix {
        "" => 1.0,
        "n" => 1.0 / 1_000_000_000.0,
        "u" => 1.0 / 1_000_000.0,
        "m" => 1.0 / 1000.0,
        "Ki" => 1024.0,
        "Mi" => 1024.0_f64.powi(2),
        "Gi" => 1024.0_f64.powi(3),
        "Ti" => 1024.0_f64.powi(4),
        "Pi" => 1024.0_f64.powi(5),
        "Ei" => 1024.0_f64.powi(6),
        "K" => 1000.0,
        "M" => 1000.0_f64.powi(2),
        "G" => 1000.0_f64.powi(3),
        "T" => 1000.0_f64.powi(4),
        "P" => 1000.0_f64.powi(5),
        "E" => 1000.0_f64.powi(6),
        _ => 1.0,
    };
    parse_decimal_to_u64(num, multiplier)
}

fn parse_decimal_to_u64(value: &str, multiplier: f64) -> u64 {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * multiplier).max(0.0).round() as u64)
        .unwrap_or(0)
}

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

fn owner_reference_matches(raw: &serde_json::Value, owner_kind: &str, owner_name: &str) -> bool {
    raw.pointer("/metadata/ownerReferences")
        .and_then(serde_json::Value::as_array)
        .map(|owners| {
            owners.iter().any(|owner| {
                owner.get("kind").and_then(serde_json::Value::as_str) == Some(owner_kind)
                    && owner.get("name").and_then(serde_json::Value::as_str) == Some(owner_name)
            })
        })
        .unwrap_or(false)
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

fn parse_resource_alias(token: &str) -> ResourceAlias {
    let normalized = token.to_ascii_lowercase();
    match normalized.as_str() {
        "pods" | "pod" | "po" => ResourceAlias::Supported(ResourceKind::Pods),
        "deployments" | "deployment" | "deploy" | "dp" => {
            ResourceAlias::Supported(ResourceKind::Deployments)
        }
        "replicasets" | "replicaset" | "rs" => ResourceAlias::Supported(ResourceKind::ReplicaSets),
        "statefulsets" | "statefulset" | "sts" => {
            ResourceAlias::Supported(ResourceKind::StatefulSets)
        }
        "daemonsets" | "daemonset" | "ds" => ResourceAlias::Supported(ResourceKind::DaemonSets),
        "services" | "service" | "svc" | "svcs" => ResourceAlias::Supported(ResourceKind::Services),
        "ingresses" | "ingress" | "ing" => ResourceAlias::Supported(ResourceKind::Ingresses),
        "configmaps" | "configmap" | "cm" => ResourceAlias::Supported(ResourceKind::ConfigMaps),
        "secrets" | "secret" | "sec" | "se" => ResourceAlias::Supported(ResourceKind::Secrets),
        "jobs" | "job" => ResourceAlias::Supported(ResourceKind::Jobs),
        "cronjobs" | "cronjob" | "cj" => ResourceAlias::Supported(ResourceKind::CronJobs),
        "pvcs"
        | "pvc"
        | "persistentvolumeclaim"
        | "persistentvolumeclaims"
        | "claim"
        | "claims" => ResourceAlias::Supported(ResourceKind::PersistentVolumeClaims),
        "pvs" | "pv" | "persistentvolume" | "persistentvolumes" => {
            ResourceAlias::Supported(ResourceKind::PersistentVolumes)
        }
        "nodes" | "node" | "no" => ResourceAlias::Supported(ResourceKind::Nodes),
        "namespaces" | "namespace" | "ns" => ResourceAlias::Supported(ResourceKind::Namespaces),
        "events" | "event" | "ev" => ResourceAlias::Supported(ResourceKind::Events),
        "serviceaccounts" | "serviceaccount" | "sa" => {
            ResourceAlias::Supported(ResourceKind::ServiceAccounts)
        }
        "roles" | "role" => ResourceAlias::Supported(ResourceKind::Roles),
        "rolebindings" | "rolebinding" | "rb" => {
            ResourceAlias::Supported(ResourceKind::RoleBindings)
        }
        "clusterroles" | "clusterrole" | "crole" => {
            ResourceAlias::Supported(ResourceKind::ClusterRoles)
        }
        "clusterrolebindings" | "clusterrolebinding" | "crb" => {
            ResourceAlias::Supported(ResourceKind::ClusterRoleBindings)
        }
        "networkpolicies" | "networkpolicy" | "netpol" | "np" => {
            ResourceAlias::Supported(ResourceKind::NetworkPolicies)
        }
        "hpas" | "hpa" | "horizontalpodautoscaler" | "horizontalpodautoscalers" => {
            ResourceAlias::Supported(ResourceKind::HorizontalPodAutoscalers)
        }
        "pdbs" | "pdb" | "poddisruptionbudget" | "poddisruptionbudgets" => {
            ResourceAlias::Supported(ResourceKind::PodDisruptionBudgets)
        }
        "all" | "*" => ResourceAlias::Unsupported("all resources"),
        "api" | "apis" | "apiservice" | "apiservices" => ResourceAlias::Unsupported("API services"),
        "crd" | "crds" | "customresourcedefinition" | "customresourcedefinitions" => {
            ResourceAlias::Unsupported("CustomResourceDefinitions")
        }
        "cr" | "customresources" => ResourceAlias::Unsupported("generic custom resources"),
        "ep" | "endpoint" | "endpoints" => ResourceAlias::Unsupported("Endpoints"),
        "eps" | "endpointslice" | "endpointslices" => ResourceAlias::Unsupported("EndpointSlices"),
        "rc" | "replicationcontroller" | "replicationcontrollers" => {
            ResourceAlias::Unsupported("ReplicationControllers")
        }
        "cs" | "componentstatus" | "componentstatuses" => {
            ResourceAlias::Unsupported("ComponentStatuses")
        }
        "csr" | "certificatesigningrequest" | "certificatesigningrequests" => {
            ResourceAlias::Unsupported("CertificateSigningRequests")
        }
        "sc" | "storageclass" | "storageclasses" => ResourceAlias::Unsupported("StorageClasses"),
        "ingclass" | "ingressclass" | "ingressclasses" => {
            ResourceAlias::Unsupported("IngressClasses")
        }
        "limits" | "limitrange" | "limitranges" | "lr" => ResourceAlias::Unsupported("LimitRanges"),
        "quota" | "resourcequota" | "resourcequotas" | "rq" => {
            ResourceAlias::Unsupported("ResourceQuotas")
        }
        "pc" | "priorityclass" | "priorityclasses" => ResourceAlias::Unsupported("PriorityClasses"),
        "runtimeclass" | "runtimeclasses" => ResourceAlias::Unsupported("RuntimeClasses"),
        "lease" | "leases" => ResourceAlias::Unsupported("Leases"),
        "va" | "volumeattachment" | "volumeattachments" => {
            ResourceAlias::Unsupported("VolumeAttachments")
        }
        "pt" | "podtemplate" | "podtemplates" => ResourceAlias::Unsupported("PodTemplates"),
        "mwc" | "mutatingwebhookconfiguration" | "mutatingwebhookconfigurations" => {
            ResourceAlias::Unsupported("MutatingWebhookConfigurations")
        }
        "vwc" | "validatingwebhookconfiguration" | "validatingwebhookconfigurations" => {
            ResourceAlias::Unsupported("ValidatingWebhookConfigurations")
        }
        _ => ResourceAlias::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashSet, VecDeque},
        time::Instant,
    };

    use super::{
        ResourceAlias, command_names, is_auth_refresh_log_error, is_retryable_log_error,
        is_visible_log_line, next_log_reconnect_backoff_ms, parse_log_source, parse_resource_alias,
        resource_alias_names, search_match_lines_in_logs, slice_chars,
    };
    use crate::model::ResourceKind;

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
            "resources",
            "res",
            "aliases",
            "clear",
            "clear-filter",
            "c",
            "container",
            "containers",
            "sources",
            "src",
            "pause",
            "resume",
            "tail",
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
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    }
}
