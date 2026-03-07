use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
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
    text::Line,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap},
};
use tokio::sync::mpsc;

use crate::{
    cluster::{ActionError, ActionExecutor, ResourceProvider},
    keymap::{Action, Keymap},
    model::{
        ConfirmationKind, Pane, PendingConfirmation, ResourceKey, ResourceKind, SortColumn,
        StateDelta,
    },
    state::StateStore,
    view::{SimpleViewProjector, ViewProjector, ViewRequest},
};

#[derive(Debug, Clone)]
struct ContextTabState {
    context: String,
    namespace: Option<String>,
    filter: String,
    selected: usize,
    detail_scroll: u16,
    kind_idx: usize,
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
    fn new(mode: CommandMode) -> Self {
        Self {
            mode,
            value: String::new(),
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
    },
    Contexts {
        title: String,
        contexts: Vec<String>,
        selected: usize,
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
    command_input: Option<CommandInput>,
    command_history: Vec<String>,
    last_command: Option<String>,
    history_cursor: Option<usize>,
    overlay: Option<Overlay>,
    status_line: String,
    pending_confirmation: Option<PendingConfirmation>,
    action_executor: Arc<dyn ActionExecutor>,
    resource_provider: Arc<dyn ResourceProvider>,
    keymap: Keymap,
    readonly: bool,
    show_help: bool,
}

impl App {
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
                selected: 0,
                detail_scroll: 0,
                kind_idx: 0,
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
            command_input: None,
            command_history: Vec::new(),
            last_command: None,
            history_cursor: None,
            overlay: None,
            status_line: "Press ':' for commands and '/' for filter".to_string(),
            pending_confirmation: None,
            action_executor,
            resource_provider,
            keymap,
            readonly,
            show_help,
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

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }

        if key.code == KeyCode::Char(':') {
            self.command_input = Some(CommandInput::new(CommandMode::Command));
            self.status_line = "Command mode".to_string();
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
        } else if self.keymap.is(Action::GotoTop, &key) {
            if self.current_tab().pane == Pane::Table {
                self.current_tab_mut().selected = 0;
            } else {
                self.current_tab_mut().detail_scroll = 0;
            }
        } else if self.keymap.is(Action::GotoBottom, &key) {
            if self.current_tab().pane == Pane::Table {
                self.current_tab_mut().selected = usize::MAX;
            } else {
                self.current_tab_mut().detail_scroll = u16::MAX;
            }
        } else if self.keymap.is(Action::FilterMode, &key) {
            self.command_input = Some(CommandInput::new(CommandMode::Filter));
            self.status_line = "Filter mode".to_string();
        } else if self.keymap.is(Action::CycleSort, &key) {
            self.cycle_sort();
        } else if self.keymap.is(Action::ToggleDesc, &key) {
            let tab = self.current_tab_mut();
            tab.descending = !tab.descending;
        } else if self.keymap.is(Action::CycleNamespace, &key) {
            self.cycle_namespace();
        } else if self.keymap.is(Action::ToggleHelp, &key) {
            self.show_help = !self.show_help;
        } else if self.keymap.is(Action::ToTable, &key) {
            self.current_tab_mut().pane = Pane::Table;
            self.current_tab_mut().detail_scroll = 0;
            self.overlay = None;
        } else if self.keymap.is(Action::ToggleDescribe, &key) {
            self.toggle_describe();
        } else if self.keymap.is(Action::ToEvents, &key) {
            self.current_tab_mut().pane = Pane::Events;
            self.current_tab_mut().detail_scroll = 0;
            self.overlay = None;
        } else if self.keymap.is(Action::ToLogs, &key) {
            self.current_tab_mut().pane = Pane::Logs;
            self.current_tab_mut().detail_scroll = 0;
            self.overlay = None;
        } else if self.keymap.is(Action::Delete, &key) {
            self.prepare_delete_confirmation();
        } else if self.keymap.is(Action::Confirm, &key) {
            self.confirm_action().await;
        } else if self.keymap.is(Action::Cancel, &key) {
            self.pending_confirmation = None;
            self.current_tab_mut().pane = Pane::Table;
            self.current_tab_mut().detail_scroll = 0;
            self.overlay = None;
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

    async fn handle_command_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.command_input = None;
                self.status_line = "Command canceled".to_string();
                return Ok(false);
            }
            KeyCode::Backspace => {
                if let Some(input) = &mut self.command_input {
                    input.value.pop();
                }
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
                self.current_tab_mut().filter = filter.clone();
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
                self.status_line = "Commands: :ctx [name] | :ns [name|all] | :kind <kind> | :resources | :clear | :quit | :all".to_string();
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
                self.current_tab_mut().filter.clear();
                self.status_line = "Filter cleared".to_string();
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
        });
        self.status_line = "Context list opened".to_string();
    }

    fn show_resource_aliases_overlay(&mut self) {
        let lines = vec![
            "Supported resource aliases:".to_string(),
            "po|pod, deploy|dp, rs, sts, ds, svc, ing, cm, sec|secret, job, cj".to_string(),
            "pvc|claim, pv, no|node, ns|namespace, ev|event, sa, role, rb".to_string(),
            "crole, crb, netpol|np, hpa, pdb".to_string(),
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
        });
        self.status_line = "Resource aliases opened".to_string();
    }

    async fn handle_overlay_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                self.status_line = "Closed view".to_string();
            }
            KeyCode::Enter => {
                if let Some(Overlay::Contexts { selected, .. }) = &self.overlay {
                    self.active_tab = *selected;
                    self.overlay = None;
                    self.status_line = format!("Context: {}", self.current_tab().context);
                    self.ensure_active_watch().await;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_overlay_or_select(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_overlay_or_select(1);
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
                contexts, selected, ..
            }) => {
                if contexts.is_empty() {
                    *selected = 0;
                    return;
                }
                if delta < 0 {
                    *selected = selected.saturating_sub(delta.unsigned_abs());
                } else {
                    *selected = (*selected + delta as usize).min(contexts.len() - 1);
                }
            }
            None => {}
        }
    }

    fn overlay_home(&mut self) {
        match &mut self.overlay {
            Some(Overlay::Text { scroll, .. }) => *scroll = 0,
            Some(Overlay::Contexts { selected, .. }) => *selected = 0,
            None => {}
        }
    }

    fn overlay_end(&mut self) {
        match &mut self.overlay {
            Some(Overlay::Text { scroll, .. }) => *scroll = u16::MAX,
            Some(Overlay::Contexts {
                contexts, selected, ..
            }) => {
                if contexts.is_empty() {
                    *selected = 0;
                } else {
                    *selected = contexts.len() - 1;
                }
            }
            None => {}
        }
    }

    fn scroll_detail(&mut self, delta: isize) {
        let tab = self.current_tab_mut();
        if delta < 0 {
            tab.detail_scroll = tab
                .detail_scroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            tab.detail_scroll = tab.detail_scroll.saturating_add(delta as u16);
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
        let active = self.current_tab().clone();
        let request = ViewRequest {
            context: active.context.clone(),
            kind: active.kind(),
            namespace: active.namespace.clone(),
            filter: active.filter.clone(),
            sort: active.sort,
            descending: active.descending,
        };

        let vm = self.projector.project(&self.store, &request);
        let max_selection = vm.rows.len().saturating_sub(1);
        let selected = active.selected.min(max_selection);
        self.current_tab_mut().selected = selected;

        let mut constraints = vec![
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(1),
        ];
        if self.show_help {
            constraints.push(Constraint::Length(2));
        }
        constraints.push(Constraint::Length(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        let breadcrumbs = Paragraph::new(Line::from(format!(
            "ctx:{} > ns:{} > kind:{} > filter:{}",
            active.context,
            active.namespace.as_deref().unwrap_or("all"),
            active.kind().short_name(),
            if active.filter.is_empty() {
                "-"
            } else {
                active.filter.as_str()
            }
        )))
        .style(Style::default().fg(Color::Cyan));
        frame.render_widget(breadcrumbs, chunks[0]);

        let context_titles: Vec<Line<'_>> = self
            .tabs
            .iter()
            .map(|tab| Line::from(tab.context.as_str()))
            .collect();
        let context_tabs = Tabs::new(context_titles)
            .block(Block::default().borders(Borders::ALL).title("Contexts"))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .select(self.active_tab);
        frame.render_widget(context_tabs, chunks[1]);

        if let Some(overlay) = &self.overlay {
            match overlay {
                Overlay::Text {
                    title,
                    lines,
                    scroll,
                } => {
                    let body = lines.join("\n");
                    let paragraph = Paragraph::new(body)
                        .block(Block::default().borders(Borders::ALL).title(title.clone()))
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0));
                    frame.render_widget(paragraph, chunks[2]);
                }
                Overlay::Contexts {
                    title,
                    contexts,
                    selected,
                } => {
                    let rows: Vec<Row<'_>> = contexts
                        .iter()
                        .enumerate()
                        .map(|(idx, context)| {
                            let marker = if idx == self.active_tab { "*" } else { " " };
                            Row::new(vec![Cell::from(marker), Cell::from(context.clone())])
                        })
                        .collect();

                    let table = Table::new(rows, [Constraint::Length(2), Constraint::Min(10)])
                        .header(
                            Row::new(vec!["", "Context"]).style(
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        )
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!("{title} (Enter to switch, Esc to close)")),
                        )
                        .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));

                    let max_selection = contexts.len().saturating_sub(1);
                    let selection = (*selected).min(max_selection);
                    let mut state = ratatui::widgets::TableState::default().with_selected(
                        if contexts.is_empty() {
                            None
                        } else {
                            Some(selection)
                        },
                    );
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
                        Row::new(vec!["Namespace", "Name", "Status", "Age", "Summary"]).style(
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                    )
                    .block(Block::default().borders(Borders::ALL).title(format!(
                        "{} ({})",
                        active.kind(),
                        vm.rows.len()
                    )))
                    .row_highlight_style(Style::default().bg(Color::Blue).fg(Color::White));

                    let mut state =
                        ratatui::widgets::TableState::default().with_selected(Some(selected));
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
                Pane::Describe | Pane::Events | Pane::Logs => {
                    let body = self.detail_text(&vm.rows, selected, active.pane);
                    let pane_title = match active.pane {
                        Pane::Describe => "Describe",
                        Pane::Events => "Events",
                        Pane::Logs => "Logs",
                        Pane::Table => "Table",
                    };
                    let paragraph = Paragraph::new(body)
                        .block(Block::default().borders(Borders::ALL).title(pane_title))
                        .wrap(Wrap { trim: false })
                        .scroll((active.detail_scroll, 0));
                    frame.render_widget(paragraph, chunks[2]);
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
        let status_widget = Paragraph::new(status).style(Style::default().fg(Color::Green));
        frame.render_widget(status_widget, chunks[3]);

        if self.show_help {
            let help = Paragraph::new(
                "ctrl+c quit | : command | / filter | [ ] history | - repeat | ctrl+a aliases | tab switch-ctx | j/k move/scroll | d describe | v view | ctrl+d delete | ctrl+k kill",
            )
            .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(help, chunks[4]);
        }

        let command_idx = if self.show_help { 5 } else { 4 };
        let command_line = if let Some(input) = &self.command_input {
            format!("{}{}", input.prefix(), input.value)
        } else {
            "Command: ':' for commands, '/' for filter".to_string()
        };
        let command_style = if self.command_input.is_some() {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
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
        tab.selected = 0;
        tab.detail_scroll = 0;
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
        tab.selected = 0;
        tab.detail_scroll = 0;
        tab.pane = Pane::Table;
        self.overlay = None;
    }

    fn cycle_namespace(&mut self) {
        let context = self.current_tab().context.clone();
        let mut namespaces = self.store.namespaces(&context);
        namespaces.sort();

        let tab = self.current_tab_mut();
        if namespaces.is_empty() {
            tab.namespace = None;
            self.status_line = "Namespace filter cleared".to_string();
            return;
        }

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

        self.status_line = format!(
            "Namespace filter: {}",
            tab.namespace.as_deref().unwrap_or("all")
        );
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
        let request = ViewRequest {
            context: active.context.clone(),
            kind: active.kind(),
            namespace: active.namespace.clone(),
            filter: active.filter.clone(),
            sort: active.sort,
            descending: active.descending,
        };
        let vm = self.projector.project(&self.store, &request);
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
                tab.selected = 0;
                tab.detail_scroll = 0;
                tab.pane = Pane::Table;
                tab.kind().to_string()
            };
            self.overlay = None;
            self.status_line = format!("Kind: {kind_label}");
        }
    }

    async fn ensure_active_watch(&mut self) {
        let tab = self.current_tab().clone();
        if let Err(err) = self
            .resource_provider
            .ensure_watch(&tab.context, ResourceKind::Namespaces)
            .await
        {
            self.status_line = format!("watch setup error: {err}");
            return;
        }
        if let Err(err) = self
            .resource_provider
            .ensure_watch(&tab.context, tab.kind())
            .await
        {
            self.status_line = format!("watch setup error: {err}");
            return;
        }
        if tab.pane == Pane::Events {
            let _ = self
                .resource_provider
                .ensure_watch(&tab.context, ResourceKind::Events)
                .await;
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
    use super::{ResourceAlias, parse_resource_alias};
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
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
    }
}
