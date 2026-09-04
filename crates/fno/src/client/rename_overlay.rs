//! The rename overlay's target vocabulary and open path. One buffer, one key
//! handler, one esc; the overlay widened from tab-only to squad, then to a
//! sideline registry row. Lives beside the test family (client/tests/
//! rename_tests.rs) so client.rs keeps shrinking.

use super::{TabId, View};

/// The entity a rename overlay is editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RenameTarget {
    Tab(TabId),
    Squad(u64),
    /// A sideline row's registry label, captured at open. The label is
    /// mutable (that is the point), so capture-at-open is what the send
    /// addresses: a row renamed mid-edit does not retarget this one, and the
    /// server re-resolves `name` at execute anyway, refusing a moved label.
    Agent(String),
}

impl View {
    /// Open the rename overlay modally for `target`, clearing any other
    /// keyboard-opened overlay first. A lingering selector would swallow the
    /// name.
    pub(super) fn open_rename(&mut self, target: RenameTarget) {
        self.open_rename_seeded(target, String::new())
    }

    /// [`View::open_rename`] with the buffer pre-filled: an agent rename seeds
    /// the row's CURRENT label, so Enter with no edit lands on the rename
    /// verb's same-label no-op rather than an empty refusal.
    pub(super) fn open_rename_seeded(&mut self, target: RenameTarget, seed: String) {
        self.selector = None;
        self.answers = None;
        self.yard = None;
        self.search = None;
        self.move_pick = None;
        self.attach_place = None;
        self.create = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.rename = Some((target, seed));
        self.rename_esc.clear();
    }
}
