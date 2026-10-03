//! The notifications bell panel: ready and settled questions beside fleet
//! announcements. Its projection reads stay off the UI loop.

use super::*;
use serde_json::Value;

const PANEL_W: usize = 48;
/// Outside the `chat-<hex>` namespace used by Messages read marks.
const READ_MARK: &str = "__notifications_bell__";

pub(crate) struct Panel {
    pub(super) open: bool,
    projection: Option<Value>,
    error: Option<String>,
    inflight: bool,
    last_read: Option<Instant>,
    generation: u64,
    selected: usize,
    esc: Vec<u8>,
    unread: bool,
}

#[derive(Clone)]
enum Row {
    Header(String),
    Question(String),
    Clear(usize),
    Announcement(String),
    Info(String),
}

#[derive(Debug, Clone)]
pub(super) enum Hit {
    Toggle,
    Focus,
    Clear,
    Question(String),
}

impl Panel {
    pub(crate) fn initial() -> Self {
        Self {
            open: false,
            projection: None,
            error: None,
            inflight: false,
            last_read: None,
            generation: 0,
            selected: 0,
            esc: Vec::new(),
            unread: false,
        }
    }
}

pub(crate) type Tx = tokio::sync::mpsc::UnboundedSender<(u64, Result<Value, String>)>;

pub(crate) fn open(view: &mut View) {
    let generation = view.bell.generation.wrapping_add(1);
    view.bell.open = true;
    view.bell.generation = generation;
    view.questions_kick_at = None;
    if let Some(projection) = view.bell.projection.as_ref() {
        mark_seen(projection);
        view.bell.unread = false;
    }
    view.feed = None;
    view.region_owner = super::region_focus::RegionOwner::Pane;
}

pub(crate) fn close(view: &mut View) {
    if view.bell.open {
        view.bell.open = false;
        view.bell.generation = view.bell.generation.wrapping_add(1);
    }
}

pub(crate) fn toggle(view: &mut View) {
    if view.bell.open {
        close(view);
    } else {
        open(view);
    }
}

pub(crate) fn button_label(view: &View) -> String {
    let mut label = String::from("🔔");
    let count = ready_count(view);
    if count > 0 {
        let digits = count.to_string();
        for ch in digits.chars() {
            label.push(match ch {
                '0' => '⁰',
                '1' => '¹',
                '2' => '²',
                '3' => '³',
                '4' => '⁴',
                '5' => '⁵',
                '6' => '⁶',
                '7' => '⁷',
                '8' => '⁸',
                '9' => '⁹',
                _ => ch,
            });
        }
    }
    if has_unread(view) {
        label.push('•');
    }
    label
}

pub(crate) fn button_range(view: &View, text_w: usize) -> std::ops::Range<usize> {
    let width = unicode_width::UnicodeWidthStr::width(button_label(view).as_str());
    let words_end = view
        .top_row_spans()
        .iter()
        .map(|(start, span, _)| *start + *span)
        .max()
        .unwrap_or(0)
        .saturating_add(2);
    let start = text_w.saturating_sub(width);
    if start < words_end {
        text_w..text_w
    } else {
        start..text_w
    }
}

pub(super) fn button_at(view: &View, row: u16, col: u16) -> bool {
    let top = view.sideline_top();
    let text_w = view.sideline_paint_w().saturating_sub(1);
    row as usize + 1 == top && button_range(view, text_w).contains(&(col as usize))
}

pub(crate) fn paint_button(view: &View, cells: &mut [Cell], text_w: usize, cols: usize) {
    let range = button_range(view, text_w);
    paint(
        cells,
        cols,
        0,
        range.start,
        range.len(),
        &button_label(view),
        view.theme.brand,
        true,
    );
}

pub(crate) fn maybe_kick(view: &mut View, tx: &Tx) {
    if view.panel_w() == 0 {
        return;
    }
    let panel = &mut view.bell;
    if panel.inflight
        || panel
            .last_read
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(30))
    {
        return;
    }
    panel.inflight = true;
    panel.last_read = Some(Instant::now());
    let generation = panel.generation;
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send((generation, crate::messages_model::gather().await));
    });
}

pub(crate) fn apply(view: &mut View, generation: u64, result: Result<Value, String>) {
    let panel = &mut view.bell;
    panel.inflight = false;
    if panel.generation != generation {
        return;
    }
    match result {
        Ok(value) => {
            panel.error = None;
            panel.unread = has_unread_in(&value);
            panel.projection = Some(value);
        }
        Err(error) => panel.error = Some(error),
    }
    clamp_selection(view);
}

