//! The questions block and the full questions view, split out of
//! `client.rs` because that file is over the line budget and shrink-only.
//! The block sits above the court block in the sideline: one key toggles it,
//! a key pair resizes it, and answered questions hide behind one dim count
//! line. The full view reuses the backlog board's framed list and detail
//! panes and its markdown renderer (`backlog_panes`/`backlog_md`), and
//! answers in place through [`crate::needs_overlay::answer`]. A child module
//! of `client`, so `View`'s private fields stay reachable without widening
//! them.

use super::*;

/// The questions block's operator prefs, each persisted through the view
/// store; the block reads them at layout time.
#[derive(Debug, Clone, Copy)]
pub(super) struct BlockPrefs {
    pub(super) visible: bool,
    pub(super) height: u16,
    pub(super) show_done: bool,
}

impl Default for BlockPrefs {
    fn default() -> Self {
        Self {
            visible: true,
            height: crate::view_store::QUESTIONS_DEFAULT_HEIGHT,
            show_done: false,
        }
    }
}

impl BlockPrefs {
    pub(super) fn load() -> Self {
        Self {
            visible: crate::view_store::load_questions_block(),
            height: crate::view_store::load_questions_height(),
            show_done: crate::view_store::load_questions_show_done(),
        }
    }
}

/// prefix+q: show or hide the whole block; the choice persists.
pub(super) fn toggle_block(view: &mut View) {
    view.questions_block.visible = !view.questions_block.visible;
    crate::view_store::save_questions_block(view.questions_block.visible);
    view.set_notice(questions_notice(view));
}

/// The prefix+q toggle toast: a degraded read names why the block reads as
/// empty, a clean fold with no open rows says so, and "shown" means rows
/// are up.
pub(super) fn questions_notice(view: &View) -> String {
    if !view.questions_block.visible {
        return "questions block: hidden".to_string();
    }
    if view.questions_degraded {
        return match view.questions_degraded_reason.as_deref() {
            Some(reason) => format!("questions unreadable: {reason}"),
            None => "questions unreadable".to_string(),
        };
    }
    let has_open = view
        .questions_fold
        .as_ref()
        .is_some_and(|f| f.items.iter().any(|i| i.state == "open"));
    if has_open {
        "questions block: shown".to_string()
    } else if view.questions_fold.is_some() {
        "no open questions".to_string()
    } else {
        "questions block: shown (folding)".to_string()
    }
}

/// prefix+{ / prefix+}: grow or shrink the block by a row; the height
/// persists, and the court rule still caps what renders.
pub(super) fn resize_block(view: &mut View, delta: i8) {
    let step = u16::from(delta.unsigned_abs());
    let next = if delta > 0 {
        view.questions_block.height.saturating_add(step)
    } else {
        view.questions_block.height.saturating_sub(step)
    };
    view.questions_block.height = next.clamp(2, 60);
    crate::view_store::save_questions_height(view.questions_block.height);
}

/// prefix+X: show or hide answered and done questions in the block.
pub(super) fn toggle_show_done(view: &mut View) {
    view.questions_block.show_done = !view.questions_block.show_done;
    crate::view_store::save_questions_show_done(view.questions_block.show_done);
    view.set_notice(if view.questions_block.show_done {
        "questions block: answered shown".into()
    } else {
        "questions block: answered hidden".into()
    });
}

/// The open questions in the fold, ready first then projection order.
pub(super) fn open_items(
    fold: &crate::needs_overlay::QuestionsFold,
) -> Vec<crate::needs_overlay::QuestionItem> {
    let mut open: Vec<_> = fold
        .items
        .iter()
        .filter(|q| q.state == "open")
        .cloned()
        .collect();
    open.sort_by_key(|q| !q.ready);
    open
}

/// The questions the block lists: open always, answered-state items too when
/// the operator shows done questions. Open before answered, ready first.
fn block_items(
    fold: &crate::needs_overlay::QuestionsFold,
    show_done: bool,
) -> Vec<crate::needs_overlay::QuestionItem> {
    let mut items: Vec<_> = fold
        .items
        .iter()
        .filter(|q| q.state == "open" || (show_done && q.state == "answered"))
        .cloned()
        .collect();
    items.sort_by_key(|q| (q.state != "open", !q.ready));
    items
}

/// One painted run: text plus cell flags (BOLD for a title, DIM for meta).
type Seg = (String, u8);

/// The block's lines and the hit map for the sideline paint and the click
/// test to share. The header counts open and ready; up to `height - 1` rows
/// follow (title bold, meta dim), then `+N more` (kept visible whenever rows
/// elide and the height affords it), then the answered section: per-answer
/// lines when shown, else at most one dim count line. Nothing is drawn past
/// `height`, and an empty fold (no questions, nothing answered) draws zero
/// lines. The bool answers whether the `+N more` line painted.
pub(super) fn block_layout(
    height: usize,
    show_done: bool,
    fold: &crate::needs_overlay::QuestionsFold,
    now: u64,
) -> (Vec<Vec<Seg>>, Vec<String>, bool) {
    let items = block_items(fold, show_done);
    if items.is_empty() && fold.answered.is_empty() {
        return (vec![], vec![], false);
    }
    let opens = open_items(fold);
    let open_count = opens.len();
    let ready = opens.iter().filter(|q| q.ready).count();
    let mut lines: Vec<Vec<Seg>> = Vec::new();
    lines.push(vec![(
        format!(" questions {open_count} \u{b7} {ready} ready"),
        0,
    )]);
    let mut ids: Vec<String> = Vec::new();
    let body_budget = height.saturating_sub(1);
    let mut shown = items.len().min(body_budget);
    let elided = items.len() - shown;
    // Keep the `+N more` line visible whenever rows elide and there is a
    // row to trade for it - the more line is the door to the full view.
    let mut more = false;
    if elided > 0 && body_budget >= 2 {
        shown -= 1;
        more = true;
    }
    for q in items.iter().take(shown) {
        let title = if q.title.is_empty() {
            "(no title)"
        } else {
            &q.title
        };
        let target = q.blocks.first().map(String::as_str).unwrap_or("none");
        let node = q.node.as_deref().unwrap_or(target);
        let asker = q.asker.as_ref().map(|a| a.handle.as_str()).unwrap_or("?");
        let age = age_short(&q.created_at, now);
        let done = q.state != "open";
        let title_flags = if done || !q.ready {
            cell_flags::BOLD | cell_flags::DIM
        } else {
            cell_flags::BOLD
        };
        lines.push(vec![
            (format!(" ? {title}"), title_flags),
            (
                format!(
                    "  {asker} -> {node} {age}{}",
                    if q.ready {
                        String::new()
                    } else {
                        "  not ready".to_string()
                    }
                ),
                cell_flags::DIM,
            ),
        ]);
        ids.push(q.id.clone());
    }
    if more {
        lines.push(vec![(format!(" +{} more", items.len() - shown), 0)]);
    }
    // The answered section: the fresh answers with their delivery rung when
    // shown, else at most one dim count line.
    let remaining = height.saturating_sub(lines.len());
    if show_done {
        for a in fold.answered.iter().take(remaining) {
            lines.push(vec![(answered_line(a), cell_flags::DIM)]);
        }
    } else if !fold.answered.is_empty() && remaining >= 1 {
        lines.push(vec![(
            format!(" \u{2713} {} answered", fold.answered.len()),
            cell_flags::DIM,
        )]);
    }
    (lines, ids, more)
}

