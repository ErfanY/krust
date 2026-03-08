use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorSupport {
    NoColor,
    Basic,
    Ansi256,
    TrueColor,
}

fn search_match_highlight_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }
}

pub(crate) fn highlighted_text(
    text: &str,
    query: &str,
    active_line: Option<usize>,
) -> Text<'static> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Text::from(text.to_string());
    }

    let mut out = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let lower = line.to_ascii_lowercase();
        let mut spans = Vec::new();
        let mut cursor = 0usize;
        let highlight_style = search_match_highlight_style(active_line == Some(line_idx));
        while let Some(found) = lower[cursor..].find(&needle) {
            let start = cursor + found;
            let end = start + needle.len();
            if start > cursor {
                spans.push(Span::raw(line.get(cursor..start).unwrap_or("").to_string()));
            }
            spans.push(Span::styled(
                line.get(start..end).unwrap_or("").to_string(),
                highlight_style,
            ));
            cursor = end;
        }
        if cursor < line.len() {
            spans.push(Span::raw(line.get(cursor..).unwrap_or("").to_string()));
        }
        if spans.is_empty() {
            spans.push(Span::raw(line.to_string()));
        }
        out.push(Line::from(spans));
    }
    Text::from(out)
}

#[derive(Clone, Copy)]
enum JsonTokenKind {
    Key,
    String,
    Number,
    Bool,
    Null,
    Punct,
    Plain,
}

fn json_token_style(kind: JsonTokenKind, support: ColorSupport) -> Style {
    match support {
        ColorSupport::NoColor => match kind {
            JsonTokenKind::Key => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default(),
        },
        ColorSupport::Basic => match kind {
            JsonTokenKind::Key => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            JsonTokenKind::String => Style::default().fg(Color::Green),
            JsonTokenKind::Number => Style::default().fg(Color::Magenta),
            JsonTokenKind::Bool | JsonTokenKind::Null => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            JsonTokenKind::Punct => Style::default().fg(Color::DarkGray),
            JsonTokenKind::Plain => Style::default(),
        },
        ColorSupport::Ansi256 | ColorSupport::TrueColor => match kind {
            JsonTokenKind::Key => Style::default()
                .fg(Color::Indexed(117))
                .add_modifier(Modifier::BOLD),
            JsonTokenKind::String => Style::default().fg(Color::Indexed(114)),
            JsonTokenKind::Number => Style::default().fg(Color::Indexed(213)),
            JsonTokenKind::Bool => Style::default()
                .fg(Color::Indexed(220))
                .add_modifier(Modifier::BOLD),
            JsonTokenKind::Null => Style::default().fg(Color::Indexed(180)),
            JsonTokenKind::Punct => Style::default().fg(Color::Indexed(245)),
            JsonTokenKind::Plain => Style::default(),
        },
    }
}

fn push_query_highlighted_span(
    out: &mut Vec<Span<'static>>,
    text: &str,
    base_style: Style,
    needle: &str,
    highlight_style: Style,
) {
    if needle.is_empty() {
        out.push(Span::styled(text.to_string(), base_style));
        return;
    }

    let lower = text.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(found) = lower[cursor..].find(needle) {
        let start = cursor + found;
        let end = start + needle.len();
        if start > cursor {
            out.push(Span::styled(text[cursor..start].to_string(), base_style));
        }
        out.push(Span::styled(text[start..end].to_string(), highlight_style));
        cursor = end;
    }
    if cursor < text.len() {
        out.push(Span::styled(text[cursor..].to_string(), base_style));
    }
}

