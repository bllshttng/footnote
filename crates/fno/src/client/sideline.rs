//! The sideline column's paint: the agent Table, the per-row overlays, the
//! docked new-agent composer, the court block and the divider. One Buffer,
//! one blit. Draw_sideline and its two exclusive helpers live here; the
//! file-budget gate names this module the answer to "how does the sideline
//! column paint".

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::*;

/// The sideline Table's five columns: status word, name, message, PR, age.
/// Width ranking (operator, 2026-09-20): name first, message second, status
/// third. The status words are shortened (operator, 2026-09-21, longest is
/// `Input`) so the cell fits in 5, right-aligned so a word's blank parks at
/// the margin, and every freed column goes to the name's Min(22); the
/// message keeps the Fill(3) surplus. Read by the Table and - through
/// [`sideline_column_rects`] - by the callers that need the solver's answer
/// beside the paint: one geometry authority, and it is the solver.
const SIDELINE_RIGHT_SLOT_W: u16 = 6;
pub(super) const SIDELINE_COLUMNS: [Constraint; 5] = [
    Constraint::Length(5),
    Constraint::Min(22),
    Constraint::Fill(3),
    Constraint::Length(SIDELINE_RIGHT_SLOT_W),
    // 6, not the plan's 4: the density button overlays the last two
    // columns, and a 4-wide age cell leaves the sort arrow nowhere to hide
    // under it (the regression `age_sort_arrow_survives_the_density_button`
    // pins). The two spare columns are the padding the old COL_TIME=6 gave.
    Constraint::Length(SIDELINE_RIGHT_SLOT_W),
];

/// The solver's column rects for a text width: the same call the Table makes
/// internally (same constraints, same spacing, same flex), so a caller that
/// must know a column's width reads the SAME answer the paint uses.
pub(super) fn sideline_column_rects(text_w: u16) -> std::rc::Rc<[RtRect]> {
    Layout::horizontal(SIDELINE_COLUMNS)
        .flex(Flex::Start)
        .spacing(1)
        .split(RtRect::new(0, 0, text_w, 1))
}

impl View {
    /// Rows of chrome the full-screen sideline paints under (the tab strip),
    /// so the click mappers invert the same offset the painter used.
    pub(super) fn sideline_top(&self) -> usize {
        if self.sideline_full {
            TAB_BAR_ROWS as usize
        } else {
            0
        }
    }

    /// The width the sideline paints at: the full terminal in full-screen
    /// mode, the saved panel width otherwise. The click mappers bound
    /// columns to THIS, not to `panel_w`, or the painted table's right half
    /// goes dead and clamped clicks misresolve their column.
    pub(super) fn sideline_paint_w(&self) -> usize {
        if self.sideline_full {
            self.term.1 as usize
        } else {
            self.panel_w() as usize
        }
    }

