//! The sideline column's paint: the agent Table, the per-row overlays, the
//! docked new-agent composer, the org block and the divider. One Buffer,
//! one blit. Draw_sideline and its two exclusive helpers live here; the
//! file-budget gate names this module the answer to "how does the sideline
//! column paint".

use unicode_width::UnicodeWidthStr;

use super::backlog_style::BLine;
use super::*;

/// The sideline Table's five columns: status word, name, message, PR, age.
/// Width ranking (operator, 2026-09-20): name first, message second, status
/// third. The status words are shortened (operator, 2026-09-21, longest is
/// `Input`) so the cell fits in 5, right-aligned so a word's blank parks at
/// the margin, and every freed column goes to the name's Min(22); the
/// message keeps the Fill(3) surplus. Read by the Table; callers that need
/// the solver's answer beside the paint use `worker_column_rects`, the same
/// constraints the paint feeds: one geometry authority, and it is the solver.
const SIDELINE_RIGHT_SLOT_W: u16 = 6;
/// The card edge bar: leads every card line, accent-toned on the chosen
/// card, theme-dim on a resting one.
const CARD_EDGE_BAR: char = '\u{258e}';
const CARD_COLUMNS: [Constraint; 5] = [
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

/// The sort column under each head cell. A card's third cell is the bar
/// and node, which sort nothing; its PR rides the last slot, so `age` takes
/// the empty fourth. Read by the head paint and the head click alike.
pub(super) fn head_sorts(card: bool) -> [Option<AgentSortColumn>; 5] {
    use AgentSortColumn::*;
    if card {
        [Some(Status), Some(Agent), None, Some(Age), Some(Pr)]
    } else {
        [
            Some(Status),
            Some(Agent),
            Some(LastMessage),
            Some(Pr),
            Some(Age),
        ]
    }
}

fn head_label(column: AgentSortColumn) -> &'static str {
    match column {
        AgentSortColumn::Status => "st",
        AgentSortColumn::Agent => "agent",
        AgentSortColumn::LastMessage => "last msg",
        AgentSortColumn::Pr => "pr",
        AgentSortColumn::Age => "age",
    }
}

impl View {
    fn worker_columns(&self, text_w: u16) -> Vec<Constraint> {
        // The name takes a quarter of the surplus over the fixed cells, the
        // description first (d-36438ea4); the message keeps the Fill(3)
        // surplus. Both layouts use the same rule. `Min`, not `Length`: the
        // name keeps the old yield order under deficit (names starve last),
        // and name_w floors at 22 so the narrow rail solves as it did before
        // the formula existed.
        let list_wide = self.sideline_layout == sideline_color::SidelineLayout::List
            && (self.density == Density::Extended || self.sideline_full)
            // Fixed cells and six gaps leave at least eight message columns.
            && text_w >= 5 + 22 + 6 + 6 + 7 + 4 + 6 + 8;
        if list_wide {
            vec![
                CARD_COLUMNS[0],
                Constraint::Min(row_meter::name_w(text_w, 28, 6)),
                CARD_COLUMNS[2],
                CARD_COLUMNS[3],
                CARD_COLUMNS[4],
                Constraint::Length(7),
                Constraint::Length(4),
            ]
        } else {
            vec![
                CARD_COLUMNS[0],
                Constraint::Min(row_meter::name_w(text_w, 17, 4)),
                CARD_COLUMNS[2],
                CARD_COLUMNS[3],
                CARD_COLUMNS[4],
            ]
        }
    }

    pub(super) fn worker_column_rects(&self, text_w: u16) -> std::rc::Rc<[RtRect]> {
        Layout::horizontal(self.worker_columns(text_w).iter().copied())
            .flex(Flex::Start)
            .spacing(1)
            .split(RtRect::new(0, 0, text_w, 1))
    }

