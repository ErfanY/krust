use super::*;

fn build_edited_manifest(
    pane: Pane,
    detail_format: DetailFormat,
    original: &serde_json::Value,
    edited_text: &str,
) -> Result<serde_json::Value, String> {
    match (pane, detail_format) {
        (Pane::Describe, DetailFormat::Yaml) => {
            parse_yaml_to_json(edited_text).map_err(|err| format!("Invalid YAML: {err}"))
        }
        (Pane::Describe, DetailFormat::Json) => {
            parse_json_to_json(edited_text).map_err(|err| format!("Invalid JSON: {err}"))
        }
        (Pane::SecretDecode, DetailFormat::Yaml) => {
            apply_decoded_secret_yaml(original, edited_text)
                .map_err(|err| format!("Invalid decoded secret YAML: {err}"))
        }
        (Pane::SecretDecode, DetailFormat::Json) => {
            apply_decoded_secret_json(original, edited_text)
                .map_err(|err| format!("Invalid decoded secret JSON: {err}"))
        }
        _ => Err("Edit is available in Describe/Decode panes".to_string()),
    }
}

impl App {
    pub(super) fn in_detail_pane(&self) -> bool {
        self.current_tab().pane != Pane::Table
    }

    pub(super) fn current_detail_body_for_search(&mut self) -> Option<String> {
        let tab = self.current_tab().clone();
        match tab.pane {
            Pane::Table => None,
            Pane::Logs => Some(self.log_body_text()),
            Pane::Describe | Pane::SecretDecode | Pane::Events => {
                let request = self.view_request_for_tab(&tab);
                let vm = self.projected_view(&request);
                let selected = tab.selected.min(vm.len().saturating_sub(1));
                let key = vm.key(selected).cloned();
                let raw = self.detail_text(key.as_ref(), tab.pane, tab.detail_format);
                Some(raw)
            }
        }
    }

    pub(super) fn jump_detail_match(&mut self, forward: bool) {
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

        let current_line = self
            .current_tab()
            .detail_active_match_line
            .filter(|line| matches.contains(line))
            .unwrap_or(self.current_tab().detail_scroll as usize);
        let Some((target, match_pos)) = step_match_line(&matches, current_line, forward) else {
            self.status_line = format!("No matches for '{}'", needle.trim());
            return;
        };
        let tab = self.current_tab_mut();
        tab.detail_scroll = target.min(u16::MAX as usize) as u16;
        tab.detail_active_match_line = Some(target);
        if self.current_tab().pane == Pane::Logs {
            self.logs.auto_scroll = false;
        }
        self.status_line = format!("Match {match_pos}/{}", matches.len());
    }

