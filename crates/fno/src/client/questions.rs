//! The questions block and the full-context detail overlay, split out of
//! `client.rs` because that file is over the line budget and shrink-only.
//! The block pins above the court block in the sideline and shows only what
//! is open; the overlay shows one question's full context and answers it
//! through [`crate::needs_overlay::answer`]. A child module of `client`, so
//! `View`'s private fields stay reachable without widening them.

use super::*;

/// The open questions in the fold, projection order.
pub(super) fn open_items(
    fold: &crate::needs_overlay::QuestionsFold,
) -> Vec<crate::needs_overlay::QuestionItem> {
    fold.items
        .iter()
        .filter(|q| q.state == "open")
        .cloned()
        .collect()
}

/// The block's lines, and the question id behind each body row, for the
/// sideline paint and the click hit test to share. The header counts open
/// and ready; up to five rows follow, then `+N more`. An answered section
/// (fresh answers with their delivery rung) renders DIM under the open rows.
/// Zero rows when nothing is open: the block yields entirely.
pub(super) fn block_layout(
    fold: &crate::needs_overlay::QuestionsFold,
    now: u64,
) -> (Vec<(String, bool)>, Vec<String>) {
    let open = open_items(fold);
    if open.is_empty() {
        return (vec![], vec![]);
    }
    let ready = open.iter().filter(|q| q.ready).count();
    let mut lines: Vec<(String, bool)> = vec![(
        format!(" questions {} · {} ready", open.len(), ready),
        false,
    )];
    let mut ids: Vec<String> = Vec::new();
    let shown = open.len().min(5);
    for q in open.iter().take(shown) {
        let target = q.blocks.first().map(String::as_str).unwrap_or("none");
        let node = q.node.as_deref().unwrap_or(target);
        let asker = q.asker.as_ref().map(|a| a.handle.as_str()).unwrap_or("?");
        let age = age_short(&q.created_at, now);
        lines.push((
            format!(
                " ? {asker} -> {node} {age}{}",
                if q.ready { "" } else { "  not ready" }
            ),
            !q.ready,
        ));
        ids.push(q.id.clone());
    }
    if open.len() > shown {
        lines.push((format!(" +{} more", open.len() - shown), false));
    }
    for a in &fold.answered {
        lines.push((answered_line(a), true));
    }
    (lines, ids)
}

/// The block's reserved rows: its line count, but ZERO when the terminal
/// cannot hold it beside at least one agent row - the block yields, the
/// rows never do (the court rule). `None` when the block reserves nothing.
pub(super) fn block_rows(
    view: &View,
    term_rows: usize,
) -> Option<(usize, Vec<(String, bool)>, Vec<String>)> {
    let now = crate::digest_overlay::now_secs();
    let fold = view.questions_fold.as_ref()?;
    let (lines, ids) = block_layout(fold, now);
    if lines.is_empty() {
        return None;
    }
    let court = view.court_block_layout(term_rows).0;
    let chrome = view.bottom_row_is_chrome() as usize;
    let available = term_rows.saturating_sub(court + chrome);
    if available > lines.len() {
        Some((lines.len(), lines, ids))
    } else {
        None
    }
}

/// Paint the block's lines at `start` in the sideline column: the same
/// cell loop the court block paints, with the per-line DIM flag deciding
/// whether a row reads as live or as chrome.
pub(super) fn paint_block(
    lines: Vec<(String, bool)>,
    cells: &mut [Cell],
    start: usize,
    rows: usize,
    cols: usize,
    text_w: usize,
) {
    for (k, (text, dim)) in lines.into_iter().enumerate() {
        let r = start + k;
        if r >= rows {
            break;
        }
        let flags = if dim { cell_flags::DIM } else { 0 };
        let mut col = 0usize;
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

/// The question row a click landed on, by sideline row: `Some(id)`.
pub(super) fn hit_at(view: &View, term_rows: usize, row: u16) -> Option<String> {
    let (n, _lines, ids) = block_rows(view, term_rows)?;
    let court = view.court_block_layout(term_rows).0;
    let start = term_rows.saturating_sub(court + n);
    let r = (row as usize).checked_sub(start)?;
    ids.get(r).cloned()
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
        (None, _) | (_, None) => format!(" ✓ {who} answered, not delivered yet"),
        (Some("mail"), Some("landed")) | (Some("resume"), Some("confirmed")) => {
            format!(" ✓ {who} {}", a.rung.as_deref().unwrap_or(""))
        }
        (Some("crown"), Some("sent")) => format!(" ✓ {who} -> crown"),
        _ => format!(" ✓ {who} undelivered"),
    }
}

/// The detail overlay's state: the open questions snapshot and the cursor.
/// Fields the item lacks render `NOT RECORDED`; a send shows the door's
/// receipt; a refusal shows its line and keeps the overlay open.
pub(super) struct Detail {
    pub(super) items: Vec<crate::needs_overlay::QuestionItem>,
    pub(super) idx: usize,
    pub(super) sel: Option<u32>,
    pub(super) free: Option<String>,
    pub(super) notice: Option<String>,
}

impl Detail {
    /// Open on the question with `id`, else the first open one.
    pub(super) fn open(
        fold: &crate::needs_overlay::QuestionsFold,
        id: Option<&str>,
    ) -> Option<Detail> {
        let items = open_items(fold);
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
            free: None,
            notice: None,
        })
    }

    pub(super) fn item(&self) -> &crate::needs_overlay::QuestionItem {
        &self.items[self.idx]
    }

    /// n/N: the next or previous open question.
    fn advance(&mut self, delta: isize) {
        let n = self.items.len();
        self.idx = (self.idx as isize + delta).rem_euclid(n as isize) as usize;
        self.sel = None;
        self.free = None;
        self.notice = None;
    }
}

