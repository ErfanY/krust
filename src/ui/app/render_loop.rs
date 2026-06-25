use super::*;

impl App {
    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let theme = self.theme;
        let active = self.current_tab().clone();
        let active_tab_idx = self.active_tab;
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let visible_rows = vm.len();
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
        let sort_col = match active.sort {
            SortColumn::Name => "name",
            SortColumn::Namespace => "ns",
            SortColumn::Status => "status",
            SortColumn::Age => "age",
        };
        let sort_arrow = if active.descending { "↓" } else { "↑" };
        let mut top_line = format!(
            "{} {}  [CTX] {} ({}/{})  [NS] {}  [K] {}  [P] {} {}  [SORT] {sort_col}{sort_arrow}  [CLR] {}  [REV] +{}  [STALE] {}s  [SEL] {selected_human}/{}  [VIS] {}  [CACHE] {}  [ERR] {}",
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
            && let Some(key) = vm.key(selected)
            && let Some(entity) = self.store.get(key)
            && let Some(res) = &entity.extracted.pod_resources
        {
            top_line.push_str(&format!(
                "  [SEL-RES] cpu {}/{} mem {}/{}",
                format_millicpu(res.cpu_request_m),
                format_millicpu(res.cpu_limit_m),
                format_bytes(res.mem_request_b),
                format_bytes(res.mem_limit_b)
            ));
        }

        let pulse_metrics = self.pulse_metrics_for_tab(&active);
        let running = pulse_metrics.running;
        let pending = pulse_metrics.pending;
        let failed = pulse_metrics.failed;
        let _other = pulse_metrics.other;
        let scope_pods = pulse_metrics.scope_pods;
        let cluster_pods = pulse_metrics.cluster_pods;
        let node_caps = pulse_metrics.node_caps;
        let cpu_pct = percent(cluster_pods.cpu_request_m, node_caps.cpu_alloc_m);
        let mem_pct = percent(cluster_pods.mem_request_b, node_caps.mem_alloc_b);
        let pod_pct = percent(cluster_pods.pods as u64, node_caps.pod_alloc);
        let deployments = pulse_metrics.deployments;
        let replicasets = pulse_metrics.replicasets;
        let statefulsets = pulse_metrics.statefulsets;
        let daemonsets = pulse_metrics.daemonsets;
        let services = pulse_metrics.services;
        let ingresses = pulse_metrics.ingresses;
        let jobs = pulse_metrics.jobs;
        let cronjobs = pulse_metrics.cronjobs;
        let pods = pulse_metrics.pods;

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
        // Phase 4.2: live actual usage from metrics.k8s.io when a metrics-server is present;
        // otherwise note its absence (the rows above are request/limit-based).
        match &self.metrics {
            Some(m) => {
                let cpu_sev = match ratio_percent_value(m.cpu_used_m, node_caps.cpu_alloc_m) {
                    Some(pct) if pct >= 90.0 => Severity::Err,
                    Some(pct) if pct >= 75.0 => Severity::Warn,
                    _ => Severity::Ok,
                };
                pulse_rows.push((
                    "[USE]".to_string(),
                    vec![
                        format!(
                            "cpu {} / {} ({})",
                            format_millicpu(m.cpu_used_m),
                            format_millicpu(node_caps.cpu_alloc_m),
                            percent(m.cpu_used_m, node_caps.cpu_alloc_m)
                        ),
                        format!(
                            "mem {} / {} ({})",
                            format_bytes(m.mem_used_b),
                            format_bytes(node_caps.mem_alloc_b),
                            percent(m.mem_used_b, node_caps.mem_alloc_b)
                        ),
                        format!("live · {} nodes reporting", m.nodes_reporting),
                    ],
                    cpu_sev,
                ));
            }
            None => {
                pulse_rows.push((
                    "[USE]".to_string(),
                    vec![
                        "live usage: metrics-server not available".to_string(),
                        "(rows above are request/limit-based)".to_string(),
                        String::new(),
                    ],
                    Severity::Ok,
                ));
            }
        }
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
        let help_text = self.context_help_text(&active);
        let help_height = if self.show_help {
            max_vertical_scroll_for_text(help_text.as_str(), frame.area().width.max(1), 1, true) + 1
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
                sync_table_viewport(selected, table_offset, viewport_rows, vm.len());
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
                    // Materialize only the visible window — O(viewport), not O(total rows).
                    // Pods get live CPU/MEM usage columns (metrics.k8s.io) instead of Summary.
                    let pods_view = active.kind() == ResourceKind::Pods;
                    let viewport_rows = table_viewport_rows(chunks[2].height);
                    let start = table_offset;
                    let end = start.saturating_add(viewport_rows).min(vm.len());
                    let rows: Vec<Row<'_>> = (start..end)
                        .filter_map(|idx| {
                            let key = vm.key(idx)?;
                            let row = materialize_row(&self.store, key)?;
                            let sev = classify_status_severity(&row.status);
                            let status = format!("{} {}", severity_tag(sev), row.status);
                            let cells = if pods_view {
                                let usage = self.pod_usage(key.namespace.as_deref(), &key.name);
                                let res =
                                    self.store.get(key).and_then(|e| e.extracted.pod_resources);
                                let cpu = pod_usage_cells(
                                    usage.map(|(c, _)| c),
                                    res.map(|r| r.cpu_request_m).unwrap_or(0),
                                    res.map(|r| r.cpu_limit_m).unwrap_or(0),
                                    format_millicpu,
                                );
                                let mem = pod_usage_cells(
                                    usage.map(|(_, m)| m),
                                    res.map(|r| r.mem_request_b).unwrap_or(0),
                                    res.map(|r| r.mem_limit_b).unwrap_or(0),
                                    format_bytes,
                                );
                                // Right-aligned numeric cell, optionally severity-colored.
                                let num = |s: String, sev: Severity| {
                                    Cell::from(Line::from(s).alignment(Alignment::Right))
                                        .style(severity_style(&theme, sev))
                                };
                                vec![
                                    Cell::from(row.namespace),
                                    Cell::from(row.name),
                                    Cell::from(status),
                                    Cell::from(row.age),
                                    num(cpu.used, Severity::Ok),
                                    num(cpu.req_pct, cpu.req_sev),
                                    num(cpu.lim_pct, cpu.lim_sev),
                                    num(mem.used, Severity::Ok),
                                    num(mem.req_pct, mem.req_sev),
                                    num(mem.lim_pct, mem.lim_sev),
                                ]
                            } else {
                                vec![
                                    Cell::from(row.namespace),
                                    Cell::from(row.name),
                                    Cell::from(status),
                                    Cell::from(row.age),
                                    Cell::from(row.summary),
                                ]
                            };
                            Some(Row::new(cells).style(severity_style(&theme, sev)))
                        })
                        .collect();

                    // Mark the active sort column in the header with a direction arrow.
                    let arrow = if active.descending { " ↓" } else { " ↑" };
                    // Left-aligned, sortable text header.
                    let mark = |label: &str, col: SortColumn| {
                        let text = if active.sort == col {
                            format!("{label}{arrow}")
                        } else {
                            label.to_string()
                        };
                        Cell::from(text)
                    };
                    // Right-aligned numeric header (matches the right-aligned numeric cells).
                    let num_head = |label: &str| {
                        Cell::from(Line::from(label.to_string()).alignment(Alignment::Right))
                    };
                    let (header, widths): (Vec<Cell>, Vec<Constraint>) = if pods_view {
                        (
                            vec![
                                mark("Namespace", SortColumn::Namespace),
                                mark("Name", SortColumn::Name),
                                mark("Status", SortColumn::Status),
                                mark("Age", SortColumn::Age),
                                num_head("CPU"),
                                num_head("CR"),
                                num_head("CL"),
                                num_head("MEM"),
                                num_head("MR"),
                                num_head("ML"),
                            ],
                            vec![
                                Constraint::Length(16),
                                Constraint::Min(20),
                                Constraint::Length(16),
                                Constraint::Length(6),
                                Constraint::Length(8),
                                Constraint::Length(6),
                                Constraint::Length(6),
                                Constraint::Length(9),
                                Constraint::Length(6),
                                Constraint::Length(6),
                            ],
                        )
                    } else {
                        (
                            vec![
                                mark("Namespace", SortColumn::Namespace),
                                mark("Name", SortColumn::Name),
                                mark("Status", SortColumn::Status),
                                mark("Age", SortColumn::Age),
                                Cell::from("Summary"),
                            ],
                            vec![
                                Constraint::Length(20),
                                Constraint::Min(24),
                                Constraint::Length(18),
                                Constraint::Length(8),
                                Constraint::Min(10),
                            ],
                        )
                    };
                    let title = if pods_view {
                        let legend = if self.pod_metrics.is_empty() {
                            "CPU/MEM: metrics-server n/a"
                        } else {
                            "CPU/MEM used · CR/CL = cpu %req/%lim · MR/ML = mem %req/%lim"
                        };
                        format!("[KIND] {} ({}) · {legend}", active.kind(), vm.len())
                    } else {
                        format!("[KIND] {} ({})", active.kind(), vm.len())
                    };
                    let table = Table::new(rows, widths)
                        .header(Row::new(header).style(theme.table_header))
                        .column_spacing(2)
                        .block(Block::default().borders(Borders::ALL).title(title))
                        .row_highlight_style(theme.row_highlight);

                    // Rows are pre-sliced to the window, so the widget offset is 0 and the
                    // selection is window-relative.
                    let mut state = ratatui::widgets::TableState::default()
                        .with_selected(Some(selected.saturating_sub(start)))
                        .with_offset(0);
                    frame.render_stateful_widget(table, chunks[2], &mut state);
                }
                Pane::Describe | Pane::SecretDecode | Pane::Events => {
                    let key = vm.key(selected).cloned();
                    let raw_body =
                        self.detail_text(key.as_ref(), active.pane, active.detail_format);
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

    pub(super) fn context_help_text(&self, active: &ContextTabState) -> String {
        if let Some(input) = &self.command_input {
            return match input.mode {
                CommandMode::Command => [
                    "mode:command",
                    "type command",
                    "Tab autocomplete",
                    "Enter execute",
                    "ctrl+w del-word",
                    "Backspace del-char",
                    "Esc cancel",
                ]
                .join(" | "),
                CommandMode::Filter => [
                    "mode:filter/search",
                    "type to update live",
                    "n/N next/prev match",
                    "Tab autocomplete",
                    "Enter keep",
                    "Esc clear+cancel",
                    "ctrl+w del-word",
                ]
                .join(" | "),
            };
        }

        if let Some(overlay) = &self.overlay {
            return match overlay {
                Overlay::Contexts { .. } => [
                    "overlay:contexts",
                    "j/k or up/down move",
                    "Enter switch",
                    "/ filter",
                    "Esc close",
                ]
                .join(" | "),
                Overlay::Containers { .. } => [
                    "overlay:containers",
                    "j/k or up/down move",
                    "Enter select",
                    "/ filter",
                    "Esc close",
                ]
                .join(" | "),
                Overlay::LogSources { .. } => [
                    "overlay:log-sources",
                    "j/k or up/down move",
                    "Enter/Space toggle",
                    "a enable all",
                    "/ filter",
                    "Esc close",
                ]
                .join(" | "),
                Overlay::Text { .. } => [
                    "overlay:text",
                    "j/k or up/down scroll",
                    "w wrap",
                    "left/right h-scroll",
                    "Esc close",
                ]
                .join(" | "),
            };
        }

        // Key hints are derived from the active keymap so they stay correct when remapped.
        let k = &self.keymap;
        let lit = |s: &str| s.to_string();
        let mut items: Vec<String> = vec![
            format!("{} quit", k.hint(Action::Quit)),
            lit(": command"),
            format!(
                "{}/{} ctx",
                k.hint(Action::NextContext),
                k.hint(Action::PrevContext)
            ),
            lit("[ ]/- history"),
            lit("ctrl+a aliases :api"),
        ];

        match active.pane {
            Pane::Table => {
                items.extend([
                    format!(
                        "{}/{} move",
                        k.hint(Action::MoveDown),
                        k.hint(Action::MoveUp)
                    ),
                    format!(
                        "{}/{} top/bot",
                        k.hint(Action::GotoTop),
                        k.hint(Action::GotoBottom)
                    ),
                    format!("{} filter", k.hint(Action::FilterMode)),
                    lit("Enter open/select-ns"),
                    format!("{} describe", k.hint(Action::ToggleDescribe)),
                    format!("{} logs", k.hint(Action::ToLogs)),
                    format!("{} events", k.hint(Action::ToEvents)),
                    lit("c containers"),
                    format!("{} namespace", k.hint(Action::CycleNamespace)),
                    format!("{} sort", k.hint(Action::CycleSort)),
                    format!("{} reverse", k.hint(Action::ToggleDesc)),
                    format!("{} help", k.hint(Action::ToggleHelp)),
                    format!("{} delete", k.hint(Action::Delete)),
                    format!("{} clear-filter", k.hint(Action::Cancel)),
                ]);
            }
            Pane::Describe | Pane::SecretDecode | Pane::Events => {
                items.extend([
                    format!(
                        "{}/{} scroll",
                        k.hint(Action::MoveDown),
                        k.hint(Action::MoveUp)
                    ),
                    lit("gg/G top/bottom"),
                    lit("ctrl+d/u half-page"),
                    lit("n/N match"),
                    lit("/ search"),
                    lit("w wrap"),
                    lit("left/right h-scroll"),
                    lit("y copy"),
                    format!("{} describe-toggle", k.hint(Action::ToggleDescribe)),
                ]);
                if matches!(active.pane, Pane::Describe | Pane::SecretDecode) {
                    items.extend([lit("e edit"), lit(":fmt yaml|json")]);
                }
                if active.kind() == ResourceKind::Secrets {
                    items.push(lit("x decode toggle"));
                }
                items.push(format!("{} logs", k.hint(Action::ToLogs)));
                items.push(format!("{} table", k.hint(Action::ToTable)));
                items.push(format!("{} close", k.hint(Action::Cancel)));
            }
            Pane::Logs => {
                items.extend([
                    format!(
                        "{}/{} scroll",
                        k.hint(Action::MoveDown),
                        k.hint(Action::MoveUp)
                    ),
                    lit("gg/G top/bottom"),
                    lit("ctrl+d/u half-page"),
                    lit("n/N match"),
                    lit("/ search"),
                    lit("w wrap"),
                    lit("left/right h-scroll"),
                    lit("y copy"),
                    format!("{} tail", k.hint(Action::CycleSort)),
                    lit("p pause"),
                    lit("S sources"),
                    lit("L latest"),
                    lit("c containers"),
                    format!("{} describe", k.hint(Action::ToggleDescribe)),
                    format!("{} table", k.hint(Action::ToTable)),
                    format!("{} close", k.hint(Action::Cancel)),
                ]);
            }
        }

        items.join(" | ")
    }
}
