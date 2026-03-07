use std::{fs, path::PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub runtime: RuntimeConfig,
    pub ui: UiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig::default(),
            ui: UiConfig::default(),
        }
    }
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
    pub namespace: Option<String>,
    pub default_context: Option<String>,
    pub watch_buffer_size: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            fps_limit: 60,
            delta_channel_capacity: 2048,
            namespace: None,
            default_context: None,
            watch_buffer_size: 2048,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub show_help: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            show_help: true,
        }
    }
}

pub fn default_config_path() -> PathBuf {
    let mut base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.push("krs");
    base.push("config.toml");
    base
}

pub fn default_keymap_path() -> PathBuf {
    let mut base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.push("krs");
    base.push("keymap.toml");
    base
}
