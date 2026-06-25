use super::*;

/// Kinds whose Enter drills into a filtered Pods view. Returns the owner kind itself when it
/// supports drill-down, else None (leaf kinds fall through to describe).
fn drill_owner_kind(kind: ResourceKind) -> Option<ResourceKind> {
    matches!(
        kind,
        ResourceKind::Deployments
            | ResourceKind::ReplicaSets
            | ResourceKind::StatefulSets
            | ResourceKind::DaemonSets
            | ResourceKind::Nodes
    )
    .then_some(kind)
}

impl App {
    pub(super) fn handle_enter_key(&mut self) {
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

        // Drill-down: Enter on a workload or node opens its pods (filtered Pods view).
        if drill_owner_kind(row.key.kind).is_some() {
            self.drill_into(&row);
            return;
        }

        self.current_tab_mut().pane = Pane::Describe;
        self.current_tab_mut().detail_scroll = 0;
        self.current_tab_mut().detail_hscroll = 0;
        self.status_line = format!("Describe: {} {}", row.key.kind.short_name(), row.key.name);
    }

    /// Switch to the Pods view scoped to the selected owner (Deployment/RS/STS/DS/Node).
    fn drill_into(&mut self, row: &crate::view::ViewRow) {
        let pods_idx = ResourceKind::ORDERED
            .iter()
            .position(|kind| *kind == ResourceKind::Pods)
            .unwrap_or(0);
        let owner_kind = row.key.kind;
        let owner_name = row.key.name.clone();
        let label = format!("{}/{}", owner_kind.short_name(), owner_name);
        let tab = self.current_tab_mut();
        // Node pods span namespaces; workload pods live in the owner's namespace.
        if owner_kind != ResourceKind::Nodes {
            tab.namespace = row.key.namespace.clone();
        } else {
            tab.namespace = None;
        }
        tab.drill = Some(DrillFilter {
            owner_kind,
            owner_name,
        });
        tab.kind_idx = pods_idx;
        tab.last_non_namespace_kind_idx = pods_idx;
        tab.selected = 0;
        tab.table_offset = 0;
        tab.pane = Pane::Table;
        self.overlay = None;
        self.status_line = format!("Drill-down: pods of {label} (esc to clear)");
    }

