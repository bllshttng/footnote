//! The sideline column's paint: the agent Table, the per-row overlays, the
//! docked new-agent composer, the court block and the divider. One Buffer,
//! one blit. Draw_sideline and its two exclusive helpers live here; the
//! file-budget gate names this module the answer to "how does the sideline
//! column paint".

use super::*;

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
        // The docked new-agent composer takes the bottom rows while open,
        // and the passive court block yields to it: an active editor
        // outranks glance chrome.
        let chrome_rows = self.bottom_row_is_chrome() as usize;
        let dock_len = self
            .launcher
            .as_ref()
            .map_or(0, |l| l.dock_layout(rows - chrome_rows, text_w).0);
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
                    self.sideline_table_row(drow, depth, name_w, now)
                })
                .collect();
            let table = RtTable::new(table_rows, SIDELINE_COLUMNS)
                .flex(Flex::Start)
                .highlight_spacing(HighlightSpacing::Never);
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
        // bar. The bar XORs INVERSE - a focused row's standing band
        // de-inverts under the cursor - and ratatui's patch-based highlight
        // can only add a modifier, never subtract one, so the bar lands
        // here, after the blit, on the same cells the old painter wrote.
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
            }
            if mark_caret && text_w >= 1 {
                cells[r * cols].fg = self.theme.accent;
            }
            if matches!(drow, DisplayRow::NewSquad) {
                self.paint_new_squad_footer(cells, r, cols, text_w, panel_w);
            }
            let highlit =
                !row_is_inert(drow) && (self.selector == Some(i) || self.hover_row == Some(i));
            if highlit {
                for j in 0..text_w {
                    cells[r * cols + j].flags ^= cell_flags::INVERSE;
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
        now: u64,
    ) -> RtRow<'static> {
        // The focused pane's owning row is the sole standing full-width
        // INVERSE band. An EXITED focused row is legibly dead: DIM accent
        // instead of the bright band (a dead "you are here" never reads as a
        // live one).
        let is_focus = matches!(drow, DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus));
        let focus_exited = matches!(
            drow,
            DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus) && a.exited
        );
        let (row_cells, band): (Vec<RtCell>, u8) = match drow {
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
                // The focused row's band carries INVERSE (or DIM when
                // exited) and the accent ON the cells - the render patches
                // span styles over row/cell styles, so anything not on the
                // line itself is overpainted by the cell's own fg.
                let focus_bit = if is_focus {
                    if focus_exited {
                        cell_flags::DIM
                    } else {
                        cell_flags::INVERSE
                    }
                } else {
                    0
                };
                let cell_fg = if is_focus {
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
                let dnd = if a.dnd { " [DND]" } else { "" };
                let mut name = if depth > 0 {
                    format!("{}{mark} {}", "  ".repeat(depth), a.name)
                } else {
                    format!("{mark} {}", a.name)
                };
                name.push_str(dnd);
                if let Some(tok) =
                    sideline_color::deviation_token(a.harness.as_deref(), a.model.as_deref())
                {
                    name.push_str(&format!(" {tok}"));
                }
                if let Some(idx) = a.portal {
                    name.push_str(&format!(" \u{25ab}{idx}"));
                }
                match self.agent_tab_context(a.squad, a.tab) {
                    Some(_)
                        if a.squad == Some(self.layout.active_squad)
                            && a.tab == self.active_squad_active_tab_id() => {}
                    Some(TabContext::Named(ctx)) => name.push_str(&format!(" \u{b7}{ctx}")),
                    Some(TabContext::Ordinal(ord)) => name.push_str(&format!(" \u{b7}{ord}")),
                    None => {
                        if a.squad.is_none() {
                            if let Some(base) = a.cwd_base.as_deref() {
                                name.push_str(&format!(" ({base})"));
                            }
                        }
                    }
                }
                if let Some(reason) = a.reason.as_deref().filter(|x| !x.is_empty()) {
                    name.push_str(": ");
                    name.push_str(reason);
                }
                if let Some(level) = a.crown_level {
                    let scope = a.crown_scope.as_deref().unwrap_or("?");
                    name.push_str(&format!(" [L{level} {scope}]"));
                }
                // The message column reads the sentence, not the markup, and
                // leads with the separator. A PR row with no output names the
                // session driving it (the server's graph join: the live claim
                // holder's session, else the node's last do/ship session); no
                // session id says so. The widest column keeps the handle
                // visible where the old inline suffix clipped.
                let tail = match a.tail.as_deref().filter(|t| !t.is_empty()) {
                    Some(t) => format!("\u{b7} {}", strip_md(t)),
                    None => match (a.pr, a.pr_session_short.as_deref()) {
                        (Some(_), Some(sid)) => format!("\u{b7} attach {sid}"),
                        (Some(_), None) => "\u{b7} no session".to_string(),
                        (None, _) => String::new(),
                    },
                };
                let pr =
                    a.pr.map(|n| format!("#{n}"))
                        .unwrap_or_else(|| "\u{2014}".into());
                let age = match (a.last_activity_age_s, a.updated_at) {
                    (Some(s), _) => humanize_age(Some(s)),
                    (None, Some(u)) => humanize_age(Some(now.saturating_sub(u))),
                    (None, None) => humanize_age(None),
                };
                let quiet = if flags & cell_flags::DIM != 0 {
                    cell_flags::DIM
                } else {
                    0
                };
                (
                    vec![
                        // Right-aligned: a short word's blank parks against
                        // the margin, so the word sits one spacing column
                        // from the name instead of mid-cell.
                        rt_cell(status_word(lat).to_string(), cell_fg, cell_flags_v, true),
                        rt_cell(fit_ellipsis(&name, name_w), cell_fg, cell_flags_v, false),
                        rt_cell(tail, cell_fg, quiet | focus_bit, false),
                        rt_cell(pr, cell_fg, quiet | focus_bit, true),
                        rt_cell(age, cell_fg, quiet | focus_bit, true),
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
        // The focused row's band: INVERSE (or DIM when exited) with the
        // accent carried across every cell and the band's padding, so the
        // whole row is one colour.
        let mut row_style = RtStyle::new();
        if is_focus {
            let focus_flags = if focus_exited {
                cell_flags::DIM
            } else {
                cell_flags::INVERSE
            };
            row_style = row_style
                .fg(rt_color(self.theme.accent))
                .add_modifier(rt_modifier(focus_flags));
        }
        if band != 0 {
            row_style = row_style.add_modifier(rt_modifier(band));
        }
        RtRow::new(row_cells).style(row_style)
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

/// Toggle the composer: close retains the draft, as Esc does. Opening shows
/// the sideline first when hidden, so the composer is never open and
/// unpainted; a terminal too narrow to admit the rail opens nothing and
/// says so.
pub(super) async fn toggle_composer(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    if view.launcher.is_some() {
        agent_launcher::close(view);
    } else {
        let was_on = view.panel_on;
        view.panel_on = true;
        if view.panel_w() == 0 {
            view.panel_on = was_on;
            view.set_notice("terminal too narrow for the composer".into());
        } else {
            if !was_on {
                let (r, c) = view.content_dims();
                write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                    .await
                    .map_err(|e| format!("resize send failed: {e}"))?;
            }
            agent_launcher::open(view);
        }
    }
    Ok(())
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
