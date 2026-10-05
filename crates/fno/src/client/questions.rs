//! The full questions detail view, split out of `client.rs` because that file
//! is over the line budget and shrink-only. It reuses the backlog board's
//! framed list and detail panes and markdown renderer (`backlog_panes` /
//! `backlog_md`), and answers through [`crate::needs_overlay::answer`].
//! A child module of `client`, so `View`'s private fields stay local.

use super::*;
use std::sync::atomic::Ordering;

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

pub(super) struct Detail {
    /// The opened question; the bell panel tabs are the board, so the old
    /// list pane is gone and a view holds exactly the row it opened on.
    pub(super) item: crate::needs_overlay::QuestionItem,
    pub(super) sel: Option<u32>,
    pub(super) notes: Option<String>,
    pub(super) free: Option<String>,
    pub(super) notice: Option<String>,
    /// The page line the pane windows to.
    pub(super) scroll: usize,
    /// `a` picked "agents decide": Enter hands the question to the team.
    pub(super) delegate: bool,
    /// The first `X` armed archive-all; the second sends it.
    pub(super) archive_armed: bool,
}

impl Detail {
    /// Open on the question with `id`; None when the fold does not hold it.
    pub(super) fn open(fold: &crate::needs_overlay::QuestionsFold, id: &str) -> Option<Detail> {
        let item = fold.items.iter().find(|q| q.id == id).cloned()?;
        Some(Detail {
            item,
            sel: None,
            notes: None,
            free: None,
            notice: None,
            scroll: 0,
            delegate: false,
            archive_armed: false,
        })
    }