    /// Rows of chrome the full-screen sideline paints under (the tab strip),
    /// so the click mappers invert the same offset the painter used.
    pub(super) fn sideline_top(&self) -> usize {
        // The strip row (`Agents  Messages`) owns the first painted row in
        // every view, so the content region starts one row down. Full-screen
        // composes below the tab strip, normal mode at the slice's row 0.
        if self.sideline_full {
            (TAB_BAR_ROWS as usize) + 1
        } else {
            1
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

    /// Sideline rows the cursor can occupy: the full terminal height (the
    /// sideline owns row 0 since US1) minus the bottom chrome row,
    /// minus the org block's rows at the bottom. The block is the
    /// subtraction point's only second customer, so `clamp_sideline_scroll`
    /// and `reveal_focus_row` inherit the shrunk window without a second
    /// fix.
    pub(super) fn sideline_visible_rows(&self) -> usize {
        // The sticky menu footer comes off the region before the scroll math
        // runs (h): scrolling to the end lands the last row above the footer.
        // The footer
        // only reserves a row it can spare - a region down to its last row
        // keeps that row as list, never as chrome (the org rule).
        let rows = (self.term.0 as usize)
            .saturating_sub(1) // the strip row
            .saturating_sub(self.bottom_row_is_chrome() as usize)
            .saturating_sub(self.org_block_rows());
        let pinned = self.painted_rows().len() > rows && rows >= 2;
        rows.saturating_sub(pinned as usize)
    }

    /// The `display_rows()` index a hover cell falls on in the sideline, or
    /// `None` when the cell is not a sideline text cell - a pane, the divider
    /// column, the tab bar, or the bottom chrome row. Mirrors [`chrome_hit`]'s
    /// sideline geometry exactly so the highlight lands where a click would
    ///.
    pub(super) fn sideline_row_at(&self, row: u16, col: u16) -> Option<usize> {
        // The board column paints its own surface and owns no agents display
        // rows: every resolver that answers "which sideline row is this"
        // (hover, right-click menu, drag pickup, press-hold) must answer
        // none there, or a press on the board acts on a phantom row.
        if self.sideline_view != crate::view_store::SidelineView::Agents {
            return None;
        }
        // The sideline owns row 0 in normal mode (the strip moved right of
        // the divider), so display row `i` maps directly from `row`. A cell
        // on the divider or in the strip's content columns returns None.
        // Sideline: the painted width minus its divider (the full terminal
        // in full-screen mode). Off/narrow => no panel.
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
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        // The sticky menu footer (h): when the rows overflow, the
        // menu/add-workspace row pins directly above the org block, so
        // a click or hover there is the footer's row even though its display
        // row has scrolled away. Checked ahead of the offset path: the
        // covered display row must never win. The pinned test reads the same
        // raw region `sideline_visible_rows` starts from, so a list that
        // exactly fits never reads as pinned here.
        let list_rows = (self.term.0 as usize)
            .saturating_sub(1) // the strip row
            .saturating_sub(self.org_block_rows());
        let raw_rows = list_rows.saturating_sub(self.bottom_row_is_chrome() as usize);
        let pinned = self.painted_rows().len() > raw_rows && raw_rows >= 2;
        if pinned && row as usize >= top && row as usize == top + list_rows.saturating_sub(2) {
            return self
                .painted_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::NewSquad));
        }
        let i = row as usize - top + self.sideline_offset();
        if i < self.painted_rows().len() {
            return Some(i);
        }
        None
    }