/// What a click on the block landed on: a question row's id, or the `+N
/// more` line (the full questions view).
pub(super) enum QuestionHit {
    Row(String),
    More,
}

/// The block's reserved rows: its line count under the operator's height
/// pref, but ZERO when the terminal cannot hold it beside at least one agent
/// row - the block yields, the rows never do (the court rule). `None` when
/// the block reserves nothing.
pub(super) struct BlockRows {
    pub(super) n: usize,
    pub(super) lines: Vec<Vec<Seg>>,
    pub(super) ids: Vec<String>,
    pub(super) more: bool,
}

pub(super) fn block_rows(view: &View, term_rows: usize) -> Option<BlockRows> {
    if !view.questions_block.visible {
        return None;
    }
    // The block is agents-view chrome like the court block: under the docked
    // board it paints nothing, so it holds no rows and claims no clicks.
    if view.sideline_view != crate::view_store::SidelineView::Agents {
        return None;
    }
    let now = crate::digest_overlay::now_secs();
    let fold = view.questions_fold.as_ref()?;
    let court = view.court_block_layout(term_rows).0;
    let chrome = view.bottom_row_is_chrome() as usize;
    let available = term_rows.saturating_sub(court + chrome);
    let height =
        (view.questions_block.height as usize).clamp(2, available.saturating_sub(1).max(2));
    let (lines, ids, more) = block_layout(height, view.questions_block.show_done, fold, now);
    if lines.is_empty() {
        return None;
    }
    if available > lines.len() {
        Some(BlockRows {
            n: lines.len(),
            lines,
            ids: ids.clone(),
            more,
        })
    } else {
        None
    }
}

/// The question row a click landed on, by sideline row. Line 0 is the
/// header, body rows carry ids in order, and the first row past them is the
/// `+N more` line when it painted.
pub(super) fn hit_at(view: &View, term_rows: usize, row: u16) -> Option<QuestionHit> {
    let b = block_rows(view, term_rows)?;
    let court = view.court_block_layout(term_rows).0;
    let start = term_rows.saturating_sub(court + b.n);
    let r = (row as usize).checked_sub(start)?.checked_sub(1)?;
    if let Some(id) = b.ids.get(r) {
        return Some(QuestionHit::Row(id.clone()));
    }
    (b.more && r == b.ids.len()).then_some(QuestionHit::More)
}

/// Paint the block's lines at `start` in the sideline column: the same cell
/// loop the court block paints, with each segment's flags deciding whether a
/// run reads bold (a title), dim (meta and chrome), or plain.
pub(super) fn paint_block(
    lines: Vec<Vec<Seg>>,
    cells: &mut [Cell],
    start: usize,
    rows: usize,
    cols: usize,
    text_w: usize,
) {
    for (k, segs) in lines.into_iter().enumerate() {
        let r = start + k;
        if r >= rows {
            break;
        }
        let mut col = 0usize;
        for (text, flags) in segs {
            for ch in text.chars() {
                let w = glyph_cols(ch);
                if col + w > text_w {
                    break;
                }
                cells[r * cols + col] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags,
                };
                if w == 2 {
                    cells[r * cols + col + 1] = Cell {
                        c: ' ',
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: flags | cell_flags::WIDE_SPACER,
                    };
                }
                col += w;
            }
        }
    }
}

fn age_short(created_at: &str, now: u64) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return String::new();
    };
    let age = now.saturating_sub(t.timestamp().max(0) as u64);
    if age >= 3600 {
        format!("{}h", age / 3600)
    } else {
        format!("{}m", age / 60)
    }
}

fn answered_line(a: &crate::needs_overlay::AnsweredItem) -> String {
    let who = a.asker.as_deref().unwrap_or("?");
    match (a.rung.as_deref(), a.outcome.as_deref()) {
        (None, _) | (_, None) => format!(" \u{2713} {who} answered, not delivered yet"),
        (Some("mail"), Some("landed")) | (Some("resume"), Some("confirmed")) => {
            format!(" \u{2713} {who} {}", a.rung.as_deref().unwrap_or(""))
        }
        (Some("crown"), Some("sent")) => format!(" \u{2713} {who} -> crown"),
        _ => format!(" \u{2713} {who} undelivered"),
    }
}

/// The full questions view's state: every question in the fold (open before
/// answered, ready first), the list cursor, and the answer gesture. `sel`
/// names the highlighted option, `0` meaning none of these; `free` is the
/// open notes input (the answer line itself on a no-option question); the
/// page follows the cursor's question.
pub(super) struct Detail {
    pub(super) items: Vec<crate::needs_overlay::QuestionItem>,
    pub(super) idx: usize,
    pub(super) sel: Option<u32>,
    pub(super) notes: Option<String>,
    pub(super) free: Option<String>,
    pub(super) notice: Option<String>,
    /// false = the list pane holds focus, true = the question page does.
    pub(super) focus: bool,
    /// The page line the detail pane windows to when it holds focus.
    pub(super) scroll: usize,
}

impl Detail {
    /// Open on the question with `id` (the page holds focus), else on the
    /// list (the `+N more` gesture). `None` when the fold has no questions.
    pub(super) fn open(
        fold: &crate::needs_overlay::QuestionsFold,
        id: Option<&str>,
    ) -> Option<Detail> {
        let items = block_items(fold, true);
        if items.is_empty() {
            return None;
        }
        let idx = id
            .and_then(|id| items.iter().position(|q| q.id == id))
            .unwrap_or(0);
        Some(Detail {
            items,
            idx,
            sel: None,
            notes: None,
            free: None,
            notice: None,
            focus: id.is_some(),
            scroll: 0,
        })
    }

    pub(super) fn item(&self) -> &crate::needs_overlay::QuestionItem {
        &self.items[self.idx]
    }

    /// Move the list cursor; the pick and the page reset with it.
    fn advance(&mut self, delta: isize) {
        let n = self.items.len();
        self.idx = (self.idx as isize + delta).rem_euclid(n as isize) as usize;
        self.sel = None;
        self.notes = None;
        self.free = None;
        self.notice = None;
        self.scroll = 0;
    }
}

