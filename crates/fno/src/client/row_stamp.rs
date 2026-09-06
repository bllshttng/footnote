//! Row-scoped outcome stamps for the sideline (x-f191), plus the tab-bar
//! notice channel they ride (the notice text builders and the overlay
//! paint). The notice stays the full-text channel (x-0175: every error
//! surface renders in the UI); the stamp is the placement fix: a row-scoped
//! action's outcome renders AT the row the operator acted on, not only the
//! tab bar's opposite corner. Extracted from client.rs under the file
//! budget.

use std::time::{Duration, Instant};

use super::{
    cell_flags, glyph_cols, AgentNoPaneReason, AgentRow, Cell, Color, ConfirmKind, DisplayRow, View,
};

/// Transient notice lifetime on the tab bar.
pub(super) const NOTICE_TTL: Duration = Duration::from_secs(3);

/// (x-f191) How long a row-scoped outcome stamp stays on its sideline row.
/// Outlives NOTICE_TTL on purpose: the tab-bar notice expires first, and the
/// stamp at the row is what the operator falls back to finding.
const ROW_STAMP_TTL: Duration = Duration::from_secs(8);
/// How long an armed row stamp waits for the action's outcome notice. Must
/// exceed the longest row action: stop can fall through to rm, two bounded
/// subprocesses of 20s each.
const ROW_STAMP_ARM_TTL: Duration = Duration::from_secs(60);
/// The substrings that mark an outcome notice a FAILURE. Positive success is
/// everything else that resolves the arm - a stamp on an unclassified
/// outcome reads as success, which is the safe polarity: the row is still
/// there to be pressed again either way.
const ROW_FAILURE_MARKS: [&str; 8] = [
    ": timed out",
    ": failed",
    ": unavailable",
    "no such agent",
    "is still live",
    "is ambiguous",
    "is external",
    "no longer a live external row",
];

/// (x-f191) A row-scoped action outcome, stamped ON the sideline row it
/// names. The tab-bar notice stays the full-text channel (x-0175); the stamp
/// is the placement fix: the outcome renders at the row the operator acted
/// on, not the far corner of the screen.
pub(super) struct RowStamp {
    name: String,
    text: String,
    failure: bool,
    expires: Instant,
}

/// The armed half: an action dispatched at a named row, waiting for the
/// notice that resolves it. At most one at a time - a second arm replaces
/// the first, whose outcome then lands in the tab bar only.
pub(super) struct RowArm {
    name: String,
    expires: Instant,
}

/// (x-f191) Whether a notice names `row_name` tightly enough to be its
/// verdict: the name must sit at a token boundary, and the notice must not
/// be an in-flight progress line (those end in `…` and never resolve the
/// arm - the external verbs emit "stopping X…" long before the verdict).
fn notice_names_row(text: &str, row_name: &str) -> bool {
    if text.ends_with('…') {
        return false;
    }
    let Some(i) = text.find(row_name) else {
        return false;
    };
    let before = text[..i].chars().next_back();
    let after = text[i + row_name.len()..].chars().next();
    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
}

impl View {
    pub(super) fn set_notice(&mut self, text: String) {
        self.notice = Some((text, Instant::now() + NOTICE_TTL));
    }

    /// (x-f191) Arm the row stamp for a row-scoped confirm commit: the next
    /// notice naming this row renders beside the row, not only in the tab
    /// bar. No-op for bulk and non-row confirms (reap, squad, close-tab).
    pub(super) fn arm_row_stamp(&mut self, action: &ConfirmKind) {
        let name = match action {
            ConfirmKind::StopAgent { name, .. }
            | ConfirmKind::RemoveAgent { name, .. }
            | ConfirmKind::StopExternal { name, .. }
            | ConfirmKind::RemoveExternal { name, .. } => Some(name.clone()),
            _ => None,
        };
        if let Some(name) = name {
            self.row_arm = Some(RowArm {
                name,
                expires: Instant::now() + ROW_STAMP_ARM_TTL,
            });
        }
    }

    /// (x-f191) Resolve an armed row stamp against an incoming notice: a
    /// notice naming the armed row stamps that row (success or failure) and
    /// disarms. An unrelated or late notice leaves the arm alone.
    pub(super) fn resolve_row_stamp(&mut self, text: &str) {
        let now = Instant::now();
        let Some(arm) = self.row_arm.as_ref() else {
            return;
        };
        if now >= arm.expires {
            self.row_arm = None;
            return;
        }
        let name = arm.name.clone();
        if !notice_names_row(text, &name) {
            return;
        }
        let failure = ROW_FAILURE_MARKS.iter().any(|m| text.contains(m));
        self.row_arm = None;
        self.row_stamp = Some(RowStamp {
            name,
            text: text.to_string(),
            failure,
            expires: now + ROW_STAMP_TTL,
        });
    }

