//! THROWAWAY SPIKE - not production code (evaluating ratatui for the mux chrome
//! layer). A one-way `ratatui::Buffer -> proto::Cell` bridge: render a chrome
//! widget into a local-coordinate `Buffer` (never a ratatui `Terminal`/`Backend`
//! - that would fight the mux Compositor) and blit the mapped cells into the
//! frame's flat `Vec<Cell>` at a rect offset.
//!
//! The rect boundary is the whole point: a widget renders in local coordinates
//! and physically cannot address a cell outside its rect, so the per-write
//! `text_w >= 3` / `.take()` / `pad_to` guards that pepper the hand-rolled chrome
//! have nowhere to be wrong. Truncation and alignment become ratatui layout
//! instead of hand arithmetic.
//!
//! One-way (Buffer -> Cell, never back), allocation-bounded (one Buffer per
//! call), coordinate-local. No mux state is read back into ratatui.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::proto::{cell_flags, Cell, Color};

/// Render `widget` into a `rows` x `cols` Buffer and blit the result into
/// `cells` (a `frame_rows` x `frame_cols` flat grid) with its top-left at
/// `(r0, c0)`. A degenerate (zero-row or zero-col) rect renders nothing.
pub fn render_chrome<W: Widget>(
    widget: W,
    (rows, cols): (u16, u16),
    (r0, c0): (usize, usize),
    cells: &mut [Cell],
    frame_rows: usize,
    frame_cols: usize,
) {
    if rows == 0 || cols == 0 {
        return;
    }
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);
    widget.render(area, &mut buf);
    blit(&buf, (r0, c0), cells, frame_rows, frame_cols);
}

/// Map a rendered ratatui `Buffer` into the frame's flat `Vec<Cell>` at
/// `(r0, c0)`. Clipped to the frame on every axis so an oversized rect or a
/// near-edge offset writes only in-bounds cells (never panics).
fn blit(
    buf: &Buffer,
    (r0, c0): (usize, usize),
    cells: &mut [Cell],
    frame_rows: usize,
    frame_cols: usize,
) {
    let (cols, rows) = (buf.area.width as usize, buf.area.height as usize);
    for y in 0..rows {
        let fr = r0 + y;
        if fr >= frame_rows {
            break;
        }
        let mut x = 0usize;
        while x < cols {
            // Buffer indexing is absolute (offset by buf.area origin). This
            // bridge always renders into a (0,0)-origin Buffer, but honor the
            // offset so the mapping stays correct if reused with another rect.
            let bc = &buf[(buf.area.x + x as u16, buf.area.y + y as u16)];
            let sym = bc.symbol();
            // ratatui marks a wide glyph's continuation column with an empty
            // symbol; we synthesize the WIDE_SPACER from the lead cell's display
            // width, so an empty continuation is consumed, never re-emitted.
            if sym.is_empty() {
                x += 1;
                continue;
            }
            let fc = c0 + x;
            if fc >= frame_cols {
                break;
            }
            let flags = map_flags(bc.modifier);
            let (fg, bg) = (map_color(bc.fg), map_color(bc.bg));
            // Chrome text is fno-authored (labels, names): a single scalar per
            // cell. A multi-scalar grapheme (combining/ZWJ) is a lossy mapping -
            // flag it loudly in debug so the finding records whether real chrome
            // content ever hits it, rather than shipping a wrong char silently.
            debug_assert!(
                sym.chars().count() == 1,
                "chrome_bridge: lossy grapheme mapping for {sym:?}"
            );
            let wide = UnicodeWidthStr::width(sym) >= 2;
            // A wide (CJK/emoji) glyph owns two columns: the lead cell carries
            // the char, the next carries WIDE_SPACER so the compositor never
            // overdraws its right half. If that spacer column would fall off the
            // frame, drop the glyph WHOLE (blank the lead) - keep the boundary,
            // never write a wide char with nowhere for its right half to live.
            if wide && c0 + x + 1 >= frame_cols {
                cells[fr * frame_cols + fc] = Cell {
                    c: ' ',
                    fg,
                    bg,
                    flags,
                };
                break;
            }
            let ch = sym.chars().next().unwrap_or(' ');
            cells[fr * frame_cols + fc] = Cell {
                c: ch,
                fg,
                bg,
                flags,
            };
            if wide {
                cells[fr * frame_cols + fc + 1] = Cell {
                    c: ' ',
                    fg,
                    bg,
                    flags: flags | cell_flags::WIDE_SPACER,
                };
                x += 2;
            } else {
                x += 1;
            }
        }
    }
}

