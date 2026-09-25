//! The pane paint pass, moved out of client.rs `compose_at` under the
//! file-budget ratchet : the blit, the pane frames and their edge
//! fields, the link underline, grips, the scroll indicator, the letterbox +
//! divider pass, the empty state, the drop band, and the pane-id reveal.

use super::*;

impl View {
    pub(super) fn paint_panes(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        origin_r: usize,
        origin_c: usize,
        now: Instant,
    ) {
        // Content area: dividers first (uncovered cells), panes blitted over.
        let mut covered = vec![false; rows * cols];
        // cells owned by the focused pane, so the divider pass can accent
        // the seams that bound it (a standing "you are here" outline).
        let mut focused = vec![false; rows * cols];
        // cells owned by a FRAMED pane, so the divider pass can leave the
        // gap between two frames blank instead of a divider glyph.
        let mut framed_cell = vec![false; rows * cols];
        for (pid, rect) in &self.layout.panes {
            let frame = self.frames.get(pid);
            let content = crate::pane_border::content_rect(*rect);
            let is_framed = crate::pane_border::framed(*rect);
            for fr in 0..rect.rows as usize {
                let r = origin_r + rect.y as usize + fr;
                if r >= rows {
                    break;
                }
                for fc in 0..rect.cols as usize {
                    let c = origin_c + rect.x as usize + fc;
                    if c >= cols {
                        break;
                    }
                    covered[r * cols + c] = true;
                    if *pid == self.layout.focus {
                        focused[r * cols + c] = true;
                    }
                    if is_framed {
                        framed_cell[r * cols + c] = true;
                    }
                }
            }
            // The pty grid is the CONTENT rect: it blits one cell in, inside
            // the frame, never under it. Clamped to the content dims so a
            // stale full-rect frame in flight cannot overwrite the ring.
            if let Some(f) = frame {
                let frows = (f.rows as usize).min(content.rows as usize);
                let fcols = (f.cols as usize).min(content.cols as usize);
                for fr in 0..frows {
                    let r = origin_r + content.y as usize + fr;
                    if r >= rows {
                        break;
                    }
                    for fc in 0..fcols {
                        let c = origin_c + content.x as usize + fc;
                        if c >= cols {
                            break;
                        }
                        cells[r * cols + c] = f.cells[fr * f.cols as usize + fc];
                    }
                }
            }
        }
        // Pane frames: after the blit (border wins its own ring), before the
        // underline/grips (affordances paint over the frame).
        for (pid, rect) in &self.layout.panes {
            self.paint_frame(cells, rows, cols, origin_r, origin_c, *pid, *rect);
        }
        // (hover affordance) Underline the accepted link span, client-local:
        // OR UNDERLINE into exactly the pane-local cells the server named,
        // after the blit (content is in place) and before the grip/indicator/
        // overlay passes (chrome and overlays still win their cells). The
        // cached server `Frame` is untouched, so the span clears on the next
        // compose the moment it is dropped or invalidated. Suppressed while
        // ANY modal is open: `menu_usurping_open` names the full set, which
        // closes the keyboard-opened and overlay-blind cases the pointer-event
        // clear cannot see (an overlay opened with no pointer motion paints no
        // hover affordance beneath or around itself).
        if !self.menu_usurping_open() {
            if let Some((pid, span)) = self.link_hover.accepted.as_ref() {
                if let Some((_, rect)) = self.layout.panes.iter().find(|(p, _)| p == pid) {
                    let content = crate::pane_border::content_rect(*rect);
                    for &(fr, fc) in span {
                        let r = origin_r + content.y as usize + fr as usize;
                        let c = origin_c + content.x as usize + fc as usize;
                        if r < rows && c < cols {
                            cells[r * cols + c].flags |= cell_flags::UNDERLINE;
                        }
                    }
                }
            }
        }
        // pane grips, drawn on cells the pane owns, hence after the blit
        // - but BEFORE the scroll indicator, which is state rather than an
        // affordance and so wins the cells they contend for on a narrow pane. The dragged pane's own grip stays lit for the
        // whole gesture, which is what keeps the origin marked (AC3-UI) once
        // the pointer has run off to a zone somewhere else.
        let dragged = self.pane_drag.map(|d| d.mover);
        // Hidden on a single-pane tab, matching `grip_at`: no grip is drawn
        // where none can be pressed.
        for (pid, rect) in self
            .layout
            .panes
            .iter()
            .filter(|_| self.layout.panes.len() >= 2)
        {
            let Some((grow, gcols)) = self.grip_span(*rect) else {
                continue;
            };
            let lit = dragged == Some(*pid) || (dragged.is_none() && self.hover_grip == Some(*pid));
            let (fg, flags) = if lit {
                (self.theme.accent, cell_flags::BOLD)
            } else {
                (Color::Default, cell_flags::DIM)
            };
            let (r, start, end) = (grow as usize, gcols.start as usize, gcols.end as usize);
            if r >= rows {
                continue;
            }
            blank_straddling_pair(cells, cols, r, start, end);
            for (i, ch) in GRIP.chars().enumerate() {
                let c = start + i;
                if c < cols {
                    cells[r * cols + c] = Cell {
                        c: ch,
                        fg,
                        bg: Color::Default,
                        flags,
                    };
                }
            }
        }

        // Scroll indicator (US1, AC1-UI): a minimal `[+N]` at a scrolled pane's
        // top-right, inverse-video so it reads over content. Present iff the
        // pane's frame reports a non-zero offset (group 2's status row becomes
        // its canonical home). A pane too narrow to fit the label skips it.
        // A framed pane anchors it on the first CONTENT row, so the top
        // edge's status field keeps its cells.
        for (pid, rect) in &self.layout.panes {
            let Some(f) = self.frames.get(pid) else {
                continue;
            };
            if f.scroll_offset == 0 {
                continue;
            }
            let label = format!("[+{}]", f.scroll_offset);
            let w = label.chars().count();
            let content = crate::pane_border::content_rect(*rect);
            let is_framed = crate::pane_border::framed(*rect);
            let anchor = if is_framed { content } else { *rect };
            let r = origin_r + anchor.y as usize;
            if (anchor.cols as usize) < w || r >= rows {
                continue;
            }
            let start_c = origin_c + anchor.x as usize + anchor.cols as usize - w;
            for (k, ch) in label.chars().enumerate() {
                let c = start_c + k;
                if c < cols {
                    cells[r * cols + c] = Cell {
                        c: ch,
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::INVERSE,
                    };
                }
            }
        }
        // Letterbox (AC1-UI): the server tiled its rects into `Layout.area`
        // (the view-scoped clamp); content anchors top-left and everything
        // beyond `area` up to the local content edge is visibly-inert dim
        // filler, never divider glyphs. `(0, 0)` is the pre-Layout
        // placeholder: no filler until the first real Layout names a bound.
        let (a_rows, a_cols) = self.layout.area;
        let boxed = self.layout.area != (0, 0);
        // A drag keeps the accent on the seam it grabbed even as the pointer
        // runs ahead of it, so the thing being moved stays the thing lit.
        let active_seam = self.seam_drag.map(|d| d.seam).or(self.hover_seam);
        // the candidate drop zone, lit while a relocation drag is live.
        // The tab-cell and sideline-row drags reuse the SAME content-edge
        // zone vocabulary, so one of the three lights the seam/band identically.
        let drop_zone = self
            .pane_drag
            .and_then(|d| d.zone)
            .or_else(|| self.tab_drag.and_then(|d| d.zone))
            .or_else(|| self.row_drag.as_ref().and_then(|d| d.zone));
        // Divider glyphs for in-area content cells no pane covers: pick by
        // which neighbors are panes so vertical strips read '│', horizontal
        // '─', crossings '┼'. Dim so chrome never shouts over content. The
        // gap between two FRAMES reads blank: each pane is its own box.
        for r in origin_r..rows {
            for c in origin_c..cols {
                if covered[r * cols + c] {
                    continue;
                }
                if boxed && (r - origin_r >= a_rows as usize || c - origin_c >= a_cols as usize) {
                    cells[r * cols + c] = Cell {
                        c: '·',
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::DIM,
                    };
                    continue;
                }
                let framed_by =
                    |rr: usize, cc: usize| rr < rows && cc < cols && framed_cell[rr * cols + cc];
                let beside_framed = framed_by(r, c - 1)
                    || framed_by(r, c + 1)
                    || framed_by(r - 1, c)
                    || framed_by(r + 1, c);
                // One-sided framing keeps the divider: a narrow unframed pane
                // beside a framed one still owns its seam glyph (AC7-EDGE);
                // only a gap BETWEEN frames reads blank.
                let covered_by =
                    |rr: usize, cc: usize| rr < rows && cc < cols && covered[rr * cols + cc];
                let has_unframed_nb = (covered_by(r, c - 1) && !framed_cell[r * cols + c - 1])
                    || (covered_by(r, c + 1) && !framed_cell[r * cols + c + 1])
                    || (covered_by(r - 1, c) && !framed_cell[(r - 1) * cols + c])
                    || (covered_by(r + 1, c) && !framed_cell[(r + 1) * cols + c]);
                // a divider cell that borders the focused pane paints in
                // the lattice accent at full brightness (not the DIM chrome), so
                // the focused pane wears a standing outline that moves with focus.
                // Interior seams only - an edge pane has no divider on that side.
                // Orthogonal neighbours suffice: a `┼` is emitted only when a cell
                // has a covered horizontal AND vertical neighbour, so every
                // visible junction is already orthogonally adjacent to its pane.
                // The lone diagonal-only cell is the 1-wide crossing where four
                // dividers meet, which renders blank (no covered ortho neighbour)
                // - accenting a space would be invisible, so we don't. A FRAMED
                // pane carries its own border, so the outline fires only for an
                // unframed focused pane.
                let focused_by = |rr: usize, cc: usize| {
                    rr < rows
                        && cc < cols
                        && focused[rr * cols + cc]
                        && !framed_cell[rr * cols + cc]
                };
                let outline = focused_by(r, c - 1)
                    || focused_by(r, c + 1)
                    || focused_by(r - 1, c)
                    || focused_by(r + 1, c);
                // the seam under the pointer (or held in a drag) reads
                // BOLD, distinct from both idle DIM chrome and the focus
                // outline's plain accent. A terminal cannot portably change the
                // cursor shape, so this is the only signal a divider is
                // draggable before the press.
                let grabbable =
                    active_seam.is_some_and(|s| self.seam_at(r as u16, c as u16) == Some(s));
                // the candidate zone outranks the hover accent - during
                // a drag the only question on screen is where the pane lands.
                let dropping =
                    drop_zone.is_some_and(|z| self.drop_zone_at(r as u16, c as u16) == Some(z));
                let blank = beside_framed && !has_unframed_nb && !grabbable && !dropping;
                let (fg, flags) = if dropping {
                    (self.theme.accent, cell_flags::INVERSE)
                } else if grabbable {
                    (self.theme.accent, cell_flags::BOLD)
                } else if outline {
                    (self.theme.accent, 0)
                } else if blank {
                    (Color::Default, 0)
                } else {
                    (Color::Default, cell_flags::DIM)
                };
                cells[r * cols + c] = Cell {
                    c: if blank && !outline {
                        ' '
                    } else {
                        match (
                            c > origin_c && covered[r * cols + c - 1]
                                || c + 1 < cols && covered[r * cols + c + 1],
                            r > origin_r && covered[(r - 1) * cols + c]
                                || r + 1 < rows && covered[(r + 1) * cols + c],
                        ) {
                            (true, true) => '┼',
                            (true, false) => '│',
                            (false, true) => '─',
                            (false, false) => ' ',
                        }
                    },
                    fg,
                    bg: Color::Default,
                    flags,
                };
            }
        }

        // The empty state: nothing attached yet, so the content area wears
        // the raised two-row f[no] mark instead of bare filler.
        if self.layout.panes.is_empty() {
            let grid = wordmark::two_row();
            let w = grid[0].len();
            let avail_r = rows.saturating_sub(origin_r + 1);
            let avail_c = cols.saturating_sub(origin_c);
            if avail_r >= grid.len() && avail_c >= w {
                let r0 = origin_r + (avail_r - grid.len()) / 2;
                let c0 = origin_c + (avail_c - w) / 2;
                for (i, line) in grid.iter().enumerate() {
                    for (j, (ch, role)) in line.iter().enumerate() {
                        let (fg, _, flags) = cell_style(*role, &self.theme);
                        cells[(r0 + i) * cols + c0 + j] = Cell {
                            c: *ch,
                            fg,
                            bg: Color::Default,
                            flags,
                        };
                    }
                }
            }
        }

        // an edge drop zone lands on cells a PANE owns, which the
        // divider pass above skips by construction. Lit here, after the blit,
        // so the rim reads as a candidate the same way a seam does.
        if let Some((band_rows, band_cols)) = drop_zone.and_then(|z| self.drop_band(z)) {
            for r in band_rows.start as usize..(band_rows.end as usize).min(rows) {
                for c in band_cols.start as usize..(band_cols.end as usize).min(cols) {
                    if covered[r * cols + c] {
                        let cell = &mut cells[r * cols + c];
                        cell.fg = self.theme.accent;
                        cell.flags |= cell_flags::INVERSE;
                    }
                }
            }
        }

        if self.pane_ids_until.is_some_and(|until| now < until) {
            for (pid, rect) in &self.layout.panes {
                let label = format!("pane {pid}");
                let width = label.chars().count();
                let content = crate::pane_border::content_rect(*rect);
                let is_framed = crate::pane_border::framed(*rect);
                let anchor = if is_framed { content } else { *rect };
                let row = origin_r + anchor.y as usize;
                if (anchor.cols as usize) < width || row >= rows {
                    continue;
                }
                let start = origin_c + anchor.x as usize + anchor.cols as usize - width;
                for (offset, ch) in label.chars().enumerate() {
                    let col = start + offset;
                    if col < cols {
                        cells[row * cols + col] = Cell {
                            c: ch,
                            fg: Color::Default,
                            bg: Color::Default,
                            flags: cell_flags::INVERSE | cell_flags::DIM,
                        };
                    }
                }
            }
        }
    }