fn is_json_literal_boundary(line: &str, index: usize) -> bool {
    line.get(index..)
        .and_then(|rest| rest.chars().next())
        .map(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
        .unwrap_or(true)
}

pub(crate) fn json_spans_for_line(line: &str, support: ColorSupport) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < line.len() {
        let Some(ch) = line[idx..].chars().next() else {
            break;
        };
        if ch == '"' {
            let start = idx;
            idx = idx.saturating_add(ch.len_utf8());
            let mut escaped = false;
            while idx < line.len() {
                let Some(c) = line[idx..].chars().next() else {
                    break;
                };
                if escaped {
                    escaped = false;
                    idx = idx.saturating_add(c.len_utf8());
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                    idx = idx.saturating_add(c.len_utf8());
                    continue;
                }
                if c == '"' {
                    idx = idx.saturating_add(c.len_utf8());
                    break;
                }
                idx = idx.saturating_add(c.len_utf8());
            }
            let token = line.get(start..idx).unwrap_or("").to_string();
            let mut probe = idx;
            while probe < line.len() {
                let Some(ws) = line[probe..].chars().next() else {
                    break;
                };
                if ws.is_whitespace() {
                    probe = probe.saturating_add(ws.len_utf8());
                } else {
                    break;
                }
            }
            let kind = if line[probe..].starts_with(':') {
                JsonTokenKind::Key
            } else {
                JsonTokenKind::String
            };
            out.push((token, json_token_style(kind, support)));
            continue;
        }

        if ch == '-' || ch.is_ascii_digit() {
            let start = idx;
            idx = idx.saturating_add(ch.len_utf8());
            while idx < line.len() {
                let Some(c) = line[idx..].chars().next() else {
                    break;
                };
                if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-' {
                    idx = idx.saturating_add(c.len_utf8());
                } else {
                    break;
                }
            }
            out.push((
                line.get(start..idx).unwrap_or("").to_string(),
                json_token_style(JsonTokenKind::Number, support),
            ));
            continue;
        }

        if line[idx..].starts_with("true") && is_json_literal_boundary(line, idx + 4) {
            out.push((
                "true".to_string(),
                json_token_style(JsonTokenKind::Bool, support),
            ));
            idx += 4;
            continue;
        }
        if line[idx..].starts_with("false") && is_json_literal_boundary(line, idx + 5) {
            out.push((
                "false".to_string(),
                json_token_style(JsonTokenKind::Bool, support),
            ));
            idx += 5;
            continue;
        }
        if line[idx..].starts_with("null") && is_json_literal_boundary(line, idx + 4) {
            out.push((
                "null".to_string(),
                json_token_style(JsonTokenKind::Null, support),
            ));
            idx += 4;
            continue;
        }

        if matches!(ch, '{' | '}' | '[' | ']' | ':' | ',') {
            out.push((
                ch.to_string(),
                json_token_style(JsonTokenKind::Punct, support),
            ));
            idx = idx.saturating_add(ch.len_utf8());
            continue;
        }

        let start = idx;
        idx = idx.saturating_add(ch.len_utf8());
        while idx < line.len() {
            let Some(c) = line[idx..].chars().next() else {
                break;
            };
            if c == '"'
                || c == '-'
                || c.is_ascii_digit()
                || matches!(c, '{' | '}' | '[' | ']' | ':' | ',')
                || line[idx..].starts_with("true")
                || line[idx..].starts_with("false")
                || line[idx..].starts_with("null")
            {
                break;
            }
            idx = idx.saturating_add(c.len_utf8());
        }
        out.push((
            line.get(start..idx).unwrap_or("").to_string(),
            json_token_style(JsonTokenKind::Plain, support),
        ));
    }
    out
}

pub(crate) fn highlighted_json_text(
    text: &str,
    query: &str,
    support: ColorSupport,
    active_line: Option<usize>,
) -> Text<'static> {
    let needle = query.trim().to_ascii_lowercase();
    let mut out = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let mut spans = Vec::new();
        let highlight_style = search_match_highlight_style(active_line == Some(line_idx));
        for (segment, style) in json_spans_for_line(line, support) {
            push_query_highlighted_span(&mut spans, &segment, style, &needle, highlight_style);
        }
        if spans.is_empty() {
            spans.push(Span::raw(String::new()));
        }
        out.push(Line::from(spans));
    }
    Text::from(out)
}

#[derive(Clone, Copy)]
enum YamlTokenKind {
    Key,
    String,
    Number,
    Bool,
    Null,
    Comment,
    Punct,
    Plain,
}