    /// The live stamp for the row being painted, if it names this row.
    /// `None` past expiry, so the stamp self-clears without a timer.
    pub(super) fn row_stamp_for(&self, drow: &DisplayRow) -> Option<(bool, &str)> {
        let stamp = self.row_stamp.as_ref()?;
        if Instant::now() >= stamp.expires {
            return None;
        }
        match drow {
            DisplayRow::Agent(a) if a.name == stamp.name => {
                Some((stamp.failure, stamp.text.as_str()))
            }
            _ => None,
        }
    }

    /// (x-f191 scope a+c) A row-scoped commit keeps the sideline open and
    /// the selection on the acted row: the operator opened the sideline to
    /// act, and nothing about the action says they are done. Resolve by row
    /// identity so the cursor follows the row through its state flip; a row
    /// the action removed falls back to the slot it was armed from, clamped -
    /// the neighbour, never a reset to nothing.
    pub(super) fn reanchor_after_row_commit(&mut self, name: Option<&str>) {
        let slot = self.row_slot.take();
        let Some(name) = name else {
            return;
        };
        let rows = self.display_rows();
        let idx = rows
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == name))
            .or(slot);
        if let Some(i) = idx {
            let last = rows.len().saturating_sub(1);
            self.selector = Some(i.min(last));
        }
    }

    /// The transient notice, right-aligned, clipped to the strip.
    ///
    /// Clipping ELLIPSIZES. A silently cut notice reads as a whole sentence, so
    /// on a 40-column strip "…is not TOML, keys are on defaults" became a path
    /// fragment that looked like the entire message. Write notices meaning-first
    /// for the same reason: whatever the strip cannot hold is what a narrow
    /// terminal loses.
    pub(super) fn notice_overlay(&self, cols: usize) -> Option<(usize, String)> {
        let (full, _) = self.notice.as_ref()?;
        let room = cols.saturating_sub(1);
        let text: String = if full.chars().count() > room {
            full.chars()
                .take(room.saturating_sub(1))
                .chain(std::iter::once('…'))
                .collect()
        } else {
            full.clone()
        };
        let start = cols.saturating_sub(text.chars().count() + 1);
        Some((start, text))
    }
}

/// The notice text for a paneless row: what the operator learns when a row
/// has no pane here. Lives with the notice channel it feeds.
pub(super) fn no_pane_notice(a: &AgentRow) -> String {
    match a.no_pane_reason {
        Some(AgentNoPaneReason::LivePaneless) => format!(
            "fno agents peek {} --follow — worker {} is live but has no pane; resume refused because it would create a second writer",
            a.name, a.name
        ),
        Some(AgentNoPaneReason::BackendNotLive) => format!(
            "worker {} has no pane here: registry backend is not live; inspect its state before resuming",
            a.name
        ),
        Some(AgentNoPaneReason::LivenessUnmeasured) => format!(
            "worker {} has no pane here: liveness reading is absent (neither confirmed dead nor confirmed live); run fno agents peek {} to see before resuming",
            a.name, a.name
        ),
        Some(AgentNoPaneReason::MissingHarness) => {
            format!("worker {} has no pane here: no harness recorded", a.name)
        }
        Some(AgentNoPaneReason::MissingSessionId) => format!(
            "worker {} has no pane here: supported harness has no session id",
            a.name
        ),
        Some(AgentNoPaneReason::UnsupportedHarness) => {
            format!("worker {} has no pane here: unsupported harness", a.name)
        }
        None => "agent has no pane here".into(),
    }
}

/// Paint the tab-bar notice overlay: transient, right-aligned, INVERSE
/// (paired with the BEL the event handler already sounded).
pub(super) fn paint_notice_overlay(
    cells: &mut [Cell],
    cols: usize,
    overlay: Option<(usize, String)>,
) {
    if let Some((start, text)) = overlay {
        for (i, ch) in text.chars().enumerate() {
            let idx = start + i;
            if idx < cols {
                cells[idx] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags: cell_flags::INVERSE,
                };
            }
        }
    }
}