/// The question page as markdown, the shape the backlog board's renderer
/// turns into the detail pane's lines: title and meta, the context sections,
/// the numbered options (the recommended one marked), and the answer line.
fn question_page(item: &crate::needs_overlay::QuestionItem, d: &Detail, now: u64) -> String {
    let mut s = String::new();
    let title = if item.title.is_empty() {
        "(no title)"
    } else {
        &item.title
    };
    s.push_str(&format!("# {title}\n\n"));
    let asker = item.asker.as_ref();
    s.push_str(&format!(
        "{} \u{b7} {} \u{b7} node {} \u{b7} {} {}{}\n\n",
        asker.map(|a| a.handle.as_str()).unwrap_or("?"),
        asker.and_then(|a| a.harness.as_deref()).unwrap_or("?"),
        item.node.as_deref().unwrap_or("none"),
        age_short(&item.created_at, now),
        item.state,
        if item.ready {
            String::new()
        } else {
            format!(" \u{b7} missing: {}", item.missing.join(", "))
        }
    ));
    if let Some(body) = item.body.as_deref() {
        if !body.is_empty() && body != item.title {
            s.push_str(&format!("{body}\n\n"));
        }
    }
    let not_recorded = "NOT RECORDED";
    s.push_str(&format!(
        "## why asked\n\n{}\n\n",
        item.blocked_because
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(not_recorded)
    ));
    s.push_str(&format!(
        "## why these options\n\n{}\n\n",
        item.options_rationale
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(not_recorded)
    ));
    s.push_str("## options\n\n");
    for o in &item.options {
        let mut line = format!("{}. {}", o.n, o.text);
        if d.sel == Some(o.n) {
            line.push_str("  [selected]");
        }
        if item
            .recommendation
            .as_ref()
            .is_some_and(|r| r.option == o.n)
        {
            line.push_str("  [recommended]");
        }
        s.push_str(&format!("{line}\n"));
        for p in &o.pros {
            s.push_str(&format!("   + {p}\n"));
        }
        for c in &o.cons {
            s.push_str(&format!("   - {c}\n"));
        }
    }
    if !item.options.is_empty() {
        let none_sel = d.sel == Some(0);
        s.push_str(&format!(
            "0. none of these{}\n",
            if none_sel { "  [selected]" } else { "" }
        ));
    }
    s.push('\n');
    if let Some(r) = &item.recommendation {
        s.push_str(&format!(
            "## recommended\n\noption {} \u{b7} {}{}\n\n",
            r.option,
            r.why,
            r.downside
                .as_deref()
                .map(|x| format!(" \u{b7} downside: {x}"))
                .unwrap_or_default()
        ));
    }
    s.push_str(&format!(
        "## reversible\n\n{}\n\n",
        item.reversible
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(not_recorded)
    ));
    if item.reversible.as_deref() == Some("costly") {
        s.push_str(&format!(
            "## cost if wrong\n\n{}\n\n",
            item.cost_if_wrong
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(not_recorded)
        ));
    }
    s.push_str(&format!(
        "## meanwhile\n\n{}\n\n",
        item.meanwhile
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(not_recorded)
    ));
    s.push_str(&format!(
        "## not thought through\n\n{}\n\n",
        item.unknowns
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(not_recorded)
    ));
    if let Some(notes) = &d.notes {
        s.push_str(&format!("## answer\n\nnotes: {notes}\n\n"));
    }
    s
}

/// The question page's rendered lines at the detail pane's width.
fn page_lines(d: &Detail, w: usize, now: u64) -> Vec<backlog_style::BLine> {
    super::node_detail::backlog_md::md_lines(&question_page(d.item(), d, now), w, usize::MAX)
}

/// The detail pane's body width at a terminal size, shared by the draw and
/// the scroll clamp so the window never follows past a line the pane hides:
/// side-by-side splits at 45%, stacked spans the terminal.
fn page_width(rows: usize, cols: usize) -> usize {
    let hint_h = if rows >= 8 { 2 } else { 0 };
    let panes_h = rows.saturating_sub(hint_h);
    let side_by_side = cols >= 100 && panes_h >= 10;
    let pane_w = if side_by_side {
        cols.saturating_sub(cols * 45 / 100)
    } else {
        cols
    };
    pane_w.saturating_sub(2).max(10)
}