    pub(super) async fn handle_command_key(&mut self, key: KeyEvent) -> anyhow::Result<bool> {
        match key.code {
            KeyCode::Esc => {
                if let Some(input) = self.command_input.take() {
                    if matches!(input.mode, CommandMode::Filter) {
                        if self.clear_active_filter_value() {
                            self.status_line = self.filter_status_message("");
                        } else {
                            self.status_line = format!("{} canceled", self.active_filter_label());
                        }
                    } else {
                        self.status_line = "Command canceled".to_string();
                    }
                }
                Ok(false)
            }
            KeyCode::Backspace => {
                if let Some(input) = &mut self.command_input {
                    input.value.pop();
                    if matches!(input.mode, CommandMode::Filter) {
                        let filter = input.value.trim().to_string();
                        self.set_active_filter_value(filter.clone());
                        self.status_line = self.filter_status_message(&filter);
                    }
                }
                Ok(false)
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = &mut self.command_input {
                    delete_previous_word(&mut input.value);
                    if matches!(input.mode, CommandMode::Filter) {
                        let filter = input.value.trim().to_string();
                        self.set_active_filter_value(filter.clone());
                        self.status_line = self.filter_status_message(&filter);
                    }
                }
                Ok(false)
            }
            KeyCode::Tab => {
                self.autocomplete_command_input();
                Ok(false)
            }
            KeyCode::Enter => {
                let Some(input) = self.command_input.take() else {
                    return Ok(false);
                };
                Ok(self.execute_command_input(input).await)
            }
            KeyCode::Char(ch) => {
                if let Some(input) = &mut self.command_input {
                    input.value.push(ch);
                    if matches!(input.mode, CommandMode::Filter) {
                        let filter = input.value.trim().to_string();
                        self.set_active_filter_value(filter.clone());
                        self.status_line = self.filter_status_message(&filter);
                    }
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    pub(super) async fn execute_command_input(&mut self, input: CommandInput) -> bool {
        match input.mode {
            CommandMode::Filter => {
                let filter = input.value.trim().to_string();
                self.set_active_filter_value(filter.clone());
                self.status_line = self.filter_status_message(&filter);
                false
            }
            CommandMode::Command => self.execute_colon_command(input.value.trim()).await,
        }
    }

    pub(super) async fn execute_colon_command(&mut self, raw: &str) -> bool {
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
                self.status_line = "Commands: :ctx [name] | :ns [name|all] | :kind <kind> | :api [filter] | :<resource> [name] | :fmt [yaml|json] | :c | :sources | :pause | :resume | :edit [yaml|json] | :tail | :copy | :dump <path> | :resources | :clear | :quit | :all".to_string();
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
            "helm" => {
                match args.first().map(|a| a.to_ascii_lowercase()) {
                    Some(a) if a == "show" || a == "on" => {
                        let tab = self.current_tab_mut();
                        tab.show_helm_secrets = true;
                        tab.selected = 0;
                        tab.table_offset = 0;
                        self.status_line = "Helm release secrets: shown".to_string();
                    }
                    Some(a) if a == "hide" || a == "off" => {
                        let tab = self.current_tab_mut();
                        tab.show_helm_secrets = false;
                        tab.selected = 0;
                        tab.table_offset = 0;
                        self.status_line = "Helm release secrets: hidden".to_string();
                    }
                    _ => self.toggle_helm_secrets(),
                }
                false
            }
            "sort" => {
                if let Some(token) = args.first() {
                    let column = match token.to_ascii_lowercase().as_str() {
                        "name" => Some(SortColumn::Name),
                        "namespace" | "ns" => Some(SortColumn::Namespace),
                        "status" => Some(SortColumn::Status),
                        "age" => Some(SortColumn::Age),
                        _ => None,
                    };
                    let Some(column) = column else {
                        self.status_line =
                            "Usage: :sort name|namespace|status|age [asc|desc]".to_string();
                        return false;
                    };
                    let tab = self.current_tab_mut();
                    tab.sort = column;
                    match args.get(1).map(|d| d.to_ascii_lowercase()) {
                        Some(d) if d == "desc" => tab.descending = true,
                        Some(d) if d == "asc" => tab.descending = false,
                        _ => {}
                    }
                }
                let tab = self.current_tab();
                let col = match tab.sort {
                    SortColumn::Name => "name",
                    SortColumn::Namespace => "namespace",
                    SortColumn::Status => "status",
                    SortColumn::Age => "age",
                };
                let dir = if tab.descending { "desc" } else { "asc" };
                self.status_line = format!("Sort: {col} {dir}");
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
                self.status_line = self.filter_status_message("");
                false
            }
            "fmt" | "format" => {
                if args.is_empty() {
                    self.status_line = format!(
                        "Detail format: {}",
                        self.current_tab().detail_format.label()
                    );
                    return false;
                }
                let Some(format) = DetailFormat::parse(args[0]) else {
                    self.status_line = "Usage: :fmt <yaml|json>".to_string();
                    return false;
                };
                self.current_tab_mut().detail_format = format;
                self.current_tab_mut().detail_scroll = 0;
                self.current_tab_mut().detail_hscroll = 0;
                self.status_line = format!("Detail format: {}", format.label());
                false
            }
            "yaml" | "yml" => {
                self.current_tab_mut().detail_format = DetailFormat::Yaml;
                self.current_tab_mut().detail_scroll = 0;
                self.current_tab_mut().detail_hscroll = 0;
                self.status_line = "Detail format: yaml".to_string();
                false
            }
            "json" => {
                self.current_tab_mut().detail_format = DetailFormat::Json;
                self.current_tab_mut().detail_scroll = 0;
                self.current_tab_mut().detail_hscroll = 0;
                self.status_line = "Detail format: json".to_string();
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
            "edit" => {
                if let Some(token) = args.first() {
                    let Some(format) = DetailFormat::parse(token) else {
                        self.status_line = "Usage: :edit [yaml|json]".to_string();
                        return false;
                    };
                    self.edit_current_view(Some(format)).await;
                } else {
                    self.edit_current_view(None).await;
                }
                false
            }
            "tail" => {
                self.jump_logs_to_latest();
                false
            }
            "copy" | "yank" => {
                self.copy_current_view_to_clipboard();
                false
            }
            "dump" | "screendump" | "sd" => {
                self.dump_current_view(args);
                false
            }
            "api" | "apis" => {
                self.show_api_catalog(args.first().copied()).await;
                false
            }
            "xray" => {
                // :xray [namespace|all] — default to the current namespace; `all` = whole cluster.
                let namespace = match args.first() {
                    Some(token) if token.eq_ignore_ascii_case("all") => None,
                    Some(token) => Some(token.to_string()),
                    None => self.current_tab().namespace.clone(),
                };
                self.show_xray_overlay(namespace);
                false
            }
            "pulse" | "pulses" | "pu" | "popeye" | "pop" | "plugins" | "plugin" => {
                self.status_line =
                    format!("Command ':{cmd}' is recognized but not implemented yet");
                false
            }
            _ => match parse_resource_alias(cmd.as_str()) {
                ResourceAlias::Supported(kind) => {
                    self.execute_resource_command(kind, args);
                    false
                }
                // Not a curated kind — try the dynamic path (any discovered resource/CRD).
                // `:<resource>` lists objects; `:<resource> <name>` describes one.
                other => {
                    let name = args.first().copied();
                    if self.browse_dynamic(&cmd, name).await {
                        return false;
                    }
                    self.status_line = match other {
                        ResourceAlias::Unsupported(resource) => format!(
                            "'{resource}' not found on this context (try the full plural, e.g. :customresourcedefinitions)"
                        ),
                        _ => format!("Unknown command or resource: :{raw}"),
                    };
                    false
                }
            },
        }
    }

    /// Discovered API resources for a context, cached after the first lookup (Phase 4.1).
    pub(super) async fn discovery_for(&mut self, context: &str) -> Vec<DiscoveredResource> {
        if let Some(cached) = self.discovery_cache.get(context) {
            return cached.clone();
        }
        let provider = self.resource_provider.clone();
        match provider.discover(context).await {
            Ok(list) => {
                self.discovery_cache
                    .insert(context.to_string(), list.clone());
                list
            }
            Err(err) => {
                self.status_line = format!("API discovery failed: {err}");
                Vec::new()
            }
        }
    }

    /// `:api [filter]` — show the discovered API resource catalog in a scrollable overlay.
    pub(super) async fn show_api_catalog(&mut self, filter: Option<&str>) {
        let context = self.current_tab().context.clone();
        let catalog = self.discovery_for(&context).await;
        if catalog.is_empty() {
            return;
        }
        let needle = filter.map(|f| f.to_ascii_lowercase());
        let mut lines = vec![
            format!("API resources on {context} ({} total)", catalog.len()),
            String::new(),
            format!("{:<46} {:<26} SCOPE", "NAME", "KIND"),
        ];
        let mut shown = 0usize;
        for r in &catalog {
            if let Some(n) = &needle
                && !r.full_name().to_ascii_lowercase().contains(n.as_str())
                && !r.kind().to_ascii_lowercase().contains(n.as_str())
            {
                continue;
            }
            shown += 1;
            lines.push(format!(
                "{:<46} {:<26} {}",
                r.full_name(),
                r.kind(),
                if r.namespaced {
                    "namespaced"
                } else {
                    "cluster"
                }
            ));
        }
        lines.push(String::new());
        lines.push("Browse: :<plural|kind>     Describe: :<plural|kind> <name>".to_string());
        self.overlay = Some(Overlay::Text {
            title: "API Resources".to_string(),
            lines,
            scroll: 0,
            hscroll: 0,
            wrap: false,
        });
        self.status_line = format!("API resources: {shown} shown / {} total", catalog.len());
    }

    /// Resolve `token` against discovery and either list its objects or, with `name`, describe one.
    /// Returns false if the token doesn't match any discovered resource.
    pub(super) async fn browse_dynamic(&mut self, token: &str, name: Option<&str>) -> bool {
        let context = self.current_tab().context.clone();
        let tab_namespace = self.current_tab().namespace.clone();
        let catalog = self.discovery_for(&context).await;
        let Some(resource) = catalog
            .iter()
            .find(|r| {
                r.plural().eq_ignore_ascii_case(token)
                    || r.kind().eq_ignore_ascii_case(token)
                    || r.full_name().eq_ignore_ascii_case(token)
            })
            .cloned()
        else {
            return false;
        };

        let namespace = if resource.namespaced {
            tab_namespace
        } else {
            None
        };
        let provider = self.resource_provider.clone();

        if let Some(name) = name {
            match provider
                .get_dynamic(&context, &resource, namespace.as_deref(), name)
                .await
            {
                Ok(obj) => {
                    let yaml = to_pretty_yaml(&obj);
                    self.overlay = Some(Overlay::Text {
                        title: format!("{} {}", resource.kind(), name),
                        lines: yaml.lines().map(str::to_string).collect(),
                        scroll: 0,
                        hscroll: 0,
                        wrap: false,
                    });
                    self.status_line = format!("Describe {} {name}", resource.kind());
                }
                Err(err) => {
                    self.status_line = format!("get {} {name} failed: {err}", resource.kind())
                }
            }
            return true;
        }

        match provider
            .list_dynamic(&context, &resource, namespace.as_deref())
            .await
        {
            Ok(rows) => {
                let capped = if rows.len() >= 500 { ", capped" } else { "" };
                let mut lines = vec![
                    format!(
                        "{} ({} shown{capped})  ns={}",
                        resource.full_name(),
                        rows.len(),
                        namespace.as_deref().unwrap_or("all")
                    ),
                    String::new(),
                    format!("{:<24} {:<50} {:<8} STATUS", "NAMESPACE", "NAME", "AGE"),
                ];
                for row in &rows {
                    lines.push(format!(
                        "{:<24} {:<50} {:<8} {}",
                        row.namespace.as_deref().unwrap_or("-"),
                        row.name,
                        short_age(row.age),
                        row.status
                    ));
                }
                lines.push(String::new());
                lines.push(format!("Describe: :{token} <name>"));
                self.overlay = Some(Overlay::Text {
                    title: format!("{} objects", resource.kind()),
                    lines,
                    scroll: 0,
                    hscroll: 0,
                    wrap: false,
                });
                self.status_line = format!("{} {} object(s)", rows.len(), resource.kind());
            }
            Err(err) => self.status_line = format!("list {} failed: {err}", resource.full_name()),
        }
        true
    }

    pub(super) fn autocomplete_command_input(&mut self) {
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
            "fmt" | "format" | "edit" => {
                let prefix = arg_prefix.to_ascii_lowercase();
                let candidates: Vec<String> = ["yaml", "yml", "json"]
                    .iter()
                    .filter(|fmt| fmt.starts_with(&prefix))
                    .map(|fmt| (*fmt).to_string())
                    .collect();
                self.apply_completion_for_argument(&command, candidates, &prefix);
            }
            _ => {
                self.status_line = "No autocomplete candidates".to_string();
            }
        }
    }

    pub(super) fn apply_completion_for_first_token(
        &mut self,
        candidates: Vec<String>,
        prefix: &str,
    ) {
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

    pub(super) fn apply_completion_for_argument(
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

    pub(super) fn record_command_history(&mut self, command: String) {
        if self.command_history.last() != Some(&command) {
            self.command_history.push(command.clone());
        }
        self.last_command = Some(command);
        self.history_cursor = None;
    }

    pub(super) fn history_step_back(&mut self) -> Option<String> {
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

    pub(super) fn history_step_forward(&mut self) -> Option<String> {
        let current = self.history_cursor?;
        if current + 1 >= self.command_history.len() {
            self.history_cursor = None;
            return None;
        }
        let next = current + 1;
        self.history_cursor = Some(next);
        self.command_history.get(next).cloned()
    }

    pub(super) fn execute_resource_command(&mut self, kind: ResourceKind, args: &[&str]) {
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
            } else if (arg == "-n" || arg == "--namespace") && idx + 1 < args.len() {
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
}
