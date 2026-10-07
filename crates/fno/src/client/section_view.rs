//! A section's effective view: the one authority behind the caret glyph
//! and the row filter, named by the question it answers. Moved out of the
//! over-budget client.rs with the default it gained (live `~ elsewhere`
//! rows render; d-954c2cbf).

use super::*;

impl View {
    /// A section's effective view, resolved live every frame. The one
    /// authority behind both the caret glyph and the row filter, so they can
    /// never disagree. Order:
    ///   1. An explicit persisted operator choice wins verbatim - it survives a
    ///      restart and outranks every computed default below (Locked 2, AC1-FR).
    ///   2. Else a computed default, recomputed from the layout in hand:
    ///      - the active squad opens `Expanded`, downgrading to
    ///        `LiveOnly` when the section is majority-exited so the dead rows
    ///        fold behind the header's `✗N` while the live agents stay up;
    ///      - an inactive squad stays `Collapsed` - surfacing live rows across
    ///        every idle workspace is the opposite of attention-focus;
    ///      - the pull-section `~ elsewhere` takes the same
    ///        `Expanded`/`LiveOnly` default: every spawn must appear in the
    ///        sideline (d-954c2cbf), and a collapsed fold reads as absence.
    /// The active-squad default lives HERE, not in a map-seed: a seed is
    /// a one-time snapshot that cannot downgrade to LiveOnly as agents exit
    /// mid-session, and it pollutes the map that should hold only choices.
    pub(super) fn section_view(&self, key: &SectionKey) -> SectionView {
        if let Some(chosen) = self.section_view.get(key).copied() {
            return chosen;
        }
        match key {
            SectionKey::Squad(_) if self.is_active_squad(key) => self.expanded_or_live_only(key),
            SectionKey::Squad(_) => SectionView::Collapsed,
            SectionKey::Elsewhere => self.expanded_or_live_only(key),
            // A group band defaults open; the click cycle persists from there.
            SectionKey::Group(_) => self.expanded_or_live_only(key),
        }
    }

    /// The Expanded-tier computed default: `Expanded`, or `LiveOnly` when the
    /// section is majority-exited (its dead rows then fold behind the header's
    /// `✗N` while the live rows stay). Only ever downgrades an Expanded default;
    /// never upgrades a Collapsed inactive squad (Locked 3).
    pub(super) fn expanded_or_live_only(&self, key: &SectionKey) -> SectionView {
        if self.majority_exited(key) {
            SectionView::LiveOnly
        } else {
            SectionView::Expanded
        }
    }

    /// Whether `key` names the currently active squad. Compared through
    /// `squad_matches` (allocation-free) rather than minting a `SectionKey` for
    /// the active id on every call - `section_view` is per-section-per-frame hot.
    pub(super) fn is_active_squad(&self, key: &SectionKey) -> bool {
        self.layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .is_some_and(|s| squad_matches(s, key))
    }

    /// Strict-majority-exited over the section's own rows (`exited * 2 > total`).
    /// Zero rows is never a majority (an empty section keeps Expanded) and a
    /// 50/50 split is not either, so only a real majority downgrades to LiveOnly.
    /// Walks the same membership `section_dead_rows` does, live off the layout
    /// and never cached, so it tracks agents exiting mid-session. Reached by
    /// the Expanded-tier keys (the active squad, and `~ elsewhere` since it
    /// took the same default); an inactive squad reads as "not a majority".
    pub(super) fn majority_exited(&self, key: &SectionKey) -> bool {
        let Some(id) = self
            .layout
            .squads
            .iter()
            .find(|s| squad_matches(s, key))
            .map(|s| s.id)
        else {
            if *key != SectionKey::Elsewhere {
                return false;
            }
            let mut total = 0usize;
            let mut exited = 0usize;
            for a in self.orphans() {
                total += 1;
                exited += a.exited as usize;
            }
            return exited * 2 > total;
        };
        let mut total = 0usize;
        let mut exited = 0usize;
        for a in self.layout.agents.iter().filter(|a| a.squad == Some(id)) {
            total += 1;
            exited += a.exited as usize;
        }
        exited * 2 > total
    }
}