/// ratatui `Modifier` bits -> mux `cell_flags`. Bits the mux `Cell` cannot carry
/// (SLOW_BLINK, RAPID_BLINK, CROSSED_OUT, HIDDEN, and ratatui's separate
/// underline-color) are a documented drop-list, not silent loss. `SELECTED` is
/// server-authored (co-viewer highlight) and is never emitted from chrome.
fn map_flags(m: Modifier) -> u8 {
    let mut f = 0u8;
    if m.contains(Modifier::BOLD) {
        f |= cell_flags::BOLD;
    }
    if m.contains(Modifier::ITALIC) {
        f |= cell_flags::ITALIC;
    }
    if m.contains(Modifier::UNDERLINED) {
        f |= cell_flags::UNDERLINE;
    }
    if m.contains(Modifier::REVERSED) {
        f |= cell_flags::INVERSE;
    }
    if m.contains(Modifier::DIM) {
        f |= cell_flags::DIM;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    /// A `frame_rows` x `frame_cols` grid pre-filled with a sentinel char so a
    /// test can prove which cells the bridge did and did NOT touch.
    fn frame(rows: usize, cols: usize, fill: char) -> Vec<Cell> {
        vec![
            Cell {
                c: fill,
                ..Cell::default()
            };
            rows * cols
        ]
    }

    fn at(cells: &[Cell], cols: usize, r: usize, c: usize) -> Cell {
        cells[r * cols + c]
    }

    // AC1-HP: a styled widget maps to byte-identical cells; cells outside the
    // target rect are untouched.
    #[test]
    fn maps_styled_spans_to_exact_cells_and_leaves_outside_untouched() {
        let (fr, fc) = (3usize, 10usize);
        let mut cells = frame(fr, fc, '#');
        // "Hi" bold-red + "!" italic-rgb, rendered into a 1x5 rect at (1, 2).
        let line = Line::from(vec![
            Span::styled(
                "Hi",
                Style::default()
                    .fg(RColor::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "!",
                Style::default()
                    .fg(RColor::Rgb(1, 2, 3))
                    .add_modifier(Modifier::ITALIC),
            ),
        ]);
        render_chrome(Paragraph::new(line), (1, 5), (1, 2), &mut cells, fr, fc);

        assert_eq!(
            at(&cells, fc, 1, 2),
            Cell {
                c: 'H',
                fg: Color::Indexed(1),
                bg: Color::Default,
                flags: cell_flags::BOLD
            }
        );
        assert_eq!(
            at(&cells, fc, 1, 3),
            Cell {
                c: 'i',
                fg: Color::Indexed(1),
                bg: Color::Default,
                flags: cell_flags::BOLD
            }
        );
        assert_eq!(
            at(&cells, fc, 1, 4),
            Cell {
                c: '!',
                fg: Color::Rgb(1, 2, 3),
                bg: Color::Default,
                flags: cell_flags::ITALIC
            }
        );
        // Trailing cells of the rect are ratatui's blank fill (space, no style).
        assert_eq!(at(&cells, fc, 1, 5).c, ' ');
        // Everything outside the rect keeps the sentinel: no stray writes.
        assert_eq!(at(&cells, fc, 0, 2).c, '#');
        assert_eq!(at(&cells, fc, 2, 2).c, '#');
        assert_eq!(at(&cells, fc, 1, 0).c, '#');
        assert_eq!(at(&cells, fc, 1, 1).c, '#');
    }

    // AC1-EDGE: a 0-row, 0-col, or 1-col rect writes nothing and does not panic.
    #[test]
    fn degenerate_rects_render_nothing() {
        let (fr, fc) = (2usize, 6usize);
        for dims in [(0u16, 4u16), (3, 0)] {
            let mut cells = frame(fr, fc, '#');
            render_chrome(Paragraph::new("xx"), dims, (0, 0), &mut cells, fr, fc);
            assert!(
                cells.iter().all(|c| c.c == '#'),
                "dims {dims:?} wrote cells"
            );
        }
        // A 1-col rect renders (one column) but never escapes it.
        let mut cells = frame(fr, fc, '#');
        render_chrome(Paragraph::new("xx"), (1, 1), (0, 0), &mut cells, fr, fc);
        assert_eq!(at(&cells, fc, 0, 0).c, 'x');
        assert_eq!(
            at(&cells, fc, 0, 1).c,
            '#',
            "1-col rect stayed in its column"
        );
    }

    // AC1-ERR: text longer than the rect truncates inside it (ratatui clips);
    // no cell outside the rect is written, nothing panics.
    #[test]
    fn overflow_never_escapes_the_rect() {
        let (fr, fc) = (1usize, 12usize);
        let mut cells = frame(fr, fc, '#');
        render_chrome(
            Paragraph::new("this label is far too wide"),
            (1, 4),
            (0, 2),
            &mut cells,
            fr,
            fc,
        );
        // Only columns 2..6 written; 0,1 and 6.. keep the sentinel.
        assert_eq!(at(&cells, fc, 0, 1).c, '#');
        assert_eq!(at(&cells, fc, 0, 2).c, 't');
        assert_eq!(at(&cells, fc, 0, 5).c, 's');
        assert_eq!(at(&cells, fc, 0, 6).c, '#', "no write past the rect");
    }

    // AC2-ERR: a wide glyph maps to lead-char + WIDE_SPACER continuation.
    #[test]
    fn wide_glyph_emits_lead_char_plus_wide_spacer() {
        let (fr, fc) = (1usize, 8usize);
        let mut cells = frame(fr, fc, '#');
        // Two CJK glyphs (each width 2) into a 4-col rect: fills all four cols.
        render_chrome(Paragraph::new("中文"), (1, 4), (0, 0), &mut cells, fr, fc);
        assert_eq!(at(&cells, fc, 0, 0).c, '中');
        assert_eq!(at(&cells, fc, 0, 0).flags & cell_flags::WIDE_SPACER, 0);
        assert_eq!(at(&cells, fc, 0, 1).c, ' ');
        assert_eq!(
            at(&cells, fc, 0, 1).flags & cell_flags::WIDE_SPACER,
            cell_flags::WIDE_SPACER,
            "continuation cell is a WIDE_SPACER"
        );
        assert_eq!(at(&cells, fc, 0, 2).c, '文');
        assert_eq!(
            at(&cells, fc, 0, 3).flags & cell_flags::WIDE_SPACER,
            cell_flags::WIDE_SPACER
        );
    }

    // AC2-ERR (boundary), two distinct clips:
    // (a) ratatui's layer: a wide glyph too big for the RECT is clipped by
    //     ratatui itself (rendered blank) - the bridge never sees a torn glyph.
    // (b) the bridge's layer: a wide glyph whose spacer column would fall off the
    //     FRAME is dropped WHOLE (the lead is blanked), never left as a torn
    //     half-glyph with nowhere for its right half to live.
    #[test]
    fn wide_glyph_at_boundary_never_tears() {
        // (a) 2-col rect, "a" then a wide glyph: the glyph needs cols 1..3 but
        // the rect ends at col 2, so ratatui leaves col 1 blank.
        let (fr, fc) = (1usize, 4usize);
        let mut cells = frame(fr, fc, '#');
        render_chrome(Paragraph::new("a中"), (1, 2), (0, 0), &mut cells, fr, fc);
        assert_eq!(at(&cells, fc, 0, 0).c, 'a');
        assert_eq!(
            at(&cells, fc, 0, 1).c,
            ' ',
            "wide glyph clipped by rect, blank"
        );
        assert_eq!(
            at(&cells, fc, 0, 1).flags & cell_flags::WIDE_SPACER,
            0,
            "no spacer for a clipped glyph"
        );

        // (b) frame is only 2 cols; rect starts at frame col 1, so a wide glyph's
        // lead would land on the last frame col with its spacer (col 2) off-frame.
        // The glyph is dropped whole - the last col is blanked, not left torn.
        let (fr, fc) = (1usize, 2usize);
        let mut cells = frame(fr, fc, '#');
        render_chrome(Paragraph::new("中"), (1, 2), (0, 1), &mut cells, fr, fc);
        assert_eq!(
            at(&cells, fc, 0, 1).c,
            ' ',
            "wide glyph dropped at the frame edge, not left torn"
        );
        assert_eq!(
            at(&cells, fc, 0, 1).flags & cell_flags::WIDE_SPACER,
            0,
            "a dropped glyph leaves no orphan spacer flag"
        );
        assert_eq!(cells.len(), fr * fc, "no out-of-bounds growth, no panic");
    }

    // AC2-EDGE: chrome output never carries the server-authored SELECTED flag,
    // even when a widget asks for REVERSED (which maps to INVERSE, not SELECTED).
    #[test]
    fn selected_flag_is_never_chrome_authored() {
        let (fr, fc) = (2usize, 20usize);
        let mut cells = frame(fr, fc, ' ');
        let styled = Paragraph::new(Line::from(Span::styled(
            "reversed and bold",
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        )));
        render_chrome(styled, (2, 20), (0, 0), &mut cells, fr, fc);
        assert!(
            cells.iter().all(|c| c.flags & cell_flags::SELECTED == 0),
            "no chrome cell may carry SELECTED"
        );
        // REVERSED did map to INVERSE somewhere (sanity: styling reached cells).
        assert!(cells.iter().any(|c| c.flags & cell_flags::INVERSE != 0));
    }

    // Named ANSI colors fold onto their indices (spot-check the tricky trio:
    // Gray=7, DarkGray=8, White=15).
    #[test]
    fn named_colors_fold_to_ansi_indices() {
        assert_eq!(map_color(RColor::Gray), Color::Indexed(7));
        assert_eq!(map_color(RColor::DarkGray), Color::Indexed(8));
        assert_eq!(map_color(RColor::White), Color::Indexed(15));
        assert_eq!(map_color(RColor::Reset), Color::Default);
        assert_eq!(map_color(RColor::Indexed(200)), Color::Indexed(200));
    }
}

/// ratatui `Color` -> mux `Color`. Total: the 16 named ANSI colors fold onto
/// their indices, `Reset` onto `Default`, and Indexed/Rgb pass through.
fn map_color(c: RColor) -> Color {
    match c {
        RColor::Reset => Color::Default,
        RColor::Black => Color::Indexed(0),
        RColor::Red => Color::Indexed(1),
        RColor::Green => Color::Indexed(2),
        RColor::Yellow => Color::Indexed(3),
        RColor::Blue => Color::Indexed(4),
        RColor::Magenta => Color::Indexed(5),
        RColor::Cyan => Color::Indexed(6),
        RColor::Gray => Color::Indexed(7),
        RColor::DarkGray => Color::Indexed(8),
        RColor::LightRed => Color::Indexed(9),
        RColor::LightGreen => Color::Indexed(10),
        RColor::LightYellow => Color::Indexed(11),
        RColor::LightBlue => Color::Indexed(12),
        RColor::LightMagenta => Color::Indexed(13),
        RColor::LightCyan => Color::Indexed(14),
        RColor::White => Color::Indexed(15),
        RColor::Indexed(i) => Color::Indexed(i),
        RColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
