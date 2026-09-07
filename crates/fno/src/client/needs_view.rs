//! The needs-me overlay's projection and render, moved out of client.rs
//! (x-4433 file-budget shrink): the projection is one question - "what needs
//! me, worst first" - so it answers from its own module. The gesture sites,
//! the run-loop arms and the tests all read these items through the re-exports
//! in the parent. The feed overlay (`feed_view.rs`) reuses
//! [`View::join_fold_row`]'s join keys, so a feed row and a sideline row reach
//! a session through one deep-link path.

use super::*;

impl View {
    /// The roster row a fold item joins to: a name / node / session-id match
    /// against a layout row's name or its cwd basename (`cwd_base`, now carried
    /// on every row since x-6851 US3, not only orphans).
    pub(crate) fn join_fold_row(&self, item: &crate::needs_overlay::FoldItem) -> Option<&AgentRow> {
        let keys: Vec<&str> = [
            item.name.as_deref(),
            item.node.as_deref(),
            Some(item.session_id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.layout.agents.iter().find(|a| {
            keys.iter().any(|k| a.name == *k)
                || a.cwd_base.as_deref().is_some_and(|c| keys.contains(&c))
        })
    }

    /// The two-lane overlay projection: MINE first (open before done, file
    /// order preserved within each group - the operator wrote that order),
    /// then the operator-filtered needs queue (already worst-first). Each
    /// lane is capped independently at ten so a noisy lane never crowds the
    /// other out; `*_total` carries the true count for the footer.
    pub(crate) fn needs_projection(&self) -> NeedsProjection {
        let mut mine: Vec<crate::needs_overlay::MineItem> =
            self.mine_fold.clone().unwrap_or_default();
        mine.sort_by_key(|m| m.done);
        let mine_total = mine.len();
        let mine_shown = mine.len().min(MINE_CAP);
        let mut rows: Vec<NeedsOverlayRow> = mine
            .into_iter()
            .take(MINE_CAP)
            .map(NeedsOverlayRow::Mine)
            .collect();

        // Questions lead the NEED section (x-f730 task 2.3): a real operator
        // question, with an asker to answer back to, outranks a bare
        // carveout/claims pile. Ranked by the record's own `rank` (x-7979
        // already orders these); an unranked row sorts last within the group
        // rather than floating to the front on a missing field.
        let mut questions: Vec<crate::needs_overlay::QuestionItem> =
            self.questions_fold.clone().unwrap_or_default();
        questions.sort_by_key(|q| q.rank.unwrap_or(u32::MAX));
        let need = self.needs_operator_queue();
        let need_total = questions.len() + need.len();
        let need_shown = need_total.min(NEEDS_CAP);
        rows.extend(
            questions
                .into_iter()
                .map(NeedsOverlayRow::Question)
                .chain(need.into_iter().map(NeedsOverlayRow::Need))
                .take(NEEDS_CAP),
        );

        NeedsProjection {
            rows,
            mine_shown,
            mine_total,
            need_shown,
            need_total,
        }
    }
}

pub(crate) struct NeedsProjection {
    pub(crate) rows: Vec<NeedsOverlayRow>,
    pub(crate) mine_shown: usize,
    pub(crate) mine_total: usize,
    pub(crate) need_shown: usize,
    pub(crate) need_total: usize,
}

impl NeedsProjection {
    pub(crate) fn selected_line(&self, sel: usize) -> usize {
        if sel < self.mine_shown {
            // instruction + MINE heading
            2 + sel
        } else {
            // instruction + MINE heading/rows/footer + THEY NEED YOU heading
            4 + self.mine_shown + sel.saturating_sub(self.mine_shown)
        }
    }
}

/// Build the needs-me overlay lines (x-feec, two-laned by x-f730): MINE (the
/// operator's own priorities) above THEY NEED YOU (the severity-ranked union
/// + the selected row's answer options), each with its own state footer, on
/// the shared inverse-video chrome. `sel` is pre-clamped by the caller and
/// indexes `projection.rows` (MINE rows first, then need rows) - a `▸` marks
/// the selected row wherever it falls. An answerable row lists its numbered
/// options only when it is the selected NEED row; a focus-only row is tagged.
/// Always renders both headings - an empty NEED lane shows "nothing needs
/// you", so the overlay never opens blank. Layout is pinned 1:1 with
/// [`NeedsProjection::selected_line`]: MINE rows occupy exactly
/// `mine_shown` lines and need rows exactly `need_shown` (or one "nothing
/// needs you" line), with no extra divider row between them.
pub(crate) fn needs_overlay_lines(
    projection: &NeedsProjection,
    sel: usize,
    mine_footer: NeedsFooter,
    need_footer: NeedsFooter,
) -> Vec<String> {
    let mine_rows = &projection.rows[..projection.mine_shown];
    let need_rows = &projection.rows[projection.mine_shown..];

    let mut lines = vec![pad_to(
        " needs me · digit answers · n/N cycle · ⏎ goto · q close",
        ANSWER_OVERLAY_W,
    )];

    lines.push(pad_to(" MINE", ANSWER_OVERLAY_W));
    for (i, row) in mine_rows.iter().enumerate() {
        let NeedsOverlayRow::Mine(item) = row else {
            continue;
        };
        let marker = if i == sel { '▸' } else { ' ' };
        let check = if item.done { '✓' } else { ' ' };
        lines.push(pad_to(
            &format!(" {marker} [{check}] {}", item.text),
            ANSWER_OVERLAY_W,
        ));
    }
    let mine_footer_line = match mine_footer {
        NeedsFooter::Folding => "   folding...".to_string(),
        NeedsFooter::Degraded => "   MINE unavailable".to_string(),
        NeedsFooter::AsOf if projection.mine_total > projection.mine_shown => format!(
            "   {} of {} shown",
            projection.mine_shown, projection.mine_total
        ),
        NeedsFooter::AsOf => String::new(),
    };
    lines.push(pad_to(&mine_footer_line, ANSWER_OVERLAY_W));

    lines.push(pad_to(" THEY NEED YOU", ANSWER_OVERLAY_W));
    if need_rows.is_empty() {
        lines.push(pad_to("   nothing needs you", ANSWER_OVERLAY_W));
    } else {
        for (i, row) in need_rows.iter().enumerate() {
            let idx = projection.mine_shown + i;
            let marker = if idx == sel { '▸' } else { ' ' };
            match row {
                NeedsOverlayRow::Question(q) => {
                    // Render `ask` as the headline (falls back to the prose
                    // question when the asker gave no one-liner); the prose
                    // itself appears only when selected, below.
                    let stale = if q.live == Some(false) { "  STALE" } else { "" };
                    lines.push(pad_to(
                        &format!(
                            " {marker} {} {}{stale}",
                            need_glyph(NeedKind::Question),
                            q.ask.as_deref().unwrap_or(&q.question)
                        ),
                        ANSWER_OVERLAY_W,
                    ));
                }
                NeedsOverlayRow::Need(r) => {
                    let tag = match r.kind {
                        NeedKind::BlockedFocusOnly => "  ⚠ focus",
                        _ => "",
                    };
                    lines.push(pad_to(
                        &format!(
                            " {marker} {} {}  {}{tag}",
                            need_glyph(r.kind),
                            r.name,
                            r.reason
                        ),
                        ANSWER_OVERLAY_W,
                    ));
                }
                NeedsOverlayRow::Mine(_) => {}
            }
        }
        let selected_need = sel
            .checked_sub(projection.mine_shown)
            .and_then(|i| need_rows.get(i));
        match selected_need {
            Some(NeedsOverlayRow::Need(r)) => {
                if let Some(ans) = r.answerable.as_ref() {
                    if !ans.prompt.is_empty() {
                        lines.push(pad_to(
                            &format!("   {}", ans.prompt.replace('\n', " ")),
                            ANSWER_OVERLAY_W,
                        ));
                    }
                    for o in &ans.options {
                        lines.push(pad_to(
                            &format!("     {}. {}", o.idx, o.label),
                            ANSWER_OVERLAY_W,
                        ));
                    }
                }
            }
            Some(NeedsOverlayRow::Question(q)) => {
                // The prose beneath the headline - only when `ask` was used
                // as the headline above; if there was no `ask`, the headline
                // already IS the question and repeating it would be noise.
                if q.ask.is_some() && !q.question.is_empty() {
                    lines.push(pad_to(
                        &format!("   {}", q.question.replace('\n', " ")),
                        ANSWER_OVERLAY_W,
                    ));
                }
                for (i, opt) in q.options.iter().enumerate() {
                    lines.push(pad_to(&format!("     {}. {opt}", i + 1), ANSWER_OVERLAY_W));
                }
                if q.live == Some(false) {
                    lines.push(pad_to(
                        "   the answer is recorded but reaches no session",
                        ANSWER_OVERLAY_W,
                    ));
                }
            }
            _ => {}
        }
    }
    let need_footer_line = match need_footer {
        NeedsFooter::Folding => "   folding events...".to_string(),
        NeedsFooter::Degraded => "   events fold unavailable - live badges only".to_string(),
        NeedsFooter::AsOf if projection.need_total > projection.need_shown => format!(
            "   {} of {} shown",
            projection.need_shown, projection.need_total
        ),
        NeedsFooter::AsOf => "   as of now".to_string(),
    };
    lines.push(pad_to(&need_footer_line, ANSWER_OVERLAY_W));
    lines
}
