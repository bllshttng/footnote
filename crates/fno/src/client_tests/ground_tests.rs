//! The OSC background takeover's client-side contract: the compositor's
//! ground fill, and the kill switch's shape.

use super::*;

/// One row of plain `a` cells on a Default background, drawn through a
/// compositor carrying the superscript base (#141414 = rgb 20,20,20), must
/// emit that base as the cell background (SGR 48;2;20;20;20).
#[test]
fn the_ground_fills_default_background_cells() {
    let compositor = Compositor::new(Some(Color::Rgb(20, 20, 20)));
    let frame = Frame {
        rows: 1,
        cols: 4,
        cells: vec![Cell::default(); 4],
        cursor_row: 0,
        cursor_col: 0,
        cursor_visible: false,
        scroll_offset: 0,
    };
    let mut out = Vec::new();
    compositor
        .draw_row(&mut out, &frame, 0)
        .expect("draw_row into memory");
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains("48;2;20;20;20"),
        "the base fills Default bg cells: {s}"
    );
}

/// Without a ground the painter must leave Default backgrounds alone - the
/// pre-takeover render, byte for byte.
#[test]
fn no_ground_leaves_default_backgrounds_untouched() {
    let compositor = Compositor::new(None);
    let frame = Frame {
        rows: 1,
        cols: 4,
        cells: vec![Cell::default(); 4],
        cursor_row: 0,
        cursor_col: 0,
        cursor_visible: false,
        scroll_offset: 0,
    };
    let mut out = Vec::new();
    compositor
        .draw_row(&mut out, &frame, 0)
        .expect("draw_row into memory");
    let s = String::from_utf8(out).unwrap();
    assert!(
        !s.contains("48;2;"),
        "no ground: no RGB background emitted at all: {s}"
    );
}

/// The ground fill only touches DEFAULT backgrounds: an explicit bg (the
/// sel surface, a swatch) survives untouched.
#[test]
fn explicit_backgrounds_survive_the_ground_fill() {
    let compositor = Compositor::new(Some(Color::Rgb(20, 20, 20)));
    let frame = Frame {
        rows: 1,
        cols: 2,
        cells: vec![
            Cell {
                c: 'x',
                fg: Color::Default,
                bg: Color::Rgb(1, 2, 3),
                flags: 0,
            },
            Cell::default(),
        ],
        cursor_row: 0,
        cursor_col: 0,
        cursor_visible: false,
        scroll_offset: 0,
    };
    let mut out = Vec::new();
    compositor
        .draw_row(&mut out, &frame, 0)
        .expect("draw_row into memory");
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("48;2;1;2;3"), "explicit bg survives: {s}");
    assert!(s.contains("48;2;20;20;20"), "ground fills the rest: {s}");
    assert!(
        !s.contains("48;2;20;20;20") || s.matches("48;2;20;20;20").count() >= 1,
        "sanity"
    );
}
