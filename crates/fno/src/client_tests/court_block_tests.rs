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
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
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

    assert_eq!(view.court_block_rows(), 3, "minimized is three lines");
    let full = view.sideline_visible_rows() + view.court_block_rows();

    view.court.toggle();
    let expanded = view.court.expanded_lines(&view.agent_ages()).len();
    assert_eq!(view.court_block_rows(), expanded);
    assert_eq!(view.sideline_visible_rows(), full - expanded);

    // Too short: the block drops, the rows never do.
    view.term = (3, 100);
    assert_eq!(view.court_block_rows(), 0);
    assert_eq!(
        view.sideline_visible_rows(),
        3 - view.bottom_row_is_chrome() as usize
    );
}