/// One wrapped field line, or `NOT RECORDED` when the item lacks it.
fn field_line(label: &str, value: Option<&str>, w: usize) -> Vec<String> {
    let text = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "NOT RECORDED".to_string());
    let wrapped = wrap_words(&text, w);
    let mut out = vec![format!(" {label}: {}", wrapped[0])];
    for rest in &wrapped[1..] {
        out.push(format!("   {rest}"));
    }
    out
}

/// The detail overlay's lines, in the plan's order: title and body; asker
/// facts; node and blocks; why it is asked; why these options; each option;
/// the recommendation; reversible and cost; meanwhile; unknowns.
pub(super) fn detail_lines(
    item: &crate::needs_overlay::QuestionItem,
    d: &Detail,
    w: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let title = if item.title.is_empty() {
        "(no title)"
    } else {
        &item.title
    };
    lines.push(pad_to(format!(" {title}").as_str(), w));
    if let Some(body) = item.body.as_deref() {
        if !body.is_empty() && body != item.title {
            for l in wrap_words(body, w) {
                lines.push(format!("   {l}"));
            }
        }
    }
    let asker = item.asker.as_ref();
    lines.push(pad_to(
        format!(
            " asker: {} · {}{}",
            asker.map(|a| a.handle.as_str()).unwrap_or("?"),
            asker.and_then(|a| a.harness.as_deref()).unwrap_or("?"),
            match asker.and_then(|a| a.reach.as_deref()) {
                Some(r) => format!(" · {r}"),
                None => String::new(),
            }
        )
        .as_str(),
        w,
    ));
    lines.push(pad_to(
        format!(
            " node: {} · blocks: {} · state: {}{}",
            item.node.as_deref().unwrap_or("none"),
            if item.blocks.is_empty() {
                "none".to_string()
            } else {
                item.blocks.join(", ")
            },
            item.state,
            if item.ready {
                String::new()
            } else {
                format!(" · missing: {}", item.missing.join(", "))
            }
        )
        .as_str(),
        w,
    ));
    lines.extend(field_line("why asked", item.blocked_because.as_deref(), w));
    lines.extend(field_line(
        "why these options",
        item.options_rationale.as_deref(),
        w,
    ));
    for o in &item.options {
        let marker = if d.sel == Some(o.n) { "▸" } else { " " };
        lines.push(pad_to(
            format!(
                " {marker} {}. {} -> {}",
                o.n,
                o.text,
                o.next.as_deref().unwrap_or("?")
            )
            .as_str(),
            w,
        ));
        for p in &o.pros {
            lines.push(pad_to(format!("      + {p}").as_str(), w));
        }
        for c in &o.cons {
            lines.push(pad_to(format!("      - {c}").as_str(), w));
        }
    }
    if let Some(r) = &item.recommendation {
        lines.extend(field_line(
            "recommended",
            Some(&format!(
                "option {} · {}{}",
                r.option,
                r.why,
                r.downside
                    .as_deref()
                    .map(|d| format!(" · downside: {d}"))
                    .unwrap_or_default()
            )),
            w,
        ));
    }
    lines.extend(field_line("reversible", item.reversible.as_deref(), w));
    if item.reversible.as_deref() == Some("costly") {
        lines.extend(field_line(
            "cost if wrong",
            item.cost_if_wrong.as_deref(),
            w,
        ));
    }
    lines.extend(field_line("meanwhile", item.meanwhile.as_deref(), w));
    lines.extend(field_line(
        "not thought through",
        item.unknowns.as_deref(),
        w,
    ));
    lines
}

