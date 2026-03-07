use std::{fmt, fs};

use anyhow::Context;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::config::default_keymap_path;

#[derive(Debug, Clone)]
pub struct Keymap {
    quit: Binding,
    next_context: Binding,
    prev_context: Binding,
    next_kind: Binding,
    prev_kind: Binding,
    move_down: Binding,
    move_up: Binding,
    goto_top: Binding,
    goto_bottom: Binding,
    filter_mode: Binding,
    cycle_sort: Binding,
    toggle_desc: Binding,
    cycle_namespace: Binding,
    toggle_help: Binding,
    to_table: Binding,
    toggle_describe: Binding,
    to_events: Binding,
    to_logs: Binding,
    delete: Binding,
    confirm: Binding,
    cancel: Binding,
}

impl Keymap {
    pub fn load() -> anyhow::Result<Self> {
        let path = default_keymap_path();
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read keymap file {}", path.display()))?;
        let raw: RawKeymap = toml::from_str(&content)
            .with_context(|| format!("failed to parse keymap file {}", path.display()))?;

        Self::try_from(raw)
    }

    pub fn is(&self, action: Action, key: &KeyEvent) -> bool {
        let binding = match action {
            Action::Quit => &self.quit,
            Action::NextContext => &self.next_context,
            Action::PrevContext => &self.prev_context,
            Action::NextKind => &self.next_kind,
            Action::PrevKind => &self.prev_kind,
            Action::MoveDown => &self.move_down,
            Action::MoveUp => &self.move_up,
            Action::GotoTop => &self.goto_top,
            Action::GotoBottom => &self.goto_bottom,
            Action::FilterMode => &self.filter_mode,
            Action::CycleSort => &self.cycle_sort,
            Action::ToggleDesc => &self.toggle_desc,
            Action::CycleNamespace => &self.cycle_namespace,
            Action::ToggleHelp => &self.toggle_help,
            Action::ToTable => &self.to_table,
            Action::ToggleDescribe => &self.toggle_describe,
            Action::ToEvents => &self.to_events,
            Action::ToLogs => &self.to_logs,
            Action::Delete => &self.delete,
            Action::Confirm => &self.confirm,
            Action::Cancel => &self.cancel,
        };
        binding.matches(key)
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            quit: Binding::from_str("ctrl+c").expect("valid default"),
            next_context: Binding::from_str("tab").expect("valid default"),
            prev_context: Binding::from_str("shift+tab").expect("valid default"),
            next_kind: Binding::from_str("alt+right").expect("valid default"),
            prev_kind: Binding::from_str("alt+left").expect("valid default"),
            move_down: Binding::from_str("j").expect("valid default"),
            move_up: Binding::from_str("k").expect("valid default"),
            goto_top: Binding::from_str("g").expect("valid default"),
            goto_bottom: Binding::from_str("shift+g").expect("valid default"),
            filter_mode: Binding::from_str("/").expect("valid default"),
            cycle_sort: Binding::from_str("s").expect("valid default"),
            toggle_desc: Binding::from_str("r").expect("valid default"),
            cycle_namespace: Binding::from_str("n").expect("valid default"),
            toggle_help: Binding::from_str("?").expect("valid default"),
            to_table: Binding::from_str("t").expect("valid default"),
            toggle_describe: Binding::from_str("d").expect("valid default"),
            to_events: Binding::from_str("shift+e").expect("valid default"),
            to_logs: Binding::from_str("l").expect("valid default"),
            delete: Binding::from_str("ctrl+d").expect("valid default"),
            confirm: Binding::from_str("y").expect("valid default"),
            cancel: Binding::from_str("esc").expect("valid default"),
        }
    }
}

impl TryFrom<RawKeymap> for Keymap {
    type Error = anyhow::Error;