    pub(super) fn item(&self) -> &crate::needs_overlay::QuestionItem {
        &self.item
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
        if item.settled {
            "answered"
        } else {
            &item.state
        },
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
    if d.delegate {
        s.push_str(
            "## answer\n\nagents decide: the team over the node decides a reversible call\n\n",
        );
    }
    if let Some(notes) = &d.notes {
        s.push_str(&format!("## answer\n\nnotes: {notes}\n\n"));
    }
    s
}

/// The question page's rendered lines at the detail pane's width.
fn page_lines(d: &Detail, w: usize, now: u64) -> Vec<backlog_style::BLine> {
    super::node_detail::backlog_md::md_lines(&question_page(d.item(), d, now), w)
}

/// The page's wrapped width: the framed pane's inner width at any terminal.
fn page_width(cols: usize) -> usize {
    cols.saturating_sub(chrome::Chrome::FRAME_COLS).max(10)
}

/// Draw the question page over the content viewport: one framed pane, the
/// key hint below, and the notes input floating over it all. The bell panel
/// tabs are the board (the 2026-10-05 ruling), so the old list pane is
/// gone. Returns false when no view is open, so the draw chain's `else if`
/// arm is one call.
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
    let page = page_lines(d, page_width(cols), now);
    let page_follow = Some(d.scroll.min(page.len().saturating_sub(1)));
    let chrome = chrome::Chrome::new(
        format!("question \u{b7} {}", d.item().id),
        crate::popup::Anchor::Center,
    )
    .flat();
    let body: Vec<chrome::BodyLine> = page
        .iter()
        .map(|l| backlog_style::to_body_line(&l.clone().pad_to(cols.saturating_sub(2))))
        .collect();
    super::backlog_board::backlog_panes::framed_region(
        cells,
        rows,
        cols,
        (0, 0, panes_h, cols),
        &chrome,
        &body,
        page_follow,
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
    } else if d.item().options.is_empty() {
        "Enter answer \u{b7} j/k scroll \u{b7} n notes \u{b7} Esc close"
    } else {
        "1-9 option \u{b7} 0 none \u{b7} a agents decide \u{b7} j/k select \u{b7} Enter sends \u{b7} n notes \u{b7} ^d/^u scroll \u{b7} x archive \u{b7} X archive all \u{b7} Esc close"
    };
    if hint_h > 0 {
        let wrapped = super::backlog_board::backlog_panes::hint_rows(hint, cols);
        let hint_lines: Vec<backlog_style::BLine> = wrapped
            .into_iter()
            .take(2)
            .map(backlog_style::BLine::meta)
            .collect();
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

pub(super) type FoldMsg = Result<crate::needs_overlay::QuestionsFold, String>;

/// The projection leg's channel: the newest fold wins.
pub(super) fn fold_channel() -> (
    tokio::sync::mpsc::UnboundedSender<FoldMsg>,
    tokio::sync::mpsc::UnboundedReceiver<FoldMsg>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

/// The index leg's channel: lands first, seeds the bell's list.
pub(super) fn index_channel() -> (
    tokio::sync::mpsc::UnboundedSender<crate::needs_overlay::QuestionsFold>,
    tokio::sync::mpsc::UnboundedReceiver<crate::needs_overlay::QuestionsFold>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Refresh the shared questions projection while the sidebar is visible.
/// The read runs off the UI loop; the index fold rides `index_tx` back the
/// moment it lands, the projection rides `tx` when the subprocess answers.
pub(super) fn maybe_kick(
    view: &mut View,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<crate::needs_overlay::QuestionsFold, String>>,
    index_tx: &tokio::sync::mpsc::UnboundedSender<crate::needs_overlay::QuestionsFold>,
) {
    if view.panel_w() == 0 {
        return;
    }
    // The index flies on its own flight and clock: a projection wedged at
    // its 30s bound must not starve the ms-fast list refresh.
    let now_ms = crate::digest_overlay::now_secs() * 1000;
    if now_ms.saturating_sub(INDEX_KICKED_AT_MS.load(Ordering::Relaxed)) >= 10_000
        && !INDEX_INFLIGHT.swap(true, Ordering::SeqCst)
    {
        INDEX_KICKED_AT_MS.store(now_ms, Ordering::Relaxed);
        let index_tx = index_tx.clone();
        tokio::spawn(async move {
            if let Some(fold) = questions_index_now().await {
                let _ = index_tx.send(fold);
            }
            INDEX_INFLIGHT.store(false, Ordering::SeqCst);
        });
    }
    // The projection keeps the View single-flight: one subprocess at a time.
    if view.questions_inflight {
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

/// The index leg's own single-flight and cadence clock, module-local so the
/// View carries no second kick state.
static INDEX_INFLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static INDEX_KICKED_AT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The question pages index, resolved through `state path questions` and
/// parsed in-process: the fast leg that lets the bell paint the board
/// without waiting on the projection subprocess. Best-effort - any failure
/// reads as None and the projection stays the only source.
pub(super) async fn questions_index_now() -> Option<crate::needs_overlay::QuestionsFold> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin())
            .args(["state", "path", "questions"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let text = std::fs::read_to_string(dir.join("questions.md")).ok()?;
    parse_questions_index(&text)
}

/// Parse the attention arm's generated index (`## Open (n)` then `## Done`,
/// one `- [[stem|title]] \u{b7} kind \u{b7} trail` row per question) into the fold
/// shape every questions reader already consumes. Ids come from the stem's
/// `q-<8 hex>`; a stem that names no such id drops (the healthy projection
/// still covers it).
fn parse_questions_index(text: &str) -> Option<crate::needs_overlay::QuestionsFold> {
    let mut items: Vec<crate::needs_overlay::QuestionItem> = Vec::new();
    let mut done = false;
    let mut any = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            done = line.contains("Done");
            continue;
        }
        let Some(row) = line.strip_prefix("- [[") else {
            continue;
        };
        let Some((wiki, _trail)) = row.split_once("]]") else {
            continue;
        };
        let Some((stem, title)) = wiki.split_once('|') else {
            continue;
        };
        let Some(id) = stem_id(stem) else {
            continue;
        };
        let (state, settled) = if done {
            ("answered", true)
        } else {
            ("open", false)
        };
        let kind = row
            .split('\u{b7}')
            .nth(1)
            .map(str::trim)
            .unwrap_or("question");
        let kind = if kind == "pin" { "pin" } else { "question" };
        let created_at = stem_date(stem);
        items.push(crate::needs_overlay::QuestionItem {
            id: id.into(),
            kind: kind.into(),
            title: title.trim().into(),
            state: state.into(),
            ready: true,
            missing: vec![],
            created_at: created_at.into(),
            options: vec![],
            settled,
            ..Default::default()
        });
        any = true;
    }
    if !any {
        return None;
    }
    Some(crate::needs_overlay::QuestionsFold {
        items,
        ..Default::default()
    })
}

/// The stem's question id: the `q` segment followed by an 8-hex segment.
fn stem_id(stem: &str) -> Option<String> {
    let segments: Vec<&str> = stem.split('-').collect();
    let i = segments.iter().position(|s| *s == "q")?;
    let hex = segments.get(i + 1)?;
    if hex.len() == 8 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("q-{hex}"))
    } else {
        None
    }
}

/// Day precision from the stem's `YYYYMMDD` prefix; empty when absent.
fn stem_date(stem: &str) -> String {
    let Some(head) = stem.get(..8) else {
        return String::new();
    };
    if head.chars().all(|c| c.is_ascii_digit()) {
        let (y, rest) = head.split_at(4);
        let (m, d) = rest.split_at(2);
        format!("{y}-{m}-{d}T00:00:00Z")
    } else {
        String::new()
    }
}

/// Kick a queued answer or archive off the UI loop; the result rides `tx`
/// back. `question_acting` was set at enqueue time, so one send at a time.
pub(super) fn kick_action(
    view: &mut View,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<String, String>>,
) {
    let tx = tx.clone();
    if let Some((qid, pick)) = view.question_action.take() {
        tokio::spawn(async move {
            let _ = tx.send(crate::needs_overlay::answer(&qid, pick).await);
        });
    } else if let Some(ids) = view.question_archive.take() {
        tokio::spawn(async move {
            let _ = tx.send(crate::needs_overlay::archive(ids).await);
        });
    } else if std::mem::take(&mut view.question_clear_settled) {
        tokio::spawn(async move {
            let _ = tx.send(crate::needs_overlay::clear_settled().await);
        });
    }
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
    let open_ids: Vec<String> = view
        .questions_merged()
        .map(|fold| {
            fold.items
                .iter()
                .filter(|q| q.state == "open")
                .map(|q| q.id.clone())
                .collect()
        })
        .unwrap_or_default();
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
                0x15 => {
                    buf.clear();
                }
                0x20..=0x7e => buf.push(k as char),
                _ => {}
            }
            continue;
        }
        let armed = std::mem::take(&mut d.archive_armed);
        match k {
            b'x' | b'X' => {
                let ids: Vec<String> = if k == b'x' {
                    Some(d.item())
                        .filter(|q| q.state == "open")
                        .map(|q| q.id.clone())
                        .into_iter()
                        .collect()
                } else {
                    open_ids.clone()
                };
                if ids.is_empty() {
                    d.notice = Some("no open question to archive".into());
                } else if k == b'X' && !armed {
                    d.archive_armed = true;
                    d.notice = Some(format!(
                        "press X again to archive {} open questions",
                        ids.len()
                    ));
                } else if !view.question_acting {
                    view.question_archive = Some(ids);
                    view.question_acting = true;
                }
            }
            b'a' => {
                let (open, pin) = (d.item().state == "open", d.item().kind == "pin");
                if !open {
                    d.notice = Some("this question is answered - read only".into());
                } else if pin {
                    d.notice = Some("a pin has no delegate".into());
                } else {
                    d.delegate = true;
                    d.sel = None;
                }
            }
            b'j' => {
                if d.item().options.is_empty() {
                    // No options to select: j scrolls the page.
                    let lines = page_lines(
                        d,
                        page_width(view.term.1 as usize),
                        crate::digest_overlay::now_secs(),
                    )
                    .len();
                    d.scroll = (d.scroll + 1).min(lines.saturating_sub(1));
                } else {
                    select_neighbor(d, 1);
                }
            }
            b'k' => {
                if d.item().options.is_empty() {
                    d.scroll = d.scroll.saturating_sub(1);
                } else {
                    select_neighbor(d, -1);
                }
            }
            // Ctrl+d / Ctrl+u: half a page down/up. The scroll gesture for
            // option questions, where j/k are the selection.
            0x04 | 0x15 => {
                let lines = page_lines(
                    d,
                    page_width(view.term.1 as usize),
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
            b'0'..=b'9' => {
                let n = (k - b'0') as u32;
                let opts = &d.item().options;
                let valid = n == 0 && !opts.is_empty() || opts.iter().any(|o| o.n == n);
                if valid {
                    d.sel = Some(n);
                    d.delegate = false;
                }
            }
            b'n' => {
                let saved = d.notes.take().unwrap_or_default();
                d.free = Some(saved);
            }
            b'\r' | b'\n' => match submit(d) {
                Submit::Queued((id, pick)) => {
                    if !view.question_acting {
                        view.question_action = Some((id, pick));
                        view.question_acting = true;
                    }
                }
                Submit::Notice(msg) => d.notice = Some(msg.to_string()),
                Submit::OpenFree => d.free = Some(String::new()),
            },
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
    if item.state != "open" || item.settled {
        return Submit::Notice("this question is answered - read only");
    }
    if item.kind == "pin" {
        return Submit::Queued((item.id, crate::needs_overlay::AnswerPick::Done));
    }
    if d.delegate {
        return Submit::Queued((item.id, crate::needs_overlay::AnswerPick::Delegate));
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
    /// Open the full questions view on one question by id (the page holds
    /// focus).
    pub(super) fn open_detail_on(&mut self, id: &str) {
        let empty = crate::needs_overlay::QuestionsFold::default();
        let merged = self.questions_merged();
        let fold = merged.as_ref().unwrap_or(&empty);
        self.question_detail = Detail::open(fold, id);
    }

    pub(super) fn list_selector(&self) -> Option<usize> {
        self.selector
    }

    /// Apply the latest questions projection. An Err remains visible in the
    /// bell panel instead of becoming an empty list.
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
                // The last good fold stays: an unread board never renders 0,
                // and a timed-out read never blanks the cached one.
                self.questions_degraded = true;
                self.questions_degraded_reason = Some(reason);
            }
        }
        bell::clamp_selection(self);
    }

    /// The bell's visible question fold: the index seeds the list, the full
    /// projection enriches it when healthy. A degraded projection yields to
    /// the index on every id both hold, so a timed-out read never outvotes
    /// the fresh list; ids only one side holds append.
    pub(super) fn questions_merged(&self) -> Option<crate::needs_overlay::QuestionsFold> {
        let index = self.questions_index.as_ref();
        let fold = self.questions_fold.as_ref();
        let items = match (index, fold) {
            (None, None) => return None,
            (Some(index), None) => index.items.clone(),
            (None, Some(fold)) => fold.items.clone(),
            (Some(index), Some(fold)) => {
                let healthy = !self.questions_degraded;
                let primary = if healthy { fold } else { index };
                let secondary = if healthy { index } else { fold };
                let mut items: Vec<_> = primary.items.clone();
                let held: std::collections::HashSet<&str> =
                    primary.items.iter().map(|q| q.id.as_str()).collect();
                for q in &secondary.items {
                    if !held.contains(q.id.as_str()) {
                        items.push(q.clone());
                    }
                }
                items
            }
        };
        Some(crate::needs_overlay::QuestionsFold {
            items,
            ..Default::default()
        })
    }
    /// Apply the index fold: the fast seed the bell renders before the
    /// projection answers.
    pub(super) fn apply_questions_index(&mut self, fold: crate::needs_overlay::QuestionsFold) {
        self.questions_index = Some(fold);
        bell::clamp_selection(self);
    }

    /// Apply a finished question answer: success re-folds so the row leaves
    /// the queue on the next fold and closes the view; a failure shows the
    /// door's own line and keeps the view open on it.
    pub(super) fn apply_question_action_result(&mut self, result: Result<String, String>) {
        self.question_acting = false;
        match result {
            Ok(receipt) => {
                self.needs_want = true;
                self.questions_kick_at = None;
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
    use crate::needs_overlay::{QuestionItem, QuestionOption, QuestionsFold};

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
        let d = Detail::open(&fold_with(vec![q.clone()]), "q-1").unwrap();
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
        let d = Detail::open(&fold_with(vec![q.clone()]), "q-2").unwrap();
        let page = question_page(&q, &d, 0);
        assert!(page.contains("NOT RECORDED"));

        let q = item("q-1", true);
        let mut d = Detail::open(&fold_with(vec![q.clone()]), "q-1").unwrap();
        d.sel = Some(2);
        let page = question_page(&q, &d, 0);
        assert!(page.contains("2. wide  [selected]"));
        d.sel = Some(0);
        let page = question_page(&q, &d, 0);
        assert!(page.contains("0. none of these  [selected]"));
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
        // `a` on a pin refuses; there is nothing to decide.
        v.question_action = None;
        v.question_acting = false;
        detail_keys(&mut v, b"a", &mut Vec::new()).await.unwrap();
        let d = v.question_detail.as_ref().unwrap();
        assert_eq!(d.notice.as_deref(), Some("a pin has no delegate"));

        // `a` then Enter hands the question to the team.
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-1", true)]));
        v.open_detail_on("q-1");
        detail_keys(&mut v, b"a\r", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_action,
            Some(("q-1".into(), crate::needs_overlay::AnswerPick::Delegate))
        );

        // X arms archive-all on the first press and sends every open id on
        // the second; any other key between disarms.
        let mut v = view_with_agents(vec![]);
        let mut done = item("q-d", true);
        done.state = "answered".into();
        v.questions_fold = Some(fold_with(vec![
            item("q-a", true),
            item("q-b", true),
            item("q-c", false),
            done,
        ]));
        v.open_detail_on("q-a");
        detail_keys(&mut v, b"X", &mut Vec::new()).await.unwrap();
        assert!(v.question_archive.is_none(), "one X archives nothing");
        assert_eq!(
            v.question_detail.as_ref().unwrap().notice.as_deref(),
            Some("press X again to archive 3 open questions")
        );
        detail_keys(&mut v, b"kX", &mut Vec::new()).await.unwrap();
        assert!(v.question_archive.is_none(), "a key between disarms");
        detail_keys(&mut v, b"X", &mut Vec::new()).await.unwrap();
        assert_eq!(
            v.question_archive,
            Some(vec!["q-a".into(), "q-b".into(), "q-c".into()])
        );
        // x archives the opened question alone.
        v.question_archive = None;
        v.question_acting = false;
        detail_keys(&mut v, b"x", &mut Vec::new()).await.unwrap();
        assert_eq!(v.question_archive, Some(vec!["q-a".into()]));
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

        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-a", true)]));
        v.open_detail_on("q-a");
        detail_keys(&mut v, b"\x1b", &mut Vec::new()).await.unwrap();
        // The lone Esc rides the carry until the next chunk proves it bare.
        detail_keys(&mut v, b"", &mut Vec::new()).await.unwrap();
        assert!(v.question_detail.is_none());
    }

    #[tokio::test]
    async fn draw_detail_rows() {
        let mut v = view_with_agents(vec![]);
        v.questions_fold = Some(fold_with(vec![item("q-a", true)]));
        v.open_detail_on("q-a");
        let (rows, cols) = (30usize, 120usize);
        v.term = (rows as u16, cols as u16);
        let mut cells = vec![Cell::default(); rows * cols];
        assert!(draw_detail(
            &v,
            &mut cells,
            (rows, cols),
            (0, 0),
            (rows, cols)
        ));
        // The page owns the full width: the opened question's title renders
        // on the page, no list pane beside it.
        let row1: String = cells[cols..2 * cols].iter().map(|c| c.c).collect();
        assert!(row1.contains("title for q-a"), "the page renders: {row1}");

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
        v.apply_questions_fold(Ok(fold_with(vec![item("q-a", true)])));
        v.apply_questions_fold(Err("events.jsonl: permission denied".into()));
        assert!(v.questions_degraded);
        assert_eq!(
            v.questions_degraded_reason.as_deref(),
            Some("events.jsonl: permission denied")
        );
        assert_eq!(
            v.questions_merged().unwrap().items.len(),
            1,
            "a failed read keeps the last good fold, never a 0 board"
        );
        v.apply_questions_fold(Ok(fold_with(vec![item("q-a", true)])));
        assert!(!v.questions_degraded);
        assert_eq!(v.questions_degraded_reason, None);
    }

    #[test]
    fn questions_index_and_merge_rows() {
        let text = "---\nfno_generated: questions-index\n---\n\n## Open (2)\n\n- [[20261005-q-aaaaaaaa-pin-kind-q-aaaa|pin title]] \u{b7} pin \u{b7} quill\n- [[20261005-q-bbbbbbbb-the-other-one-x-97db|other title]] \u{b7} question \u{b7} blocks x-97db \u{b7}\n\n## Done\n\n- [[q-cccccccc|closed title]] \u{b7} closed 2026-10-05 \u{b7} node-closed\n- [[ask-deadbeef|an ask]] \u{b7} answered 2026-10-04 \u{b7} PR\n";
        let fold = parse_questions_index(text).unwrap();
        assert_eq!(fold.items.len(), 3, "ask- pages stay out of the fold");
        let pin = &fold.items[0];
        assert_eq!(pin.id, "q-aaaaaaaa");
        assert_eq!(pin.kind, "pin");
        assert_eq!(pin.state, "open");
        assert!(!pin.settled);
        assert_eq!(pin.created_at, "2026-10-05T00:00:00Z");
        let done = &fold.items[2];
        assert!(done.settled);
        assert_eq!(done.state, "answered");

        // A healthy projection wins shared ids and keeps its richer item;
        // ids only one side holds survive either way.
        let mut v = view_with_agents(vec![]);
        v.apply_questions_index(fold);
        let mut rich = item("q-aaaaaaaa", true);
        rich.title = "rich".into();
        v.apply_questions_fold(Ok(fold_with(vec![rich, item("q-ffff0000", true)])));
        let merged = v.questions_merged().unwrap();
        let by_id = |id: &str| {
            merged
                .items
                .iter()
                .find(|q| q.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
                .clone()
        };
        assert_eq!(by_id("q-aaaaaaaa").title, "rich", "projection enriches");
        assert_eq!(
            by_id("q-bbbbbbbb").title,
            "other title",
            "index-only ids survive"
        );
        assert!(!by_id("q-ffff0000").settled, "projection-only ids survive");

        // A degraded projection yields to the index on shared ids: the
        // timed-out read never outvotes the fresh list.
        v.questions_degraded = true;
        v.questions_degraded_reason = Some("timed out after 30000ms".into());
        let merged = v.questions_merged().unwrap();
        let pin = merged.items.iter().find(|q| q.id == "q-aaaaaaaa").unwrap();
        assert_eq!(pin.title, "pin title", "the index wins when degraded");
    }
}