    /// The row list the sideline PAINTS: the Extended table in full-screen
    /// mode, the stored density's rows otherwise. The click mappers, row
    /// actions and the scroll clamp index THIS, or a click resolves a
    /// different row than the one drawn.
    pub(super) fn painted_rows(&self) -> Vec<DisplayRow<'_>> {
        if self.sideline_full {
            self.table_rows_with_depths().0
        } else {
            self.display_rows()
        }
    }

    pub(super) fn draw_sideline(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        panel_w: usize,
    ) {
        let text_w = panel_w - 1; // last column is the divider
                                  // Read the clock ONCE per paint, not per row: every row's age is
                                  // relative to the same instant, so a mid-paint tick cannot make one
                                  // row read older than the row above it.
        let now = crate::digest_overlay::now_secs();
        // Full-screen sideline forces the Extended table (the full column
        // list) for this paint; the stored density returns untouched on exit.
        let full = self.sideline_full;
        let card = self.sideline_layout == sideline_color::SidelineLayout::Card;
        let density = if full {
            Density::Extended
        } else {
            self.density
        };
        let (display, row_depths) = if full {
            self.table_rows_with_depths()
        } else {
            self.display_rows_with_depths()
        };
        // The docked new-agent composer takes the bottom rows while open in
        // BOTTOM mode; in sheet mode the sideline paints untouched (the
        // sheet is an overlay drawn by the client's overlay chain), and the
        // passive court block yields to an active editor.
        let chrome_rows = self.bottom_row_is_chrome() as usize;
        let dock_len = if agent_launcher::form_mode(self) == agent_launcher::Mode::Bottom {
            self.launcher
                .as_ref()
                .map_or(0, |l| l.dock_layout(rows - chrome_rows, text_w).0)
        } else {
            0
        };
        let (block_rows, block_lines) = if dock_len > 0 {
            (0, Vec::new())
        } else {
            self.court_block_layout(rows)
        };
        let list_rows = rows.saturating_sub(block_rows + dock_len);
        // The scroll policy (`clamp_sideline_scroll`) keeps the cursor inside
        // the terminal minus the bottom chrome row; the widget area must
        // answer to the same height, or the render-time scroll lands the
        // selected row under the chrome that paints over it.
        let table_rows_n = list_rows.saturating_sub(chrome_rows);
        // The widget renders into a standalone Buffer (no terminal, no
        // backend) and the blit copies it into the compositor's cells. The
        // court block and the dock own the rows below the list, so the
        // widget area stops above them. The dock paints into the SAME Buffer
        // before that one blit.
        let btn_reserved = self
            .density_button_range(panel_w)
            .map_or(text_w, |range| range.start);
        let area = RtRect::new(0, 0, text_w as u16, rows as u16);
        let mut buf = RtBuffer::empty(area);
        // The widget area is the top slice of the column; the dock paints
        // into the same Buffer below it, before the one blit.
        let table_area = RtRect::new(0, 0, text_w as u16, table_rows_n as u16);
        // The selector rides the TableState's `selected`, which is what the
        // widget's render-time scroll keeps visible.
        let mut st = self.sideline_state.get().with_selected(self.selector);
        let mut off = st.offset();
        let rects = sideline_column_rects(text_w as u16);
        if density != Density::Slim {
            let name_w = rects[1].width as usize;
            let table_rows: Vec<RtRow> = display
                .iter()
                .enumerate()
                .map(|(i, drow)| {
                    let depth = row_depths.get(i).copied().unwrap_or(0);
                    self.sideline_table_row(drow, depth, name_w, rects[4].width as usize, now)
                })
                .collect();
            let table = RtTable::new(table_rows, SIDELINE_COLUMNS)
                .flex(Flex::Start)
                .highlight_spacing(HighlightSpacing::Never)
                // The overlay pass is the one band painter: the Table's own
                // row highlight (REVERSED by default) would paint an INVERSE
                // band the spec forbids inside a highlight.
                .row_highlight_style(RtStyle::new());
            use ratatui_core::widgets::StatefulWidget;
            StatefulWidget::render(&table, table_area, &mut buf, &mut st);
            off = st.offset();
            // The render-adjusted state IS the truth: persist it so the hit
            // tests and the confirm anchor read the offset that painted.
            self.sideline_state.set(st);
        }
        // The docked composer paints into the SAME Buffer, pinned above the
        // bottom chrome row. On a panel too small for even the chip rows the
        // painter clips; the dock never hides while open.
        if dock_len > 0 {
            if let Some(l) = &self.launcher {
                let top = (rows - chrome_rows).saturating_sub(dock_len);
                l.paint(
                    self,
                    &mut buf,
                    RtRect::new(0, top as u16, text_w as u16, dock_len as u16),
                );
            }
        }
        crate::ratatui_blit::blit(&buf, cells, cols);
        // Per-row overlays the widget cannot express: the full-width rows
        // (bands, sublines, the idle fold, the footer, the empty state - see
        // the catch-all in `sideline_table_row`), the active-squad caret
        // accent, the row-scoped outcome stamp, and the selector / hover
        // bar. The list bar XORs INVERSE so a focused row's standing band
        // de-inverts under the cursor. Card mode clears the Table's selection
        // style; its Agent and CardDetail rows use one paired overlay here.
        for (i, drow) in display.iter().enumerate().skip(off) {
            let r = i - off;
            if r >= table_rows_n {
                break;
            }
            let mark_caret = matches!(
                drow,
                DisplayRow::Sel(row)
                    if row.tab.is_none() && row.squad == self.layout.active_squad
            );
            let band_w = if r == 0 { btn_reserved } else { text_w };
            let legacy = match drow {
                DisplayRow::Sel(row) => {
                    let Some(squad) = self.layout.squads.iter().find(|s| s.id == row.squad) else {
                        continue;
                    };
                    let is_active = squad.id == self.layout.active_squad;
                    match row.tab {
                        None => {
                            let caret = view_caret(self.section_view(&section_key(squad)));
                            let mark = if is_active { '*' } else { ' ' };
                            let rollup = section_rollup(
                                self.layout
                                    .agents
                                    .iter()
                                    .filter(|a| a.squad == Some(squad.id))
                                    .map(agent_lattice_state),
                            );
                            Some((
                                header_band_text(
                                    &format!("{caret}{mark}{}", squad.name),
                                    &rollup,
                                    band_w,
                                ),
                                header_band_flags(is_active),
                            ))
                        }
                        Some(t) => {
                            let marker = if is_active && t == squad.active_tab {
                                '*'
                            } else {
                                ' '
                            };
                            let label = match squad.tabs.get(t) {
                                Some(tm) => tab_label_text(&tm.name, t, tm.named),
                                None => (t + 1).to_string(),
                            };
                            Some((format!("  {marker}{label}"), 0))
                        }
                    }
                }
                DisplayRow::Header {
                    label,
                    rollup,
                    view,
                    ..
                } => Some((
                    header_band_text(&format!("{}{label}", view_caret(*view)), rollup, band_w),
                    header_band_flags(false),
                )),
                DisplayRow::Sub(sub) => Some((format!("    {sub}"), cell_flags::DIM)),
                DisplayRow::CardDetail(a) => {
                    Some((self.card_detail_text(a, now, text_w), cell_flags::DIM))
                }
                DisplayRow::TableEmpty => Some(("  no agents".to_string(), cell_flags::DIM)),
                DisplayRow::IdleFold {
                    hidden, expanded, ..
                } => Some((
                    if *expanded {
                        "    - fewer".to_string()
                    } else {
                        format!("    +{hidden} more")
                    },
                    cell_flags::DIM,
                )),
                _ => None,
            };
            if let Some((text, flags)) = legacy {
                paint_legacy_row(cells, r, cols, text_w, &text, flags);
                if matches!(drow, DisplayRow::CardDetail(_)) {
                    // Card line 2 on a light terminal: DIM washes the default
                    // fg toward a light background until it vanishes. The
                    // palette's own dim gray (index 8) dims a dark scheme and
                    // stays a readable gray on a light one - the fg follows
                    // the terminal instead of fighting it.
                    for cell in &mut cells[r * cols..r * cols + text_w] {
                        cell.fg = Color::Indexed(8);
                        cell.flags &= !cell_flags::DIM;
                        cell.flags &= !cell_flags::BOLD;
                    }
                }
            }
            if card {
                self.paint_card_pr_if_it_fits(cells, r, cols, text_w, drow);
            }
            if mark_caret && text_w >= 1 {
                cells[r * cols].fg = self.theme.accent;
            }
            if matches!(drow, DisplayRow::NewSquad) {
                self.paint_new_squad_footer(cells, r, cols, text_w, panel_w);
            }
            let chosen = matches!(drow, DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus) && !a.exited);
            let mut highlit = chosen || self.selector == Some(i) || self.hover_row == Some(i);
            if card {
                highlit = self.card_pair_highlit(&display, i, highlit);
            }
            let card_pair =
                card && matches!(drow, DisplayRow::Agent(_) | DisplayRow::CardDetail(_));
            let card_chosen = card_pair
                && matches!(drow, DisplayRow::Agent(a) | DisplayRow::CardDetail(a)
                    if a.pane_id == Some(self.layout.focus) && !a.exited);
            if card_chosen {
                // The chosen card paints its color across BOTH lines, full
                // width, hover included: the chosen color wins on hover.
                highlit = true;
            }
            if highlit {
                // One solid band across the full width of the row, gaps
                // included: an explicit background and an explicit fg picked
                // to contrast with it, never per-span inversion, so a span's
                // own color cannot patch the highlight and the band reads the
                // same on a dark and a light terminal.
                let (fg, bg, flags) =
                    crate::theme::band_style(card_chosen || (!card && chosen), &self.theme);
                for cell in &mut cells[r * cols..r * cols + text_w] {
                    cell.bg = bg;
                    cell.fg = fg;
                    cell.flags = flags;
                }
            }
            let row_stamp = self.row_stamp_for(drow);
            paint_row_stamp(cells, r, cols, text_w, row_stamp);
        }
        // The density button, painted LAST over the sideline's top row.
        // Overlaying is what keeps it pinned to row 0 while the rows beneath it
        // scroll, and it costs no display row - so the invariant (every
        // painted line is exactly one display row) still holds and
        // `sideline_row_at` needs no special case.
        //
        // Layout is [inverse glyph][plain pad]: the glyph leads and the
        // divider-adjacent cell is a NON-inverse space, so the button reads one
        // column in from the border (the operator's padding ask) without shifting
        // `range.start`.
        if rows > 0 && !full {
            if let Some(range) = self.density_button_range(panel_w) {
                let glyph = density_glyph(self.density);
                let start = range.start;
                for (n, c) in range.clone().enumerate() {
                    let is_pad = n + 1 == DENSITY_BTN_W; // the trailing cell is the pad
                                                         // The button yields to a blitted row's own cells: an
                                                         // agent row scrolled to the top keeps its right-aligned
                                                         // PR and age, and the density cycle keeps its keybind
                                                         // (Locked Decision 5) - the button is never the only way.
                    if cells[c].c != ' ' {
                        continue;
                    }
                    cells[c] = Cell {
                        c: if c == start { glyph } else { ' ' },
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: if is_pad { 0 } else { cell_flags::INVERSE },
                    };
                }
            }
        }
        // The court block: three glance lines minimized, the full
        // reading expanded, pinned to the bottom rows of the column. The row
        // list already stopped above it; the block renders DIM so it reads as
        // chrome beside the live rows, and the painter truncates to the panel
        // width - the same rule every sideline row follows.
        court_block::paint_court_block(cells, block_lines, list_rows, rows, cols, text_w);
        // The divider column, now full terminal height (the sideline owns row
        // 0 too; the strip sits right of the divider) - US1.
        //
        // Accent it while hovered or dragged, the same signal a pane
        // seam wears. A terminal cannot change the cursor shape, so this
        // accent IS the affordance.
        let border_active = self.hover_sideline_border || self.sideline_drag.is_some();
        let (border_fg, border_flags) = if border_active {
            (self.theme.accent, cell_flags::BOLD)
        } else {
            (Color::Default, cell_flags::DIM)
        };
        for r in 0..rows {
            cells[r * cols + (panel_w - 1)] = Cell {
                c: '\u{2502}',
                fg: border_fg,
                bg: Color::Default,
                flags: border_flags,
            };
        }
    }

    /// One sideline display row as a five-cell Table row (status word, name,
    /// message, PR, age - [`SIDELINE_COLUMNS`]). The glyph lattice moves to
    /// the status column as its word, and the focused row's band rides the
    /// row style. The full-width rows (bands, sublines, footer, spacers)
    /// return empty cells and paint in the overlay pass.
    fn sideline_table_row(
        &self,
        drow: &DisplayRow<'_>,
        depth: usize,
        name_w: usize,
        right_slot_w: usize,
        now: u64,
    ) -> RtRow<'static> {
        // An EXITED focus row is legibly dead: DIM accent on its cells and
        // no band (a dead "you are here" never reads as a live one). A live
        // focus row's band is the overlay's accent highlight.
        let is_focus = matches!(drow, DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus));
        let focus_exited = is_focus && matches!(drow, DisplayRow::Agent(a) if a.exited);
        let (row_cells, _): (Vec<RtCell>, u8) = match drow {
            // The full-width rows - squad and section bands, sublines, the
            // idle fold, the footer, the empty state - paint in the overlay
            // pass (`paint_legacy_row`): a band is edge-to-edge at EVERY
            // width, which five columns cannot give a 16-column rail, and
            // the narrow-panel pair-dropping in `header_band_text` survives
            // untouched. The cells here only give the row its height and
            // its scroll identity in the Table.
            DisplayRow::Sel(_)
            | DisplayRow::Header { .. }
            | DisplayRow::NewSquad
            | DisplayRow::Sub(_)
            | DisplayRow::Blank
            | DisplayRow::TableEmpty
            | DisplayRow::CardDetail(_)
            | DisplayRow::IdleFold { .. } => {
                (vec![rt_cell(String::new(), Color::Default, 0, false); 5], 0)
            }
            DisplayRow::Agent(a) => {
                let lat = agent_lattice_state(a);
                let style = lattice_style(lat, self.theme.accent);
                let mut flags = style.flags;
                if a.external && lat != LatticeState::Blocked {
                    flags |= cell_flags::DIM;
                }
                let status_fg = agent_lane_fg(a, lat, style.fg);
                // An EXITED focus row is legibly dead: DIM accent on the
                // cells, and the overlay gives it no band. A live focus
                // row's band is the overlay's accent highlight; the cells
                // stay ordinary.
                let focus_bit = if focus_exited { cell_flags::DIM } else { 0 };
                let cell_fg = if focus_exited {
                    self.theme.accent
                } else {
                    status_fg
                };
                let cell_flags_v = flags | focus_bit;
                // The name cell keeps the compact row's identity vocabulary:
                // recruit mark, DND, deviation token, portal index, tab
                // context, orphan cwd, reason, crown badge - depth-indented,
                // ellipsized to the width the solver admits.
                let mark = if a
                    .attach_id
                    .as_deref()
                    .is_some_and(|id| self.marks.contains(id))
                {
                    '*'
                } else {
                    ' '
                };
                let prefix = if depth > 0 {
                    format!("{}{mark} ", "  ".repeat(depth))
                } else {
                    format!("{mark} ")
                };
                let mut suffix = if a.dnd {
                    " [DND]".to_string()
                } else {
                    String::new()
                };
                if let Some(tok) =
                    sideline_color::deviation_token(a.harness.as_deref(), a.model.as_deref())
                {
                    suffix.push_str(&format!(" {tok}"));
                }
                if let Some(idx) = a.portal {
                    suffix.push_str(&format!(" \u{25ab}{idx}"));
                }
                match self.agent_tab_context(a.squad, a.tab) {
                    Some(_)
                        if a.squad == Some(self.layout.active_squad)
                            && a.tab == self.active_squad_active_tab_id() => {}
                    Some(TabContext::Named(ctx)) => suffix.push_str(&format!(" \u{b7}{ctx}")),
                    Some(TabContext::Ordinal(ord)) => suffix.push_str(&format!(" \u{b7}{ord}")),
                    None => {
                        if a.squad.is_none() {
                            if let Some(base) = a.cwd_base.as_deref() {
                                suffix.push_str(&format!(" ({base})"));
                            }
                        }
                    }
                }
                if let Some(reason) = a.reason.as_deref().filter(|x| !x.is_empty()) {
                    suffix.push_str(": ");
                    suffix.push_str(reason);
                }
                if let Some(level) = a.crown_level {
                    let scope = a.crown_scope.as_deref().unwrap_or("?");
                    match a.crown_name.as_deref() {
                        Some(name) => {
                            suffix.push_str(&format!(" [L{level} {name} \u{b7} {scope}]"))
                        }
                        None => suffix.push_str(&format!(" [L{level} {scope}]")),
                    }
                } else if let Some(name) = a.crown_name.as_deref() {
                    // A worker that rolls up to a named crown: the name rides
                    // the name cell so the lineage reads without a lookup.
                    suffix.push_str(&format!(" [{name}]"));
                }
                let prefix_width = prefix.width();
                let suffix_width = suffix.width();
                let base_width = name_w.saturating_sub(prefix_width);
                let name = if suffix_width < base_width {
                    let base = fit_name(&a.name, base_width - suffix_width);
                    format!("{prefix}{base}{suffix}")
                } else {
                    let base = fit_name(&a.name, base_width);
                    format!("{prefix}{base}")
                };
                // The message column reads the sentence, not the markup, and
                // leads with the separator. A PR row with no output names the
                // session driving it (the server's graph join: the live claim
                // holder's session, else the node's last do/ship session); no
                // session id says so. The widest column keeps the handle
                // visible where the old inline suffix clipped.
                let tail = row_message_text(a)
                    .map(|t| format!("\u{b7} {t}"))
                    .unwrap_or_default();
                let card = self.sideline_layout == sideline_color::SidelineLayout::Card;
                let pr =
                    a.pr.map(|n| format!("#{n}"))
                        .unwrap_or_else(|| "\u{2014}".into());
                let age = row_age(a, now);
                let (pr_cell, age_cell) = if card {
                    let pr_cell = if pr.width() <= right_slot_w {
                        pr
                    } else {
                        String::new()
                    };
                    (String::new(), pr_cell)
                } else {
                    (pr, age)
                };
                let quiet = if flags & cell_flags::DIM != 0 {
                    cell_flags::DIM
                } else {
                    0
                };
                (
                    vec![
                        // Card line 1: glyph in the status column, the word
                        // in the message column, no age on line 1. List mode
                        // paints the exact pre-card cells.
                        rt_cell(
                            if card {
                                style.glyph.to_string()
                            } else {
                                status_word(lat).to_string()
                            },
                            cell_fg,
                            cell_flags_v,
                            true,
                        ),
                        rt_cell(fit_name(&name, name_w), cell_fg, cell_flags_v, false),
                        rt_cell(
                            if card {
                                status_word(lat).to_string()
                            } else {
                                tail
                            },
                            cell_fg,
                            quiet | focus_bit,
                            false,
                        ),
                        rt_cell(pr_cell, cell_fg, quiet | focus_bit, true),
                        rt_cell(age_cell, cell_fg, quiet | focus_bit, true),
                    ],
                    0,
                )
            }
            DisplayRow::TableHead => {
                let marker = |column: AgentSortColumn| {
                    if self.agent_sort.column == column {
                        match self.agent_sort.direction {
                            SortDirection::Ascending => " \u{2191}",
                            SortDirection::Descending => " \u{2193}",
                        }
                    } else {
                        ""
                    }
                };
                let age_marker = if self.agent_sort.column == AgentSortColumn::Age {
                    match self.agent_sort.direction {
                        SortDirection::Ascending => "\u{2191}",
                        SortDirection::Descending => "\u{2193}",
                    }
                } else {
                    ""
                };
                (
                    vec![
                        rt_cell(
                            format!("st{}", marker(AgentSortColumn::Status)),
                            Color::Default,
                            cell_flags::DIM,
                            true,
                        ),
                        rt_cell(
                            format!("agent{}", marker(AgentSortColumn::Agent)),
                            Color::Default,
                            cell_flags::DIM,
                            false,
                        ),
                        rt_cell(
                            format!("last msg{}", marker(AgentSortColumn::LastMessage)),
                            Color::Default,
                            cell_flags::DIM,
                            false,
                        ),
                        rt_cell(
                            format!("pr{}", marker(AgentSortColumn::Pr)),
                            Color::Default,
                            cell_flags::DIM,
                            false,
                        ),
                        // Left-aligned like every head label: right-aligned,
                        // the arrow sits under the density button's two
                        // overlay columns and the toggle reads dead.
                        rt_cell(
                            format!("age{age_marker}"),
                            Color::Default,
                            cell_flags::DIM,
                            false,
                        ),
                    ],
                    0,
                )
            }
        };
        RtRow::new(row_cells)
    }

    /// The card expansion of the display enumeration. `List` returns the
    /// input unchanged (byte-identical to the pre-card rows). `Card` gives
    /// each `Agent` a two-line card - `Blank, Agent, CardDetail` - with the
    /// `Sub` lines that follow it inside the card, one blank of padding
    /// above and below, adjacent cards sharing one blank, and an existing
    /// spacer counting as the bottom padding. Every agent depth is forced to
    /// 0: the king shows on line 2, not as an indent.
    pub(super) fn card_rows<'a>(
        &self,
        rows: Vec<DisplayRow<'a>>,
        depths: Vec<usize>,
    ) -> (Vec<DisplayRow<'a>>, Vec<usize>) {
        if self.sideline_layout != sideline_color::SidelineLayout::Card {
            return (rows, depths);
        }
        let mut out_rows: Vec<DisplayRow<'_>> = Vec::with_capacity(rows.len() * 2);
        let mut out_depths: Vec<usize> = Vec::with_capacity(rows.len() * 2);
        let mut in_card = false;
        for (row, depth) in rows.into_iter().zip(depths) {
            match row {
                DisplayRow::Agent(a) => {
                    if in_card {
                        // Close the previous card; adjacent cards share this
                        // one blank between them.
                        out_rows.push(DisplayRow::Blank);
                        out_depths.push(0);
                    } else if !matches!(out_rows.last(), Some(DisplayRow::Blank)) {
                        out_rows.push(DisplayRow::Blank);
                        out_depths.push(0);
                    }
                    out_rows.push(DisplayRow::Agent(a));
                    out_depths.push(0);
                    out_rows.push(DisplayRow::CardDetail(a));
                    out_depths.push(0);
                    in_card = true;
                }
                sub @ DisplayRow::Sub(_) if in_card => {
                    out_rows.push(sub);
                    out_depths.push(0);
                }
                row => {
                    if in_card && !matches!(row, DisplayRow::Blank) {
                        out_rows.push(DisplayRow::Blank);
                        out_depths.push(0);
                    }
                    in_card = false;
                    out_rows.push(row);
                    out_depths.push(depth);
                }
            }
        }
        if in_card {
            out_rows.push(DisplayRow::Blank);
            out_depths.push(0);
        }
        (out_rows, out_depths)
    }

    /// The `+ new` footer row, painted over the blitted cells in its legacy
    /// full-width composition: the menu button rides the footer's right edge
    /// at the exact column [`View::footer_menu_range`] names, because that
    /// range routes a click there - paint and hit range cannot diverge.
    fn paint_new_squad_footer(
        &self,
        cells: &mut [Cell],
        r: usize,
        cols: usize,
        text_w: usize,
        panel_w: usize,
    ) {
        let base = if self.marks.is_empty() {
            FOOTER_NEW_LABEL.to_string()
        } else {
            format!("{FOOTER_NEW_LABEL}   {} marked \u{b7}R", self.marks.len())
        };
        let label = match self.footer_menu_range(panel_w) {
            Some(range) => format!("{}{FOOTER_MENU}", pad_to(&base, range.start)),
            None => base,
        };
        paint_legacy_row(cells, r, cols, text_w, &label, cell_flags::BOLD);
    }

    /// A narrow Regular panel can clip every Table column after the name.
    /// Keep a fitting card PR visible at the right edge when its cells are
    /// otherwise empty, without overwriting the status or identity.
    fn paint_card_pr_if_it_fits(
        &self,
        cells: &mut [Cell],
        row: usize,
        cols: usize,
        text_w: usize,
        drow: &DisplayRow<'_>,
    ) {
        let DisplayRow::Agent(agent) = drow else {
            return;
        };
        let Some(number) = agent.pr else {
            return;
        };
        let label = format!("#{number}");
        let width = label.width();
        if width > SIDELINE_RIGHT_SLOT_W as usize || width > text_w {
            return;
        }
        let line = &mut cells[row * cols..row * cols + text_w];
        let start = text_w - width;
        if line[start..].iter().any(|cell| cell.c != ' ') {
            return;
        }
        let Some(style) = line.iter().find(|cell| cell.c != ' ').cloned() else {
            return;
        };
        let lattice = agent_lattice_state(agent);
        let quiet = if agent.external && lattice != LatticeState::Blocked {
            cell_flags::DIM
        } else {
            0
        };
        let focus = if agent.pane_id == Some(self.layout.focus) {
            if agent.exited {
                cell_flags::DIM
            } else {
                cell_flags::INVERSE
            }
        } else {
            0
        };
        for (offset, ch) in label.chars().enumerate() {
            let mut cell = style.clone();
            cell.c = ch;
            cell.flags = quiet | focus;
            line[start + offset] = cell;
        }
    }

    /// Line 2 of a card: two spaces, then `harness · king · message`, with
    /// the age right-aligned to the panel edge. Segments that are `None`
    /// drop out of the join; a worker with no harness, king or message
    /// paints just its age.
    pub(super) fn card_detail_text(&self, a: &AgentRow, now: u64, text_w: usize) -> String {
        let mut segments: Vec<String> = Vec::new();
        if let Some(h) = a.harness.as_deref() {
            segments.push(h.to_string());
        }
        if let Some(k) = self.king_label(a) {
            segments.push(k);
        }
        let msg = row_message_text(a);
        if let Some(msg) = msg {
            segments.push(msg);
        }
        let mut text = String::from("  ");
        if !segments.is_empty() {
            text.push_str(&segments.join(" \u{b7} "));
        }
        let age = row_age(a, now);
        let head_w = text_w.saturating_sub(age.width());
        let head = if text.width() <= head_w {
            text
        } else {
            let limit = head_w.saturating_sub(1);
            let mut fitted = String::new();
            let mut width = 0;
            for ch in text.chars() {
                let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if width + char_width > limit {
                    break;
                }
                fitted.push(ch);
                width += char_width;
            }
            if head_w > 0 {
                fitted.push('…');
            }
            fitted
        };
        let padding = " ".repeat(head_w.saturating_sub(head.width()));
        format!("{head}{padding}{age}")
    }

    /// The card-mode highlight pairing: a card's lower half inverts when
    /// the `Agent` row above it is selected or hovered, and an `Agent` row
    /// also inverts when its lower half is hovered. `base` is the ordinary
    /// (non-inert) highlight for this row.
    pub(super) fn card_pair_highlit(
        &self,
        display: &[DisplayRow<'_>],
        i: usize,
        base: bool,
    ) -> bool {
        match display.get(i) {
            Some(DisplayRow::CardDetail(_)) => {
                base || self.selector == Some(i)
                    || self.hover_row == Some(i)
                    || self.selector == Some(i.saturating_sub(1))
                    || self.hover_row == Some(i.saturating_sub(1))
            }
            Some(DisplayRow::Agent(_)) => {
                base || matches!(display.get(i + 1), Some(DisplayRow::CardDetail(_)))
                    && (self.selector == Some(i + 1) || self.hover_row == Some(i + 1))
            }
            _ => base,
        }
    }

    /// The king label for line 2 of a card: the crowned row itself shows its
    /// crown scope; a worker walks its lineage to the first crowned
    /// ancestor and shows [`crown_display_name`]. No crowned ancestor, or a
    /// lineage cycle (capped at one step per agent), labels nothing.
    pub(super) fn king_label(&self, a: &AgentRow) -> Option<String> {
        if a.crown_level.is_some() {
            return a.crown_scope.clone();
        }
        let mut parent = lineage_parent(a);
        let mut steps = 0;
        while let Some(pid) = parent {
            if steps >= self.layout.agents.len() {
                return None;
            }
            let row = self
                .layout
                .agents
                .iter()
                .find(|r| r.harness_session_id.as_deref() == Some(pid))?;
            if row.crown_level.is_some() {
                return Some(crown_display_name(row).to_string());
            }
            parent = lineage_parent(row);
            steps += 1;
        }
        None
    }
}