/// Paint a row-scoped outcome stamp: right-aligned INVERSE on the row, the
/// same grammar as the tab-bar notice, so the outcome of the action the
/// operator just confirmed renders AT the row they acted on instead of the
/// opposite corner. Skipped on the pinned top row, whose right edge the
/// density button owns.
pub(super) fn paint_row_stamp(
    cells: &mut [Cell],
    r: usize,
    cols: usize,
    text_w: usize,
    stamp: Option<(bool, &str)>,
) {
    if r == 0 {
        return;
    }
    let Some((failure, stamp_text)) = stamp else {
        return;
    };
    let mark = if failure { "✗ " } else { "✓ " };
    // (x-f191 review) Ellipsize to leave the row's leading identity cells: a
    // stamp that swallows the name is a placement fix that eats its own target.
    let cap = text_w.saturating_sub(4);
    let full = format!("{mark}{stamp_text}");
    let mut stamp: Vec<(char, usize)> = Vec::new();
    let mut width = 0usize;
    let mut truncated = false;
    for ch in full.chars() {
        let w = glyph_cols(ch);
        if width + w > cap {
            truncated = true;
            break;
        }
        width += w;
        stamp.push((ch, w));
    }
    if truncated && width < cap {
        stamp.push(('…', 1));
        width += 1;
    }
    let mut start = text_w.saturating_sub(width);
    for (ch, w) in stamp {
        if start + w > text_w {
            break;
        }
        cells[r * cols + start] = Cell {
            c: ch,
            fg: Color::Default,
            bg: Color::Default,
            flags: cell_flags::INVERSE,
        };
        if w == 2 {
            cells[r * cols + start + 1] = Cell {
                c: ' ',
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::INVERSE | cell_flags::WIDE_SPACER,
            };
        }
        start += w;
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        agent_row_at, focus_agent, lifecycle_row, two_pane_view, view_with_agents,
    };
    use super::super::{confirm_keys, selector_keys};
    use super::*;

    fn corpse_row() -> AgentRow {
        let mut a = focus_agent(0);
        a.pane_id = None;
        a.name = "corpse".into();
        a
    }

    #[test]
    fn row_stamp_resolves_from_the_outcome_notice() {
        // An unrelated notice does not resolve the arm; the outcome notice
        // stamps the named row with its failure.
        let mut view = two_pane_view();
        view.arm_row_stamp(&ConfirmKind::StopAgent {
            sid: None,
            name: "corpse".into(),
            pane_id: None,
        });
        assert!(view.row_arm.is_some(), "a row-scoped commit arms the stamp");
        view.resolve_row_stamp("reaping exited agents…");
        assert!(
            view.row_stamp.is_none(),
            "an unrelated notice leaves the arm"
        );
        view.resolve_row_stamp("stop corpse: failed: claude stop corpse failed: agent not found");
        let stamp = view.row_stamp.as_ref().expect("the outcome stamps the row");
        assert!(stamp.failure, "a failure reason reads as a failure");
        assert!(view.row_arm.is_none(), "the arm disarms on resolution");
        let row = corpse_row();
        let (failure, text) = view
            .row_stamp_for(&DisplayRow::Agent(&row))
            .expect("the named row carries the stamp");
        assert!(failure);
        assert!(text.contains("failed"), "the row carries the reason");
    }

    #[test]
    fn row_stamp_success_reads_as_success() {
        let mut view = two_pane_view();
        view.arm_row_stamp(&ConfirmKind::RemoveAgent {
            sid: None,
            name: "corpse".into(),
            pane_id: None,
        });
        view.resolve_row_stamp("removed corpse");
        let row = corpse_row();
        let (failure, _) = view
            .row_stamp_for(&DisplayRow::Agent(&row))
            .expect("success stamps the named row too");
        assert!(!failure, "a clean outcome is not a failure stamp");
    }

    #[test]
    fn row_stamp_arm_expires_without_a_notice() {
        // A lost outcome must not stamp some LATER notice onto the row.
        let mut view = two_pane_view();
        view.arm_row_stamp(&ConfirmKind::StopAgent {
            sid: None,
            name: "corpse".into(),
            pane_id: None,
        });
        view.row_arm.as_mut().unwrap().expires = Instant::now();
        view.resolve_row_stamp("some later notice naming corpse");
        assert!(view.row_arm.is_none(), "an expired arm disarms");
        assert!(view.row_stamp.is_none(), "an expired arm stamps nothing");
    }

    #[test]
    fn row_stamp_skips_progress_lines_and_resolves_on_the_verdict() {
        // (x-f191 review) The external verbs emit "stopping X…" long before
        // the verdict; the arm must survive it and resolve on the outcome.
        let mut view = two_pane_view();
        view.arm_row_stamp(&ConfirmKind::StopExternal {
            attach_id: "abcd1234".into(),
            name: "corpse".into(),
        });
        view.resolve_row_stamp("stopping corpse…");
        assert!(view.row_stamp.is_none(), "a progress line never stamps");
        assert!(view.row_arm.is_some(), "the arm survives a progress line");
        // A name embedded in a larger word is not the row's verdict either.
        view.resolve_row_stamp("the corpseflower squad was renamed");
        assert!(view.row_stamp.is_none(), "a substring hit is not the row");
        view.resolve_row_stamp("stop corpse: timed out");
        let row = corpse_row();
        let (failure, _) = view
            .row_stamp_for(&DisplayRow::Agent(&row))
            .expect("the verdict stamps the row");
        assert!(failure);
    }

    #[test]
    fn sideline_paints_the_failure_stamp_on_the_row() {
        // The placement fix itself: the outcome renders at the row the
        // operator acted on, not only the tab bar's opposite corner.
        let mut view = two_pane_view();
        view.layout.agents.push(corpse_row());
        view.row_stamp = Some(RowStamp {
            name: "corpse".into(),
            text: "stop corpse: timed out".into(),
            failure: true,
            expires: Instant::now() + ROW_STAMP_TTL,
        });
        let frame = view.compose();
        let cols = frame.cols as usize;
        let agent_row_cells = |r: usize| frame.cells[r * cols..r * cols + cols].to_vec();
        let has_stamp = |cells: &[Cell]| {
            cells
                .iter()
                .any(|c| c.c == '✗' && c.flags & cell_flags::INVERSE == cell_flags::INVERSE)
        };
        assert!(
            has_stamp(&agent_row_cells(1)),
            "the agent row carries the ✗ stamp"
        );
        assert!(
            !has_stamp(&agent_row_cells(0)),
            "the pinned header row never carries the stamp"
        );
    }

    #[test]
    fn sideline_stamp_ellipsizes_and_keeps_the_row_identity() {
        // (x-f191 review) A stamp longer than the row must not swallow the
        // name: it ellipsizes and leaves the row's leading cells alone.
        let mut view = two_pane_view();
        view.layout.agents.push(corpse_row());
        view.row_stamp = Some(RowStamp {
            name: "corpse".into(),
            text: "stop corpse: failed: a very long daemon reason that cannot fit any sideline"
                .into(),
            failure: true,
            expires: Instant::now() + ROW_STAMP_TTL,
        });
        let frame = view.compose();
        let cols = frame.cols as usize;
        let row = &frame.cells[cols..cols * 2];
        assert!(row.iter().any(|c| c.c == '✗'), "the stamp still renders");
        let lead = row[0];
        assert!(
            lead.c != '✗' && lead.c != '…',
            "the row's leading identity cell survives: got {:?}",
            lead.c
        );
    }

    #[test]
    fn notice_names_row_requires_a_token_boundary() {
        // The name must stand alone: a substring of a longer word is not
        // the row's verdict.
        assert!(notice_names_row("removed corpse", "corpse"));
        assert!(notice_names_row("stop corpse: failed: gone", "corpse"));
        assert!(!notice_names_row(
            "the corpseflower squad was renamed",
            "corpse"
        ));
    }

    #[tokio::test]
    async fn selector_x_commit_reanchors_the_selector_on_the_row() {
        // (x-f191 scope a+c) The sideline comes back after the commit: the
        // selection resolves onto the acted row by identity, so the operator
        // is never thrown out to re-find a greyed row.
        let mut v = view_with_agents(vec![lifecycle_row("worker-a", false, false)]);
        v.set_squad_view(1, crate::view_store::SectionView::Expanded);
        let idx = agent_row_at(&v, |a| a.name == "worker-a");
        v.selector = Some(idx);
        let mut buf: Vec<u8> = Vec::new();
        selector_keys(&mut v, b"x", &mut buf).await.unwrap();
        assert_eq!(v.selector, None, "the confirm closes the selector at arm");
        confirm_keys(&mut v, b"\r", &mut buf).await.unwrap();
        assert_eq!(
            v.selector,
            Some(idx),
            "the commit re-anchors the selector on the acted row"
        );
    }
}
