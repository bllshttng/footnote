//! The agents-list table's cell renderers, lifted out of the client binary
//! (file budget: that file is over the shrink-only line; this move banks
//! the shrink). Pure display code: the cap is kept in step with Python's
//! `_LAST_MESSAGE_WIDTH` in cli/src/fno/agents/format.py (the two tables
//! are functional parallels, not byte-exact, but the cap is the one value
//! worth holding together).

use chrono;

/// Display cap for the LAST MESSAGE cell.
pub const LAST_MESSAGE_WIDTH: usize = 40;

pub fn format_age_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Render `last_reconciled_at` (raw RFC3339, or None) as the CHECKED cell:
/// `never` when never probed, the compact age otherwise, or `?` when the stored
/// timestamp cannot be parsed (explicit, never blank -- Silent-Failure check).
pub fn render_checked(
    last_reconciled_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    match last_reconciled_at {
        None => "never".to_string(),
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(then) => format_age_secs((now - then.with_timezone(&chrono::Utc)).num_seconds()),
            Err(_) => "?".to_string(),
        },
    }
}

/// Right-aligned ellipsis truncation, chars not bytes (mirrors Python's
/// `_truncate`), so a long transcript line cannot own the table.
pub fn truncate_cell(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        s.chars().take(width).collect()
    } else {
        let mut t: String = s.chars().take(width - 1).collect();
        t.push('…');
        t
    }
}