fn yaml_token_style(kind: YamlTokenKind, support: ColorSupport) -> Style {
    match support {
        ColorSupport::NoColor => match kind {
            YamlTokenKind::Key => Style::default().add_modifier(Modifier::BOLD),
            YamlTokenKind::Comment => Style::default().add_modifier(Modifier::DIM),
            _ => Style::default(),
        },
        ColorSupport::Basic => match kind {
            YamlTokenKind::Key => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            YamlTokenKind::String => Style::default().fg(Color::Green),
            YamlTokenKind::Number => Style::default().fg(Color::Magenta),
            YamlTokenKind::Bool | YamlTokenKind::Null => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            YamlTokenKind::Comment => Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
            YamlTokenKind::Punct => Style::default().fg(Color::DarkGray),
            YamlTokenKind::Plain => Style::default(),
        },
        ColorSupport::Ansi256 | ColorSupport::TrueColor => match kind {
            YamlTokenKind::Key => Style::default()
                .fg(Color::Indexed(117))
                .add_modifier(Modifier::BOLD),
            YamlTokenKind::String => Style::default().fg(Color::Indexed(114)),
            YamlTokenKind::Number => Style::default().fg(Color::Indexed(213)),
            YamlTokenKind::Bool => Style::default()
                .fg(Color::Indexed(220))
                .add_modifier(Modifier::BOLD),
            YamlTokenKind::Null => Style::default().fg(Color::Indexed(180)),
            YamlTokenKind::Comment => Style::default()
                .fg(Color::Indexed(244))
                .add_modifier(Modifier::DIM),
            YamlTokenKind::Punct => Style::default().fg(Color::Indexed(245)),
            YamlTokenKind::Plain => Style::default(),
        },
    }
}

fn find_yaml_comment_index(line: &str) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut prev: Option<char> = None;
    for (idx, ch) in line.char_indices() {
        if in_double && escaped {
            escaped = false;
            prev = Some(ch);
            continue;
        }
        if ch == '\\' && in_double {
            escaped = true;
            prev = Some(ch);
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            prev = Some(ch);
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            prev = Some(ch);
            continue;
        }
        if ch == '#' && !in_single && !in_double {
            let starts_comment = idx == 0 || prev.map(|c| c.is_whitespace()).unwrap_or(true);
            if starts_comment {
                return Some(idx);
            }
        }
        prev = Some(ch);
    }
    None
}

fn is_yaml_scalar_boundary(line: &str, index: usize) -> bool {
    line.get(index..)
        .and_then(|rest| rest.chars().next())
        .map(|ch| ch.is_whitespace() || matches!(ch, ',' | ']' | '}' | ':' | '#'))
        .unwrap_or(true)
}

fn is_yaml_number_token(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }
    if token.starts_with("0x") || token.starts_with("0o") || token.starts_with("0b") {
        return token.len() > 2;
    }
    if token == "-" || token == "+" {
        return false;
    }
    let mut dots = 0usize;
    let mut digits = 0usize;
    for (idx, ch) in token.chars().enumerate() {
        if ch.is_ascii_digit() {
            digits += 1;
            continue;
        }
        if (ch == '-' || ch == '+') && idx == 0 {
            continue;
        }
        if ch == '.' {
            dots += 1;
            if dots > 1 {
                return false;
            }
            continue;
        }
        return false;
    }
    digits > 0
}

fn is_yaml_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/')
}

