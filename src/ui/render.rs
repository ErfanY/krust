use std::env;

use ratatui::style::{Color, Modifier, Style};

use super::highlight::ColorSupport;

#[derive(Clone, Copy)]
pub(crate) struct UiTheme {
    pub(crate) header: Style,
    pub(crate) block: Style,
    pub(crate) table_header: Style,
    pub(crate) row_highlight: Style,
    pub(crate) row_ok: Style,
    pub(crate) row_warn: Style,
    pub(crate) row_err: Style,
    pub(crate) status_ok: Style,
    pub(crate) status_warn: Style,
    pub(crate) status_err: Style,
    pub(crate) help: Style,
    pub(crate) command_active: Style,
    pub(crate) command_idle: Style,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Ok,
    Warn,
    Err,
}

pub(crate) fn detect_color_support() -> ColorSupport {
    detect_color_support_from_env(
        env::var("NO_COLOR").ok().as_deref(),
        env::var("COLORTERM").ok().as_deref(),
        env::var("TERM").ok().as_deref(),
    )
}

pub(crate) fn detect_color_support_from_env(
    no_color: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
) -> ColorSupport {
    if no_color.is_some() {
        return ColorSupport::NoColor;
    }
    let colorterm = colorterm.unwrap_or("").to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return ColorSupport::TrueColor;
    }
    let term = term.unwrap_or("").to_ascii_lowercase();
    if term.contains("256color") {
        return ColorSupport::Ansi256;
    }
    if term == "dumb" || term.is_empty() {
        return ColorSupport::NoColor;
    }
    ColorSupport::Basic
}

pub(crate) fn ui_theme_for(support: ColorSupport) -> UiTheme {
    match support {
        ColorSupport::TrueColor => UiTheme {
            header: Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(255, 242, 204))
                .add_modifier(Modifier::BOLD),
            block: Style::default().fg(Color::Rgb(238, 244, 255)),
            table_header: Style::default()
                .fg(Color::Rgb(255, 247, 214))
                .add_modifier(Modifier::BOLD),
            row_highlight: Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(186, 223, 255))
                .add_modifier(Modifier::BOLD),
            row_ok: Style::default().fg(Color::Rgb(219, 252, 219)),
            row_warn: Style::default().fg(Color::Rgb(255, 233, 168)),
            row_err: Style::default().fg(Color::Rgb(255, 184, 184)),
            status_ok: Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(214, 245, 214)),
            status_warn: Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(255, 235, 179)),
            status_err: Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(255, 204, 204))
                .add_modifier(Modifier::BOLD),
            help: Style::default().fg(Color::Rgb(192, 208, 235)),
            command_active: Style::default()
                .fg(Color::Black)
                .bg(Color::Rgb(229, 242, 255))
                .add_modifier(Modifier::BOLD),
            command_idle: Style::default().fg(Color::Rgb(161, 180, 214)),
        },
        ColorSupport::Ansi256 => UiTheme {
            header: Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(229))
                .add_modifier(Modifier::BOLD),
            block: Style::default().fg(Color::Indexed(254)),
            table_header: Style::default()
                .fg(Color::Indexed(230))
                .add_modifier(Modifier::BOLD),
            row_highlight: Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(117))
                .add_modifier(Modifier::BOLD),
            row_ok: Style::default().fg(Color::Indexed(120)),
            row_warn: Style::default().fg(Color::Indexed(220)),
            row_err: Style::default().fg(Color::Indexed(210)),
            status_ok: Style::default().fg(Color::Black).bg(Color::Indexed(120)),
            status_warn: Style::default().fg(Color::Black).bg(Color::Indexed(220)),
            status_err: Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(210))
                .add_modifier(Modifier::BOLD),
            help: Style::default().fg(Color::Indexed(145)),
            command_active: Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(153))
                .add_modifier(Modifier::BOLD),
            command_idle: Style::default().fg(Color::Indexed(109)),
        },
        ColorSupport::Basic => UiTheme {
            header: Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            block: Style::default().fg(Color::White),
            table_header: Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            row_highlight: Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            row_ok: Style::default().fg(Color::Green),
            row_warn: Style::default().fg(Color::Yellow),
            row_err: Style::default().fg(Color::Red),
            status_ok: Style::default().fg(Color::Black).bg(Color::Green),
            status_warn: Style::default().fg(Color::Black).bg(Color::Yellow),
            status_err: Style::default().fg(Color::White).bg(Color::Red),
            help: Style::default().fg(Color::DarkGray),
            command_active: Style::default()
                .fg(Color::Black)
                .bg(Color::White)
                .add_modifier(Modifier::BOLD),
            command_idle: Style::default().fg(Color::DarkGray),
        },
        ColorSupport::NoColor => UiTheme {
            header: Style::default().add_modifier(Modifier::BOLD),
            block: Style::default(),
            table_header: Style::default().add_modifier(Modifier::BOLD),
            row_highlight: Style::default().add_modifier(Modifier::REVERSED),
            row_ok: Style::default(),
            row_warn: Style::default().add_modifier(Modifier::DIM),
            row_err: Style::default().add_modifier(Modifier::BOLD),
            status_ok: Style::default(),
            status_warn: Style::default().add_modifier(Modifier::DIM),
            status_err: Style::default().add_modifier(Modifier::BOLD),
            help: Style::default().add_modifier(Modifier::DIM),
            command_active: Style::default().add_modifier(Modifier::BOLD),
            command_idle: Style::default().add_modifier(Modifier::DIM),
        },
    }
}

pub(crate) fn classify_status_severity(status: &str) -> Severity {
    let lower = status.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("fail")
        || lower.contains("crash")
        || lower.contains("oom")
        || lower.contains("forbidden")
        || lower.contains("denied")
        || lower.contains("blocked")
        || lower.contains("evicted")
        || lower.contains("terminated")
    {
        return Severity::Err;
    }
    if lower.contains("pending")
        || lower.contains("unknown")
        || lower.contains("waiting")
        || lower.contains("init")
        || lower.contains("terminating")
        || lower.contains("notready")
    {
        return Severity::Warn;
    }
    Severity::Ok
}

pub(crate) fn severity_tag(severity: Severity) -> &'static str {
    match severity {
        Severity::Ok => "[OK]",
        Severity::Warn => "[!!]",
        Severity::Err => "[XX]",
    }
}

pub(crate) fn severity_style(theme: &UiTheme, severity: Severity) -> Style {
    match severity {
        Severity::Ok => theme.row_ok,
        Severity::Warn => theme.row_warn,
        Severity::Err => theme.row_err,
    }
}

pub(crate) fn color_support_label(support: ColorSupport) -> &'static str {
    match support {
        ColorSupport::NoColor => "mono",
        ColorSupport::Basic => "basic",
        ColorSupport::Ansi256 => "256",
        ColorSupport::TrueColor => "truecolor",
    }
}

pub(crate) fn status_style_for_line(theme: &UiTheme, status: &str) -> Style {
    let lower = status.to_ascii_lowercase();
    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("denied")
        || lower.contains("forbidden")
        || lower.contains("blocked")
    {
        return theme.status_err;
    }
    if lower.contains("warn")
        || lower.contains("retry")
        || lower.contains("pending")
        || lower.contains("paused")
    {
        return theme.status_warn;
    }
    theme.status_ok
}