    fn try_from(raw: RawKeymap) -> Result<Self, Self::Error> {
        Ok(Self {
            quit: Binding::from_str(&raw.quit)?,
            next_context: Binding::from_str(&raw.next_context)?,
            prev_context: Binding::from_str(&raw.prev_context)?,
            next_kind: Binding::from_str(&raw.next_kind)?,
            prev_kind: Binding::from_str(&raw.prev_kind)?,
            move_down: Binding::from_str(&raw.move_down)?,
            move_up: Binding::from_str(&raw.move_up)?,
            goto_top: Binding::from_str(&raw.goto_top)?,
            goto_bottom: Binding::from_str(&raw.goto_bottom)?,
            filter_mode: Binding::from_str(&raw.filter_mode)?,
            cycle_sort: Binding::from_str(&raw.cycle_sort)?,
            toggle_desc: Binding::from_str(&raw.toggle_desc)?,
            cycle_namespace: Binding::from_str(&raw.cycle_namespace)?,
            toggle_help: Binding::from_str(&raw.toggle_help)?,
            to_table: Binding::from_str(&raw.to_table)?,
            toggle_describe: Binding::from_str(&raw.toggle_describe)?,
            to_events: Binding::from_str(&raw.to_events)?,
            to_logs: Binding::from_str(&raw.to_logs)?,
            delete: Binding::from_str(&raw.delete)?,
            confirm: Binding::from_str(&raw.confirm)?,
            cancel: Binding::from_str(&raw.cancel)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Quit,
    NextContext,
    PrevContext,
    NextKind,
    PrevKind,
    MoveDown,
    MoveUp,
    GotoTop,
    GotoBottom,
    FilterMode,
    CycleSort,
    ToggleDesc,
    CycleNamespace,
    ToggleHelp,
    ToTable,
    ToggleDescribe,
    ToEvents,
    ToLogs,
    Delete,
    Confirm,
    Cancel,
}

#[derive(Debug, Clone)]
struct Binding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Binding {
    fn from_str(raw: &str) -> anyhow::Result<Self> {
        parse_binding(raw)
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        let normalized = normalize_event(*key);
        self.code == normalized.code && self.modifiers == normalized.modifiers
    }
}

#[derive(Debug, Clone, Copy)]
struct NormalizedKey {
    code: KeyCode,
    modifiers: KeyModifiers,
}

fn normalize_event(key: KeyEvent) -> NormalizedKey {
    match key.code {
        KeyCode::Char(ch) if ch.is_ascii_uppercase() => NormalizedKey {
            code: KeyCode::Char(ch.to_ascii_lowercase()),
            modifiers: key.modifiers | KeyModifiers::SHIFT,
        },
        _ => NormalizedKey {
            code: key.code,
            modifiers: key.modifiers,
        },
    }
}

fn parse_binding(raw: &str) -> anyhow::Result<Binding> {
    let mut modifiers = KeyModifiers::empty();
    let mut code = None;

    for token in raw.to_lowercase().split('+') {
        let token = token.trim();
        match token {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "alt" => modifiers |= KeyModifiers::ALT,
            "tab" => code = Some(KeyCode::Tab),
            "enter" => code = Some(KeyCode::Enter),
            "esc" | "escape" => code = Some(KeyCode::Esc),
            "up" => code = Some(KeyCode::Up),
            "down" => code = Some(KeyCode::Down),
            "left" => code = Some(KeyCode::Left),
            "right" => code = Some(KeyCode::Right),
            "backspace" => code = Some(KeyCode::Backspace),
            "space" => code = Some(KeyCode::Char(' ')),
            "?" => code = Some(KeyCode::Char('?')),
            _ if token.len() == 1 => {
                let ch = token
                    .chars()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("invalid key binding: {raw}"))?;
                code = Some(KeyCode::Char(ch));
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "unsupported key token '{token}' in '{raw}'"
                ));
            }
        }
    }

    if raw.to_lowercase() == "shift+tab" {
        return Ok(Binding {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::SHIFT,
        });
    }

