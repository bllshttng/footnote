//! The one-way bridge from a ratatui [`Buffer`](ratatui_core::buffer::Buffer)
//! into the compositor's cell slice.
//!
//! [`draw_sideline`](crate::client) renders ratatui widgets into a standalone
//! Buffer (no `Terminal`, no backend) and [`blit`] copies that Buffer into the
//! `proto::Cell` slice the compositor already consumes. Color and modifier
//! mapping is explicit and total over the ratatui enums; anything the proto
//! cell cannot carry (blink, crossed-out, an underline color) is dropped, never
//! guessed into a neighbor flag.

use ratatui_core::buffer::{Buffer, CellWidth};
use ratatui_core::style::{Color as RtColor, Modifier};

use crate::proto::{self, cell_flags};

/// Copy every Buffer cell into `cells` (row stride `frame_cols`).
///
/// The symbol's first char becomes `Cell::c`; a double-width glyph writes a
/// `WIDE_SPACER` space over the following cell and the next glyph is read from
/// the cell after that, so a CJK/emoji name shifts the rest of the row by
/// exactly its own width. Buffer cells beyond `cells` (a wider Buffer than
/// the frame) are left untouched.
pub fn blit(buf: &Buffer, cells: &mut [proto::Cell], frame_cols: usize) {
    let area = buf.area;
    for y in 0..area.height as usize {
        let mut x = 0usize;
        while x < area.width as usize {
            let src = &buf.content[y * area.width as usize + x];
            let wide = src.cell_width() == 2;
            let Some(slot) = cells.get_mut(y * frame_cols + x) else {
                break;
            };
            *slot = proto::Cell {
                c: src.symbol().chars().next().unwrap_or(' '),
                fg: map_color(src.fg),
                bg: map_color(src.bg),
                flags: map_flags(src.modifier),
            };
            if wide {
                // The glyph claims the next column; the compositor skips a
                // WIDE_SPACER so the glyph's right half is never overdrawn.
                if let Some(pad) = cells.get_mut(y * frame_cols + x + 1) {
                    *pad = proto::Cell {
                        flags: cell_flags::WIDE_SPACER,
                        ..proto::Cell::default()
                    };
                }
                x += 1;
            }
            x += 1;
        }
    }
}

/// ratatui named colors ride their fixed ANSI indices, so a lane painted
/// `Red` and one painted `Indexed(1)` land on the same terminal color.
fn map_color(color: RtColor) -> proto::Color {
    match color {
        RtColor::Reset => proto::Color::Default,
        RtColor::Indexed(i) => proto::Color::Indexed(i),
        RtColor::Rgb(r, g, b) => proto::Color::Rgb(r, g, b),
        RtColor::Black => proto::Color::Indexed(0),
        RtColor::Red => proto::Color::Indexed(1),
        RtColor::Green => proto::Color::Indexed(2),
        RtColor::Yellow => proto::Color::Indexed(3),
        RtColor::Blue => proto::Color::Indexed(4),
        RtColor::Magenta => proto::Color::Indexed(5),
        RtColor::Cyan => proto::Color::Indexed(6),
        RtColor::Gray => proto::Color::Indexed(7),
        RtColor::DarkGray => proto::Color::Indexed(8),
        RtColor::LightRed => proto::Color::Indexed(9),
        RtColor::LightGreen => proto::Color::Indexed(10),
        RtColor::LightYellow => proto::Color::Indexed(11),
        RtColor::LightBlue => proto::Color::Indexed(12),
        RtColor::LightMagenta => proto::Color::Indexed(13),
        RtColor::LightCyan => proto::Color::Indexed(14),
        RtColor::White => proto::Color::Indexed(15),
    }
}

/// The five proto flag bits; every other modifier is dropped.
fn map_flags(modifier: Modifier) -> u8 {
    let mut flags = 0;
    if modifier.contains(Modifier::BOLD) {
        flags |= cell_flags::BOLD;
    }
    if modifier.contains(Modifier::ITALIC) {
        flags |= cell_flags::ITALIC;
    }
    if modifier.contains(Modifier::UNDERLINED) {
        flags |= cell_flags::UNDERLINE;
    }
    if modifier.contains(Modifier::REVERSED) {
        flags |= cell_flags::INVERSE;
    }
    if modifier.contains(Modifier::DIM) {
        flags |= cell_flags::DIM;
    }
    flags
}

