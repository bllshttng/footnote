//! The bottom chrome row: which modality owns the terminal's last row and
//! what it paints there. Split out of `client.rs` (file budget); the
//! renderer and `chrome_hit` share [`bottom_row_is_chrome`] as the single
//! truth of what the row belongs to.

use super::*;

impl View {
    /// The bottom chrome line (US4). While a prefix chord is pending past
    /// [`HINT_DELAY`] it is the which-key hint (painted over whatever the row
    /// held - even with the status row toggled off, discoverability does not
    /// die with the toggle; tmux's message-line behavior). Otherwise it is
    /// the status row (AC4-UI): session name, focused pane cwd, the focused
    /// pane's scroll offset (the canonical `[+N]` home; the per-pane inline
    /// indicator stays so a scrolled UNFOCUSED pane is still observable),
    /// and `? for keys`. Too-short terminals draw neither (AC4-ERR).
    /// The bottom terminal row is chrome (search line / which-key hint / status
    /// row, painted last by `draw_bottom_row`) rather than content or a sideline
    /// row drawn underneath. Below minimum geometry both auto-hide (AC4-ERR) and
    /// the row is content (`content_dims` handed the server the full height, a
    /// pane tiled into it, so blanking would erase it). The single truth shared
    /// by the renderer and `chrome_hit` so a click matches what's painted
    /// (codex P2).
    pub(super) fn bottom_row_is_chrome(&self) -> bool {
        self.term.0 >= MIN_ROWS_FOR_STATUS
            && (self.confirm.is_some()
                || self.create.is_some()
                || self.rename.is_some()
                || self.move_to.is_some()
                || self.recruit.is_some()
                || self.search.is_some()
                || self.hint
                // The open row selector reserves the row its key hint
                // paints: the hint is the discoverability the selector
                // never had.
                || self.selector.is_some()
                || self.status_on)
    }

    pub(super) fn draw_bottom_row(&self, cells: &mut [Cell], rows: usize, cols: usize) {
        if !self.bottom_row_is_chrome() {
            return;
        }
        // A card-dispatch confirm is modal - it owns the row above everything
        // else while the operator decides.
        if let Some(c) = &self.confirm {
            self.draw_confirm_line(cells, rows, cols, c);
            return;
        }
        // The new-workspace name input is a centered modal; the operator
        // is mid-entry, so it sits above search/hint/status.
        if let Some(name) = &self.create {
            self.draw_name_modal(cells, rows, cols, "new workspace", name, None);
            return;
        }
        // The rename input (tab; widened to squads): the noun tracks
        // the target so the operator sees what they are renaming, and the hint
        // spells out the blank-clears semantics.
        if let Some((target, name)) = &self.rename {
            let noun = match target {
                RenameTarget::Tab(_) => "tab",
                RenameTarget::Squad(_) => "workspace",
                RenameTarget::Agent(_) => "row",
            };
            let hint = match target {
                RenameTarget::Agent(_) => Some("a-z 0-9 - _ (1-64 chars)"),
                _ => Some("empty resets to auto"),
            };
            self.draw_name_modal(cells, rows, cols, &format!("rename {noun}"), name, hint);
            return;
        }
        // The move-to prompt: the typed number IS the body; the hint
        // names the grammar so a `4` never reads as "move 4 left".
        if let Some((_, buf)) = &self.move_to {
            self.draw_name_modal(
                cells,
                rows,
                cols,
                "move tab to position",
                buf,
                Some("1-based; Enter moves"),
            );
            return;
        }
        // The recruit workspace-name input: the hint names how many
        // marked agents will join (create-if-absent).
        if let Some(name) = &self.recruit {
            let n = self.marks.len();
            self.draw_name_modal(
                cells,
                rows,
                cols,
                &format!("recruit {n} into"),
                name,
                Some("create-if-absent"),
            );
            return;
        }
        // Search line takes the bottom row when active (precedence: search >
        // which-key hint > status row). It OVERLAYS whatever held the row - no
        // reserved row, so opening search never triggered a Resize/reflow.
        if let Some(sv) = &self.search {
            self.draw_search_line(cells, rows, cols, sv);
            return;
        }
        let r = rows - 1;
        // We own the row: blank it first so the divider-fill pass in `compose`
        // (which treats this uncovered row as content and paints '─' glyphs)
        // cannot bleed through the gaps between the segments below.
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
        let put = |cells: &mut [Cell], c: usize, ch: char, flags: u8| {
            if c < cols {
                cells[r * cols + c] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags,
                };
            }
        };
        if self.hint {
            let text = crate::keys::prefix_hint();
            for (i, ch) in text.chars().take(cols).enumerate() {
                put(cells, i, ch, 0);
            }
            return;
        }
        // The open row selector's key hint (the discoverability line the
        // selector never had). Only a modal-less row reaches here: the
        // confirm/name arms above outrank it.
        if self.selector.is_some() {
            let text = crate::keys::selector_hint();
            for (i, ch) in text.chars().take(cols).enumerate() {
                put(cells, i, ch, 0);
            }
            return;
        }
        let mut c = 0usize;
        for ch in format!(" {} ", self.session).chars() {
            put(cells, c, ch, cell_flags::BOLD);
            c += 1;
        }
        // Active squad's name, only when there is more than one squad to be
        // ambiguous about - the always-visible answer to "which
        // squad?" when the sideline is toggled off or auto-hidden. BOLD: it
        // is identity, like the session cell, not context like the cwd.
        if self.layout.squads.len() > 1 {
            if let Some(s) = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == self.layout.active_squad)
            {
                for ch in format!("│ {} ", s.name).chars() {
                    put(cells, c, ch, cell_flags::BOLD);
                    c += 1;
                }
            }
        }
        let cwd = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .map(|s| abbrev_home(&s.canonical_cwd))
            .unwrap_or_default();
        for ch in format!("│ {cwd} ").chars() {
            put(cells, c, ch, cell_flags::DIM);
            c += 1;
        }
        // Provenance cell for the focused pane: config-free `⚑ <node>`,
        // shown only when the focused pane was node-driven. Absent for an ad-hoc
        // pane, so a plain shell reads clean.
        if let Some(node) = &self.layout.focus_node {
            for ch in format!("⚑ {node} ").chars() {
                put(cells, c, ch, cell_flags::BOLD);
                c += 1;
            }
        }
        if let Some(f) = self.frames.get(&self.layout.focus) {
            if f.scroll_offset != 0 {
                for ch in format!("[+{}] ", f.scroll_offset).chars() {
                    put(cells, c, ch, cell_flags::INVERSE);
                    c += 1;
                }
            }
        }
        // The whole-machine meter, when toggled on: the latest one-line
        // reading, or an explicit "sensor unavailable" until a sample lands.
        // A dark sensor is named - the row never shows a zero or a blank as
        // if it were a reading.
        if self.resource_meter_on {
            let text = self
                .resource_meter_text
                .clone()
                .unwrap_or_else(|| "meter: sensor unavailable".into());
            for ch in format!("│ {text} ").chars() {
                put(cells, c, ch, cell_flags::DIM);
                c += 1;
            }
        }
        let help = "? keys · glyphs ";
        let start = cols.saturating_sub(help.chars().count());
        if start > c {
            for (i, ch) in help.chars().enumerate() {
                put(cells, start + i, ch, cell_flags::DIM);
            }
        }
    }
}