    /// The strip row's words at this panel's width. Below the width where
    /// the full words would crowd the bell and density buttons off the row,
    /// the tabs read `A  M` so both buttons keep their seat.
    pub(super) fn top_row_words(&self) -> [(&'static str, crate::view_store::SidelineView); 2] {
        // The full words end at column 19; the buttons need the bell label,
        // the density glyph, and a gap past that, so the full form pays off
        // only from here.
        const FULL_WORDS_MIN_W: usize = 30;
        let tw = self.sideline_paint_w().saturating_sub(1);
        if tw >= FULL_WORDS_MIN_W {
            [
                ("Agents", crate::view_store::SidelineView::Agents),
                ("Messages", crate::view_store::SidelineView::Messages),
            ]
        } else {
            [
                ("A", crate::view_store::SidelineView::Agents),
                ("M", crate::view_store::SidelineView::Messages),
            ]
        }
    }

    /// The strip row's words with their column spans, shared by the paint
    /// and the click map so the two cannot drift (R15). The toggle
    /// right-aligns in the strip (the operator's 2026-10-04 ask, after the
    /// bell moved to the tab bar): it ends before the density button's seat
    /// when one is reserved.
    pub(super) fn top_row_spans(&self) -> Vec<(usize, usize, crate::view_store::SidelineView)> {
        let words = self.top_row_words();
        let gap = 3usize;
        let total: usize = words.iter().map(|(w, _)| w.chars().count()).sum::<usize>()
            + gap * words.len().saturating_sub(1);
        let limit = self.sideline_paint_w().saturating_sub(1);
        let reserved = self
            .density_button_range(self.panel_w() as usize)
            .map_or(limit, |r| r.start.min(limit));
        // The floor yields to the density seat at degenerate widths: a word
        // pushed past `reserved` would paint under the button and lose its
        // click to the button's own range.
        let mut c = reserved.saturating_sub(total + 1).max(1);
        let mut out = Vec::new();
        for (word, view) in words {
            let w = word.chars().count();
            out.push((c, w, view));
            c += w + gap;
        }
        out
    }

    /// Paint the strip row at the slice's row 0: `Agents  Messages`
    /// right-aligned. The active word wears the brand accent with an
    /// underline and the resting word sits muted, so the pair reads as tabs
    /// (the operator's 2026-10-04 ruling) instead of bold-versus-plain.
    pub(super) fn paint_top_row(&self, cells: &mut [Cell], cols: usize, text_w: usize) {
        let limit = text_w.min(cols.saturating_sub(1));
        let rest_fg = crate::theme::dim_fg(&self.theme);
        for ((start, _, view), (word, _)) in
            self.top_row_spans().into_iter().zip(self.top_row_words())
        {
            let active = self.sideline_view == view;
            for (i, ch) in word.chars().enumerate() {
                let col = start + i;
                if col >= limit {
                    break;
                }
                cells[col] = Cell {
                    c: ch,
                    fg: if active { self.theme.brand } else { rest_fg },
                    bg: Color::Default,
                    flags: if active {
                        cell_flags::BOLD | cell_flags::UNDERLINE
                    } else {
                        0
                    },
                };
            }
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
                                  // The backlog view: the board's own render inside THIS column, no
                                  // second border, the cursor row wearing the sideline band. The
                                  // divider paints as in the agents view, then the agent path stops.
        if self.sideline_view != crate::view_store::SidelineView::Agents {
            // The strip row owns row 0 (R15) unless a board claims it: the
            // Org board paints rows 1 and below, the docked backlog board
            // frames its filter bar at row 0 and wins.
            self.paint_top_row(cells, cols, text_w);
            if self.sideline_view == crate::view_store::SidelineView::Org {
                if !self.board_full {
                    org_board::paint(
                        self,
                        &mut cells[cols..],
                        rows - 1,
                        cols,
                        text_w,
                        rows.saturating_sub(self.bottom_row_is_chrome() as usize) - 1,
                    );
                }
            } else if self.sideline_view == crate::view_store::SidelineView::Messages {
                // A full-surface view: the compose overlay paints it across
                // the whole terminal, so the column keeps only the strip.
            } else if let Some(b) = &self.backlog_board {
                if !self.board_full {
                    let chrome_rows = self.bottom_row_is_chrome() as usize;
                    backlog_board::backlog_panes::paint(
                        b,
                        cells,
                        rows,
                        cols,
                        (0, 1, rows - chrome_rows - 1, text_w),
                        self.input_owner() == super::region_focus::RegionOwner::Board,
                        &self.theme,
                    );
                }
            } else {
                let msg = BLine::meta("backlog off (pref)");
                let lines = [msg];
                backlog_style::paint_panel(
                    cells,
                    rows,
                    cols,
                    0,
                    text_w,
                    1,
                    &lines,
                    None,
                    &self.theme,
                );
            }
            let border_active = self.hover_sideline_border || self.sideline_drag.is_some();
            let (border_fg, border_flags) = if border_active {
                (self.theme.brand, cell_flags::BOLD)
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
            return;
        }
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
        // The new-agent composer is a centered sheet overlay now, in every
        // surface: the sideline paints untouched (the sheet is drawn by the
        // client's overlay chain), and the passive org block yields to an
        // active editor.
        let chrome_rows = self.bottom_row_is_chrome() as usize;
        let (block_rows, block_lines) = self.org_block_layout(rows);
        let list_rows = rows.saturating_sub(block_rows);
        // The scroll policy (`clamp_sideline_scroll`) keeps the cursor inside
        // the terminal minus the bottom chrome row; the widget area must
        // answer to the same height, or the render-time scroll lands the
        // selected row under the chrome that paints over it.
        let table_rows_n = list_rows.saturating_sub(chrome_rows).saturating_sub(1); // strip row
                                                                                    // The sticky menu row (h): when the rows overflow the region, the
                                                                                    // menu/add-workspace footer pins directly above the org block
                                                                                    // and the widget area gives up its last row, so the footer is never
                                                                                    // covered and the rows scroll to their true end above it. A region
                                                                                    // down to one row keeps that row as list, never as footer. The pinned
                                                                                    // copy yields while the in-list NewSquad row is inside the visible
                                                                                    // window - one instance of the label in every scroll state.
        let sticky_footer = table_rows_n > 1 && display.len() > table_rows_n;
        let table_h = table_rows_n.saturating_sub(sticky_footer as usize);
        // The widget renders into a standalone Buffer (no terminal, no
        // backend) and the blit copies it into the compositor's cells. The
        // org block and the dock own the rows below the list, so the
        // widget area stops above them. The dock paints into the SAME Buffer
        // before that one blit.
        let btn_reserved = self
            .density_button_range(panel_w)
            .map_or(text_w, |range| range.start);
        let area = RtRect::new(0, 0, text_w as u16, rows as u16);
        let mut buf = RtBuffer::empty(area);
        // The widget area is the top slice of the column; the dock paints
        // into the same Buffer below it, before the one blit. The strip row
        // is painted AFTER this blit (it owns row 0, which the blit wipes).
        let table_area = RtRect::new(0, 1, text_w as u16, table_h as u16);
        // The selector rides the TableState's `selected`, which is what the
        // widget's render-time scroll keeps visible.
        let mut st = self
            .sideline_state
            .get()
            .with_selected(self.list_selector());
        let mut off = st.offset();
        let rects = self.worker_column_rects(text_w as u16);
        if density != Density::Slim {
            let name_w = rects[1].width as usize;
            let table_rows: Vec<RtRow> = display
                .iter()
                .enumerate()
                .map(|(i, drow)| {
                    let depth = row_depths.get(i).copied().unwrap_or(0);
                    let slots = (name_w, rects[2].width as usize, rects[4].width as usize);
                    self.sideline_table_row(drow, depth, slots, now)
                })
                .collect();
            let table = RtTable::new(
                table_rows,
                self.worker_columns(text_w as u16).iter().copied(),
            )
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
        crate::ratatui_blit::blit(&buf, cells, cols);
        // The strip row owns row 0 (R15), so its words go down after the
        // one blit that would otherwise erase them.
        self.paint_top_row(cells, cols, text_w);
        // Per-row overlays the widget cannot express: the full-width rows
        // (bands, sublines, the idle fold, the footer, the empty state - see
        // the catch-all in `sideline_table_row`), the active-squad caret
        // accent, the row-scoped outcome stamp, and the selector / hover
        // bar. The list bar XORs INVERSE so a focused row's standing band
        // de-inverts under the cursor. Card mode clears the Table's selection
        // style; its Agent and CardDetail rows use one paired overlay here.
        for (i, drow) in display.iter().enumerate().skip(off) {
            let r = i - off + 1; // content paints under the strip row
            if r > table_h {
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
                // Card lines 2 and 3 carry one leading space: the edge bar
                // paints into it, so the bar never eats a text cell (line 1
                // already leads with its mark column).
                DisplayRow::CardDetail(a) => Some((
                    format!(
                        " {}",
                        self.card_detail_text(a, now, text_w.saturating_sub(1))
                    ),
                    cell_flags::DIM,
                )),
                DisplayRow::CardMetrics(a) => Some((
                    format!(
                        " {}",
                        card_line::metrics(
                            a,
                            now,
                            row_message_text(a).as_deref(),
                            text_w.saturating_sub(1),
                        )
                    ),
                    0,
                )),
                DisplayRow::Agent(a) if density == Density::Slim => {
                    // Small mode (q-334c5e9d option 1): one line per live
                    // agent - the animated glyph, then the slug, clipped to
                    // the 16-column rail. Exited rows never reach here:
                    // Slim drops them at the row fold.
                    let lat = agent_lattice_state(a);
                    Some((
                        crate::chrome::clip(
                            &format!(
                                "{} {}",
                                status_glyph(lat),
                                card_line::slug(a, &self.backlog)
                            ),
                            text_w,
                        ),
                        0,
                    ))
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
                if matches!(drow, DisplayRow::CardDetail(..)) {
                    // Card line 2 on a light terminal: DIM washes the default
                    // fg toward a light background until it vanishes. The
                    // theme's muted tone (the palette's own gray under the
                    // inherit theme) is tested at 4.5:1 against the base in
                    // both theme twins, so the line reads at rest.
                    let dim = crate::theme::dim_fg(&self.theme);
                    for cell in &mut cells[r * cols..r * cols + text_w] {
                        cell.fg = dim;
                        cell.flags &= !cell_flags::DIM;
                        cell.flags &= !cell_flags::BOLD;
                    }
                }
                if matches!(drow, DisplayRow::Agent(_)) && density == Density::Slim {
                    // The slim rail's agent lines wear the lattice color.
                    if let DisplayRow::Agent(a) = drow {
                        let fg = lattice_style(agent_lattice_state(a), self.theme.needs_you).fg;
                        for cell in &mut cells[r * cols..r * cols + text_w] {
                            cell.fg = fg;
                        }
                    }
                }
            }
            if mark_caret && text_w >= 1 {
                cells[r * cols].fg = self.theme.brand;
            }
            if matches!(drow, DisplayRow::NewSquad) {
                self.paint_new_squad_footer(cells, r, cols, text_w, panel_w);
            }
            let chosen = matches!(drow, DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus) && !a.exited);
            let mut highlit =
                chosen || self.list_selector() == Some(i) || self.hover_row == Some(i);
            if card {
                highlit = self.card_pair_highlit(&display, i, highlit);
            }
            let card_pair = card
                && matches!(
                    drow,
                    DisplayRow::Agent(_) | DisplayRow::CardDetail(..) | DisplayRow::CardMetrics(..)
                );
            let card_chosen = card_pair
                && matches!(drow, DisplayRow::Agent(a) | DisplayRow::CardDetail(a) | DisplayRow::CardMetrics(a)
                    if a.pane_id == Some(self.layout.focus) && !a.exited);
            if card_chosen {
                // The chosen card fills all 3 lines, full width, hover
                // included: the chosen fill wins on hover.
                highlit = true;
            }
            if highlit && card_chosen {
                // The chosen card fills all 3 lines with the theme accent
                // plus a left bar (the operator's 2026-10-04 sideline
                // ruling): the fill wins on hover, and no lane accent rides
                // over it. The bar paints after the fill so the fill's own
                // bg backs it.
                let (fg, bg, flags) = crate::theme::chosen_card_style(&self.theme);
                for cell in &mut cells[r * cols..r * cols + text_w] {
                    cell.bg = bg;
                    cell.fg = fg;
                    cell.flags = flags;
                }
                if text_w >= 1 && cells[r * cols].c != '*' {
                    cells[r * cols].c = CARD_EDGE_BAR;
                }
            } else if highlit {
                // One solid band across the full width of the row, gaps
                // included: an explicit background and an explicit fg picked
                // to contrast with it, never per-span inversion, so a span's
                // own color cannot patch the highlight and the band reads the
                // same on a dark and a light terminal.
                let (fg, bg, flags) = crate::theme::band_style(&self.theme);
                for cell in &mut cells[r * cols..r * cols + text_w] {
                    cell.bg = bg;
                    cell.fg = fg;
                    cell.flags = flags;
                }
                // The state accent survives the band on the glyph (and the
                // list's state word) only (the operator's color ruling); the
                // band owns the remaining cells. A row with no colored state
                // (Default) keeps the band's own explicit pair everywhere.
                if let DisplayRow::Agent(a) = drow {
                    let lat = agent_lattice_state(a);
                    let style = lattice_style(lat, self.theme.needs_you);
                    let accent = if a.pane_id == Some(self.layout.focus) && a.exited {
                        self.theme.brand
                    } else {
                        agent_lane_fg(a, lat, style.fg)
                    };
                    if accent != Color::Default {
                        let end = rects[0].x.saturating_add(rects[0].width).min(text_w as u16);
                        for col in rects[0].x..end {
                            cells[r * cols + col as usize].fg = accent;
                        }
                    }
                }
            }
            let row_stamp = self.row_stamp_for(drow);
            paint_row_stamp(cells, r, cols, text_w, row_stamp);
            if card {
                if card_pair && !card_chosen && !highlit && text_w >= 1 && cells[r * cols].c != '*'
                {
                    // The edge bar brackets every resting card's 3 lines, so
                    // adjacent cards read as separate blocks (the operator's
                    // 2026-10-04 separation ask). The chosen card's bar came
                    // with its fill, a highlight supersedes the bar, and a
                    // recruit-marked line 1 keeps its star.
                    cells[r * cols].c = CARD_EDGE_BAR;
                    cells[r * cols].fg = crate::theme::dim_fg(&self.theme);
                }
                self.paint_card_identity(cells, r, cols, text_w, drow);
            }
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
        // The org block: three glance lines minimized, the full
        // reading expanded, pinned to the bottom rows of the column. The row
        // list already stopped above it; the block renders DIM so it reads as
        // chrome beside the live rows, and the painter truncates to the panel
        // width - the same rule every sideline row follows.
        // The sticky menu footer rides directly above the org block when
        // the rows overflow (h). Its pinned copy yields while the in-list
        // NewSquad row is inside the visible window.
        let new_squad_visible = display
            .iter()
            .skip(off)
            .take(table_h)
            .any(|d| matches!(d, DisplayRow::NewSquad));
        if sticky_footer && !new_squad_visible {
            self.paint_new_squad_footer(cells, list_rows - 1, cols, text_w, panel_w);
        }
        org_block::paint_org_block(cells, block_lines, list_rows, rows, cols, text_w);
        // The divider column, now full terminal height (the sideline owns row
        // 0 too; the strip sits right of the divider) - US1.
        //
        // Accent it while hovered or dragged, the same signal a pane
        // seam wears. A terminal cannot change the cursor shape, so this
        // accent IS the affordance.
        let border_active = self.hover_sideline_border || self.sideline_drag.is_some();
        let (border_fg, border_flags) = if border_active {
            (self.theme.brand, cell_flags::BOLD)
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
        (name_w, _meter_w, _right_slot_w): (usize, usize, usize),
        now: u64,
    ) -> RtRow<'static> {
        let card = self.sideline_layout == sideline_color::SidelineLayout::Card;
        // An EXITED focus row is legibly dead: DIM accent on its cells and
        // no band (a dead "you are here" never reads as a live one). A live
        // focus row's band is the overlay's accent highlight.
        let is_focus = matches!(drow, DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus));
        let focus_exited = is_focus && matches!(drow, DisplayRow::Agent(a) if a.exited);
        let (mut row_cells, _): (Vec<RtCell>, u8) = match drow {
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
            | DisplayRow::Blank
            | DisplayRow::TableEmpty
            | DisplayRow::CardDetail(..)
            | DisplayRow::CardMetrics(..)
            | DisplayRow::IdleFold { .. } => {
                (vec![rt_cell(String::new(), Color::Default, 0, false); 5], 0)
            }
            DisplayRow::Agent(a) => {
                let lat = agent_lattice_state(a);
                let style = lattice_style(lat, self.theme.needs_you);
                let mut flags = style.flags;
                if a.external && lat != LatticeState::Blocked {
                    flags |= cell_flags::DIM;
                }
                let status_fg = agent_lane_fg(a, lat, style.fg);
                // An EXITED focus row is legibly dead: DIM accent, and the
                // overlay gives it no band. A live focus row's band is the
                // overlay's highlight; the cells stay ordinary. The
                // state/lane accent rides the glyph and the state word only
                // (the operator's color ruling) - the remaining cells read
                // default.
                let focus_bit = if focus_exited { cell_flags::DIM } else { 0 };
                let cell_fg = if focus_exited {
                    self.theme.brand
                } else {
                    status_fg
                };
                let body_fg = Color::Default;
                let cell_flags_v = flags | focus_bit;
                // The name cell keeps the compact row's identity vocabulary:
                // recruit mark, DND, deviation token, portal index, tab
                // context, orphan cwd, reason, team badge - depth-indented,
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
                // A card names its model on line 2; the token is the list's.
                if let Some(tok) =
                    sideline_color::deviation_token(a.harness.as_deref(), a.model.as_deref())
                        .filter(|_| !card)
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
                        // A squad-less row's cwd base disambiguates paneless
                        // agents launched from arbitrary directories - unless
                        // the worktree IS the agent's own node worktree, where
                        // the basename repeats the node id and prints noise.
                        // Squad members never read cwd here.
                        if a.squad.is_none() {
                            if let Some(base) = a
                                .cwd_base
                                .as_deref()
                                .filter(|b| Some(*b) != a.node.as_deref())
                            {
                                suffix.push_str(&format!(" ({base})"));
                            }
                        }
                    }
                }
                if let Some(reason) = a.reason.as_deref().filter(|x| !x.is_empty()) {
                    suffix.push_str(": ");
                    suffix.push_str(reason);
                }
                let prefix_width = prefix.width();
                let suffix_width = suffix.width();
                let base_width = name_w.saturating_sub(prefix_width);
                let label = if card {
                    card_line::slug(a, &self.backlog)
                } else {
                    a.name.clone()
                };
                // (US3, inline) A member with a different project or worktree
                // path shows it inline in parens after the slug or name, only
                // when it fits whole after the label; otherwise it drops. The
                // dim `Sub` line under the row is gone (d-36438ea4).
                let label = match self.foreign_base(a) {
                    Some(base) => {
                        let tagged = format!("{label} ({base})");
                        if crate::chrome::str_cols(&tagged) <= base_width {
                            tagged
                        } else {
                            label
                        }
                    }
                    None => label,
                };
                let name = if suffix_width < base_width {
                    let base = crate::chrome::clip_tail(&label, base_width - suffix_width);
                    format!("{prefix}{base}{suffix}")
                } else {
                    let base = crate::chrome::clip_tail(&label, base_width);
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
                let pr =
                    a.pr.map(|n| format!("#{n}"))
                        .unwrap_or_else(|| "\u{2014}".into());
                let age = row_age(a, now);
                let (pr_cell, age_cell) = if card {
                    (String::new(), String::new())
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
                        // Card line 1: glyph, slug, context bar and node, PR.
                        // The spinning glyph carries the state, so no word.
                        // List mode paints the exact pre-card cells.
                        // A Working row's spin glyph takes the blank lead
                        // column of the right-aligned word, so the word stays put.
                        rt_cell(
                            match (card, lat) {
                                (true, _) => status_glyph(lat).to_string(),
                                (false, LatticeState::Working) => {
                                    format!("{}{}", status_glyph(lat), status_word(lat))
                                }
                                (false, _) => status_word(lat).to_string(),
                            },
                            cell_fg,
                            cell_flags_v,
                            true,
                        ),
                        rt_cell(
                            crate::chrome::clip(&name, name_w),
                            body_fg,
                            cell_flags_v,
                            false,
                        ),
                        rt_cell(
                            if card { String::new() } else { tail },
                            body_fg,
                            quiet | focus_bit,
                            false,
                        ),
                        rt_cell(pr_cell, body_fg, quiet | focus_bit, true),
                        rt_cell(age_cell, body_fg, quiet | focus_bit, true),
                    ],
                    0,
                )
            }
            DisplayRow::TableHead => {
                let arrow = |column: AgentSortColumn| match self.agent_sort {
                    sort if sort.column != column => "",
                    sort if sort.direction == SortDirection::Ascending => "\u{2191}",
                    _ => "\u{2193}",
                };
                let cells = head_sorts(card).map(|sort| {
                    let text = match sort {
                        None => if card { "node · PR" } else { "ctx · node" }.to_string(),
                        // No space before the age arrow: right-aligned or
                        // spaced, it sits under the density button's two
                        // overlay columns and the toggle reads dead.
                        Some(AgentSortColumn::Age) => format!("age{}", arrow(AgentSortColumn::Age)),
                        Some(c) if arrow(c).is_empty() => head_label(c).to_string(),
                        Some(c) => format!("{} {}", head_label(c), arrow(c)),
                    };
                    let right = sort == Some(AgentSortColumn::Status);
                    rt_cell(text, Color::Default, cell_flags::DIM, right)
                });
                (cells.to_vec(), 0)
            }
        };
        if self
            .worker_columns(self.sideline_paint_w().saturating_sub(1) as u16)
            .len()
            == 7
        {
            let (ctx, up) = match drow {
                DisplayRow::Agent(a) => (
                    row_meter::ctx_cell(a.context_used_pct),
                    row_meter::up_cell(a.started_at, now),
                ),
                DisplayRow::TableHead => ("ctx".into(), "up".into()),
                _ => (String::new(), String::new()),
            };
            row_cells.push(rt_cell(ctx, Color::Default, cell_flags::DIM, false));
            row_cells.push(rt_cell(up, Color::Default, cell_flags::DIM, false));
        }
        RtRow::new(row_cells)
    }

    /// Expand cards into exactly three painted rows; list mode is untouched.
    pub(super) fn card_rows<'a>(
        &self,
        rows: Vec<DisplayRow<'a>>,
        depths: Vec<usize>,
    ) -> (Vec<DisplayRow<'a>>, Vec<usize>) {
        if self.sideline_layout != sideline_color::SidelineLayout::Card {
            return (rows, depths);
        }
        let mut out_rows = Vec::with_capacity(rows.len() * 3);
        let mut out_depths = Vec::with_capacity(rows.len() * 3);
        for (row, depth) in rows.into_iter().zip(depths) {
            match row {
                DisplayRow::Agent(a) => {
                    out_rows.extend([
                        DisplayRow::Agent(a),
                        DisplayRow::CardDetail(a),
                        DisplayRow::CardMetrics(a),
                    ]);
                    out_depths.extend([0, 0, 0]);
                }
                DisplayRow::Blank => {
                    out_rows.push(DisplayRow::Blank);
                    out_depths.push(depth);
                }
                other => {
                    out_rows.push(other);
                    out_depths.push(depth);
                }
            }
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
            Some(range) => format!(
                "{}{FOOTER_MENU}",
                pad_to(&crate::chrome::clip(&base, range.start), range.start)
            ),
            None => base,
        };
        paint_legacy_row(cells, r, cols, text_w, &label, cell_flags::BOLD);
    }

    fn paint_card_identity(
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
        let line = &mut cells[row * cols..row * cols + text_w];
        let lattice = agent_lattice_state(agent);
        let status_fg = if agent.pane_id == Some(self.layout.focus) && agent.exited {
            self.theme.brand
        } else {
            agent_lane_fg(
                agent,
                lattice,
                lattice_style(lattice, self.theme.needs_you).fg,
            )
        };
        let spans = card_line::identity_spans(agent, text_w);
        // On the chosen card the accent fill owns the line: the node and PR
        // spans read in the fill's base tone (brand-on-brand text would
        // vanish - the composed contrast test pins this at 3:1).
        let (fill_fg, _, _) = crate::theme::chosen_card_style(&self.theme);
        let on_chosen_fill = agent.pane_id == Some(self.layout.focus) && !agent.exited;
        if let (Some(node), Some(span)) = (agent.node.as_deref(), spans.node) {
            let node_start = span.start;
            for (cell, ch) in line[span].iter_mut().zip(node.chars()) {
                cell.c = ch;
                cell.fg = if on_chosen_fill { fill_fg } else { status_fg };
                cell.flags = cell_flags::BOLD;
            }
            // The name yields before the node: an ellipsis marks the cut
            // and one space keeps the gap. Under three columns
            // there is no room for both, so the name side blanks past the
            // glyph and the node stands alone.
            if node_start >= 3 {
                if line[node_start - 1].c != ' ' {
                    line[node_start - 1].c = '…';
                }
                if line[node_start - 2].c != ' ' {
                    line[node_start - 2].c = ' ';
                }
            } else {
                for cell in line[1..node_start.max(1)].iter_mut() {
                    cell.c = ' ';
                }
            }
        }
        if let Some(span) = spans.separator {
            for (cell, ch) in line[span].iter_mut().zip(" · ".chars()) {
                cell.c = ch;
            }
        }
        if let Some(span) = spans.pr {
            for (cell, ch) in line[span]
                .iter_mut()
                .zip(format!("#{}", agent.pr.unwrap()).chars())
            {
                cell.c = ch;
                cell.fg = if on_chosen_fill {
                    fill_fg
                } else {
                    self.theme.brand
                };
                cell.flags = 0;
            }
        }
    }

    /// Line 2 keeps model and lead left, with created and activity ages right.
    pub(super) fn card_detail_text(&self, a: &AgentRow, now: u64, text_w: usize) -> String {
        let mut segments: Vec<String> = Vec::new();
        if let Some(model) = card_line::model_label(a) {
            segments.push(model);
        }
        if let Some(k) = self.lead_label(a) {
            segments.push(k);
        }
        let left = segments.join(" \u{b7} ");
        let created = a
            .started_at
            .map(|_| row_meter::up_cell(a.started_at, now))
            .unwrap_or_else(|| "–".into());
        let activity = if a.last_activity_age_s.is_some() || a.updated_at.is_some() {
            row_age(a, now).trim().to_string()
        } else {
            "–".into()
        };
        let tail = format!("{created} · {activity}");
        let tail_w = crate::chrome::str_cols(&tail).min(text_w);
        let head_w = text_w.saturating_sub(tail_w);
        let head = crate::chrome::fit_ellipsis(&left, head_w);
        let pad = " ".repeat(head_w.saturating_sub(crate::chrome::str_cols(&head)));
        format!("{head}{pad}{tail}")
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
            Some(DisplayRow::CardDetail(..) | DisplayRow::CardMetrics(..)) => {
                base || self.list_selector() == Some(i)
                    || self.hover_row == Some(i)
                    || self.list_selector() == Some(i.saturating_sub(1))
                    || self.hover_row == Some(i.saturating_sub(1))
                    || self.list_selector() == Some(i.saturating_sub(2))
                    || self.hover_row == Some(i.saturating_sub(2))
            }
            Some(DisplayRow::Agent(_)) => {
                base || matches!(display.get(i + 1), Some(DisplayRow::CardDetail(..)))
                    && (self.list_selector() == Some(i + 1) || self.hover_row == Some(i + 1))
                    || matches!(display.get(i + 2), Some(DisplayRow::CardMetrics(..)))
                        && (self.list_selector() == Some(i + 2) || self.hover_row == Some(i + 2))
            }
            _ => base,
        }
    }

