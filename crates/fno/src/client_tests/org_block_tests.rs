//! (x-aeab) The org block's sideline placement test, split out of
//! `client_tests.rs` because that file is over the line budget and
//! shrink-only. A child of the client test module, so the private `View`
//! surface stays reachable.

use super::*;

#[test]
fn the_org_block_shrinks_the_sideline_and_yields_when_too_short() {
    let mut view = View::new(
        (24, 100),
        "main".into(),
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
            focus_node: None,
        },
    );
    view.org.toggle(); // the fold runs expanded-only; a reading can only land there
    assert!(view.org.take_want(&[]).is_some());
    view.org.apply(crate::org_overlay::FoldPart::Lanes(Some(
        crate::org_overlay::Org::default(),
    )));

    view.org.toggle(); // back to minimized for the glance-shape checks
    assert_eq!(view.org_block_rows(), 5, "minimized is five lines");
    let full = view.sideline_visible_rows() + view.org_block_rows();

    view.org.toggle();
    let expanded = view
        .org
        .expanded_lines(&view.layout.agents, view.sideline_paint_w() - 1, 40)
        .len();
    assert_eq!(view.org_block_rows(), expanded);
    assert_eq!(view.sideline_visible_rows(), full - expanded);

    // Too short: the block drops, the rows never do. The strip still owns
    // its row, so a 3-row terminal leaves one row less.
    view.term = (3, 100);
    assert_eq!(view.org_block_rows(), 0);
    assert_eq!(
        view.sideline_visible_rows(),
        3 - 1 - view.bottom_row_is_chrome() as usize
    );
}

// the block is agents-view chrome. The board view paints its own
// full-column surface, so an expanded fold must not hold rows there - the
// reserved rows came off the hit math's region and pinned the pinned-footer
// row inside the board's painted area.
#[test]
fn the_org_fold_holds_no_rows_in_the_board_view() {
    let mut view = View::new(
        (24, 100),
        "main".into(),
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
            focus_node: None,
        },
    );
    view.org.toggle(); // the fold runs expanded-only; a reading can only land there
    assert!(view.org.take_want(&[]).is_some());
    view.org.apply(crate::org_overlay::FoldPart::Lanes(Some(
        crate::org_overlay::Org::default(),
    )));
    view.sideline_view = crate::view_store::SidelineView::Backlog;

    assert_eq!(
        view.org_block_rows(),
        0,
        "the expanded fold holds no rows under the board"
    );
    assert_eq!(
        view.sideline_visible_rows(),
        24 - 1 - view.bottom_row_is_chrome() as usize,
        "the full column is the list region again"
    );
}

// The org-board view-state helpers, out of client.rs and then out of the
// over-budget aggregator: inherent methods on View, test-only, resolved by
// type from every client test module.
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

// `draw_sideline` paints at `panel_w - 1` (the divider column is not text). An
// org line fitted to the full panel width lost its last character to that
// clip: `team 0/3 live · 1 observed` painted as `· 1 observe`. Sweep the
// panel width so some width lands a fitted line exactly on the boundary.
#[test]
fn a_fitted_org_line_paints_whole_at_every_panel_width() {
    let mut view = View::new(
        (24, 100),
        "main".into(),
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
            focus_node: None,
        },
    );
    view.org.apply(crate::org_overlay::FoldPart::Gate(Ok(
        crate::org_overlay::GateReading {
            slots: Some(0),
            max_live: Some(3),
            ..Default::default()
        },
    )));
    // Text columns only: the last column of the panel is the divider glyph.
    for width in 20..=70u16 {
        view.sideline_width = width;
        let panel_w = view.panel_w() as usize;
        let (_, lines) = view.org_block_layout(24);
        let want = lines
            .iter()
            .find(|l| l.starts_with("  team"))
            .expect("the minimized glance carries a team line");
        let (rows, cols) = (24usize, 100usize);
        let mut cells = vec![crate::proto::Cell::default(); rows * cols];
        view.draw_sideline(&mut cells, rows, cols, panel_w);
        let painted = (0..rows)
            .map(|r| {
                cells[r * cols..r * cols + panel_w - 1]
                    .iter()
                    .map(|c| c.c)
                    .collect::<String>()
            })
            .find(|row| row.starts_with("  team"))
            .expect("the team line painted");
        assert_eq!(painted.trim_end(), want.trim_end(), "panel width {width}");
    }
}
