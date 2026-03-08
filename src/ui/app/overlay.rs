use super::*;

impl App {
    pub(super) fn show_contexts_overlay(&mut self) {
        let contexts = self.tabs.iter().map(|tab| tab.context.clone()).collect();
        self.overlay = Some(Overlay::Contexts {
            title: "Contexts".to_string(),
            contexts,
            selected: self.active_tab,
            filter: String::new(),
        });
        self.status_line = "Context list opened".to_string();
    }

    pub(super) fn show_log_sources_overlay(&mut self) {
        let sources = self.available_log_sources();
        self.overlay = Some(Overlay::LogSources {
            title: "Log Sources".to_string(),
            sources,
            selected: 0,
            filter: String::new(),
        });
        self.status_line = "Log source filter opened".to_string();
    }

    pub(super) fn open_container_picker_from_selection(&mut self) {
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

    pub(super) fn show_resource_aliases_overlay(&mut self) {
        let lines = vec![
            "Supported resource aliases:".to_string(),
            "po|pod, deploy|dp, rs, sts, ds, svc, ing, cm, sec|secret, job, cj".to_string(),
            "pvc|claim, pv, no|node, ns|namespace, ev|event, sa, role, rb".to_string(),
            "crole, crb, netpol|np, hpa, pdb".to_string(),
            String::new(),
            "Commands: :ctx :ns :kind :fmt :c :sources :edit :copy :dump :resources :clear :quit"
                .to_string(),
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

    pub(super) async fn handle_overlay_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.overlay = None;
                self.status_line = "Closed view".to_string();
            }
            KeyCode::Char('/') => {
                let existing_filter = self.active_filter_value();
                self.command_input = Some(CommandInput::new(CommandMode::Filter, existing_filter));
                self.status_line = format!("{} mode", self.active_filter_label());
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
                    && *selected < sources.len()
                {
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

    pub(super) fn scroll_overlay_or_select(&mut self, delta: isize) {
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

    pub(super) fn scroll_overlay_horizontal(&mut self, delta: isize) {
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

    pub(super) fn overlay_home(&mut self) {
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

    pub(super) fn overlay_end(&mut self) {
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
}