/// Draw the full questions view over the content viewport: the framed list
/// pane left, the framed question page right (stacked when narrow), the key
/// hint below, and the notes input floating over it all. Returns false when
/// no view is open, so the draw chain's `else if` arm is one call.
pub(super) fn draw_detail(
    view: &View,
    cells: &mut [Cell],
    (rows, cols): (usize, usize),
    _origin: (usize, usize),
    _dims: (usize, usize),
) -> bool {
    let Some(d) = &view.question_detail else {
        return false;
    };
    let now = crate::digest_overlay::now_secs();
    let hint_h = if rows >= 8 { 2 } else { 0 };
    let panes_h = rows.saturating_sub(hint_h);
    let side_by_side = cols >= 100 && panes_h >= 10;
    let list_w = if side_by_side { cols * 45 / 100 } else { cols };
    // The list pane: one bold title row plus one dim meta row per question.
    let mut list_lines: Vec<backlog_style::BLine> = Vec::new();
    let mut cursor_line: Option<usize> = None;
    for (i, q) in d.items.iter().enumerate() {
        let title = if q.title.is_empty() {
            "(no title)"
        } else {
            &q.title
        };
        let mark = if i == d.idx { "\u{25b8}" } else { " " };
        let done = q.state != "open";
        let mut title_line = backlog_style::BLine::of(&[
            backlog_style::BSeg {
                text: format!("{mark} "),
                role: backlog_style::BRole::Body,
            },
            backlog_style::BSeg {
                text: title.to_string(),
                role: backlog_style::BRole::Label,
            },
        ]);
        title_line.band = i == d.idx;
        list_lines.push(title_line);
        let target = q.blocks.first().map(String::as_str).unwrap_or("none");
        let node = q.node.as_deref().unwrap_or(target);
        let asker = q.asker.as_ref().map(|a| a.handle.as_str()).unwrap_or("?");
        let mut meta = format!(
            "  {asker} \u{2192} {node} {} {age}",
            q.state,
            age = age_short(&q.created_at, now)
        );
        if !q.ready && !done {
            meta.push_str(" \u{b7} not ready");
        }
        list_lines.push(backlog_style::BLine::meta(meta));
        if i == d.idx {
            cursor_line = Some(list_lines.len() - 2);
        }
    }
    if list_lines.is_empty() {
        list_lines.push(backlog_style::BLine::meta("no questions in the fold"));
    }
    let list_chrome = chrome::Chrome::new("questions", crate::popup::Anchor::Center).flat();
    let list_body: Vec<chrome::BodyLine> = list_lines
        .iter()
        .map(|l| backlog_style::to_body_line(&l.clone().pad_to(list_w.saturating_sub(2))))
        .collect();
    let (list_rect, detail_rect) = if side_by_side {
        ((0, 0, panes_h, list_w), (0, list_w, panes_h, cols - list_w))
    } else {
        let half = panes_h / 2;
        ((0, 0, half, cols), (half, 0, panes_h - half, cols))
    };
    let inner_list_w = list_rect.3.saturating_sub(chrome::Chrome::FRAME_COLS);
    let _ = inner_list_w;
    super::backlog_board::backlog_panes::framed_region(
        cells,
        rows,
        cols,
        list_rect,
        &list_chrome,
        &list_body,
        cursor_line,
        cursor_line,
        &view.theme,
    );
    // The detail pane: the cursor question's page, wrapped to the pane the
    // layout actually gave it (the full width when the panes stack).
    let page = page_lines(d, page_width(rows, cols), now);
    let page_follow = Some(d.scroll.min(page.len().saturating_sub(1)));
    let detail_chrome = chrome::Chrome::new(
        format!("question \u{b7} {}", d.item().id),
        crate::popup::Anchor::Center,
    )
    .flat();
    let detail_body: Vec<chrome::BodyLine> = page
        .iter()
        .map(|l| backlog_style::to_body_line(&l.clone().pad_to(detail_rect.3.saturating_sub(2))))
        .collect();
    super::backlog_board::backlog_panes::framed_region(
        cells,
        rows,
        cols,
        detail_rect,
        &detail_chrome,
        &detail_body,
        if d.focus { page_follow } else { None },
        None,
        &view.theme,
    );
    // The two-row hint, mirroring the backlog board's. A refusal or a
    // "pick an option" nudge reads in the hint's place until the next key.
    let hint = if let Some(n) = &d.notice {
        n.as_str()
    } else if let Some(_) = &d.free {
        if d.item().options.is_empty() && d.item().kind != "pin" {
            "type the answer \u{b7} Enter sends \u{b7} Esc cancels"
        } else {
            "notes \u{b7} Enter saves \u{b7} Esc cancels"
        }
    } else if d.focus {
        if d.item().options.is_empty() {
            "Enter answer \u{b7} j/k scroll \u{b7} n notes \u{b7} Tab list \u{b7} Esc close"
        } else {
            "1-9 option \u{b7} 0 none \u{b7} j/k select \u{b7} Enter sends \u{b7} n notes \u{b7} ^d/^u scroll \u{b7} Tab list \u{b7} Esc close"
        }
    } else {
        "j/k move \u{b7} Enter opens \u{b7} Tab page \u{b7} n notes \u{b7} 0 none of these \u{b7} Esc close"
    };
    if hint_h > 0 {
        let [a, b] = super::backlog_board::backlog_panes::hint_rows(hint, cols);
        let hint_lines = [backlog_style::BLine::meta(a), backlog_style::BLine::meta(b)];
        backlog_style::paint_panel(
            cells,
            rows,
            cols,
            rows - hint_h,
            cols,
            hint_h,
            &hint_lines,
            None,
            &view.theme,
        );
    }
    // The notes input (g): the composer's look in miniature, the same frame,
    // prompt marker and colors, one to three wrapped lines growing as typed.
    if let Some(text) = &d.free {
        draw_notes_box(cells, rows, cols, text, &view.theme, d);
    }
    true
}

/// The notes input: a small framed box over the popup's bottom center, the
/// `❯ ` prompt gutter, the text wrapped to one to three lines, the cursor
/// block at the end.
fn draw_notes_box(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    text: &str,
    theme: &Theme,
    d: &Detail,
) {
    // Never wider than the terminal can hold: a tiny column keeps the box
    // inside its four-column margin instead of a forced minimum floor.
    let w = 64.min(cols.saturating_sub(4).max(1));
    let inner_w = w.saturating_sub(2);
    let text_w = inner_w.saturating_sub(2);
    let mut wrapped: Vec<String> = Vec::new();
    node_detail::wrap_line(&format!("{text}\u{258f}"), text_w, &mut wrapped);
    if wrapped.is_empty() {
        wrapped.push("\u{258f}".to_string());
    }
    let body_lines = wrapped.len().clamp(1, 3);
    let box_h = body_lines + 2;
    let top = rows.saturating_sub(2).saturating_sub(box_h).max(1);
    let left = cols.saturating_sub(w) / 2;
    let chrome = chrome::Chrome::new("notes", crate::popup::Anchor::Center);
    let body: Vec<chrome::BodyLine> = (0..body_lines)
        .map(|_| chrome::BodyLine::plain(""))
        .collect();
    let framed = chrome::frame(&body, &chrome, inner_w, None);
    chrome::blit(cells, rows, cols, (top, left), &framed, theme);
    let hint = if d.item().options.is_empty() && d.item().kind != "pin" {
        "Enter sends \u{b7} Esc cancels"
    } else {
        "Enter saves \u{b7} Esc cancels"
    };
    let hint_col = left + w.saturating_sub(hint.chars().count() + 1);
    for (i, ch) in hint.chars().enumerate() {
        let c = hint_col + i;
        if c < cols && top > 0 {
            let r = top - 1;
            cells[r * cols + c] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::DIM,
            };
        }
    }
    for (k, line) in wrapped.iter().take(body_lines).enumerate() {
        let y = top + 1 + k;
        if y >= rows {
            break;
        }
        let mut col = left + 1;
        let paint = if k == 0 {
            format!("\u{276f} {line}")
        } else {
            format!("  {line}")
        };
        for ch in paint.chars() {
            let gw = glyph_cols(ch);
            if col + gw > left + 1 + inner_w || col + gw > cols {
                break;
            }
            let flags = if ch == '\u{276f}' || ch == '\u{258f}' {
                0
            } else {
                0
            };
            cells[y * cols + col] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags,
            };
            if gw == 2 {
                cells[y * cols + col + 1] = Cell {
                    c: ' ',
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: cell_flags::WIDE_SPACER,
                };
            }
            col += gw;
        }
    }
}

/// The 10 s refresh: at most one projection fold in flight while the
/// sideline is shown. The read runs off the UI loop; the fold rides `tx`
/// back to the run loop.
pub(super) fn maybe_kick(
    view: &mut View,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<crate::needs_overlay::QuestionsFold, String>>,
) {
    if view.panel_w() == 0 || view.questions_inflight {
        return;
    }
    let due = view
        .questions_kick_at
        .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10));
    if !due {
        return;
    }
    view.questions_inflight = true;
    view.questions_kick_at = Some(Instant::now());
    let tx = tx.clone();
    tokio::spawn(async move {
        let fold = crate::needs_overlay::questions_now().await;
        let _ = tx.send(fold);
    });
}