/// The age cell's text, shared by the list row and the card's line 2 so the
/// two cannot drift: the server's measured age when it carries one, else
/// now minus the row's update stamp.
fn row_age(a: &AgentRow, now: u64) -> String {
    match (a.last_activity_age_s, a.updated_at) {
        (Some(s), _) => humanize_age(Some(s)),
        (None, Some(u)) => humanize_age(Some(now.saturating_sub(u))),
        (None, None) => humanize_age(None),
    }
}

/// The name a crowned row shows on its workers' cards. Today the king's
/// handle; the crown's own name replaces it when the wire carries one.
fn crown_display_name(king: &AgentRow) -> &str {
    &king.name
}

/// The message text an agent's row shows: the markup-stripped tail, or the
/// PR-session fallback when the tail is empty. No separator - the list cell
/// prefixes its dot and the card line joins with dots.
fn row_message_text(a: &AgentRow) -> Option<String> {
    match a.tail.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => Some(strip_md(t)),
        None => match (a.pr, a.pr_session_short.as_deref()) {
            (Some(_), Some(sid)) => Some(format!("attach {sid}")),
            (Some(_), None) => Some("no session".to_string()),
            (None, _) => None,
        },
    }
}

/// Full-screen sideline: the sideline owns every cell, so every report
/// routes here - the dock first, then the hit path a normal-mode sideline
/// click runs (clamped into the panel's column space) - and no byte ever
/// reaches a pane.
pub(super) async fn route_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    use crate::proto::{MouseButton, MouseKind};
    if view.launcher.is_some() && agent_launcher::launcher_mouse(view, rep, sock_w).await? {
        return Ok(());
    }
    match rep.kind {
        MouseKind::WheelUp | MouseKind::WheelDown => {
            view.scroll_sideline(matches!(rep.kind, MouseKind::WheelDown));
        }
        MouseKind::Press(MouseButton::Left) => {
            let top = view.sideline_top() as u16;
            if rep.row < top {
                return Ok(());
            }
            let pw = view.sideline_paint_w().max(1) as u16;
            let col = rep.col.min(pw.saturating_sub(2));
            if let Some(hit) = view.chrome_hit(rep.row, col) {
                apply_hit(view, hit, sock_w).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The composer is a modal like every other: prefix chords still resolve
/// while it holds the keyboard (which-key parity), so the chunk scans first
/// and only the plain-byte chunks feed the composer's own folder. Nothing
/// here reaches a pane.
pub(super) async fn route_launcher_keys(
    view: &mut View,
    scanner: &mut Scanner,
    passthrough: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    // The composer owns every key while open: a bare H/J/K/L must be text
    // for the draft, never a pane resize, so the repeat window a prefix
    // chord armed closes the moment the composer takes the byte.
    scanner.disarm_repeat();
    for event in scanner.scan(passthrough, std::time::Instant::now()) {
        match event {
            Event::Forward(chunk) => {
                agent_launcher::launcher_keys(view, &chunk, sock_w).await?;
            }
            event => match dispatch_event(view, event, sock_w).await? {
                DispatchFlow::Continue => {}
                DispatchFlow::Break => break,
                DispatchFlow::Detach => return Ok(StdinFlow::Detach),
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Toggle the composer: close retains the draft, as Esc does. Opening never
/// touches the sideline or the pane's size - the sheet is an overlay; a
/// terminal too short to admit it opens nothing and says so.
pub(super) async fn toggle_composer(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if view.launcher.is_some() {
        agent_launcher::close(view);
    } else {
        show_composer(view, sock_w).await?;
    }
    Ok(())
}

/// The show half of [`toggle_composer`], shared with the board's `t` key:
/// open the composer under the width rule. The sheet needs no sideline and
/// sends NO Resize - the sheet is an overlay, so no pane changes width; the
/// bottom form is only reachable from the full-screen sideline, which also
/// needs no Resize. `true` when the composer is open.
pub(super) async fn show_composer(
    show: &mut View,
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<bool, String> {
    let was_none = show.launcher.is_none();
    // An already-open dock keeps its held draft; open() replaces it.
    if was_none {
        agent_launcher::open(show);
    }
    // The sheet's minimum is 12 rows; nothing opens and the bottom row says
    // why - the refusal the too-narrow sidebar used to give.
    if agent_launcher::form_mode(show) == agent_launcher::Mode::Sheet && show.term.0 < 12 {
        if was_none {
            agent_launcher::close(show);
        }
        show.set_notice("terminal too short for the composer".into());
        return Ok(false);
    }
    Ok(true)
}

/// The agent-view pattern: entering full-screen opens the composer (a list
/// with an input at the bottom). Leaving leaves the composer as it is;
/// content_dims never changed, so no Resize travels in either direction.
pub(super) fn toggle_full(view: &mut View) {
    view.sideline_full = !view.sideline_full;
    if view.sideline_full && view.launcher.is_none() {
        agent_launcher::open(view);
        view.panel_on = true;
    }
}