    /// (US3, inline) The foreign-cwd base an agent shows inline in parens:
    /// `Some` only when the agent's cwd basename differs from its squad's
    /// project basename. The dim `Sub` row's join, moved into the label.
    pub(super) fn foreign_base<'a>(&self, a: &'a AgentRow) -> Option<&'a str> {
        let squad_id = a.squad?;
        let squad = self.layout.squads.iter().find(|s| s.id == squad_id)?;
        let base = super::section_project_base(&squad.canonical_cwd);
        if super::agent_is_foreign(a, base) {
            a.cwd_base.as_deref()
        } else {
            None
        }
    }

    /// The lead label for line 2 of a card: the teamed row itself shows its
    /// people title, else its team scope; a worker walks its lineage to the
    /// first teamed ancestor and shows [`team_display_name`]. No teamed
    /// ancestor, or a lineage cycle (capped at one step per agent), labels
    /// nothing.
    pub(super) fn lead_label(&self, a: &AgentRow) -> Option<String> {
        if a.crown_level.is_some() {
            if let Some(title) = a.crown_title.as_deref().filter(|t| !t.is_empty()) {
                return Some(title.to_string());
            }
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
                return Some(team_display_name(row).to_string());
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

/// The name a teamed row shows on its workers' cards. Today the lead's
/// handle; the team's own name replaces it when the wire carries one.
fn team_display_name(lead: &AgentRow) -> &str {
    &lead.name
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
            if rep.row + 1 == top {
                // The strip row: only its words act (R15).
                let pw = view.sideline_paint_w().max(1) as u16;
                let col = rep.col.min(pw.saturating_sub(2));
                if let Some(hit) = view.chrome_hit(rep.row, col) {
                    apply_hit(view, hit, sock_w).await?;
                }
                return Ok(());
            }
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
    if show.term.0 < 12 {
        if was_none {
            agent_launcher::close(show);
        }
        show.set_notice("terminal too short for the composer".into());
        return Ok(false);
    }
    Ok(true)
}

/// Full-screen is a display toggle: it flips `sideline_full` and reveals a
/// hidden sideline on the way in. The composer stays owned by prefix+i
/// (`toggle-composer`); F no longer opens it. Leaving leaves the composer
/// as it is; content_dims never changed, so no Resize travels in either
/// direction.
pub(super) fn toggle_full(view: &mut View) {
    view.sideline_full = !view.sideline_full;
    if view.sideline_full {
        view.panel_on = true;
    }
}
