//! The rename overlay's target vocabulary and open path. One buffer, one key
//! handler, one esc; the overlay widened from tab-only to squad, then to a
//! sideline registry row. Lives beside the test family (client/tests/
//! rename_tests.rs) so client.rs keeps shrinking.

use super::{DisplayRow, TabId, View};

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
    /// Open the rename overlay for the renamable entity under the selector.
    /// Resolve the row before mutating the view so the layout borrow ends first.
    pub(super) fn rename_at_cursor(&mut self, cur: usize) {
        let result = match self.display_rows().get(cur) {
            Some(DisplayRow::Sel(row)) if row.tab.is_none() => {
                Ok((RenameTarget::Squad(row.squad), String::new()))
            }
            Some(DisplayRow::Agent(agent)) if !agent.external => {
                Ok((RenameTarget::Agent(agent.name.clone()), agent.name.clone()))
            }
            Some(DisplayRow::Agent(_)) => {
                Err("an external row's name belongs to its claude session")
            }
            _ => Err("r renames a workspace or agent row"),
        };
        match result {
            Ok((target, seed)) => self.open_rename_seeded(target, seed),
            Err(message) => self.set_notice(message.into()),
        }
    }

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