pub(crate) fn yaml_spans_for_line(line: &str, support: ColorSupport) -> Vec<(String, Style)> {
    let (code, comment) = if let Some(comment_start) = find_yaml_comment_index(line) {
        (&line[..comment_start], Some(&line[comment_start..]))
    } else {
        (line, None)
    };

    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < code.len() {
        let Some(ch) = code[idx..].chars().next() else {
            break;
        };

        if ch.is_whitespace() {
            let start = idx;
            idx = idx.saturating_add(ch.len_utf8());
            while idx < code.len() {
                let Some(next) = code[idx..].chars().next() else {
                    break;
                };
                if !next.is_whitespace() {
                    break;
                }
                idx = idx.saturating_add(next.len_utf8());
            }
            out.push((
                code.get(start..idx).unwrap_or("").to_string(),
                yaml_token_style(YamlTokenKind::Plain, support),
            ));
            continue;
        }

        if ch == '\'' || ch == '"' {
            let quote = ch;
            let start = idx;
            idx = idx.saturating_add(ch.len_utf8());
            let mut escaped = false;
            while idx < code.len() {
                let Some(next) = code[idx..].chars().next() else {
                    break;
                };
                if quote == '"' && escaped {
                    escaped = false;
                    idx = idx.saturating_add(next.len_utf8());
                    continue;
                }
                if quote == '"' && next == '\\' {
                    escaped = true;
                    idx = idx.saturating_add(next.len_utf8());
                    continue;
                }
                idx = idx.saturating_add(next.len_utf8());
                if next == quote {
                    break;
                }
            }
            out.push((
                code.get(start..idx).unwrap_or("").to_string(),
                yaml_token_style(YamlTokenKind::String, support),
            ));
            continue;
        }

        if matches!(ch, '[' | ']' | '{' | '}' | ',' | '?') {
            out.push((
                ch.to_string(),
                yaml_token_style(YamlTokenKind::Punct, support),
            ));
            idx = idx.saturating_add(ch.len_utf8());
            continue;
        }

        if ch == '-' {
            let next = code[idx + ch.len_utf8()..].chars().next();
            if next.is_some_and(char::is_whitespace) {
                out.push((
                    "-".to_string(),
                    yaml_token_style(YamlTokenKind::Punct, support),
                ));
                idx = idx.saturating_add(ch.len_utf8());
                continue;
            }
        }

        if is_yaml_word_char(ch) {
            let start = idx;
            idx = idx.saturating_add(ch.len_utf8());
            while idx < code.len() {
                let Some(next) = code[idx..].chars().next() else {
                    break;
                };
                if !is_yaml_word_char(next) {
                    break;
                }
                idx = idx.saturating_add(next.len_utf8());
            }
            let token = code.get(start..idx).unwrap_or("");

            let mut probe = idx;
            while probe < code.len() {
                let Some(ws) = code[probe..].chars().next() else {
                    break;
                };
                if ws.is_whitespace() {
                    probe = probe.saturating_add(ws.len_utf8());
                } else {
                    break;
                }
            }
            if code[probe..].starts_with(':') {
                out.push((
                    token.to_string(),
                    yaml_token_style(YamlTokenKind::Key, support),
                ));
                if probe > idx {
                    out.push((
                        code.get(idx..probe).unwrap_or("").to_string(),
                        yaml_token_style(YamlTokenKind::Plain, support),
                    ));
                }
                out.push((
                    ":".to_string(),
                    yaml_token_style(YamlTokenKind::Punct, support),
                ));
                idx = probe + 1;
                continue;
            }

            let lower = token.to_ascii_lowercase();
            let kind = if (lower == "true" || lower == "false" || lower == "yes" || lower == "no")
                && is_yaml_scalar_boundary(code, idx)
            {
                YamlTokenKind::Bool
            } else if (lower == "null" || lower == "~") && is_yaml_scalar_boundary(code, idx) {
                YamlTokenKind::Null
            } else if is_yaml_number_token(token) {
                YamlTokenKind::Number
            } else {
                YamlTokenKind::Plain
            };
            out.push((token.to_string(), yaml_token_style(kind, support)));
            continue;
        }

        out.push((
            ch.to_string(),
            yaml_token_style(YamlTokenKind::Plain, support),
        ));
        idx = idx.saturating_add(ch.len_utf8());
    }

    if let Some(comment) = comment {
        out.push((
            comment.to_string(),
            yaml_token_style(YamlTokenKind::Comment, support),
        ));
    }

    out
}

pub(crate) fn highlighted_yaml_text(
    text: &str,
    query: &str,
    support: ColorSupport,
    active_line: Option<usize>,
) -> Text<'static> {
    let needle = query.trim().to_ascii_lowercase();
    let mut out = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        let mut spans = Vec::new();
        let highlight_style = search_match_highlight_style(active_line == Some(line_idx));
        for (segment, style) in yaml_spans_for_line(line, support) {
            push_query_highlighted_span(&mut spans, &segment, style, &needle, highlight_style);
        }
        if spans.is_empty() {
            spans.push(Span::raw(String::new()));
        }
        out.push(Line::from(spans));
    }
    Text::from(out)
}
