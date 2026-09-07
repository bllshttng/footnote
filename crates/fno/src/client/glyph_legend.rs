//! The sideline's glyph legend (x-b5d1): what the header band's counts and
//! each row's leading glyph mean. Generated from the one lattice table the
//! rows render (`lattice_style` -> `lattice_glyph` -> `SEVERITY_ORDER`),
//! so the legend cannot drift from what the screen draws. A hand-written
//! copy of the mapping would be a second source that ships its first lie
//! the next time a state is added. Lives outside client.rs under the
//! file-budget gate.

use super::{lattice_glyph, LatticeState, PopupRow, SEVERITY_ORDER};

/// (x-b5d1) The one-line reading of a glyph. The strings restate the
/// `LatticeState` enum docs' semantics; the exhaustive match keeps a new
/// state a compile error here, the same lock every spelling of the table
/// carries.
fn lattice_label(s: LatticeState) -> &'static str {
    match s {
        LatticeState::Working => "working",
        LatticeState::Idle => "idle, waiting for input",
        LatticeState::Blocked => "needs attention",
        LatticeState::DoneUnseen => "done, not yet viewed",
        LatticeState::Exited => "exited, confirmed dead, respawn is safe",
        LatticeState::Unmeasured => "unmeasured, no confirmed reading, look before you spawn",
        LatticeState::Empty => "empty shell, nothing running yet",
    }
}

/// The legend section, in header-band order: the single-sourced table turned
/// into inert popup rows. `enabled: false` makes each row inert (arrows
/// skip, Enter cannot fire, click swallowed) - the popup's documented
/// disabled-Entry semantics.
pub(super) fn legend_rows() -> Vec<PopupRow> {
    let mut rows = vec![PopupRow::Header("sideline glyphs".into())];
    rows.extend(SEVERITY_ORDER.iter().map(|&s| PopupRow::Entry {
        glyph: lattice_glyph(s).0.to_string(),
        label: lattice_label(s).to_string(),
        hint: String::new(),
        enabled: false,
    }));
    rows
}
