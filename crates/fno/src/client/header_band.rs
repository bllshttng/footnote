//! The section header band's pieces: the severity-ordered rollup fold,
//! the band composer and its rule, and the flags. Split from client.rs
//! under the file-budget gate; the painters reach these through
//! `super::header_band` via client.rs's re-import.

use super::*;

/// (US2) Severity order for the header rollup strip: most-severe first,
/// so the strip reads `▲ ✓ ● ○ ∅ ✗ ?` and narrow-panel truncation drops from
/// the least-severe (`?`) end. `Unmeasured` sits after `Exited`: it is a
/// sub-case of the same terminal bucket, just less certain, so it never
/// outranks a live state. `Empty` sits after `Idle` and before the
/// terminal pair: a pristine shell is less severe than any worker state but
/// still a live pane, not a terminal one. The one ordering the fold and the
/// truncation share.
///
/// NOT the same ordering as [`PaneState`]'s derive, and this comment used to
/// claim to be "the single ordering authority" beside a sibling claiming the
/// same thing, which cannot both be true. They answer different questions and
/// have disagreed since before, on `Working` versus `DoneUnseen`. This
/// one answers IN WHAT ORDER A SECTION'S COUNTS ARE LISTED AND TRUNCATED;
/// `PaneState` answers WHICH ROW IS WORST for a `min()` rollup. A known
/// consequence of the split, left as is: `Unmeasured` is now reachable for
/// LIVE rows, and truncation drops from that end, so a narrow panel can keep
/// a dead `✗` count and drop live `?` rows. Changing that is a display-policy
/// decision, not a correctness fix - do not "align" the two lists to make it
/// go away.
pub(super) const SEVERITY_ORDER: [LatticeState; 7] = [
    LatticeState::Blocked,
    LatticeState::DoneUnseen,
    LatticeState::Working,
    LatticeState::Idle,
    LatticeState::Empty,
    LatticeState::Exited,
    LatticeState::Unmeasured,
];

/// (US2) Fold a section's rows into per-state counts, nonzero only, in
/// severity order. Exhaustive over `LatticeState` (the lock-3 posture):
/// a new state is a compile error here, never a silently uncounted glyph.
pub(super) fn section_rollup(
    states: impl Iterator<Item = LatticeState>,
) -> Vec<(LatticeState, usize)> {
    let mut counts = [0usize; SEVERITY_ORDER.len()];
    for st in states {
        let idx = match st {
            LatticeState::Blocked => 0,
            LatticeState::DoneUnseen => 1,
            LatticeState::Working => 2,
            LatticeState::Idle => 3,
            LatticeState::Empty => 4,
            LatticeState::Exited => 5,
            LatticeState::Unmeasured => 6,
        };
        // The match is exhaustive (a new state breaks the build), but the index
        // mapping is coupled by hand to SEVERITY_ORDER's order; this catches a
        // reorder that would silently miscount (gemini review).
        debug_assert_eq!(
            SEVERITY_ORDER[idx], st,
            "SEVERITY_ORDER and section_rollup indices are out of sync"
        );
        counts[idx] += 1;
    }
    SEVERITY_ORDER
        .iter()
        .zip(counts)
        .filter(|&(_, n)| n > 0)
        .map(|(&s, n)| (s, n))
        .collect()
}

/// The flag set for a section header, demoted from the old always-on
/// INVERSE band: the full-width INVERSE is now the focused-row signal, not the
/// header's, so a header carries zero standing INVERSE cells. The rollup counts
/// (`header_band_text`) are unchanged and still fill the full width.
///
/// EVERY header is BOLD, active or not: the earlier split left an inactive
/// header at exactly the weight of the agent rows beneath it. Active stays
/// legible through the `*` marker and the accented caret. Weight alone does not
/// separate a section though - see [`section_rule`].
pub(super) fn header_band_flags(_active: bool) -> u8 {
    cell_flags::BOLD
}

/// (US1+US2) Compose one section header band: the label at the left, the
/// rollup counts right-aligned, spaces between so the whole string is exactly
/// the panel width `w` (the caller paints it as one INVERSE band). Counts are
/// compact `{glyph}{n}` pairs; when the panel is too narrow, whole pairs drop
/// from the least-severe (`✗`) end - a glyph never renders without its count
/// (AC11) - and the label clips (via `chrome::clip`, no marker) only after every pair is
/// gone. Widths are measured in DISPLAY columns via `glyph_cols` (matching the
/// painter), so a double-width char in a squad name aligns the band instead of
/// overflowing it.
/// The `gap` columns between a header's label and its rollup counts, drawn as a
/// horizontal rule with a space of breathing room at each end (`gap < 3` stays
/// blank - a one-cell dash reads as debris, not a rule).
///
/// The section separator, and the only one available. A terminal grid has one
/// font at one size, and the rest of the vocabulary is already spoken for: BOLD
/// is agent liveness (`lattice_style` bolds working, blocked and done rows, so a
/// header cannot out-weigh a busy workspace), full-width INVERSE is the focused
/// row, DIM is dead, amber is needs-attention. A rule spends none of them and
/// costs no rows, filling space the header already padded with blanks.
fn section_rule(gap: usize) -> String {
    match gap {
        0..=2 => " ".repeat(gap),
        _ => format!(" {} ", "\u{2500}".repeat(gap - 2)),
    }
}

pub(super) fn header_band_text(label: &str, rollup: &[(LatticeState, usize)], w: usize) -> String {
    let mut pairs: Vec<String> = rollup
        .iter()
        .map(|(s, n)| format!("{}{}", lattice_glyph(*s).0, n))
        .collect();
    loop {
        if pairs.is_empty() {
            let label_w: usize = label.chars().map(glyph_cols).sum();
            return match w.checked_sub(label_w) {
                Some(gap) => format!("{label}{}", section_rule(gap)),
                None => crate::chrome::clip(label, w),
            };
        }
        let counts = pairs.join(" ");
        let label_w: usize = label.chars().map(glyph_cols).sum();
        let counts_w: usize = counts.chars().map(glyph_cols).sum();
        if label_w + 1 + counts_w <= w {
            let gap = w - label_w - counts_w;
            return format!("{label}{}{counts}", section_rule(gap));
        }
        pairs.pop(); // drop the least-severe pair and retry
    }
}
