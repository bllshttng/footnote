//! The frame painter: a row-level diff against its own last output, the
//! theme-ground fill for the OSC takeover, and the cell style writer.

use std::io::Write;

use crossterm::cursor;
use crossterm::queue;
use crossterm::style;
use crossterm::style::Color as CtColor;
use crossterm::terminal;

use super::image_layer::{self, Placement};
use crate::proto::{self, Cell, Color, Frame};

/// Draws frames with a row-level diff against what was actually drawn last -
/// safe precisely because it diffs against its own output, never against a
/// prediction of server state.
pub(super) struct Compositor {
    last: Option<Frame>,
    /// The theme's ground color, when a footnote theme paints one: a drawn
    /// cell whose bg is Default renders the base instead, so terminals that
    /// ignore the OSC still show it. `None` = the kill switch or the
    /// terminal theme paints no ground.
    ground: Option<Color>,
    /// The image the terminal shows now, as last written.
    shown: Option<Placement>,
}

impl Compositor {
    pub(super) fn new(ground: Option<Color>) -> Self {
        Compositor {
            last: None,
            ground,
            shown: None,
        }
    }

    /// A theme switch moves the ground without rebuilding the compositor;
    /// the next draw paints every Default-bg cell in the new base.
    pub(super) fn set_ground(&mut self, ground: Option<Color>) {
        self.ground = ground;
    }

    #[cfg(test)]
    pub(super) fn ground(&self) -> Option<Color> {
        self.ground
    }

    pub(super) fn draw(&mut self, frame: &Frame) -> std::io::Result<()> {
        let placed = image_layer::take();
        self.draw_to(&mut std::io::stdout().lock(), frame, placed)
    }

    /// The frame, then the image layer. While the placement's rect still
    /// holds only `HOLE` cells, rows write nothing over it and its bytes go
    /// out once, again only after a full redraw or a new placement. A modal
    /// painted over part of the rect hides the whole image until it closes.
    pub(super) fn draw_to(
        &mut self,
        out: &mut impl Write,
        frame: &Frame,
        placed: Option<Placement>,
    ) -> std::io::Result<()> {
        let full = match &self.last {
            Some(prev) => prev.rows != frame.rows || prev.cols != frame.cols,
            None => true,
        };
        let placed = placed.filter(|p| holes_intact(frame, p.rect));
        let skip = placed.as_ref().map(|p| p.rect);
        let fresh = placed
            .as_ref()
            .is_some_and(|p| full || !self.shown.as_ref().is_some_and(|s| s.same(p)));
        // The image on screen goes: every row it covered redraws, even rows
        // whose cells did not change, so no part of it outlives its rect.
        let retired = self
            .shown
            .as_ref()
            .map(|s| s.rect)
            .filter(|_| placed.is_none() || fresh);
        let under_retired = |r: usize| {
            retired.is_some_and(|s| (usize::from(s.row)..usize::from(s.row + s.h)).contains(&r))
        };
        if full {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
        }
        queue!(out, cursor::Hide)?;
        for r in 0..frame.rows as usize {
            if !full && !under_retired(r) {
                // Row unchanged since we drew it? Skip the write entirely.
                let prev = self.last.as_ref().unwrap();
                let w = frame.cols as usize;
                if prev.cells[r * w..(r + 1) * w] == frame.cells[r * w..(r + 1) * w] {
                    continue;
                }
            }
            self.draw_row(out, frame, r, skip)?;
        }
        if retired.is_some() && self.shown.as_ref().is_some_and(|s| s.kitty) {
            out.write_all(image_layer::KITTY_DELETE)?;
        }
        if let Some(p) = placed.as_ref().filter(|_| fresh) {
            self.blank_rect(out, p.rect)?;
            out.write_all(&p.bytes)?;
        }
        self.shown = placed;
        queue!(out, cursor::MoveTo(frame.cursor_col, frame.cursor_row))?;
        if frame.cursor_visible {
            queue!(out, cursor::Show)?;
        } else {
            queue!(out, cursor::Hide)?;
        }
        out.flush()?;
        self.last = Some(frame.clone());
        Ok(())
    }

    /// One row, writing nothing over `skip`'s columns when the row crosses
    /// it: the image layer owns those cells on the terminal.
    pub(super) fn draw_row(
        &self,
        out: &mut impl Write,
        frame: &Frame,
        r: usize,
        skip: Option<image_layer::Rect>,
    ) -> std::io::Result<()> {
        queue!(out, cursor::MoveTo(0, r as u16))?;
        let w = frame.cols as usize;
        let hole = skip
            .filter(|s| (usize::from(s.row)..usize::from(s.row) + usize::from(s.h)).contains(&r))
            .map(|s| (usize::from(s.col), usize::from(s.col) + usize::from(s.w)));
        let mut style_of: Option<(Color, Color, u8)> = None;
        for (c, cell) in frame.cells[r * w..(r + 1) * w].iter().enumerate() {
            if let Some((from, to)) = hole {
                if c == from {
                    queue!(out, cursor::MoveTo(to as u16, r as u16))?;
                }
                if (from..to).contains(&c) {
                    continue;
                }
            }
            if cell.flags & proto::cell_flags::WIDE_SPACER != 0 {
                continue; // the wide glyph before it already covers this column
            }
            let mut cell = if *cell == image_layer::HOLE {
                Cell::default()
            } else {
                *cell
            };
            // The ground fill: a Default background renders the theme's base
            // when the takeover is active, so OSC-ignoring terminals still
            // show it.
            if let Some(base) = self.ground {
                if cell.bg == Color::Default {
                    cell.bg = base;
                }
            }
            let key = (cell.fg, cell.bg, cell.flags);
            if style_of != Some(key) {
                apply_style(out, &cell)?;
                style_of = Some(key);
            }
            queue!(out, style::Print(cell.c))?;
        }
        // Leave the line in a reset state so scrolling artifacts never bleed.
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
        Ok(())
    }

