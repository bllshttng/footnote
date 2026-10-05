//! The org-board view-state test helpers, moved out of client.rs under the
//! file budget: inherent methods on View, test-only, resolved by type from
//! every client test module.

use super::*;

impl View {
    pub(crate) fn squad_view(&self, id: u64) -> SectionView {
        match squad_key(&self.layout, id) {
            Some(key) => self.section_view(&key),
            None => SectionView::Collapsed,
        }
    }

    pub(crate) fn cycle_squad(&mut self, id: u64) {
        if let Some(key) = squad_key(&self.layout, id) {
            self.cycle_section(key);
        }
    }

    pub(crate) fn set_squad_view(&mut self, id: u64, view: SectionView) {
        if let Some(key) = squad_key(&self.layout, id) {
            self.section_view.insert(key, view);
        }
    }

    pub(crate) fn expand_pull_sections(&mut self) {
        self.section_view
            .insert(SectionKey::Elsewhere, SectionView::Expanded);
    }
}