fn mark_seen(projection: &Value) {
    if let Some(ts) = projection
        .get("announcements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("ts").and_then(Value::as_str))
        .filter_map(|ts| timestamp_key(ts).map(|epoch| (epoch, ts)))
        .max_by_key(|(epoch, _)| *epoch)
        .map(|(_, ts)| ts)
    {
        crate::view_store::save_messages_read_mark(READ_MARK, ts);
    }
}

fn rows(view: &View) -> Vec<Row> {
    let mut out = Vec::new();
    let mut open: Vec<_> = view
        .questions_fold
        .as_ref()
        .into_iter()
        .flat_map(|fold| fold.items.iter())
        .filter(|q| q.state == "open" && !q.settled)
        .collect();
    open.sort_by_key(|q| !q.ready);
    let settled: Vec<_> = view
        .questions_fold
        .as_ref()
        .into_iter()
        .flat_map(|fold| fold.items.iter())
        .filter(|q| q.settled)
        .collect();
    out.push(Row::Header(format!("Questions ({})", open.len())));
    if view.questions_degraded {
        out.push(Row::Info(format!(
            "questions unavailable: {}",
            view.questions_degraded_reason
                .as_deref()
                .unwrap_or("read failed")
        )));
    } else if view.questions_fold.is_none() {
        out.push(Row::Info("loading questions...".into()));
    } else if open.is_empty() {
        out.push(Row::Info("no open questions".into()));
    }
    for q in open {
        out.push(Row::Question(q.id.clone()));
    }
    if !settled.is_empty() {
        out.push(Row::Header(format!("Answered ({})", settled.len())));
        for q in &settled {
            out.push(Row::Question(q.id.clone()));
        }
        out.push(Row::Clear(settled.len()));
    }
    let announcements = view
        .bell
        .projection
        .as_ref()
        .and_then(|p| p.get("announcements"))
        .and_then(Value::as_array);
    let count = announcements.map_or(0, Vec::len);
    out.push(Row::Header(format!("Announcements ({count})")));
    if let Some(items) = announcements {
        let mut ordered: Vec<&Value> = items.iter().collect();
        ordered.sort_by(|a, b| timestamp_of(b).cmp(&timestamp_of(a)));
        for item in ordered {
            let sender = item.get("from").and_then(Value::as_str).unwrap_or("system");
            let summary = item.get("summary").and_then(Value::as_str).unwrap_or("");
            let badge = if item.get("system").and_then(Value::as_bool) == Some(true) {
                "⚙ "
            } else {
                ""
            };
            out.push(Row::Announcement(format!("{badge}{sender}: {summary}")));
            if let Some(body) = item
                .get("body")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                out.push(Row::Info(format!("  {body}")));
            }
            if let Some(expires) = item.get("expires").and_then(Value::as_str) {
                out.push(Row::Info(format!("  expires {expires}")));
            }
        }
    } else if let Some(error) = view.bell.error.as_ref() {
        out.push(Row::Info(format!("unavailable: {error}")));
    } else {
        out.push(Row::Info("loading announcements...".into()));
    }
    out
}

pub(super) fn hit(view: &View, row: u16, col: u16) -> Option<ChromeHit> {
    let panel = &view.bell;
    if !panel.open {
        return None;
    }
    let width = PANEL_W.min(view.term.1 as usize);
    let x0 = view.term.1 as usize - width;
    if (col as usize) < x0 || row as usize >= view.term.0 as usize {
        return None;
    }
    if row == 0 || row as usize == view.term.0.saturating_sub(1) as usize {
        return Some(ChromeHit::Bell(Hit::Focus));
    }
    if col as usize == x0 {
        return Some(ChromeHit::Bell(Hit::Focus));
    }
    let all = rows(view);
    let visible = (view.term.0 as usize).saturating_sub(2);
    let start = panel.selected.saturating_sub(visible.saturating_sub(1));
    match all.get(start + row as usize - 1)? {
        Row::Question(id) => Some(ChromeHit::Bell(Hit::Question(id.clone()))),
        Row::Clear(_) => Some(ChromeHit::Bell(Hit::Clear)),
        _ => Some(ChromeHit::Bell(Hit::Focus)),
    }
}

pub(crate) fn apply_hit(view: &mut View, hit: Hit) {
    if let Some(projection) = view.bell.projection.as_ref() {
        mark_seen(projection);
        view.bell.unread = false;
    }
    match hit {
        Hit::Toggle => toggle(view),
        Hit::Focus => {}
        Hit::Clear if !view.question_acting => {
            view.question_clear_settled = true;
            view.question_acting = true;
        }
        Hit::Clear => {}
        Hit::Question(id) => view.open_detail_on(&id),
    }
}