/// Draw the detail overlay over the content viewport. Returns false when no
/// detail is open, so the draw chain's `else if` arm is one call.
pub(super) fn draw_detail(
    view: &View,
    cells: &mut [Cell],
    (rows, cols): (usize, usize),
    origin: (usize, usize),
    dims: (usize, usize),
) -> bool {
    let Some(d) = &view.question_detail else {
        return false;
    };
    let item = d.item();
    let w = (cols.saturating_sub(4)).min(100).max(20);
    let mut lines = detail_lines(item, d, w.saturating_sub(2));
    if let Some(free) = &d.free {
        lines.push(pad_to(
            format!(" answer> {free}▏ (Enter sends, Esc cancels)").as_str(),
            w,
        ));
    }
    let footer = match (&d.notice, &d.free) {
        (Some(n), _) => n.clone(),
        _ if d.free.is_some() => "type the answer".to_string(),
        _ => {
            let has_opts = !item.options.is_empty();
            match (item.kind.as_str(), has_opts) {
                ("pin", _) => "digit/Enter done · n/N next · Esc close".to_string(),
                _ if has_opts => {
                    "digit selects · Enter sends · n/N next · j/k scroll · Esc close".to_string()
                }
                _ => "Enter to answer · n/N next · Esc close".to_string(),
            }
        }
    };
    let chrome = chrome::Chrome::new("question", Anchor::Center).footer(&footer);
    let follow = d.free.is_some().then(|| lines.len().saturating_sub(1));
    draw_lines_overlay(
        cells,
        rows,
        cols,
        origin,
        dims,
        &chrome,
        &lines,
        &view.theme,
        follow,
    );
    true
}

