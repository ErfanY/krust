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

mod command_mode;
mod detail_pane;
mod logs;
mod overlay;

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
    theme: UiTheme,
    color_support: ColorSupport,
    pulse_snapshot: Option<PulseSnapshot>,
    pulse_last_change_at: Instant,
    pulse_last_revision: u64,
    pulse_last_revision_at: Instant,
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
                Overlay::Text { .. } => "Search",
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
            pulse_snapshot: None,
            pulse_last_change_at: now,
            pulse_last_revision: 0,
            pulse_last_revision_at: now,
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
                let mut lines = Vec::with_capacity(vm.rows.len().saturating_add(1));
                lines.push("namespace\tname\tstatus\tage\tsummary".to_string());
                for row in &vm.rows {
                    lines.push(format!(
                        "{}\t{}\t{}\t{}\t{}",
                        row.namespace, row.name, row.status, row.age, row.summary
                    ));
                }
                Some(lines.join("\n"))
            }
            Pane::Describe | Pane::SecretDecode | Pane::Events => {
                let request = self.view_request_for_tab(&active);
                let vm = self.projected_view(&request);
                let selected = active.selected.min(vm.rows.len().saturating_sub(1));
                let raw = self.detail_text(&vm.rows, selected, active.pane, active.detail_format);
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

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let theme = self.theme;
        let active = self.current_tab().clone();
        let active_tab_idx = self.active_tab;
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let visible_rows = vm.rows.len();
        let max_selection = visible_rows.saturating_sub(1);
        let mut selected = active.selected.min(max_selection);
        self.current_tab_mut().selected = selected;

        let pane_label = match active.pane {
            Pane::Table => "table",
            Pane::Describe => "describe",
            Pane::SecretDecode => "decode",
            Pane::Events => "events",
            Pane::Logs => "logs",
        };
        let ns_label = if active.kind().is_namespaced() {
            active.namespace.as_deref().unwrap_or("all")
        } else {
            "cluster"
        };
        let now_instant = Instant::now();
        let revision = self.store.revision();
        let rev_delta = revision.saturating_sub(self.pulse_last_revision);
        if rev_delta > 0 {
            self.pulse_last_revision = revision;
            self.pulse_last_revision_at = now_instant;
        }
        let stale_secs = self.pulse_last_revision_at.elapsed().as_secs();
        let now = Local::now().format("%Y-%m-%d %H:%M:%S");
        let selected_human = if visible_rows == 0 { 0 } else { selected + 1 };
        let hb = activity_icon(revision);
        let ctx_short = compact_context_name(&active.context);
        let mut top_line = format!(
            "{} {}  [CTX] {} ({}/{})  [NS] {}  [K] {}  [P] {} {}  [CLR] {}  [REV] +{}  [STALE] {}s  [SEL] {selected_human}/{}  [VIS] {}  [CACHE] {}  [ERR] {}",
            hb,
            now,
            ctx_short,
            self.active_tab + 1,
            self.tabs.len(),
            ns_label,
            active.kind().short_name(),
            pane_label,
            pane_icon(active.pane),
            color_support_label(self.color_support),
            rev_delta,
            stale_secs,
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

        let (running, pending, failed, _other) = self.pod_phase_counts_for_tab(&active);
        let scope_pods = self.pod_resource_totals(&active.context, active.namespace.as_deref());
        let cluster_pods = self.pod_resource_totals(&active.context, None);
        let node_caps = self.node_capacity_totals(&active.context);
        let cpu_pct = percent(cluster_pods.cpu_request_m, node_caps.cpu_alloc_m);
        let mem_pct = percent(cluster_pods.mem_request_b, node_caps.mem_alloc_b);
        let pod_pct = percent(cluster_pods.pods as u64, node_caps.pod_alloc);
        let deployments = self.count_kind_for_tab(&active, ResourceKind::Deployments);
        let replicasets = self.count_kind_for_tab(&active, ResourceKind::ReplicaSets);
        let statefulsets = self.count_kind_for_tab(&active, ResourceKind::StatefulSets);
        let daemonsets = self.count_kind_for_tab(&active, ResourceKind::DaemonSets);
        let services = self.count_kind_for_tab(&active, ResourceKind::Services);
        let ingresses = self.count_kind_for_tab(&active, ResourceKind::Ingresses);
        let jobs = self.count_kind_for_tab(&active, ResourceKind::Jobs);
        let cronjobs = self.count_kind_for_tab(&active, ResourceKind::CronJobs);
        let pods = self.count_kind_for_tab(&active, ResourceKind::Pods);

        let scope_label = active.namespace.as_deref().unwrap_or("all namespaces");
        let watch_error = self.store.error_for_context(&active.context);
        let watch_health = if watch_error.is_some() {
            "[XX]"
        } else {
            "[OK]"
        };
        let pod_health = health_icon(failed, pending);
        // 2 columns removed for borders + 2 columns for left/right block padding.
        let pulse_width = frame.area().width.saturating_sub(4).max(1);
        let pulse_cols = 3usize;
        let pulse_gaps = 2usize * pulse_cols.saturating_sub(1);
        let pulse_body_chars = pulse_width.saturating_sub(PULSE_TAG_WIDTH as u16 + 1) as usize;
        let pulse_col_width = pulse_body_chars
            .saturating_sub(pulse_gaps)
            .checked_div(pulse_cols)
            .unwrap_or(0)
            .max(8);
        let cpu_meter = ascii_meter(cluster_pods.cpu_request_m, node_caps.cpu_alloc_m, 12);
        let mem_meter = ascii_meter(cluster_pods.mem_request_b, node_caps.mem_alloc_b, 12);
        let pod_meter = ascii_meter(cluster_pods.pods as u64, node_caps.pod_alloc, 12);

        let current_snapshot = PulseSnapshot {
            context: active.context.clone(),
            namespace: active.namespace.clone(),
            cluster_cpu_req_m: cluster_pods.cpu_request_m,
            cluster_mem_req_b: cluster_pods.mem_request_b,
            cluster_pods: cluster_pods.pods as u64,
            running,
            pending,
            failed,
        };
        let previous_snapshot = self
            .pulse_snapshot
            .as_ref()
            .filter(|prev| {
                prev.context == current_snapshot.context
                    && prev.namespace == current_snapshot.namespace
            })
            .cloned();

        let cpu_delta_m = previous_snapshot
            .as_ref()
            .map(|prev| value_delta(current_snapshot.cluster_cpu_req_m, prev.cluster_cpu_req_m))
            .unwrap_or(0);
        let mem_delta_b = previous_snapshot
            .as_ref()
            .map(|prev| value_delta(current_snapshot.cluster_mem_req_b, prev.cluster_mem_req_b))
            .unwrap_or(0);
        let pods_delta = previous_snapshot
            .as_ref()
            .map(|prev| value_delta(current_snapshot.cluster_pods, prev.cluster_pods))
            .unwrap_or(0);
        let run_delta = previous_snapshot
            .as_ref()
            .map(|prev| current_snapshot.running as i64 - prev.running as i64)
            .unwrap_or(0);
        let pend_delta = previous_snapshot
            .as_ref()
            .map(|prev| current_snapshot.pending as i64 - prev.pending as i64)
            .unwrap_or(0);
        let fail_delta = previous_snapshot
            .as_ref()
            .map(|prev| current_snapshot.failed as i64 - prev.failed as i64)
            .unwrap_or(0);

        let any_metric_delta = cpu_delta_m != 0
            || mem_delta_b != 0
            || pods_delta != 0
            || run_delta != 0
            || pend_delta != 0
            || fail_delta != 0;
        if any_metric_delta {
            self.pulse_last_change_at = now_instant;
        }
        self.pulse_snapshot = Some(current_snapshot);
        let metric_stale_secs = self.pulse_last_change_at.elapsed().as_secs();

        let cpu_ratio = ratio_percent_value(cluster_pods.cpu_request_m, node_caps.cpu_alloc_m);
        let mem_ratio = ratio_percent_value(cluster_pods.mem_request_b, node_caps.mem_alloc_b);
        let live_severity = if watch_error.is_some() {
            Severity::Err
        } else if stale_secs > 60 {
            Severity::Warn
        } else {
            Severity::Ok
        };
        let cpu_severity = match cpu_ratio {
            Some(pct) if pct >= 95.0 => Severity::Err,
            Some(pct) if pct >= 80.0 => Severity::Warn,
            _ => Severity::Ok,
        };
        let mem_severity = match mem_ratio {
            Some(pct) if pct >= 95.0 => Severity::Err,
            Some(pct) if pct >= 80.0 => Severity::Warn,
            _ => Severity::Ok,
        };
        let pod_severity = if failed > 0 {
            Severity::Err
        } else if pending > 0 {
            Severity::Warn
        } else {
            Severity::Ok
        };

        let mut pulse_rows: Vec<(String, Vec<String>, Severity)> = vec![
            (
                "[LIVE]".to_string(),
                vec![
                    format!("rev +{rev_delta}  metric-delta:{}s", metric_stale_secs),
                    format!(
                        "watch {watch_health}  log {}  pod {pod_health}",
                        logs_state_icon(&self.logs)
                    ),
                    format!("state-age {}s  act {}", stale_secs, hb),
                ],
                live_severity,
            ),
            (
                "[SCOPE]".to_string(),
                vec![
                    format!(
                        "ctx {} ({}/{})",
                        ctx_short,
                        self.active_tab + 1,
                        self.tabs.len()
                    ),
                    format!("scope {}", scope_label),
                    format!(
                        "kind {} pane {} clr {}",
                        active.kind().short_name(),
                        pane_icon(active.pane),
                        color_support_label(self.color_support)
                    ),
                ],
                Severity::Ok,
            ),
            (
                "[CPU]".to_string(),
                vec![
                    format!("cluster {cpu_meter}"),
                    format!(
                        "req/alloc {} / {}",
                        format_millicpu(cluster_pods.cpu_request_m),
                        format_millicpu(node_caps.cpu_alloc_m)
                    ),
                    format!(
                        "delta {}  scope {} / {}",
                        format_signed_millicpu(cpu_delta_m),
                        format_millicpu(scope_pods.cpu_request_m),
                        format_millicpu(scope_pods.cpu_limit_m)
                    ),
                ],
                cpu_severity,
            ),
            (
                "[MEM]".to_string(),
                vec![
                    format!("cluster {mem_meter}"),
                    format!(
                        "req/alloc {} / {}",
                        format_bytes(cluster_pods.mem_request_b),
                        format_bytes(node_caps.mem_alloc_b)
                    ),
                    format!(
                        "delta {}  scope {} / {}",
                        format_signed_bytes(mem_delta_b),
                        format_bytes(scope_pods.mem_request_b),
                        format_bytes(scope_pods.mem_limit_b)
                    ),
                ],
                mem_severity,
            ),
            (
                "[PODS]".to_string(),
                vec![
                    format!("cluster {pod_meter}"),
                    format!(
                        "run {}({}) pend {}({}) fail {}({})",
                        running,
                        format_signed_count(run_delta),
                        pending,
                        format_signed_count(pend_delta),
                        failed,
                        format_signed_count(fail_delta)
                    ),
                    format!(
                        "delta {}  nodes {}/{} uns {}",
                        format_signed_count(pods_delta),
                        node_caps.nodes_ready,
                        node_caps.nodes_total,
                        node_caps.nodes_unschedulable
                    ),
                ],
                pod_severity,
            ),
            (
                "[WORK]".to_string(),
                vec![
                    format!("po {pods} dp {deployments} rs {replicasets}"),
                    format!("sts {statefulsets} ds {daemonsets}"),
                    format!("svc {services} ing {ingresses} job {jobs} cj {cronjobs}"),
                ],
                Severity::Ok,
            ),
            (
                "[UTIL]".to_string(),
                vec![
                    format!("cluster cpu {cpu_pct}"),
                    format!("cluster mem {mem_pct}"),
                    format!("cluster pods {pod_pct}"),
                ],
                Severity::Ok,
            ),
        ];
        if let Some(err) = watch_error {
            pulse_rows.push((
                "[ALERT]".to_string(),
                vec![
                    "api/watch error".to_string(),
                    err.to_string(),
                    "verify RBAC and selected context".to_string(),
                ],
                Severity::Err,
            ));
        }
        let pulse_rows_rendered: Vec<(String, String, Severity)> = pulse_rows
            .iter()
            .map(|(tag, cells, sev)| {
                (
                    fixed_width_cell(tag, PULSE_TAG_WIDTH),
                    format_pulse_cells(cells, pulse_cols, pulse_col_width),
                    *sev,
                )
            })
            .collect();
        let pulse_text = pulse_rows_rendered
            .iter()
            .map(|(tag, body, _)| format!("{tag} {body}"))
            .collect::<Vec<_>>()
            .join("\n");
        let help_text = "ctrl+c quit | : command | / ? filter/search | n/N next/prev | gg/G top/bottom | ctrl+d/u half-page | [ ] history | - repeat | ctrl+a aliases | tab switch-ctx | j/k move/scroll | left/right h-scroll (wrap off) | w wrap toggle | y copy detail | d describe | x decode secret | e edit | :fmt yaml|json | l logs(stream) | s tail on/off | p pause/resume logs | S sources | L latest | c container picker | ctrl+d delete(table) | ctrl+k kill";
        let help_height = if self.show_help {
            max_vertical_scroll_for_text(help_text, frame.area().width.max(1), 1, true) + 1
        } else {
            0
        };
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

        let mut table_offset = active.table_offset;
        if active.pane == Pane::Table {
            let viewport_rows = table_viewport_rows(chunks[2].height);
            let (synced_selected, synced_offset) =
                sync_table_viewport(selected, table_offset, viewport_rows, vm.rows.len());
            selected = synced_selected;
            table_offset = synced_offset;
            let tab = self.current_tab_mut();
            tab.selected = synced_selected;
            tab.table_offset = synced_offset;
        }

        let top_status = Paragraph::new(Line::from(top_line)).style(theme.header);
        frame.render_widget(top_status, chunks[0]);

        let pulse_lines: Vec<Line<'_>> = pulse_rows_rendered
            .iter()
            .map(|(tag, body, sev)| {
                Line::from(vec![
                    Span::styled(format!("{tag} "), theme.table_header),
                    Span::styled(body.clone(), severity_style(&theme, *sev)),
                ])
            })
            .collect();
        let pulse_widget = Paragraph::new(Text::from(pulse_lines))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("[PULSE] Cluster Pulse")
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
                            let sev = classify_status_severity(&row.status);
                            let status = format!("{} {}", severity_tag(sev), row.status);
                            Row::new(vec![
                                Cell::from(row.namespace.clone()),
                                Cell::from(row.name.clone()),
                                Cell::from(status),
                                Cell::from(row.age.clone()),
                                Cell::from(row.summary.clone()),
                            ])
                            .style(severity_style(&theme, sev))
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

                    let mut state = ratatui::widgets::TableState::default()
                        .with_selected(Some(selected))
                        .with_offset(table_offset);
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
                Pane::Describe | Pane::SecretDecode | Pane::Events => {
                    let raw_body =
                        self.detail_text(&vm.rows, selected, active.pane, active.detail_format);
                    let body = raw_body;
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
                        Pane::SecretDecode => "Decode",
                        Pane::Events => "Events",
                        Pane::Logs => "Logs",
                        Pane::Table => "Table",
                    };
                    let pane_title = format!("{pane_title} ({})", active.detail_format.label());
                    let search_query = active.detail_filter.trim();
                    let match_lines = search_match_lines(&body, search_query);
                    let active_line = resolved_active_match_line(
                        detail_scroll,
                        &match_lines,
                        active.detail_active_match_line,
                    );
                    {
                        let tab = self.current_tab_mut();
                        tab.detail_active_match_line = active_line;
                    }
                    let total_lines = body.lines().count();
                    let title = detail_viewer_title(
                        &pane_title,
                        detail_wrap,
                        search_query,
                        &match_lines,
                        detail_scroll,
                        total_lines,
                        active_line,
                    );
                    let detail_text = highlighted_structured_text(
                        &body,
                        search_query,
                        active.detail_format,
                        self.color_support,
                        active_line,
                    );
                    let mut paragraph = Paragraph::new(detail_text)
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
                    let active_line = resolved_active_match_line(
                        detail_scroll,
                        match_lines.as_slice(),
                        active.detail_active_match_line,
                    );
                    {
                        let tab = self.current_tab_mut();
                        tab.detail_active_match_line = active_line;
                    }
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
                            total_lines,
                            active_line
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
                        let paragraph = Paragraph::new(highlighted_text(
                            body.as_str(),
                            search_query,
                            active_line,
                        ))
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
                        let active_rel = active_line.and_then(|line| {
                            let start = detail_scroll as usize;
                            let end = start.saturating_add(visible.len());
                            if line >= start && line < end {
                                Some(line - start)
                            } else {
                                None
                            }
                        });
                        let paragraph =
                            Paragraph::new(highlighted_text(&body, search_query, active_rel))
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

    let leave_result = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    if let Err(err) = leave_result {
        let _ = fs::remove_file(&path);
        return Err(err.to_string());
    }

    let status = Command::new(&editor).arg(&path).status();

    let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture);
    let _ = enable_raw_mode();

    let result = match status {
        Ok(status) if status.success() => fs::read_to_string(&path)
            .map(Some)
            .map_err(|err| err.to_string()),
        Ok(status) => Err(format!("editor exited with status {status}")),
        Err(err) => Err(err.to_string()),
    };

    let _ = fs::remove_file(&path);
    result
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
        App, ColorSupport, ResourceAlias, base64_decode, base64_encode, classify_status_severity,
        color_support_label, command_names, decoded_secret_text, detect_color_support_from_env,
        highlighted_json_text, highlighted_text, highlighted_yaml_text, is_auth_refresh_log_error,
        is_retryable_log_error, is_visible_log_line, json_spans_for_line,
        next_log_reconnect_backoff_ms, parse_log_source, parse_resource_alias,
        resolved_active_match_line, resource_alias_names, search_match_lines_in_logs, severity_tag,
        slice_chars, step_match_line, sync_table_viewport, table_viewport_rows,
        truncate_for_clipboard, ui_theme_for, yaml_spans_for_line,
    };
    use crate::{
        cluster::{
            ActionError, ActionExecutor, ActionResult, PodLogRequest, PodLogStream,
            ResourceProvider, WatchTarget,
        },
        keymap::Keymap,
        model::{Pane, ResourceEntity, ResourceKey, ResourceKind, StateDelta},
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
            summary: "snapshot".to_string(),
            raw: serde_json::json!({
                "metadata": { "name": name, "namespace": namespace.unwrap_or("default") },
                "status": { "phase": status }
            }),
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

        let snap = render_snapshot(&mut app, 120, 32);
        assert!(snap.contains("[CTX]"));
        assert!(snap.contains("[PULSE] Cluster Pulse"));
        assert!(snap.contains("[LIVE]"));
        assert!(snap.contains("[OK] Running"));
        assert!(snap.contains("[!!] Pending"));
        assert!(snap.contains("pod-bad"));
        assert!(snap.contains("[XX]"));
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
        assert!(snap.contains("Describe (yaml) | NORMAL"));
        assert!(snap.contains("search:/metadata"));
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
        assert!(snap.contains("Logs | target:pod default/pod-a"));
        assert!(snap.contains("[default/pod-a/main] line-1"));
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