    /// One pane's frame: the two `edges` rows and the side `│` cells, styled
    /// from the sideline's own vocabulary. Skips unframed panes.
    fn paint_frame(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        origin_r: usize,
        origin_c: usize,
        pid: u64,
        rect: Rect,
    ) {
        if !crate::pane_border::framed(rect) {
            return;
        }
        let focused_pane = pid == self.layout.focus;
        // The grip rides the top edge of multi-pane tabs (None on a lone
        // pane or one too narrow to spare the cells), exactly as drawn.
        let has_grip = self.layout.panes.len() >= 2 && self.grip_span(rect).is_some();
        let meta = self.pane_meta_for(pid);
        let agent = self.layout.agents.iter().find(|a| a.pane_id == Some(pid));
        let status = agent.map(|a| {
            let st = agent_lattice_state(a);
            let ls = lattice_style(st, self.theme.accent);
            (ls.glyph, ls.flags, ls.fg, status_word(st))
        });
        let fields = crate::pane_border::EdgeFields {
            name: meta.map(|m| m.label.as_str()).unwrap_or("shell"),
            status: status.map(|(g, _, _, w)| (g, w)),
            model: agent
                .and_then(|a| a.model.as_deref())
                .or_else(|| agent.and_then(|a| a.harness.as_deref())),
            node: meta.and_then(|m| m.node.as_deref()),
            branch: meta.and_then(|m| m.branch.as_deref()),
            ctx: meta.and_then(|m| m.ctx.as_deref()),
        };
        let laid = crate::pane_border::edges(&fields, rect, has_grip, focused_pane);
        let (border_fg, border_flags) = if focused_pane {
            (self.theme.accent, 0)
        } else {
            (Color::Default, cell_flags::DIM)
        };
        let name_style = if focused_pane {
            (self.theme.accent, cell_flags::INVERSE | cell_flags::BOLD)
        } else {
            (Color::Default, cell_flags::DIM)
        };
        let dim = (Color::Default, cell_flags::DIM);
        let style_of = move |part: crate::pane_border::Part, ch: char| -> (Color, u8) {
            use crate::pane_border::Part;
            match part {
                Part::Border => match ch {
                    '╭' | '╮' | '╰' | '╯' | '─' | '│' => (border_fg, border_flags),
                    _ => dim,
                },
                Part::Name => name_style,
                Part::Cap => (self.theme.accent, 0),
                // Glyph and word wear the lattice fg exactly as the sideline
                // paints them: accent only on Blocked, flags on the glyph.
                Part::Glyph => match status {
                    Some((_, lflags, fg, _)) => (fg, lflags),
                    None => dim,
                },
                Part::Word => match status {
                    Some((_, _, fg, _)) => (fg, 0),
                    None => dim,
                },
                Part::Model | Part::Node | Part::Branch | Part::Ctx => dim,
            }
        };
        let mut put_row = |edge: &[(char, crate::pane_border::Part)], r: usize| {
            let mut c = origin_c + rect.x as usize;
            for (ch, part) in edge {
                let (fg, flags) = style_of(*part, *ch);
                if r < rows && c < cols {
                    cells[r * cols + c] = Cell {
                        c: *ch,
                        fg,
                        bg: Color::Default,
                        flags,
                    };
                }
                c += crate::chrome::char_cols(*ch);
            }
        };
        put_row(&laid.top, origin_r + rect.y as usize);
        put_row(
            &laid.bottom,
            origin_r + rect.y as usize + rect.rows as usize - 1,
        );
        // Sides: the border ring's left/right cells on the content rows.
        for fr in 1..rect.rows as usize - 1 {
            let r = origin_r + rect.y as usize + fr;
            for c in [
                origin_c + rect.x as usize,
                origin_c + rect.x as usize + rect.cols as usize - 1,
            ] {
                if r < rows && c < cols {
                    cells[r * cols + c] = Cell {
                        c: '│',
                        fg: border_fg,
                        bg: Color::Default,
                        flags: border_flags,
                    };
                }
            }
        }
    }

    /// A pane's `PaneMeta` from the live layout (any squad, any tab).
    fn pane_meta_for(&self, pid: u64) -> Option<&crate::proto::PaneMeta> {
        self.layout
            .squads
            .iter()
            .flat_map(|s| s.tabs.iter())
            .flat_map(|t| t.panes.iter())
            .find(|m| m.id == pid)
    }

    /// The framed pane whose border ring covers an outer cell. `None` on a
    /// pane interior (those forward through `hit_test`), a gap, or chrome.
    pub(super) fn border_pane_at(&self, row: u16, col: u16) -> Option<u64> {
        let left_w = self.left_chrome_w();
        if row < TAB_BAR_ROWS || col < left_w {
            return None;
        }
        let (cr, cc) = (row - TAB_BAR_ROWS, col - left_w);
        self.layout
            .panes
            .iter()
            .find_map(|(pid, rect)| crate::pane_border::on_border(*rect, cr, cc).then_some(*pid))
    }
}
