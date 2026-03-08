pub(crate) fn search_match_lines(text: &str, query: &str) -> Vec<usize> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    text.lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            if line.to_ascii_lowercase().contains(&needle) {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn search_match_lines_in_logs(
    lines: &std::collections::VecDeque<String>,
    query: &str,
) -> Vec<usize> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            if line.to_ascii_lowercase().contains(&needle) {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

fn active_match_line(scroll: u16, match_lines: &[usize]) -> Option<usize> {
    if match_lines.is_empty() {
        return None;
    }
    let current_line = scroll as usize;
    Some(
        match_lines
            .iter()
            .copied()
            .find(|line| *line >= current_line)
            .unwrap_or(*match_lines.last().unwrap_or(&match_lines[0])),
    )
}

pub(crate) fn resolved_active_match_line(
    scroll: u16,
    match_lines: &[usize],
    preferred_line: Option<usize>,
) -> Option<usize> {
    if match_lines.is_empty() {
        return None;
    }
    if let Some(line) = preferred_line
        && match_lines.contains(&line)
    {
        return Some(line);
    }
    active_match_line(scroll, match_lines)
}

pub(crate) fn step_match_line(
    match_lines: &[usize],
    current_line: usize,
    forward: bool,
) -> Option<(usize, usize)> {
    if match_lines.is_empty() {
        return None;
    }

    if forward {
        if let Some(pos) = match_lines.iter().position(|line| *line == current_line) {
            let next = (pos + 1) % match_lines.len();
            return Some((match_lines[next], next + 1));
        }
        if let Some((idx, line)) = match_lines
            .iter()
            .enumerate()
            .find(|(_, line)| **line > current_line)
        {
            return Some((*line, idx + 1));
        }
        Some((match_lines[0], 1))
    } else {
        if let Some(pos) = match_lines.iter().position(|line| *line == current_line) {
            let prev = if pos == 0 {
                match_lines.len() - 1
            } else {
                pos - 1
            };
            return Some((match_lines[prev], prev + 1));
        }
        if let Some((idx, line)) = match_lines
            .iter()
            .enumerate()
            .rev()
            .find(|(_, line)| **line < current_line)
        {
            return Some((*line, idx + 1));
        }
        let last = match_lines.len() - 1;
        Some((match_lines[last], last + 1))
    }
}

pub(crate) fn detail_viewer_title(
    pane_title: &str,
    wrap: bool,
    search_query: &str,
    match_lines: &[usize],
    scroll: u16,
    total_lines: usize,
    active_line: Option<usize>,
) -> String {
    let search = if search_query.trim().is_empty() {
        "search:-".to_string()
    } else if match_lines.is_empty() {
        format!("search:/{} 0/0", search_query.trim())
    } else {
        let current_idx = active_line
            .and_then(|line| match_lines.iter().position(|candidate| *candidate == line))
            .map(|idx| idx + 1)
            .or_else(|| {
                let current_line = scroll as usize;
                match_lines
                    .iter()
                    .position(|line| *line >= current_line)
                    .map(|idx| idx + 1)
            })
            .unwrap_or(match_lines.len());
        format!(
            "search:/{} {}/{}",
            search_query.trim(),
            current_idx,
            match_lines.len()
        )
    };
    let total = total_lines.max(1);
    let line_pos = ((scroll as usize) + 1).min(total);
    format!(
        "{} | NORMAL | {} | ln:{}/{} | wrap:{}",
        pane_title,
        search,
        line_pos,
        total,
        if wrap { "on" } else { "off" }
    )
}
