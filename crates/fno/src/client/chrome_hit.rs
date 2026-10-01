use super::*;

impl View {
    /// Map a left-click on chrome (the tab bar or the sideline) to what it does:
    /// switch tab/squad, focus an agent's pane, open a new tab, or a local hint
    /// for a row that isn't directly actionable (a work-only agent, a card).
    /// `None` = not a chrome cell (the caller falls through to [`hit_test`]), so
    /// clicking anywhere off the panel still reaches the pane underneath.
    pub(super) fn chrome_hit(&self, row: u16, col: u16) -> Option<ChromeHit> {
        let panel_w = self.panel_w();
        if let Some(hit) = self.chrome_hit_feed(row, col) {
            return Some(hit);
        }
        // The questions block pins above the court block: a click on its
        // rows opens the full questions view on that question; the `+N more`
        // row opens the list. The header toggles nothing here (the key does).
        if col < panel_w && self.sideline_view == crate::view_store::SidelineView::Agents {
            match questions::hit_at(self, self.term.0 as usize, row) {
                Some(questions::QuestionHit::Row(id)) => {
                    return Some(ChromeHit::OpenQuestionDetail(id));
                }
                Some(questions::QuestionHit::More) => {
                    return Some(ChromeHit::OpenQuestionsList);
                }
                None => {}
            }
        }
        // Tab strip (row 0, scoped to the content columns since US1): it
        // begins at `panel_w`, walking the same spans the renderer paints (with
        // the same origin). A row-0 click LEFT of the divider (`col < panel_w`)
        // belongs to the sideline's reclaimed row 0 and falls through below.
        // `panel_w == 0` (no sideline) -> strip from col 0, unchanged.
        if row < TAB_BAR_ROWS && col >= panel_w {
            let col = col as usize;
            if let Some((start, text)) = self.notice_overlay(self.term.1 as usize) {
                if col >= start && col < start + text.chars().count() {
                    return None;
                }
            }
            let mut c = panel_w as usize;
            for span in self.tab_bar_window() {
                let w = tab_text_cols(&span.text);
                if col >= c && col < c + w {
                    return match span.hit? {
                        TabHit::Tab(tid) => Some(ChromeHit::Cmds(vec![Command::SelectTab(tid)])),
                        TabHit::NewTab => Some(ChromeHit::Cmds(vec![Command::NewTab])),
                    };
                }
                c += w;
            }
            return None;
        }
        // Sideline: the painted width minus its divider (the full terminal
        // in full-screen mode). Off/narrow => no panel. Under the docked
        // board the column is the board's own surface: no agents rows, no
        // footer, no density button - a click must resolve nothing here or
        // it acts on a phantom row.
        if self.sideline_view != crate::view_store::SidelineView::Agents {
            return None;
        }
        let paint_w = self.sideline_paint_w();
        if paint_w == 0 || col as usize >= paint_w - 1 {
            return None;
        }
        // Full-screen sideline paints below the strip; invert the same
        // offset the painter used.
        let top = self.sideline_top();
        if (row as usize) < top {
            return None;
        }
        // The bottom row is overlaid by the status / which-key / search chrome
        // (draw_bottom_row paints last), so a click there belongs to that chrome,
        // not the sideline row drawn underneath it (codex P2).
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        // The density button rides the sideline's top painted row, over
        // whatever display row is scrolled to it. It is chrome pinned to the
        // first PAINTED row, not a property of that row, so the check is on
        // the painted row and must precede the display-row resolution below.
        if row == top as u16 && !self.sideline_full {
            // In full-screen the button is not painted, so a hit there would
            // cycle a density the screen does not show.
            if let Some(range) = self.density_button_range(panel_w as usize) {
                if range.contains(&(col as usize)) {
                    return Some(ChromeHit::CycleDensity);
                }
            }
        }
        // Display row i is painted at `i - offset` (draw_sideline, since
        // the sideline owns the top painted row), so invert with the paint
        // offset - else a click on a scrolled row activates the wrong row.
        // Mirrors sideline_row_at.
        let i = row as usize - top + self.sideline_offset();
        if let Some(hit) = self.table_header_hit(i, col) {
            return Some(hit);
        }
        // US4: a click on the footer's `☰ menu` region opens the sideline
        // MENU popup; the rest of the footer row keeps its `+ new` create action.
        if matches!(self.painted_rows().get(i), Some(DisplayRow::NewSquad)) {
            if let Some(range) = self.footer_menu_range(panel_w as usize) {
                if range.contains(&(col as usize)) {
                    return Some(ChromeHit::OpenSidelineMenu { row, col });
                }
            }
        }
        if let Some(id) = self.card_node_hit(i, col) {
            return Some(ChromeHit::OpenNode(id));
        }
        self.row_action(i)
    }

    /// The node on card line 1, when `col` falls on it: the tap that opens
    /// the node's plan. Reads the same layout the paint drew.
    fn card_node_hit(&self, i: usize, col: u16) -> Option<String> {
        if self.sideline_layout != sideline_color::SidelineLayout::Card {
            return None;
        }
        let rows = self.painted_rows();
        let DisplayRow::Agent(a) = rows.get(i)? else {
            return None;
        };
        let text_w = self.sideline_paint_w().checked_sub(1)?;
        let rect = self.worker_column_rects(text_w as u16)[2];
        let span = card_line::meter_node(a, rect.width as usize).node?;
        let at = (col as usize).checked_sub(rect.x as usize)?;
        span.contains(&at).then(|| a.node.clone()).flatten()
    }

    fn table_header_hit(&self, row: usize, col: u16) -> Option<ChromeHit> {
        if (self.density != Density::Extended && !self.sideline_full)
            || !matches!(self.painted_rows().get(row), Some(DisplayRow::TableHead))
        {
            return None;
        }
        let text_w = self.sideline_paint_w().checked_sub(1)?;
        let rects = self.worker_column_rects(text_w as u16);
        let hit = |r: RtRect| col >= r.x && col < r.x + r.width;
        if hit(rects[0]) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Status))
        } else if hit(rects[1]) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Agent))
        } else if hit(rects[2]) {
            Some(ChromeHit::SortColumn(AgentSortColumn::LastMessage))
        } else if hit(rects[3]) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Pr))
        } else if hit(rects[4]) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Age))
        } else {
            None
        }
    }
}