    /// Spaces in the ground color over `rect`, so whatever was drawn there
    /// before (text, an older image) is gone when the image bytes land.
    fn blank_rect(&self, out: &mut impl Write, rect: image_layer::Rect) -> std::io::Result<()> {
        let mut blank = Cell::default();
        if let Some(base) = self.ground {
            blank.bg = base;
        }
        apply_style(out, &blank)?;
        let row = " ".repeat(usize::from(rect.w));
        for y in rect.row..rect.row + rect.h {
            queue!(out, cursor::MoveTo(rect.col, y), style::Print(&row))?;
        }
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
        Ok(())
    }
}

/// Whether every cell of `rect` is still a `HOLE`: nothing composed after the
/// viewer painted over it, and it lies inside the frame.
fn holes_intact(frame: &Frame, rect: image_layer::Rect) -> bool {
    let (rows, cols) = (usize::from(frame.rows), usize::from(frame.cols));
    let (r0, c0) = (usize::from(rect.row), usize::from(rect.col));
    let (r1, c1) = (r0 + usize::from(rect.h), c0 + usize::from(rect.w));
    rect.w > 0
        && rect.h > 0
        && r1 <= rows
        && c1 <= cols
        && (r0..r1).all(|r| {
            frame.cells[r * cols + c0..r * cols + c1]
                .iter()
                .all(|c| *c == image_layer::HOLE)
        })
}

pub(super) fn apply_style(out: &mut impl Write, cell: &Cell) -> std::io::Result<()> {
    use proto::cell_flags as cf;
    // Reset first: attribute REMOVAL (e.g. bold -> plain) has no incremental
    // form worth tracking at this scale.
    queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    if cell.flags & cf::BOLD != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if cell.flags & cf::ITALIC != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Italic))?;
    }
    if cell.flags & cf::UNDERLINE != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Underlined))?;
    }
    // A SELECTED cell (US2) toggles reverse-video: XOR with the cell's own
    // inverse so the selection is always a visible delta, even over already-
    // inverse text.
    if (cell.flags & cf::INVERSE != 0) ^ (cell.flags & cf::SELECTED != 0) {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    if cell.flags & cf::DIM != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Dim))?;
    }
    queue!(
        out,
        style::SetForegroundColor(map_color(cell.fg)),
        style::SetBackgroundColor(map_color(cell.bg))
    )?;
    Ok(())
}

fn map_color(c: Color) -> CtColor {
    match c {
        Color::Default => CtColor::Reset,
        Color::Indexed(i) => CtColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const RECT: image_layer::Rect = image_layer::Rect {
        row: 1,
        col: 2,
        w: 3,
        h: 2,
    };

    /// A 4x8 frame of `x` cells with the image rect holed.
    fn holed() -> Frame {
        let x = Cell {
            c: 'x',
            ..Cell::default()
        };
        let mut cells = vec![x; 32];
        for r in 1..3 {
            for c in 2..5 {
                cells[r * 8 + c] = image_layer::HOLE;
            }
        }
        Frame {
            rows: 4,
            cols: 8,
            cells,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            scroll_offset: 0,
            history_lines: 0,
        }
    }

    fn placement() -> Placement {
        Placement {
            rect: RECT,
            bytes: Arc::from(&b"<IMG>"[..]),
            kitty: true,
        }
    }

    fn draw(comp: &mut Compositor, frame: &Frame, p: Option<Placement>) -> String {
        let mut out = Vec::new();
        comp.draw_to(&mut out, frame, p).expect("draw into memory");
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_layer_writes_once_and_again_after_a_full_redraw() {
        let mut comp = Compositor::new(None);
        let frame = holed();
        let p = placement();
        let first = draw(&mut comp, &frame, Some(p.clone()));
        assert_eq!(first.matches("<IMG>").count(), 1, "{first:?}");
        assert!(
            !first.contains('\u{FDD0}'),
            "a hole never reaches the terminal"
        );
        // Row 1 changes outside the rect: it redraws, jumping past the image.
        let mut edited = frame.clone();
        edited.cells[8].c = 'y';
        let second = draw(&mut comp, &edited, Some(p.clone()));
        assert!(second.contains('y'));
        assert!(
            !second.contains("<IMG>"),
            "an unchanged placement is not re-sent"
        );
        assert!(second.contains("\x1b[2;6H"), "{second:?}");
        let mut shorter = holed();
        shorter.rows = 3;
        shorter.cells.truncate(24);
        draw(&mut comp, &shorter, Some(p.clone()));
        let regrown = draw(&mut comp, &frame, Some(p));
        assert_eq!(
            regrown.matches("<IMG>").count(),
            1,
            "a full redraw re-sends it"
        );
    }

    #[test]
    fn a_modal_over_the_rect_hides_the_image_and_deletes_it_once() {
        let mut comp = Compositor::new(None);
        draw(&mut comp, &holed(), Some(placement()));
        let mut covered = holed();
        covered.cells[8 + 3].c = 'M';
        let hidden = draw(&mut comp, &covered, Some(placement()));
        assert!(!hidden.contains("<IMG>"));
        assert_eq!(hidden.matches("\x1b_Ga=d").count(), 1, "{hidden:?}");
        assert!(hidden.contains('M'));
        assert!(
            hidden.contains("\x1b[3;1H"),
            "row 2 redraws though the modal missed it"
        );
        assert!(!hidden.contains('\u{FDD0}'));
        let again = draw(&mut comp, &covered, Some(placement()));
        assert!(!again.contains("\x1b_Ga=d"), "the delete goes out once");
    }
}