fn question_title(view: &View, id: &str) -> String {
    view.questions_fold
        .as_ref()
        .and_then(|f| f.items.iter().find(|q| q.id == id))
        .map(|q| q.title.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_string()
}

pub(crate) fn keys(view: &mut View, bytes: &[u8]) {
    if !bytes.is_empty() {
        if let Some(projection) = view.bell.projection.as_ref() {
            mark_seen(projection);
            view.bell.unread = false;
        }
    }
    let mut esc = std::mem::take(&mut view.bell.esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.bell.esc = esc;
    for byte in keys {
        match byte {
            0x1b | b'q' | b'B' => {
                close(view);
                return;
            }
            b'j' => {
                let len = rows(view).len();
                view.bell.selected = (view.bell.selected + 1).min(len.saturating_sub(1));
            }
            b'k' => {
                view.bell.selected = view.bell.selected.saturating_sub(1);
            }
            b'c' => {
                if rows(view).iter().any(|r| matches!(r, Row::Clear(_))) && !view.question_acting {
                    view.question_clear_settled = true;
                    view.question_acting = true;
                }
            }
            b'\r' | b'\n' => {
                let selected = view.bell.selected;
                match rows(view).get(selected).cloned() {
                    Some(Row::Question(id)) => view.open_detail_on(&id),
                    Some(Row::Clear(_)) if !view.question_acting => {
                        view.question_clear_settled = true;
                        view.question_acting = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

pub(super) fn clamp_selection(view: &mut View) {
    view.bell.selected = view.bell.selected.min(rows(view).len().saturating_sub(1));
}

pub(crate) fn draw(view: &View, cells: &mut [Cell], rows_n: usize, cols: usize) {
    let panel = &view.bell;
    if !panel.open {
        return;
    }
    if rows_n == 0 || cols == 0 {
        return;
    }
    let width = PANEL_W.min(cols);
    let x0 = cols - width;
    let mut lines = rows(view);
    if lines.is_empty() {
        lines.push(Row::Info("nothing here".into()));
    }
    let visible = rows_n.saturating_sub(2);
    let start = panel.selected.saturating_sub(visible.saturating_sub(1));
    for r in 0..rows_n {
        let y = r * cols;
        cells[y + x0] = Cell {
            c: '│',
            fg: view.theme.brand,
            bg: Color::Default,
            flags: 0,
        };
        for c in x0 + 1..cols {
            cells[y + c] = Cell {
                c: ' ',
                fg: Color::Default,
                bg: Color::Default,
                flags: 0,
            };
        }
        if r == 0 {
            paint(
                cells,
                cols,
                r,
                x0 + 2,
                width.saturating_sub(2),
                "🔔 Questions and Announcements",
                view.theme.brand,
                true,
            );
        } else if r == rows_n - 1 {
            paint(
                cells,
                cols,
                r,
                x0 + 2,
                width.saturating_sub(2),
                "j/k move · Enter open · c clear · q close",
                Color::Default,
                false,
            );
        } else if let Some(row) = lines.get(start + r - 1) {
            let (label, color, bold) = match row {
                Row::Header(s) => (s.clone(), view.theme.brand, true),
                Row::Question(id) => (
                    format!("? {}", question_title(view, id)),
                    Color::Default,
                    true,
                ),
                Row::Clear(n) => (format!("Clear all {n} answered"), view.theme.brand, true),
                Row::Announcement(s) => (format!("• {s}"), Color::Default, false),
                Row::Info(s) => (s.clone(), Color::Indexed(8), false),
            };
            paint(
                cells,
                cols,
                r,
                x0 + 2,
                width.saturating_sub(2),
                &label,
                color,
                bold,
            );
            if start + r - 1 == panel.selected {
                let (fg, bg, flags) = crate::theme::band_style(&view.theme);
                for c in (x0 + 1)..cols {
                    let cell = &mut cells[y + c];
                    cell.fg = fg;
                    cell.bg = bg;
                    cell.flags = flags;
                }
            }
        }
    }
}

fn paint(
    cells: &mut [Cell],
    cols: usize,
    row: usize,
    start: usize,
    width: usize,
    text: &str,
    fg: Color,
    bold: bool,
) {
    let mut col = start;
    for ch in text.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        let w = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(0)
            .max(1);
        if col + w > start + width || col + w > cols {
            break;
        }
        cells[row * cols + col] = Cell {
            c: ch,
            fg,
            bg: Color::Default,
            flags: if bold { cell_flags::BOLD } else { 0 },
        };
        if w == 2 && col + 1 < cols {
            cells[row * cols + col + 1] = Cell {
                c: ' ',
                fg,
                bg: Color::Default,
                flags: cell_flags::WIDE_SPACER,
            };
        }
        col += w;
    }
}

pub(crate) fn ready_count(view: &View) -> usize {
    view.questions_fold
        .as_ref()
        .map(|fold| {
            fold.items
                .iter()
                .filter(|q| q.state == "open" && q.ready && !q.settled)
                .count()
        })
        .unwrap_or(0)
}

pub(crate) fn has_unread(view: &View) -> bool {
    view.bell.unread
}

fn has_unread_in(projection: &Value) -> bool {
    let Some(items) = projection.get("announcements").and_then(Value::as_array) else {
        return false;
    };
    let seen = crate::view_store::load_messages_read_marks()
        .remove(READ_MARK)
        .unwrap_or_default();
    let seen_at = timestamp_key(&seen);
    items
        .iter()
        .any(|item| timestamp_of(item).is_some_and(|ts| seen_at.is_none_or(|seen| ts > seen)))
}

fn timestamp_of(row: &Value) -> Option<i64> {
    row.get("ts")
        .and_then(Value::as_str)
        .and_then(timestamp_key)
}

fn timestamp_key(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|date| date.timestamp_millis())
}
