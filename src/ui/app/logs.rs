use super::*;

impl App {
    pub(super) fn pod_container_names_for_key(&self, pod_key: &ResourceKey) -> Vec<String> {
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

    pub(super) fn pod_log_targets(
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

        if let Some(target_container) = container_override
            && containers.iter().any(|name| name == target_container)
        {
            return vec![LogTarget {
                context: pod_key.context.clone(),
                namespace,
                pod: pod_key.name.clone(),
                container: Some(target_container.to_string()),
            }];
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

    pub(super) fn replica_set_pod_keys(&self, rs_key: &ResourceKey) -> Vec<ResourceKey> {
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

    pub(super) fn deployment_replica_set_keys(&self, dep_key: &ResourceKey) -> Vec<ResourceKey> {
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

    pub(super) fn deployment_pod_keys(&self, dep_key: &ResourceKey) -> Vec<ResourceKey> {
        let mut pods = Vec::new();
        for rs_key in self.deployment_replica_set_keys(dep_key) {
            pods.extend(self.replica_set_pod_keys(&rs_key));
        }
        pods.sort_by(|a, b| a.name.cmp(&b.name));
        pods.dedup();
        pods
    }

    pub(super) fn desired_log_selection(&mut self) -> Option<LogSelection> {
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

    pub(super) fn stop_log_session(&mut self) {
        if let Some(session) = self.logs.session.take() {
            for task in session.tasks {
                task.abort();
            }
        }
    }

    pub(super) fn reset_log_buffer(&mut self) {
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

    pub(super) async fn start_log_session(
        &mut self,
        selection: LogSelection,
        initial: bool,
    ) -> bool {
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

    pub(super) fn schedule_log_reconnect(&mut self) {
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

    pub(super) async fn reconcile_log_session(&mut self) -> bool {
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

    pub(super) fn push_log_line(&mut self, line: String) {
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

    pub(super) fn drain_log_events(&mut self) -> bool {
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

    pub(super) fn ensure_log_joined_cache(&mut self) {
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

    pub(super) fn log_joined_text(&mut self) -> &str {
        self.ensure_log_joined_cache();
        self.logs.joined_cache.as_str()
    }

    pub(super) fn log_body_text(&mut self) -> String {
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

    pub(super) fn logs_title(&self) -> String {
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

    pub(super) fn set_log_paused(&mut self, paused: bool) {
        self.logs.paused = paused;
        if paused {
            self.logs.auto_scroll = false;
            self.status_line = "Log stream paused".to_string();
        } else {
            self.status_line = "Log stream resumed".to_string();
        }
    }

    pub(super) fn jump_logs_to_latest(&mut self) {
        self.logs.paused = false;
        self.logs.auto_scroll = true;
        self.current_tab_mut().detail_scroll = u16::MAX;
        self.status_line = "Logs: jumped to latest and resumed tailing".to_string();
    }

    pub(super) fn available_log_sources(&self) -> Vec<String> {
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

    pub(super) fn toggle_log_source(&mut self, source: &str) -> bool {
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

    pub(super) fn filtered_log_line_count_and_width(&self) -> (usize, usize) {
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

    pub(super) fn filtered_log_body_text(&mut self) -> String {
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

    pub(super) fn log_search_match_lines(&mut self, query: &str) -> Vec<usize> {
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
}