    pub(super) fn scroll_detail(&mut self, delta: isize) {
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

    pub(super) fn scroll_detail_horizontal(&mut self, delta: isize) {
        let tab = self.current_tab_mut();
        if delta < 0 {
            tab.detail_hscroll = tab
                .detail_hscroll
                .saturating_sub(delta.unsigned_abs() as u16);
        } else {
            tab.detail_hscroll = tab.detail_hscroll.saturating_add(delta as u16);
        }
    }

    pub(super) fn toggle_detail_wrap(&mut self) {
        let tab = self.current_tab_mut();
        tab.detail_wrap = !tab.detail_wrap;
        if tab.detail_wrap {
            tab.detail_hscroll = 0;
            self.status_line = "Wrap: on".to_string();
        } else {
            self.status_line = "Wrap: off (use left/right to scroll horizontally)".to_string();
        }
    }

    pub(super) fn detail_text(
        &self,
        key: Option<&ResourceKey>,
        pane: Pane,
        format: DetailFormat,
    ) -> String {
        let Some(key) = key else {
            return "No resource selected".to_string();
        };
        // Events pane on a non-Event resource shows that resource's correlated events (Phase 4.3),
        // not its manifest — served from events_cache, not the on-demand object.
        if pane == Pane::Events && key.kind != ResourceKind::Events {
            return self.format_correlated_events(key);
        }
        // The full object is fetched on demand (lean entity model) and held in detail_cache.
        let Some(detail) = self.detail_cache.as_ref().filter(|d| &d.key == key) else {
            return format!("Loading {} {} …", key.kind.short_name(), key.name);
        };
        if let Some(err) = &detail.error {
            return format!(
                "Failed to load {} {}: {err}",
                key.kind.short_name(),
                key.name
            );
        }
        let raw = &detail.value;

        match pane {
            Pane::Describe => match format {
                DetailFormat::Yaml => to_pretty_yaml(raw),
                DetailFormat::Json => to_pretty_json(raw),
            },
            Pane::SecretDecode => {
                if key.kind != ResourceKind::Secrets {
                    return "Decode pane is available for Secret resources only".to_string();
                }
                match format {
                    DetailFormat::Yaml => decoded_secret_text(raw),
                    DetailFormat::Json => decoded_secret_json_text(raw),
                }
            }
            Pane::Events => {
                // Non-Event resources are handled earlier (correlated events); here the selection
                // is an Event itself, so show its manifest.
                match format {
                    DetailFormat::Yaml => to_pretty_yaml(raw),
                    DetailFormat::Json => to_pretty_json(raw),
                }
            }
            Pane::Logs => {
                "Log streaming is planned; this pane is wired for future pod log tailing."
                    .to_string()
            }
            Pane::Table => "".to_string(),
        }
    }

    pub(super) fn toggle_describe(&mut self) {
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

    pub(super) fn toggle_secret_decode(&mut self) {
        if self.current_tab().pane == Pane::SecretDecode {
            let tab = self.current_tab_mut();
            tab.pane = Pane::Describe;
            tab.detail_scroll = 0;
            tab.detail_hscroll = 0;
            self.overlay = None;
            self.status_line = "Decode: off".to_string();
            return;
        }

        let Some(row) = self.selected_row() else {
            self.status_line = "No resource selected".to_string();
            return;
        };
        if row.key.kind != ResourceKind::Secrets {
            self.status_line = "Decode is only available for Secret resources".to_string();
            return;
        }

        let tab = self.current_tab_mut();
        tab.pane = Pane::SecretDecode;
        tab.detail_scroll = 0;
        tab.detail_hscroll = 0;
        self.overlay = None;
        self.status_line = format!(
            "Decode: {} {}",
            row.key.namespace.as_deref().unwrap_or("default"),
            row.key.name
        );
    }

    pub(super) async fn edit_current_view(&mut self, format_override: Option<DetailFormat>) {
        let pane = self.current_tab().pane;
        if !matches!(pane, Pane::Describe | Pane::SecretDecode) {
            self.status_line = "Edit is available in Describe/Decode panes".to_string();
            return;
        }

        let detail_format = format_override.unwrap_or(self.current_tab().detail_format);
        if format_override.is_some() {
            self.current_tab_mut().detail_format = detail_format;
        }

        let active = self.current_tab().clone();
        let request = self.view_request_for_tab(&active);
        let vm = self.projected_view(&request);
        let selected = active.selected.min(vm.len().saturating_sub(1));
        let Some(key) = vm.key(selected).cloned() else {
            self.status_line = "No resource selected".to_string();
            return;
        };
        // Reuse the on-demand detail object if it is for this row; otherwise fetch it now.
        let cached = self
            .detail_cache
            .as_ref()
            .filter(|d| d.key == key && d.error.is_none())
            .map(|d| d.value.clone());
        let original = match cached {
            Some(value) => value,
            None => {
                let provider = self.resource_provider.clone();
                match provider.get_object(&key).await {
                    Ok(value) => value,
                    Err(err) => {
                        self.status_line = format!("Failed to load {} for edit: {err}", key.name);
                        return;
                    }
                }
            }
        };

        let initial_text = match pane {
            Pane::Describe => match detail_format {
                DetailFormat::Yaml => to_pretty_yaml(&original),
                DetailFormat::Json => to_pretty_json(&original),
            },
            Pane::SecretDecode => {
                if key.kind != ResourceKind::Secrets {
                    self.status_line =
                        "Decode edit is only available for Secret resources".to_string();
                    return;
                }
                match detail_format {
                    DetailFormat::Yaml => decoded_secret_text(&original),
                    DetailFormat::Json => decoded_secret_json_text(&original),
                }
            }
            _ => {
                self.status_line = "Edit is available in Describe/Decode panes".to_string();
                return;
            }
        };

        let editor_result = run_external_editor(&initial_text, detail_format.extension());
        self.needs_terminal_reset = true;
        let edited_text = match editor_result {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.status_line = "Edit canceled".to_string();
                return;
            }
            Err(err) => {
                self.status_line = format!("Editor failed: {err}");
                return;
            }
        };

        if edited_text == initial_text {
            self.status_line = "No changes to apply".to_string();
            return;
        }

        let manifest = match build_edited_manifest(pane, detail_format, &original, &edited_text) {
            Ok(manifest) => manifest,
            Err(err) => {
                self.status_line = err;
                return;
            }
        };

        let result = self.action_executor.replace_resource(&key, manifest).await;
        self.status_line = match result {
            Ok(outcome) => outcome.message,
            Err(error) => render_action_error(error, &key),
        };
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DetailFormat, Pane, build_edited_manifest};
    use crate::ui::detail::base64_decode;

    fn decode_data_field(value: &serde_json::Value, key: &str) -> String {
        let encoded = value
            .get("data")
            .and_then(serde_json::Value::as_object)
            .and_then(|obj| obj.get(key))
            .and_then(serde_json::Value::as_str)
            .expect("encoded key exists");
        let bytes = base64_decode(encoded).expect("valid base64");
        String::from_utf8(bytes).expect("utf8 payload")
    }

    #[test]
    fn build_manifest_from_describe_yaml() {
        let original = json!({});
        let edited = "metadata:\n  name: demo\nspec:\n  replicas: 2\n";
        let out = build_edited_manifest(Pane::Describe, DetailFormat::Yaml, &original, edited)
            .expect("yaml describe parse succeeds");
        assert_eq!(out["metadata"]["name"], "demo");
        assert_eq!(out["spec"]["replicas"], 2);
    }

    #[test]
    fn build_manifest_from_describe_json_reports_parse_error() {
        let original = json!({});
        let err = build_edited_manifest(Pane::Describe, DetailFormat::Json, &original, "{ nope }")
            .expect_err("invalid json");
        assert!(err.starts_with("Invalid JSON:"));
    }

    #[test]
    fn build_manifest_from_secret_decode_yaml_reencodes_values() {
        let original = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "demo" },
            "data": { "old": "b2xk" },
            "stringData": { "tmp": "remove" }
        });
        let edited = "username: admin\nenabled: true\n";
        let out = build_edited_manifest(Pane::SecretDecode, DetailFormat::Yaml, &original, edited)
            .expect("secret decode yaml apply");
        assert_eq!(decode_data_field(&out, "username"), "admin");
        assert_eq!(decode_data_field(&out, "enabled"), "true");
        assert!(out.get("stringData").is_none());
    }

    #[test]
    fn build_manifest_from_secret_decode_json_reports_type_error() {
        let original = json!({ "kind": "Secret", "data": {} });
        let err = build_edited_manifest(Pane::SecretDecode, DetailFormat::Json, &original, "[]")
            .expect_err("invalid decode json");
        assert!(err.starts_with("Invalid decoded secret JSON:"));
    }

    #[test]
    fn build_manifest_from_unsupported_pane_is_rejected() {
        let original = json!({});
        let err = build_edited_manifest(Pane::Table, DetailFormat::Yaml, &original, "a: b")
            .expect_err("table pane cannot be edited");
        assert_eq!(err, "Edit is available in Describe/Decode panes");
    }
}
