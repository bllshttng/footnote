//! The sideline's density width rules: the pure canonical/floor/admit/ceiling
//! widths and the button glyph, moved verbatim out of client.rs (file budget
//! shrink; the v75 SessionRetired arms rode the same file). Parent constants
//! and `Density` resolve through the glob.
use super::*;

/// (x-2e86) The width a density jumps to when picked as a preset (the density
/// key or button): each mode's canonical size. Free-standing so the preset
/// path can price a mode without being in it.
pub(super) fn canonical_width(d: Density) -> u16 {
    match d {
        Density::Slim => SLIM_PANEL_W,
        Density::Regular => PANEL_W,
        Density::Extended => EXTENDED_PANEL_W,
    }
}

/// (x-2e86) The narrowest width at which a density still renders its structure.
/// A drag below this demotes the density (Locked 5). Only `Extended` has a
/// floor above [`MIN_SLIM_PANEL_W`]: the tree and the rail truncate gracefully
/// down to the slim floor, but the table needs room for status + agent + PR + age.
pub(super) fn min_render_width(d: Density) -> u16 {
    match d {
        Density::Slim | Density::Regular => MIN_SLIM_PANEL_W,
        Density::Extended => MIN_EXTENDED_PANEL_W,
    }
}

/// (x-2e86) The smallest terminal room ([`sideline_max_width`]) at which a
/// density is shown at all; below it the rail AUTO-HIDES so the panes keep the
/// screen, rather than rendering a rail too cramped to be worth its columns.
///
/// This is the pre-x-2e86 per-density floor, and it is deliberately NOT
/// [`min_render_width`]: a `Slim` rail stays useful squished to
/// [`MIN_SLIM_PANEL_W`], but a `Regular` tree below [`PANEL_W`] or an `Extended`
/// table below [`MIN_EXTENDED_PANEL_W`] is too tight to read, so on a narrow
/// terminal those hide (giving content the room) exactly as they did before free
/// width. It gates on terminal CAPACITY, not on the stored width, so a rail the
/// terminal CAN admit still renders at a small DRAGGED width (a drag-to-8 Regular
/// shows, because the terminal that fits 28 also fits 8).
pub(super) fn min_admit_width(d: Density) -> u16 {
    match d {
        Density::Slim => MIN_SLIM_PANEL_W,
        Density::Regular => PANEL_W,
        Density::Extended => MIN_EXTENDED_PANEL_W,
    }
}

/// (x-2e86) The largest sideline width this terminal allows: 60% of the columns,
/// but never so wide that content drops below [`MIN_CONTENT_COLS`] - the tighter
/// bound wins. Saturating throughout (a u32 intermediate for the 60%) so a
/// degenerate terminal underflows to 0 rather than panicking; `panel_w` reads
/// that 0 as "too narrow, hide the rail".
pub(super) fn sideline_max_width(term_cols: u16) -> u16 {
    let sixty = ((term_cols as u32) * 3 / 5) as u16;
    sixty.min(term_cols.saturating_sub(MIN_CONTENT_COLS))
}

pub(super) fn density_glyph(d: Density) -> char {
    // (x-2e86) A fill ramp - rail, tree, table - reading as increasing density.
    // All three are East-Asian-width 1 (U+2581/2584/2588), which the button's
    // column math and the header-band composition require.
    match d {
        Density::Slim => '▁',
        Density::Regular => '▄',
        Density::Extended => '█',
    }
}