/// The inverse of [`map_color`] for building Buffer cells from the proto
/// colors the rest of the client speaks (`sideline_color`, the theme accent).
/// Only the three proto variants exist here, so the named-color round trip
/// the blit performs never runs backwards.
pub fn rt_color(color: proto::Color) -> RtColor {
    match color {
        proto::Color::Default => RtColor::Reset,
        proto::Color::Indexed(i) => RtColor::Indexed(i),
        proto::Color::Rgb(r, g, b) => RtColor::Rgb(r, g, b),
    }
}

/// The inverse of [`map_flags`]: proto flag bits to the ratatui `Modifier`
/// the Buffer cells carry. `WIDE_SPACER` has no ratatui half - the Buffer's
/// own width handling covers it - so it is dropped here like the blit drops
/// blink and crossed-out on the way back.
pub fn rt_modifier(flags: u8) -> Modifier {
    let mut modifier = Modifier::empty();
    if flags & cell_flags::BOLD != 0 {
        modifier |= Modifier::BOLD;
    }
    if flags & cell_flags::ITALIC != 0 {
        modifier |= Modifier::ITALIC;
    }
    if flags & cell_flags::UNDERLINE != 0 {
        modifier |= Modifier::UNDERLINED;
    }
    if flags & cell_flags::INVERSE != 0 {
        modifier |= Modifier::REVERSED;
    }
    if flags & cell_flags::DIM != 0 {
        modifier |= Modifier::DIM;
    }
    modifier
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_core::layout::Rect;
    use ratatui_core::style::Style;

    fn buf3() -> Buffer {
        Buffer::empty(Rect::new(0, 0, 3, 1))
    }

    #[test]
    fn blits_color_modifier_and_default_cells() {
        let mut buf = buf3();
        buf[(0, 0)].set_char('a').set_style(
            Style::default()
                .fg(RtColor::Red)
                .add_modifier(Modifier::BOLD),
        );
        buf[(1, 0)]
            .set_char('b')
            .set_style(Style::default().add_modifier(Modifier::REVERSED));
        buf[(2, 0)].set_char('c');

        let mut cells = vec![proto::Cell::default(); 3];
        blit(&buf, &mut cells, 3);

        assert_eq!(cells[0].c, 'a');
        assert_eq!(cells[0].fg, proto::Color::Indexed(1));
        assert_eq!(cells[0].flags, cell_flags::BOLD);
        assert_eq!(cells[1].c, 'b');
        assert_eq!(cells[1].flags, cell_flags::INVERSE);
        assert_eq!(cells[2].c, 'c');
        assert_eq!(cells[2].fg, proto::Color::Default);
        assert_eq!(cells[2].flags, 0);
    }

    #[test]
    fn wide_glyph_marks_a_spacer_and_the_next_glyph_lands_after_it() {
        let mut buf = buf3();
        buf.set_string(0, 0, "王x", Style::default());

        let mut cells = vec![proto::Cell::default(); 3];
        blit(&buf, &mut cells, 3);

        assert_eq!(cells[0].c, '王');
        assert_eq!(cells[1].c, ' ');
        assert_eq!(cells[1].flags, cell_flags::WIDE_SPACER);
        assert_eq!(cells[2].c, 'x');
        assert_eq!(cells[2].flags, 0);
    }

    #[test]
    fn unmapped_modifiers_are_dropped_not_guessed() {
        let mut buf = buf3();
        buf[(0, 0)].set_char('a').set_style(
            Style::default()
                .add_modifier(Modifier::CROSSED_OUT)
                .add_modifier(Modifier::BOLD),
        );

        let mut cells = vec![proto::Cell::default(); 3];
        blit(&buf, &mut cells, 3);

        assert_eq!(cells[0].flags, cell_flags::BOLD);
    }
}