/// The 10 s refresh: at most one projection fold in flight while the
/// sideline is shown. The read runs off the UI loop; the fold rides `tx`
/// back to the run loop.
pub(super) fn maybe_kick(
    view: &mut View,
    tx: &tokio::sync::mpsc::UnboundedSender<Option<crate::needs_overlay::QuestionsFold>>,
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

/// The detail overlay's keys. Free text owns the keyboard while open; digits
/// select an option; Enter sends through the door (a no-option question
/// starts the free-text entry, a pin sends `done`); n/N move between open
/// questions; Esc closes. Consumes every byte - an open modal never leaks a
/// key to a pane (the answer-overlay invariant).
pub(super) async fn detail_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(d) = view.question_detail.as_mut() else {
        return Ok(StdinFlow::Continue);
    };
    for &k in bytes {
        if let Some(buf) = d.free.as_mut() {
            match k {
                b'\r' | b'\n' => {
                    let text = std::mem::take::<String>(buf).trim().to_string();
                    d.free = None;
                    if !text.is_empty() {
                        let id = d.item().id.clone();
                        view.question_action =
                            Some((id, crate::needs_overlay::AnswerPick::Words(text)));
                        view.question_acting = true;
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
            b'n' => d.advance(1),
            b'N' => d.advance(-1),
            b'j' => d.idx = (d.idx + 1).min(d.items.len().saturating_sub(1)),
            b'k' => d.idx = d.idx.saturating_sub(1),
            b'0'..=b'9' => {
                let n = (k - b'0') as u32;
                if d.item().options.iter().any(|o| o.n == n) {
                    d.sel = Some(n);
                }
            }
            b'\r' | b'\n' => {
                let item = d.item().clone();
                let pin = item.kind == "pin";
                let pick = if !item.options.is_empty() {
                    d.sel
                } else if pin {
                    Some(0)
                } else {
                    None
                };
                match pick {
                    Some(n) if pin => {
                        if !view.question_acting {
                            let id = item.id.clone();
                            view.question_action =
                                Some((id, crate::needs_overlay::AnswerPick::Done));
                            view.question_acting = true;
                        }
                    }
                    Some(n) if !view.question_acting => {
                        let id = item.id.clone();
                        view.question_action =
                            Some((id, crate::needs_overlay::AnswerPick::Option(n)));
                        view.question_acting = true;
                    }
                    Some(_) => {}
                    None => {
                        d.free = Some(String::new());
                    }
                }
            }
            0x1b => {
                view.question_detail = None;
                return Ok(StdinFlow::Continue);
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

impl View {
    /// Open the detail overlay on one question by id.
    pub(super) fn open_detail_on(&mut self, id: &str) {
        let empty = crate::needs_overlay::QuestionsFold::default();
        let fold = self.questions_fold.as_ref().unwrap_or(&empty);
        self.question_detail = Detail::open(fold, Some(id));
    }

    /// Apply a landed questions fold: the block always shows the latest
    /// projection, so no generation guard here.
    pub(super) fn apply_questions_fold(
        &mut self,
        fold: Option<crate::needs_overlay::QuestionsFold>,
    ) {
        self.questions_inflight = false;
        match fold {
            Some(f) => {
                self.questions_fold = Some(f);
                self.questions_degraded = false;
            }
            None => self.questions_degraded = true,
        }
    }

    /// Apply a finished question answer: success re-folds so the row leaves
    /// the queue on the next fold and closes the detail (the row leaves on
    /// the refold); a failure shows the door's own line and keeps the detail
    /// open on it.
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
    fn ac5_hp_the_block_header_counts_and_the_rows_cap_at_five() {
        let items: Vec<QuestionItem> = (0..7)
            .map(|i| {
                let mut q = item(&format!("q-{i}"), true);
                q.ready = i < 2;
                q
            })
            .collect();
        let (lines, ids) = block_layout(&fold_with(items), 0);
        assert_eq!(lines[0].0, " questions 7 · 2 ready");
        assert_eq!(ids.len(), 5, "five open rows carry ids");
        assert_eq!(lines[6].0, " +2 more");
        assert_eq!(lines.len(), 7, "header + 5 rows + more");
    }

    #[test]
    fn ac5_edge_an_empty_or_all_answered_fold_reserves_zero_rows() {
        let (lines, ids) = block_layout(&fold_with(vec![]), 0);
        assert!(lines.is_empty() && ids.is_empty());
        // An answered-only fold still yields: no open rows, no block.
        let mut answered_only = fold_with(vec![]);
        answered_only.answered = vec![AnsweredItem {
            id: "q-9".into(),
            asker: Some("w1".into()),
            answer: "narrow".into(),
            rung: Some("mail".into()),
            outcome: Some("landed".into()),
            ..Default::default()
        }];
        let (lines2, _ids2) = block_layout(&answered_only, 0);
        assert!(lines2.is_empty(), "no open questions, no block");
    }

    #[test]
    fn ac6_hp_the_detail_shows_every_recorded_context_field() {
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
        let lines = detail_lines(&q, &d, 120);
        let text = lines.join("\n");
        assert!(text.contains("title for q-1"));
        assert!(text.contains("1. narrow"));
        assert!(text.contains("recommended: option 1 · narrowest"));
        assert!(text.contains("downside: hides a feature"));
        assert!(text.contains("reversible: costly"));
        assert!(text.contains("cost if wrong: the allowance drops"));
        assert!(text.contains("meanwhile: stops"));
        assert!(text.contains("not thought through: net-zero moves"));
        assert!(
            text.contains("missing: unknowns"),
            "not-ready names its gaps"
        );
    }

    #[test]
    fn a_field_the_item_lacks_reads_not_recorded() {
        let mut q = item("q-2", true);
        q.blocked_because = None;
        q.unknowns = None;
        let d = Detail::open(&fold_with(vec![q.clone()]), Some("q-2")).unwrap();
        let text = detail_lines(&q, &d, 120).join("\n");
        assert!(text.contains("why asked: NOT RECORDED"));
        assert!(text.contains("not thought through: NOT RECORDED"));
    }

    #[test]
    fn advance_walks_the_open_questions_and_resets_the_pick() {
        let items = vec![item("q-a", true), item("q-b", true), item("q-c", true)];
        let mut d = Detail::open(&fold_with(items.clone()), None).unwrap();
        assert_eq!(d.item().id, "q-a");
        d.advance(1);
        assert_eq!(d.item().id, "q-b");
        d.sel = Some(2);
        d.advance(-1);
        assert_eq!(d.item().id, "q-a");
        assert_eq!(d.sel, None, "the pick resets on advance");
    }

    #[tokio::test]
    async fn answer_keys_enter_on_question_opens_the_detail_overlay() {
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
        let detail = v.question_detail.expect("the detail opened");
        assert_eq!(detail.item().id, "q-2");
        assert_eq!(v.answers, Some(0), "the overlay stays open");
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn answer_keys_digit_on_a_question_row_opens_the_detail() {
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
        let detail = v.question_detail.expect("the detail opened");
        assert_eq!(detail.item().id, "q-1");
        assert!(
            v.question_action.is_none(),
            "nothing is sent from the digit"
        );
        assert!(
            buf.is_empty(),
            "a question digit never sends a pane keystroke"
        );
    }

    #[tokio::test]
    async fn answer_keys_digit_with_no_matching_question_option_bels() {
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

    #[test]
    fn apply_question_action_result_success_requests_refold() {
        let mut v = view_with_agents(vec![]);
        v.needs_want = false;
        v.apply_question_action_result(Ok("recorded, delivering".into()));
        assert!(!v.question_acting);
        assert!(v.needs_want, "success re-folds so the row leaves on refold");
    }

    #[test]
    fn apply_question_action_result_failure_shows_notice_never_silent() {
        let mut v = view_with_agents(vec![]);
        v.needs_want = false;
        v.apply_question_action_result(Err("failed to close q-1: locked".into()));
        assert!(!v.question_acting);
        assert!(!v.needs_want, "a failure never triggers a re-fold");
        let notice = v.notice.as_ref().expect("failure surfaces a notice");
        assert!(notice.0.contains("failed to close q-1: locked"));
    }
}
