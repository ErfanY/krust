pub(crate) fn activity_icon(revision: u64) -> &'static str {
    const FRAMES: [&str; 4] = [".", ":", "*", "#"];
    FRAMES[(revision as usize) % FRAMES.len()]
}

pub(crate) fn ascii_meter(numerator: u64, denominator: u64, width: usize) -> String {
    let width = width.max(4);
    if denominator == 0 {
        return format!("[{}] n/a", ".".repeat(width));
    }
    let ratio = (numerator as f64 / denominator as f64).clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).min(width);
    let mut bar = String::with_capacity(width);
    for idx in 0..width {
        bar.push(if idx < filled { '#' } else { '.' });
    }
    format!("[{bar}] {:>3.0}%", ratio * 100.0)
}

pub(crate) fn value_delta(current: u64, previous: u64) -> i64 {
    if current >= previous {
        (current - previous) as i64
    } else {
        -((previous - current) as i64)
    }
}

pub(crate) fn format_signed_count(delta: i64) -> String {
    if delta > 0 {
        format!("+{delta}")
    } else {
        delta.to_string()
    }
}

pub(crate) fn ratio_percent_value(numerator: u64, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        return None;
    }
    Some((numerator as f64 / denominator as f64) * 100.0)
}

fn truncate_with_tilde(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let len = value.chars().count();
    if len <= width {
        return value.to_string();
    }
    if width == 1 {
        return "~".to_string();
    }
    let mut out: String = value.chars().take(width.saturating_sub(1)).collect();
    out.push('~');
    out
}

pub(crate) fn fixed_width_cell(value: &str, width: usize) -> String {
    let mut out = truncate_with_tilde(value, width);
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

pub(crate) fn format_pulse_cells(cells: &[String], columns: usize, cell_width: usize) -> String {
    let columns = columns.max(1);
    let mut out = String::new();
    for idx in 0..columns {
        if idx > 0 {
            out.push_str("  ");
        }
        let value = cells.get(idx).map(String::as_str).unwrap_or("");
        out.push_str(&fixed_width_cell(value, cell_width));
    }
    out
}
