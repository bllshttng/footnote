//! (x-aeab) The court block's sideline placement test, split out of
//! `client_tests.rs` because that file is over the line budget and
//! shrink-only. A child of the client test module, so the private `View`
//! surface stays reachable.

use super::*;

#[test]
fn the_court_block_shrinks_the_sideline_and_yields_when_too_short() {
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
    assert!(view.court.take_want());
    view.court.apply(Some(crate::court_overlay::Court {
        lane_count: None,
        per_lane_cpu_cores: None,
        per_lane_mem_gb: None,
        cost_source: String::new(),
        refused_reason: String::new(),
        census: Default::default(),
        arms: Vec::new(),
    }));

    assert_eq!(view.court_block_rows(), 4, "minimized is four lines");
    let full = view.sideline_visible_rows() + view.court_block_rows();

    view.court.toggle();
    let expanded = view.court.expanded_lines(&view.agent_ages()).len();
    assert_eq!(view.court_block_rows(), expanded);
    assert_eq!(view.sideline_visible_rows(), full - expanded);

    // Too short: the block drops, the rows never do. The strip still owns
    // its row, so a 3-row terminal leaves one row less.
    view.term = (3, 100);
    assert_eq!(view.court_block_rows(), 0);
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
fn the_court_fold_holds_no_rows_in_the_board_view() {
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
    assert!(view.court.take_want());
    view.court.apply(Some(crate::court_overlay::Court {
        lane_count: None,
        per_lane_cpu_cores: None,
        per_lane_mem_gb: None,
        cost_source: String::new(),
        refused_reason: String::new(),
        census: Default::default(),
        arms: Vec::new(),
    }));
    view.court.toggle();
    view.sideline_view = crate::view_store::SidelineView::Backlog;

    assert_eq!(
        view.court_block_rows(),
        0,
        "the expanded fold holds no rows under the board"
    );
    assert_eq!(
        view.sideline_visible_rows(),
        24 - 1 - view.bottom_row_is_chrome() as usize,
        "the full column is the list region again"
    );
}