    let code = code.ok_or_else(|| anyhow::anyhow!("key binding '{raw}' has no key code"))?;
    Ok(Binding { code, modifiers })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct RawKeymap {
    quit: String,
    next_context: String,
    prev_context: String,
    next_kind: String,
    prev_kind: String,
    move_down: String,
    move_up: String,
    goto_top: String,
    goto_bottom: String,
    filter_mode: String,
    cycle_sort: String,
    toggle_desc: String,
    cycle_namespace: String,
    toggle_help: String,
    to_table: String,
    toggle_describe: String,
    to_events: String,
    to_logs: String,
    delete: String,
    confirm: String,
    cancel: String,
}

impl Default for RawKeymap {
    fn default() -> Self {
        Self {
            quit: "ctrl+c".to_string(),
            next_context: "tab".to_string(),
            prev_context: "shift+tab".to_string(),
            next_kind: "alt+right".to_string(),
            prev_kind: "alt+left".to_string(),
            move_down: "j".to_string(),
            move_up: "k".to_string(),
            goto_top: "g".to_string(),
            goto_bottom: "shift+g".to_string(),
            filter_mode: "/".to_string(),
            cycle_sort: "s".to_string(),
            toggle_desc: "r".to_string(),
            cycle_namespace: "n".to_string(),
            toggle_help: "?".to_string(),
            to_table: "t".to_string(),
            toggle_describe: "d".to_string(),
            to_events: "shift+e".to_string(),
            to_logs: "l".to_string(),
            delete: "ctrl+d".to_string(),
            confirm: "y".to_string(),
            cancel: "esc".to_string(),
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Action::Quit => "quit",
            Action::NextContext => "next_context",
            Action::PrevContext => "prev_context",
            Action::NextKind => "next_kind",
            Action::PrevKind => "prev_kind",
            Action::MoveDown => "move_down",
            Action::MoveUp => "move_up",
            Action::GotoTop => "goto_top",
            Action::GotoBottom => "goto_bottom",
            Action::FilterMode => "filter_mode",
            Action::CycleSort => "cycle_sort",
            Action::ToggleDesc => "toggle_desc",
            Action::CycleNamespace => "cycle_namespace",
            Action::ToggleHelp => "toggle_help",
            Action::ToTable => "to_table",
            Action::ToggleDescribe => "toggle_describe",
            Action::ToEvents => "to_events",
            Action::ToLogs => "to_logs",
            Action::Delete => "delete",
            Action::Confirm => "confirm",
            Action::Cancel => "cancel",
        };
        write!(f, "{s}")
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Action, Keymap};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn default_keymap_matches_core_k9s_navigation_bindings() {
        let keymap = Keymap::default();

        assert!(keymap.is(
            Action::MoveDown,
            &key(KeyCode::Char('j'), KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::MoveUp,
            &key(KeyCode::Char('k'), KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::FilterMode,
            &key(KeyCode::Char('/'), KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::GotoTop,
            &key(KeyCode::Char('g'), KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::GotoBottom,
            &key(KeyCode::Char('G'), KeyModifiers::SHIFT)
        ));
        assert!(keymap.is(
            Action::NextContext,
            &key(KeyCode::Tab, KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::PrevContext,
            &key(KeyCode::BackTab, KeyModifiers::SHIFT)
        ));
        assert!(keymap.is(
            Action::ToEvents,
            &key(KeyCode::Char('E'), KeyModifiers::SHIFT)
        ));
        assert!(keymap.is(
            Action::ToLogs,
            &key(KeyCode::Char('l'), KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::CycleNamespace,
            &key(KeyCode::Char('n'), KeyModifiers::empty())
        ));
    }

    #[test]
    fn default_keymap_keeps_describe_and_delete_separate() {
        let keymap = Keymap::default();

        assert!(keymap.is(
            Action::ToggleDescribe,
            &key(KeyCode::Char('d'), KeyModifiers::empty())
        ));
        assert!(keymap.is(
            Action::Delete,
            &key(KeyCode::Char('d'), KeyModifiers::CONTROL)
        ));
        assert!(!keymap.is(
            Action::Delete,
            &key(KeyCode::Char('d'), KeyModifiers::empty())
        ));
        assert!(!keymap.is(
            Action::ToggleDescribe,
            &key(KeyCode::Char('d'), KeyModifiers::CONTROL)
        ));
    }
}