/// The full questions view's keys. The notes input owns the keyboard while
/// open (Enter saves or sends, Esc cancels); otherwise the list pane's j/k
/// moves the cursor, Tab flips panes, digits (and 0 for none of these)
/// highlight an option, Enter drills in or submits through
/// [`crate::needs_overlay::answer`], and Esc closes. Consumes every byte -
/// an open modal never leaks a key to a pane (the answer-overlay invariant).
pub(super) async fn detail_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(d) = view.question_detail.as_mut() else {
        return Ok(StdinFlow::Continue);
    };
    let mut esc = std::mem::take(&mut view.question_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.question_esc = esc;
    for &k in &keys {
        if let Some(buf) = d.free.as_mut() {
            match k {
                b'\r' | b'\n' => {
                    let text = std::mem::take::<String>(buf).trim().to_string();
                    let item = d.item().clone();
                    d.free = None;
                    if text.is_empty() {
                        d.notes = None;
                    } else if item.options.is_empty() && item.kind != "pin" {
                        // A no-option question's answer IS the line.
                        if !view.question_acting {
                            view.question_action =
                                Some((item.id, crate::needs_overlay::AnswerPick::Words(text)));
                            view.question_acting = true;
                        }
                    } else {
                        d.notes = Some(text);
                    }
                }
                0x1b => d.free = None,
                0x7f | 0x08 => {
                    buf.pop();
                }
                0x20..=0x7e => buf.push(k as char),
                _ => {}
            }
            continue;
        }
        match k {
            b'j' => {
                if !d.focus {
                    d.advance(1);
                } else if d.item().options.is_empty() {
                    // No options to select: the fold's arrow twin scrolls.
                    let (rows, cols) = view.term;
                    let lines = page_lines(
                        d,
                        page_width(rows as usize, cols as usize),
                        crate::digest_overlay::now_secs(),
                    )
                    .len();
                    d.scroll = (d.scroll + 1).min(lines.saturating_sub(1));
                } else {
                    select_neighbor(d, 1);
                }
            }
            b'k' => {
                if !d.focus {
                    d.advance(-1);
                } else if d.item().options.is_empty() {
                    d.scroll = d.scroll.saturating_sub(1);
                } else {
                    select_neighbor(d, -1);
                }
            }
            // Ctrl+d / Ctrl+u: half a page down/up. The scroll gesture for
            // option questions, where j/k are the selection.
            0x04 | 0x15 => {
                let (rows, cols) = view.term;
                let lines = page_lines(
                    d,
                    page_width(rows as usize, cols as usize),
                    crate::digest_overlay::now_secs(),
                )
                .len();
                let step = 12;
                d.scroll = if k == 0x04 {
                    (d.scroll + step).min(lines.saturating_sub(1))
                } else {
                    d.scroll.saturating_sub(step)
                };
            }
            b'\t' => d.focus = !d.focus,
            b'0'..=b'9' => {
                let n = (k - b'0') as u32;
                let opts = &d.item().options;
                let valid = n == 0 && !opts.is_empty() || opts.iter().any(|o| o.n == n);
                if valid {
                    d.sel = Some(n);
                    d.focus = true;
                }
            }
            b'n' => {
                let saved = d.notes.take().unwrap_or_default();
                d.free = Some(saved);
            }
            b'\r' | b'\n' => {
                if !d.focus {
                    d.focus = true;
                } else {
                    match submit(d) {
                        Submit::Queued((id, pick)) => {
                            if !view.question_acting {
                                view.question_action = Some((id, pick));
                                view.question_acting = true;
                            }
                        }
                        Submit::Notice(msg) => d.notice = Some(msg.to_string()),
                        Submit::OpenFree => d.free = Some(String::new()),
                    }
                }
            }
            0x1b => {
                view.question_detail = None;
                view.question_esc.clear();
                return Ok(StdinFlow::Continue);
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Move the highlighted option one step through the option list with none
/// of these last (spec f: the arrow gesture): `dir` 1 steps down, -1 up,
/// wrapping, and an unset pick starts at the first option (down) or at
/// none of these (up).
fn select_neighbor(d: &mut Detail, dir: isize) {
    let mut ns: Vec<u32> = d.item().options.iter().map(|o| o.n).collect();
    ns.sort_unstable();
    ns.dedup();
    ns.push(0);
    let pos = ns
        .iter()
        .position(|&n| Some(n) == d.sel)
        .unwrap_or(if dir > 0 { usize::MAX } else { 0 });
    let next = (pos as isize + dir).rem_euclid(ns.len() as isize) as usize;
    d.sel = Some(ns[next]);
}

/// What submitting the cursor question decided: an answer to queue, a
/// refusal to show on the view, or the answer line to open (a no-option
/// question). The caller owns every `View` write, so `submit` never borrows
/// the view; a send already in flight is the caller's guard.
enum Submit {
    Queued((String, crate::needs_overlay::AnswerPick)),
    Notice(&'static str),
    OpenFree,
}

/// Compose the cursor question's answer: a pin sends done, a no-option
/// question opens the answer line, an option (or none of these) sends
/// through the pick with any notes composed in.
fn submit(d: &mut Detail) -> Submit {
    let item = d.item().clone();
    if item.state != "open" {
        return Submit::Notice("this question is answered - read only");
    }
    if item.kind == "pin" {
        return Submit::Queued((item.id, crate::needs_overlay::AnswerPick::Done));
    }
    if item.options.is_empty() {
        return Submit::OpenFree;
    }
    let Some(n) = d.sel else {
        return Submit::Notice("pick an option: 1-9, or 0 for none of these");
    };
    let notes = d.notes.clone().unwrap_or_default();
    let pick = if n == 0 {
        crate::needs_overlay::AnswerPick::Words(if notes.is_empty() {
            "none of these".to_string()
        } else {
            format!("none of these - {notes}")
        })
    } else if notes.is_empty() {
        crate::needs_overlay::AnswerPick::Option(n)
    } else {
        let text = item
            .options
            .iter()
            .find(|o| o.n == n)
            .map(|o| o.text.clone())
            .unwrap_or_default();
        crate::needs_overlay::AnswerPick::Words(format!("{n}. {text} - notes: {notes}"))
    };
    Submit::Queued((item.id, pick))
}

impl View {
    /// Rows the questions block owns just above the court block, after the
    /// operator's height pref and the court rule. Zero when hidden, empty,
    /// or out of room.
    pub(super) fn questions_block_rows(&self) -> usize {
        block_rows(self, self.term.0 as usize)
            .map(|b| b.n)
            .unwrap_or(0)
    }

    /// Open the full questions view on one question by id (the page holds
    /// focus).
    pub(super) fn open_detail_on(&mut self, id: &str) {
        let empty = crate::needs_overlay::QuestionsFold::default();
        let fold = self.questions_fold.as_ref().unwrap_or(&empty);
        self.question_detail = Detail::open(fold, Some(id));
    }

    /// Open the full questions view on the list (the `+N more` gesture).
    pub(super) fn open_questions_list(&mut self) {
        let empty = crate::needs_overlay::QuestionsFold::default();
        let fold = self.questions_fold.as_ref().unwrap_or(&empty);
        self.question_detail = Detail::open(fold, None);
    }

    /// Apply a landed questions fold: the block always shows the latest
    /// projection, so no generation guard here. An Err names why the read
    /// failed so the toggle toast can name it too.
    pub(super) fn apply_questions_fold(
        &mut self,
        fold: Result<crate::needs_overlay::QuestionsFold, String>,
    ) {
        self.questions_inflight = false;
        match fold {
            Ok(f) => {
                self.questions_degraded = false;
                self.questions_degraded_reason = None;
                self.questions_fold = Some(f);
            }
            Err(reason) => {
                self.questions_fold = Some(crate::needs_overlay::QuestionsFold::default());
                self.questions_degraded = true;
                self.questions_degraded_reason = Some(reason);
            }
        }
    }

    /// Apply a finished question answer: success re-folds so the row leaves
    /// the queue on the next fold and closes the view; a failure shows the
    /// door's own line and keeps the view open on it.
    pub(super) fn apply_question_action_result(&mut self, result: Result<String, String>) {
        self.question_acting = false;
        match result {
            Ok(receipt) => {
                self.needs_want = true;
                self.set_notice(format!("needs: {receipt}"));
                self.question_detail = None;
            }
            Err(msg) => self.set_notice(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{question_item, view_with_agents};
    use super::*;
    use crate::needs_overlay::{AnsweredItem, QuestionItem, QuestionOption, QuestionsFold};
    const H: usize = 8;

    fn item(id: &str, ready: bool) -> QuestionItem {
        QuestionItem {
            id: id.into(),
            kind: "question".into(),
            title: format!("title for {id}"),
            state: "open".into(),
            ready,
            missing: if ready {
                vec![]
            } else {
                vec!["unknowns".into()]
            },
            created_at: "2026-09-26T04:00:00Z".into(),
            options: vec![
                QuestionOption {
                    n: 1,
                    text: "narrow".into(),
                    ..Default::default()
                },
                QuestionOption {
                    n: 2,
                    text: "wide".into(),
                    ..Default::default()
                },
            ],
            blocked_because: Some("two lanes".into()),
            ..Default::default()
        }
    }

    fn fold_with(items: Vec<QuestionItem>) -> QuestionsFold {
        QuestionsFold {
            items,
            ..Default::default()
        }
    }

    fn layout(
        height: usize,
        show_done: bool,
        fold: &QuestionsFold,
    ) -> (Vec<Vec<Seg>>, Vec<String>, bool) {
        block_layout(height, show_done, fold, 0)
    }

    #[test]
    fn block_layout_rows() {
        let items: Vec<QuestionItem> = (0..9)
            .map(|i| {
                let mut q = item(&format!("q-{i}"), true);
                q.ready = i < 2;
                q
            })
            .collect();
        let (lines, ids, more) = layout(H, false, &fold_with(items));
        assert_eq!(lines[0][0].0, " questions 9 \u{b7} 2 ready");
        // Height 8: header + 6 title/meta pairs, then the more line. Nine
        // items elide two, and the more line trades one row for the door.
        assert_eq!(ids.len(), 6, "six open rows carry ids at height 8");
        assert!(more);
        assert_eq!(lines[7][0].0, " +3 more");
        assert_eq!(lines.len(), 8, "nothing paints past the height");

        let (lines, ids, _) = layout(H, false, &fold_with(vec![]));
        assert!(lines.is_empty() && ids.is_empty());

        let mut fold = fold_with(vec![item("q-1", true)]);
        fold.answered = vec![
            AnsweredItem {
                id: "q-8".into(),
                asker: Some("w1".into()),
                ..Default::default()
            },
            AnsweredItem {
                id: "q-9".into(),
                asker: Some("w2".into()),
                ..Default::default()
            },
        ];
        let (lines, _, _) = layout(H, false, &fold);
        let answered_lines: Vec<_> = lines
            .iter()
            .filter(|l| l[0].0.contains("answered"))
            .collect();
        assert_eq!(answered_lines.len(), 1, "one count line");
        assert_eq!(answered_lines[0][0].0, " \u{2713} 2 answered");
        assert_eq!(answered_lines[0][0].1, cell_flags::DIM);
        // Shown: every fresh answer renders its own line.
        let (shown, _, _) = layout(H, true, &fold);
        assert!(shown.len() > lines.len(), "show_done expands the section");

        let mut fold = fold_with(vec![]);
        fold.answered = vec![AnsweredItem {
            id: "q-9".into(),
            asker: Some("w2".into()),
            ..Default::default()
        }];
        let (lines, ids, _) = layout(H, false, &fold);
        assert!(ids.is_empty(), "no open rows carry ids");
        assert_eq!(lines.len(), 2, "header plus the count line");
        assert!(lines[1][0].0.contains("1 answered"));

        let mut a = item("q-a", false);
        a.title = "slow lane".into();
        let mut b = item("q-b", true);
        b.title = "pick the door".into();
        let (lines, ids, _) = layout(H, false, &fold_with(vec![a, b]));
        assert_eq!(ids[0], "q-b", "ready first");
        // One row per question: the bold title leads, the dim meta trails
        // on the same line.
        assert_eq!(lines[1][0].0, " ? pick the door");
        assert_eq!(lines[1][0].1, cell_flags::BOLD, "the title is bold");
        assert!(lines[1][1].1 & cell_flags::DIM != 0, "meta is dim");
        assert!(lines[1][1].0.contains("? -> none"), "{:?}", lines[1][1].0);
        assert_eq!(lines[2][0].0, " ? slow lane", "the not-ready row follows");

        assert_eq!(crate::view_store::QUESTIONS_DEFAULT_HEIGHT, 4);
        let items: Vec<QuestionItem> = (0..9).map(|i| item(&format!("q-{i}"), true)).collect();
        let (lines, ids, more) = layout(4, false, &fold_with(items));
        assert_eq!(lines.len(), 4, "header + 2 rows + more at the default");
        assert_eq!(ids.len(), 2);
        assert!(more);
        assert_eq!(lines[3][0].0, " +7 more");

        let mut answered = item("q-old", true);
        answered.state = "answered".into();
        let (lines, ids, _) = layout(H, true, &fold_with(vec![item("q-new", true), answered]));
        assert_eq!(ids.len(), 2, "both rows carry ids");
        assert_eq!(ids[0], "q-new", "open before answered");
        assert_eq!(
            lines[2][0].1,
            cell_flags::BOLD | cell_flags::DIM,
            "the answered row reads as done"
        );
    }

    #[test]
    fn page_rows() {
        let mut q = item("q-1", false);
        q.recommendation = Some(crate::needs_overlay::QuestionRecommendation {
            option: 1,
            why: "narrowest".into(),
            downside: Some("hides a feature".into()),
        });
        q.unknowns = Some("net-zero moves".into());
        q.reversible = Some("costly".into());
        q.cost_if_wrong = Some("the allowance drops".into());
        q.meanwhile = Some("stops".into());
        let d = Detail::open(&fold_with(vec![q.clone()]), Some("q-1")).unwrap();
        let page = question_page(&q, &d, 0);
        for want in [
            "title for q-1",
            "why asked",
            "two lanes",
            "1. narrow",
            "2. wide",
            "[recommended]",
            "recommended",
            "option 1 \u{b7} narrowest",
            "downside: hides a feature",
            "reversible",
            "cost if wrong",
            "the allowance drops",
            "meanwhile",
            "stops",
            "not thought through",
            "net-zero moves",
            "missing: unknowns",
            "0. none of these",
        ] {
            assert!(page.contains(want), "{want} missing from:\n{page}");
        }

        let mut q = item("q-2", true);
        q.blocked_because = None;
        q.unknowns = None;
        let d = Detail::open(&fold_with(vec![q.clone()]), Some("q-2")).unwrap();
        let page = question_page(&q, &d, 0);
        assert!(page.contains("NOT RECORDED"));

        let q = item("q-1", true);
        let mut d = Detail::open(&fold_with(vec![q.clone()]), Some("q-1")).unwrap();
        d.sel = Some(2);
        let page = question_page(&q, &d, 0);
        assert!(page.contains("2. wide  [selected]"));
        d.sel = Some(0);
        let page = question_page(&q, &d, 0);
        assert!(page.contains("0. none of these  [selected]"));
    }

    #[test]
    fn detail_nav_rows() {
        let items = vec![item("q-a", true), item("q-b", true), item("q-c", true)];
        let mut d = Detail::open(&fold_with(items.clone()), None).unwrap();
        assert_eq!(d.item().id, "q-a");
        d.advance(1);
        assert_eq!(d.item().id, "q-b");
        d.sel = Some(2);
        d.advance(-1);
        assert_eq!(d.item().id, "q-a");
        assert_eq!(d.sel, None, "the pick resets on advance");

        let items = vec![item("q-a", true), item("q-b", true)];
        let d = Detail::open(&fold_with(items), Some("q-b")).unwrap();
        assert_eq!(d.item().id, "q-b");
        assert!(d.focus, "a row click opens with the page focused");
        let d = Detail::open(&fold_with(vec![item("q-a", true)]), None).unwrap();
        assert!(!d.focus, "the list gesture opens on the list");
    }

    #[tokio::test]
    async fn answer_keys_rows() {
        let mut v = view_with_agents(vec![]);
        v.mine_fold = Some(Vec::new());
        v.needs_fold = Some(Vec::new());
        v.questions_fold = Some(crate::needs_overlay::QuestionsFold {
            items: vec![question_item("q-2", &[], None)],
            ..Default::default()
        });
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"\r", &mut buf).await.unwrap();
        let detail = v.question_detail.expect("the full view opened");
        assert_eq!(detail.item().id, "q-2");
        assert_eq!(v.answers, Some(0), "the overlay stays open");
        assert!(buf.is_empty());

        let mut v = view_with_agents(vec![]);
        v.mine_fold = Some(Vec::new());
        v.needs_fold = Some(Vec::new());
        v.questions_fold = Some(crate::needs_overlay::QuestionsFold {
            items: vec![question_item("q-1", &["oauth", "apikey"], Some(true))],
            ..Default::default()
        });
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"2", &mut buf).await.unwrap();
        let detail = v.question_detail.expect("the full view opened");
        assert_eq!(detail.item().id, "q-1");
        assert!(
            v.question_action.is_none(),
            "nothing is sent from the digit"
        );
        assert!(
            buf.is_empty(),
            "a question digit never sends a pane keystroke"
        );

        let mut v = view_with_agents(vec![]);
        v.mine_fold = Some(Vec::new());
        v.needs_fold = Some(Vec::new());
        v.questions_fold = Some(crate::needs_overlay::QuestionsFold {
            items: vec![question_item("q-3", &["oauth"], Some(true))],
            ..Default::default()
        });
        v.answers = Some(0);
        let mut buf: Vec<u8> = Vec::new();
        answer_keys(&mut v, b"9", &mut buf).await.unwrap();
        assert_eq!(v.question_action, None);
        assert!(!v.question_acting);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn submit_rows() {
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-1", true)]));
        // A row click opens with the page focused; a digit highlights.
        v.open_detail_on("q-1");
        detail_keys(&mut v, b"2", &mut Vec::new()).await.unwrap();
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some(("q-1".into(), crate::needs_overlay::AnswerPick::Option(2)))
        );
        // None of these composes the words answer.
        v.question_action = None;
        v.question_acting = false;
        v.question_detail.as_mut().unwrap().sel = Some(0);
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some((
                "q-1".into(),
                crate::needs_overlay::AnswerPick::Words("none of these".into())
            ))
        );

        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-1", true)]));
        v.open_detail_on("q-1");
        detail_keys(&mut v, b"1", &mut Vec::new()).await.unwrap();
        detail_keys(&mut v, b"n", &mut Vec::new()).await.unwrap();
        assert!(v.question_detail.as_ref().unwrap().free.is_some());
        detail_keys(&mut v, b"check legal", &mut Vec::new())
            .await
            .unwrap();
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        let d = v.question_detail.as_ref().unwrap();
        assert!(d.free.is_none(), "the input closed");
        assert_eq!(d.notes.as_deref(), Some("check legal"));
        // Submitting composes option + notes into one words answer; the
        // page still holds focus, so Enter sends.
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some((
                "q-1".into(),
                crate::needs_overlay::AnswerPick::Words("1. narrow - notes: check legal".into())
            ))
        );

        let mut q = item("q-p", true);
        q.options = vec![];
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![q]));
        v.open_detail_on("q-p");
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert!(v.question_detail.as_ref().unwrap().free.is_some());
        detail_keys(&mut v, b"do it yourself", &mut Vec::new())
            .await
            .unwrap();
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some((
                "q-p".into(),
                crate::needs_overlay::AnswerPick::Words("do it yourself".into())
            ))
        );

        let mut q = item("q-pin", true);
        q.kind = "pin".into();
        q.options = vec![];
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![q]));
        v.open_detail_on("q-pin");
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some(("q-pin".into(), crate::needs_overlay::AnswerPick::Done))
        );
    }

    #[tokio::test]
    async fn detail_keys_nav_rows() {
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-1", true)]));
        v.open_detail_on("q-1");
        // Unset + down lands on the first option, then walks into none of
        // these and wraps back around.
        detail_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().sel, Some(1));
        detail_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().sel, Some(2));
        detail_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().sel, Some(0));
        detail_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().sel, Some(1), "wrap");
        // Up from the first option reaches none of these from the other side.
        detail_keys(&mut v, b"k", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().sel, Some(0));
        detail_keys(&mut v, b"k", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().sel, Some(2));
        // Enter submits the arrow-highlighted option.
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some(("q-1".into(), crate::needs_overlay::AnswerPick::Option(2)))
        );

        let mut q = item("q-long", true);
        q.body = Some("word ".repeat(300));
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![q]));
        v.open_detail_on("q-long");
        detail_keys(&mut v, b"\x04", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().scroll, 12, "half page");
        detail_keys(&mut v, b"\x15", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().scroll, 0);

        let items = vec![item("q-a", true), item("q-b", true)];
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(items));
        v.open_detail_on("q-a");
        detail_keys(&mut v, b"\t", &mut Vec::new()).await.unwrap();
        assert!(!v.question_detail.as_ref().unwrap().focus);
        detail_keys(&mut v, b"j", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_detail.as_ref().unwrap().item().id, "q-b");
        detail_keys(&mut v, b"\r", &mut Vec::new()).await.unwrap();
        assert!(
            v.question_detail.as_ref().unwrap().focus,
            "Enter drills into the page"
        );

        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-a", true)]));
        v.open_questions_list();
        detail_keys(&mut v, b"\x1b", &mut Vec::new()).await.unwrap();
        // The lone Esc rides the carry until the next chunk proves it bare.
        detail_keys(&mut v, b"", &mut Vec::new()).await.unwrap();
        assert!(v.question_detail.is_none());
    }

    #[test]
    fn page_width_follows_the_layout_the_draw_uses() {
        // Stacked: the page wraps at the full terminal width.
        assert_eq!(page_width(40, 80), 78);
        // Side-by-side: the detail pane's own inner width.
        assert_eq!(page_width(40, 100), 100 - 45 - 2);
        // Tiny terminal: the width floors at ten.
        assert_eq!(page_width(6, 8), 10);
    }

    #[tokio::test]
    async fn notes_box_on_a_tiny_terminal_stays_in_bounds() {
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-1", true)]));
        v.open_detail_on("q-1");
        detail_keys(&mut v, b"n", &mut Vec::new()).await.unwrap();
        detail_keys(
            &mut v,
            "a long note that must wrap and stay inside the box".as_bytes(),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        // A 20x6 terminal: the box clamps to the terminal, no panic.
        v.term = (6, 20);
        let (rows, cols) = (6usize, 20usize);
        let mut cells = vec![Cell::default(); rows * cols];
        assert!(draw_detail(
            &v,
            &mut cells,
            (rows, cols),
            (0, 0),
            (rows, cols)
        ));
    }

    #[test]
    fn apply_result_rows() {
        let mut v = view_with_agents(vec![]);
        v.needs_want = false;
        v.apply_question_action_result(Ok("recorded, delivering".into()));
        assert!(!v.question_acting);
        assert!(v.needs_want, "success re-folds so the row leaves on refold");

        let mut v = view_with_agents(vec![]);
        v.needs_want = false;
        v.apply_question_action_result(Err("failed to close q-1: locked".into()));
        assert!(!v.question_acting);
        assert!(!v.needs_want, "a failure never triggers a re-fold");
        let notice = v.notice.as_ref().expect("failure surfaces a notice");
        assert!(notice.0.contains("failed to close q-1: locked"));

        let mut v = view_with_agents(vec![]);
        v.apply_questions_fold(Err("events.jsonl: permission denied".into()));
        assert!(v.questions_degraded);
        assert_eq!(
            v.questions_degraded_reason.as_deref(),
            Some("events.jsonl: permission denied")
        );
        v.apply_questions_fold(Ok(fold_with(vec![item("q-a", true)])));
        assert!(!v.questions_degraded);
        assert_eq!(v.questions_degraded_reason, None);
    }

    #[test]
    fn toggle_toast_rows() {
        let mut v = view_with_agents(vec![]);
        v.apply_questions_fold(Err("timed out after 800ms".into()));
        assert!(v.questions_degraded);
        v.questions_block.visible = false;
        toggle_block(&mut v);
        assert_eq!(
            v.notice.as_ref().unwrap().0,
            "questions unreadable: timed out after 800ms"
        );

        let mut v = view_with_agents(vec![]);
        let mut q = item("q-a", true);
        q.state = "answered".into();
        v.apply_questions_fold(Ok(fold_with(vec![q])));
        v.questions_block.visible = false;
        toggle_block(&mut v);
        assert_eq!(v.notice.as_ref().unwrap().0, "no open questions");

        let mut v = view_with_agents(vec![]);
        v.apply_questions_fold(Ok(fold_with(vec![item("q-a", true)])));
        v.questions_block.visible = false;
        toggle_block(&mut v);
        assert_eq!(v.notice.as_ref().unwrap().0, "questions block: shown");
        toggle_block(&mut v);
        assert_eq!(v.notice.as_ref().unwrap().0, "questions block: hidden");
    }

    #[test]
    fn block_hit_rows() {
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(
            (0..9).map(|i| item(&format!("q-{i}"), true)).collect(),
        ));
        v.questions_block.visible = true;
        v.questions_block.height = 4;
        let b = block_rows(&v, 60).expect("the block reserves rows");
        assert_eq!(b.n, 4);
        assert!(b.more, "nine questions at height 4 leave more");
        v.questions_block.visible = false;
        assert!(block_rows(&v, 60).is_none(), "the toggle hides the block");
        v.questions_block.visible = true;
        v.questions_block.height = 2;
        let b = block_rows(&v, 60).expect("the block reserves rows");
        assert_eq!(b.n, 2, "height 2: header plus one row");

        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(
            (0..9).map(|i| item(&format!("q-{i}"), true)).collect(),
        ));
        v.questions_block.visible = true;
        v.questions_block.height = 4;
        // term_rows 40, court 0: the block paints [36..40): header, 2 rows,
        // the more line. Height 4 with nine items shows q-0 and q-1.
        let court = v.court_block_layout(40).0;
        let start = 40 - court - 4;
        assert!(matches!(
            hit_at(&v, 40, (start + 1) as u16),
            Some(QuestionHit::Row(id)) if id == "q-0"
        ));
        assert!(matches!(
            hit_at(&v, 40, (start + 3) as u16),
            Some(QuestionHit::More)
        ));
        assert!(hit_at(&v, 40, (start + 4) as u16).is_none());
        assert!(
            hit_at(&v, 40, start as u16).is_none(),
            "the header is inert"
        );
    }
}
