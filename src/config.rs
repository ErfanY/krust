use std::{fs, path::PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub runtime: RuntimeConfig,
    pub ui: UiConfig,
}

impl AppConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = default_config_path();
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let parsed: Self = toml::from_str(&content)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub fps_limit: u16,
    pub delta_channel_capacity: usize,
    pub warm_contexts: usize,
    pub warm_context_ttl_secs: u64,
    /// How often to re-fetch live metrics (metrics.k8s.io). metrics-server typically only produces
    /// a new sample every ~15s, so polling much faster mostly re-fetches identical numbers; lower
    /// for a more live feel at the cost of API load on large fleets.
    pub metrics_interval_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            fps_limit: 60,
            delta_channel_capacity: 2048,
            warm_contexts: 1,
            warm_context_ttl_secs: 20,
            metrics_interval_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub show_help: bool,
    /// Capture the mouse for scroll-wheel support. When true (default), the terminal's native
    /// click-drag text selection is disabled while krust runs; set false (or toggle with `:mouse`)
    /// to keep native selection. Either way `y`/`:dump` copy from within krust.
    pub mouse_capture: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            show_help: true,
            mouse_capture: true,
        }
    }
}

pub fn default_config_path() -> PathBuf {
    let mut base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.push("krust");
    base.push("config.toml");
    base
}

pub fn default_keymap_path() -> PathBuf {
    let mut base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.push("krust");
    base.push("keymap.toml");
    base
}
