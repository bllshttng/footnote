//! A pane [`Frame`] as terminal bytes, for the ssh observer. Pure: the caller
//! owns the tty. Pane text is untrusted here (the watched agent wrote it), so
//! every control character becomes a space and the only escapes in the output
//! are the cursor moves and colors this module writes itself.

use crate::proto::{cell_flags, Cell, Color, Frame};

/// Paint `frame` with its top-left cell at terminal row `top` (0-based) and
/// column 0, cropped to `rows` x `cols`. Every painted row ends by erasing to
/// the end of the line, and rows below the frame are erased, so a shrinking
/// pane leaves nothing behind.
pub fn paint(frame: &Frame, top: u16, rows: u16, cols: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let draw_rows = rows.saturating_sub(top);
    let width = frame.cols.min(cols) as usize;
    for r in 0..draw_rows {
        out.extend_from_slice(format!("\x1b[{};1H", top + r + 1).as_bytes());
        if r < frame.rows {
            let start = r as usize * frame.cols as usize;
            paint_row(&mut out, &frame.cells[start..start + width]);
        }
        out.extend_from_slice(b"\x1b[0m\x1b[K");
    }
    out
}

fn paint_row(out: &mut Vec<u8>, cells: &[Cell]) {
    let mut style: Option<(Color, Color, u8)> = None;
    for cell in cells {
        if cell.flags & cell_flags::WIDE_SPACER != 0 {
            continue;
        }
        let now = (cell.fg, cell.bg, cell.flags);
        if style != Some(now) {
            sgr(out, cell);
            style = Some(now);
        }
        let ch = if cell.c.is_control() { ' ' } else { cell.c };
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}

fn sgr(out: &mut Vec<u8>, cell: &Cell) {
    let mut seq = String::from("\x1b[0");
    for (bit, code) in [
        (cell_flags::BOLD, ";1"),
        (cell_flags::DIM, ";2"),
        (cell_flags::ITALIC, ";3"),
        (cell_flags::UNDERLINE, ";4"),
        (cell_flags::INVERSE | cell_flags::SELECTED, ";7"),
    ] {
        if cell.flags & bit != 0 {
            seq.push_str(code);
        }
    }
    for (color, base) in [(cell.fg, 38), (cell.bg, 48)] {
        match color {
            Color::Default => {}
            Color::Indexed(n) => seq.push_str(&format!(";{base};5;{n}")),
            Color::Rgb(r, g, b) => seq.push_str(&format!(";{base};2;{r};{g};{b}")),
        }
    }
    seq.push('m');
    out.extend_from_slice(seq.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rows: u16, cols: u16, cells: Vec<Cell>) -> Frame {
        Frame {
            rows,
            cols,
            cells,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            scroll_offset: 0,
        }
    }

    fn cell(c: char) -> Cell {
        Cell {
            c,
            ..Cell::default()
        }
    }

    #[test]
    fn paints_colors_crops_and_never_passes_a_control_character() {
        let red = Cell {
            fg: Color::Indexed(1),
            flags: cell_flags::BOLD,
            ..cell('R')
        };
        let wide_spacer = Cell {
            flags: cell_flags::WIDE_SPACER,
            ..cell(' ')
        };
        let f = frame(
            2,
            4,
            vec![
                red,
                cell('\u{1b}'),
                wide_spacer,
                cell('z'),
                cell('a'),
                cell('b'),
                cell('c'),
                cell('d'),
            ],
        );
        // Terminal 3 columns wide, header row 0 above the frame, 2 body rows.
        let bytes = paint(&f, 1, 3, 3);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("\x1b[2;1H\x1b[0;1;38;5;1mR"), "{text:?}");
        assert!(
            text.contains("\x1b[0m "),
            "an ESC cell is a space: {text:?}"
        );
        assert!(
            !text.contains('z'),
            "the spacer is skipped and z is cropped"
        );
        assert_eq!(
            text.matches('\x1b').count(),
            text.matches("\x1b[").count(),
            "every ESC starts a sequence this module wrote"
        );
    }
}
