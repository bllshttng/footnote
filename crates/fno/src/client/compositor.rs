//! The frame painter: a row-level diff against its own last output, the
//! theme-ground fill for the OSC takeover, and the cell style writer.

use std::io::Write;

use crossterm::cursor;
use crossterm::queue;
use crossterm::style;
use crossterm::style::Color as CtColor;
use crossterm::terminal;

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
}

impl Compositor {
    pub(super) fn new(ground: Option<Color>) -> Self {
        Compositor { last: None, ground }
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
        let mut out = std::io::stdout().lock();
        let full = match &self.last {
            Some(prev) => prev.rows != frame.rows || prev.cols != frame.cols,
            None => true,
        };
        if full {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
        }
        queue!(out, cursor::Hide)?;
        for r in 0..frame.rows as usize {
            if !full {
                // Row unchanged since we drew it? Skip the write entirely.
                let prev = self.last.as_ref().unwrap();
                let w = frame.cols as usize;
                if prev.cells[r * w..(r + 1) * w] == frame.cells[r * w..(r + 1) * w] {
                    continue;
                }
            }
            self.draw_row(&mut out, frame, r)?;
        }
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

    pub(super) fn draw_row(
        &self,
        out: &mut impl Write,
        frame: &Frame,
        r: usize,
    ) -> std::io::Result<()> {
        queue!(out, cursor::MoveTo(0, r as u16))?;
        let w = frame.cols as usize;
        let mut style_of: Option<(Color, Color, u8)> = None;
        for cell in &frame.cells[r * w..(r + 1) * w] {
            if cell.flags & proto::cell_flags::WIDE_SPACER != 0 {
                continue; // the wide glyph before it already covers this column
            }
            // The ground fill: a Default background renders the theme's base
            // when the takeover is active, so OSC-ignoring terminals still
            // show it.
            let mut cell = *cell;
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
