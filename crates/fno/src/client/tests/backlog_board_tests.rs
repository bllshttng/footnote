//! The backlog board's view tests, mounted by `backlog_board.rs`. Fixtures
//! build `Inputs` the way `backlog_model_tests.rs` does: pure functions
//! over fixture rows, no store, no process.

use super::*;
use crate::backlog_model;
use serde_json::json;

fn board_inputs() -> backlog_model::Inputs {
    let mut inp = backlog_model::Inputs::default();
    inp.backend = "graph".into();
    inp.order = vec!["x-1".into(), "x-2".into(), "x-3".into()];
    inp.rows = vec![
        json!({"id": "x-1", "status": "ready", "priority": "p1", "title": "First card", "project": "fno"}),
        json!({"id": "x-2", "status": "in_progress", "priority": "p2", "title": "mux card", "project": "fno"}),
        json!({"id": "x-3", "status": "ready", "priority": "p2", "title": "Other project", "project": "other"}),
    ];
    inp.flow = json!({"available": false, "reason": "fixture"});
    inp
}

fn board_with(inputs: backlog_model::Inputs) -> BoardView {
    let mut b = BoardView::new(0);
    b.inputs = Some(inputs);
    let q = b.query.to_query().expect("the default query parses");
    b.body = Some(backlog_model::board(b.inputs.as_ref().unwrap(), &q));
    b
}

// AC6-HP: before any gather the body says `reading board...`, never an
// empty board.
#[test]
fn reading_board_line_shows_before_the_first_gather() {
    let b = BoardView::new(0);
    let (lines, follow) = render(&b, 120);
    assert_eq!(lines[0], "reading board...");
    assert!(follow.is_none());
}

// AC4-HP: the stats line counts every column and renders the flow line.
#[test]
fn stats_line_counts_columns_and_names_flow() {
    let b = board_with(board_inputs());
    let (lines, _) = render(&b, 200);
    let stats = &lines[0];
    for word in [
        "In Progress",
        "Now",
        "Next",
        "Later",
        "Triage",
        "Done",
        "│",
        "flow: fixture",
    ] {
        assert!(stats.contains(word), "stats line missing {word}: {stats}");
    }
}

// AC5-HP: `L` cycles project -> epic -> none and keeps the cursor on the
// same card while the new grouping still shows it.
#[test]
fn lanes_cycle_keeps_the_cursor_card() {
    let mut b = board_with(board_inputs());
    first_card(&mut b);
    let card = cursor_card_id(&b);
    cycle_lanes_b(&mut b);
    assert!(matches!(b.query.lanes, backlog_model::LanesBy::Epic));
    assert_eq!(cursor_card_id(&b), card, "cursor keeps its card");
    cycle_lanes_b(&mut b);
    assert!(matches!(b.query.lanes, backlog_model::LanesBy::None));
    cycle_lanes_b(&mut b);
    assert!(
        matches!(b.query.lanes, backlog_model::LanesBy::Project),
        "third press returns to project"
    );
}

// AC6-ERR: a failed read never repaints a good board empty.
#[test]
fn failed_gather_keeps_the_last_good_board() {
    let mut b = board_with(board_inputs());
    let before = b.body.as_ref().unwrap().lanes.len();
    let mut bad = board_inputs();
    bad.rows_error = Some("the store read failed".into());
    b.inputs = Some(bad);
    rederive(&mut b);
    assert_eq!(
        b.body.as_ref().unwrap().lanes.len(),
        before,
        "last good board kept"
    );
    assert!(b.errors.iter().any(|e| e.contains("the store read failed")));
    let (lines, _) = render(&b, 200);
    assert!(lines.iter().any(|l| l.starts_with("! ")), "{lines:?}");
}

// AC7-EDGE: an unavailable flow renders its reason, never numbers.
#[test]
fn flow_unavailable_names_the_reason() {
    let mut inp = board_inputs();
    inp.flow = json!({"available": false, "reason": "no usable window start"});
    let b = board_with(inp);
    let (lines, _) = render(&b, 200);
    assert!(
        lines[0].contains("flow: no usable window start"),
        "{:?}",
        lines[0]
    );
}

// AC8-EDGE: below WIDE_CELLS_AT the cells stack with `Now  12` headers.
#[test]
fn narrow_layout_stacks_the_cells() {
    let b = board_with(board_inputs());
    let (lines, _) = render(&b, 80);
    let stacked = lines
        .iter()
        .filter(|l| {
            l.starts_with("In Progress  ") || l.starts_with("Now  ") || l.starts_with("Next  ")
        })
        .count();
    assert!(stacked >= 3, "expected stacked headers, got {lines:?}");
}

// AC11-EDGE: a filter matching nothing totals zero and says so.
#[test]
fn empty_filter_match_totals_zero_and_names_itself() {
    let mut b = board_with(board_inputs());
    b.query.q = Some("zzz-no-such-card".into());
    rederive(&mut b);
    let total: usize = b
        .body
        .as_ref()
        .unwrap()
        .lanes
        .iter()
        .map(|l| l.cells.iter().map(|c| c.total).sum::<usize>())
        .sum();
    assert_eq!(total, 0, "no cards match");
    let (lines, _) = render(&b, 120);
    assert!(
        lines.iter().any(|l| l.contains("no cards match")),
        "{lines:?}"
    );
}

// The `t` key's view flow: a board wrapped in a live View with a wire
// buffer standing in for the socket.
fn key_view(b: BoardView) -> View {
    let mut v = super::super::tests::two_pane_view();
    v.term = (24, 80);
    v.backlog_board = Some(b);
    v
}

// x-1 carries a cwd so the prefill can select the node's project.
fn target_inputs() -> backlog_model::Inputs {
    let mut inp = board_inputs();
    if let Some(r) = inp.rows.get_mut(0) {
        r["cwd"] = json!("/r/footnote");
    }
    inp
}

// AC10-HP: `t` on an unclaimed card closes the board, prefills the dock
// with /fno:target <id> and the node's project, and writes NOTHING to the
// wire - nothing spawns before the operator's Launch press.
#[test]
fn t_key_prefills_the_launcher_from_a_card() {
    let mut b = board_with(target_inputs());
    focus_card(&mut b, Some("x-1"));
    let mut v = key_view(b);
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        board_keys(&mut v, b"t", &mut sock).await.expect("t folds");
    });
    assert!(v.backlog_board.is_none(), "the board closes");
    assert!(sock.is_empty(), "nothing spawns on t");
    let l = v.launcher.as_ref().expect("the dock is open");
    assert_eq!(l.draft.message, "/fno:target x-1");
    let idx = l.draft.project_idx;
    assert_eq!(l.draft.projects[idx], "/r/footnote");
    assert_eq!(l.draft.node.as_deref(), Some("x-1"));
}

// AC11-ERR: a claimed card refuses BEFORE the dock opens; the board stays
// open and the notice names the in-flight case (the plan-refusal wording).
#[test]
fn t_key_refuses_a_card_already_being_worked() {
    let mut b = board_with(board_inputs());
    focus_card(&mut b, Some("x-2")); // status in_progress -> claimed
    let mut v = key_view(b);
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        board_keys(&mut v, b"t", &mut sock).await.expect("t folds");
    });
    assert!(
        v.backlog_board.is_some(),
        "the board stays open on the refusal"
    );
    assert!(v.launcher.is_none(), "the dock never opens");
    let notice = v.notice.as_ref().map(|(t, _)| t.as_str()).unwrap_or("");
    assert_eq!(
        notice,
        "x-2 is already being worked; open its session instead"
    );
}

// AC12-HP: `t` inside the drill-down targets the drill-down's node, the
// same prefill as a card press.
#[test]
fn t_key_inside_the_drilldown_targets_its_node() {
    let mut b = board_with(target_inputs());
    focus_card(&mut b, Some("x-1"));
    let mut v = key_view(b);
    open_detail(&mut v);
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        node_detail::detail_keys(&mut v, b"t", &mut sock)
            .await
            .expect("t folds");
    });
    assert!(v.backlog_board.is_none(), "the board closes");
    let l = v.launcher.as_ref().expect("the dock is open");
    assert_eq!(l.draft.message, "/fno:target x-1");
}

// AC13-EDGE: a kept non-empty draft is never overwritten; the dock shows
// it and the notice names the way out.
#[test]
fn t_key_keeps_a_held_draft_and_says_so() {
    let mut b = board_with(target_inputs());
    focus_card(&mut b, Some("x-1"));
    let mut v = key_view(b);
    // The dock is already open holding a draft.
    super::super::agent_launcher::open(&mut v);
    if let Some(l) = v.launcher.as_mut() {
        l.draft.message = "fix the flake".into();
        l.draft.revision += 1;
    }
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        board_keys(&mut v, b"t", &mut sock).await.expect("t folds");
    });
    let l = v.launcher.as_ref().expect("the dock stays open");
    assert_eq!(l.draft.message, "fix the flake", "the kept draft survives");
    let notice = v.notice.as_ref().map(|(t, _)| t.as_str()).unwrap_or("");
    assert!(notice.contains("holds a draft"), "notice: {notice}");
}

// ----: the docked sideline + full screen ----

// The dock's narrow render is the stacked one-column shape: each column
// header carries its count, card rows beneath - never the six-wide cells.
#[test]
fn render_at_dock_width_groups_by_column_with_counts() {
    let b = board_with(board_inputs());
    let text_w = super::BOARD_DOCK_W as usize - crate::chrome::Chrome::FRAME_COLS;
    assert!(text_w < WIDE_CELLS_AT, "the dock renders stacked");
    let (lines, follow) = render(&b, text_w);
    assert!(follow.is_some(), "the cursor card is the follow line");
    assert!(
        lines.iter().any(|l| l.starts_with("In Progress")),
        "column group headers: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("First card")));
}

// `x` cycles off -> left -> right -> off on the View (persistence is the
// view store's own test).
#[test]
fn x_key_cycles_the_dock_side() {
    let mut v = key_view(board_with(board_inputs()));
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        board_keys(&mut v, b"x", &mut sock).await.expect("x folds");
    });
    assert!(matches!(v.board_dock, crate::view_store::BoardDock::Left));
    assert!(v
        .notice
        .as_ref()
        .map(|(t, _)| t.contains("left"))
        .unwrap_or(false));
    rt.block_on(async {
        board_keys(&mut v, b"x", &mut sock).await.expect("x folds");
    });
    assert!(matches!(v.board_dock, crate::view_store::BoardDock::Right));
    rt.block_on(async {
        board_keys(&mut v, b"x", &mut sock).await.expect("x folds");
    });
    assert!(matches!(v.board_dock, crate::view_store::BoardDock::Off));
}

// `F` toggles the full-screen board and back.
#[test]
fn f_key_toggles_full_screen() {
    let mut v = key_view(board_with(board_inputs()));
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        board_keys(&mut v, b"F", &mut sock).await.expect("F folds");
    });
    assert!(v.board_full);
    rt.block_on(async {
        board_keys(&mut v, b"F", &mut sock).await.expect("F folds");
    });
    assert!(!v.board_full);
}

// Docked left: the column sits right of the agent sideline at the full
// dock width; full screen or a closed board gives the width back.
#[test]
fn dock_left_reserves_width_right_of_the_sideline() {
    let mut v = key_view(board_with(board_inputs()));
    assert_eq!(v.board_left_w(), 0, "closed board docks nothing");
    v.backlog_board = None;
    v.board_dock = crate::view_store::BoardDock::Left;
    assert_eq!(v.board_left_w(), 0, "a closed board never docks");
    v.backlog_board = Some(BoardView::new(0));
    assert_eq!(v.board_left_w(), super::BOARD_DOCK_W);
    assert_eq!(v.board_right_w(), 0);
    assert_eq!(v.left_chrome_w(), v.panel_w() + super::BOARD_DOCK_W);
    v.board_full = true;
    assert_eq!(v.board_left_w(), 0, "full screen is not docked");
}

// The dock rect: left pins to the sideline's right edge; right hugs the
// terminal's right edge (no feed panel open); full screen is None.
#[test]
fn dock_rect_pins_to_the_chosen_side() {
    let mut v = key_view(board_with(board_inputs()));
    v.board_dock = crate::view_store::BoardDock::Left;
    let ((row, col), (_h, w)) = v.backlog_dock_rect().expect("docked");
    assert_eq!(col, v.panel_w() as usize);
    assert_eq!(row, TAB_BAR_ROWS as usize);
    assert_eq!(w, super::BOARD_DOCK_W as usize);
    v.board_dock = crate::view_store::BoardDock::Right;
    let ((_row, col), _) = v.backlog_dock_rect().expect("docked");
    assert_eq!(
        col,
        80 - super::BOARD_DOCK_W as usize,
        "right dock hugs the right edge (feed closed)"
    );
    v.board_full = true;
    assert!(
        v.backlog_dock_rect().is_none(),
        "full screen paints the overlay"
    );
    v.board_full = false;
    v.board_dock = crate::view_store::BoardDock::Off;
    assert!(v.backlog_dock_rect().is_none());
}

// The composed frame paints the docked column with the board chrome.
#[test]
fn compose_paints_the_docked_board_column() {
    let mut v = key_view(board_with(board_inputs()));
    v.board_dock = crate::view_store::BoardDock::Left;
    let text = crate::vt::frame_text(&v.compose());
    assert!(text.contains("backlog"), "chrome title: {text}");
    assert!(text.contains("x side"), "dock footer hint: {text}");
    assert!(text.contains("In Progress"), "column group header: {text}");
}

// Full screen paints the board box at column 0 (the docked column paints
// it right of the sideline), never the dock hint.
#[test]
fn compose_full_screen_board_fills_the_terminal() {
    let mut v = key_view(board_with(board_inputs()));
    v.board_dock = crate::view_store::BoardDock::Left;
    v.board_full = true;
    let text = crate::vt::frame_text(&v.compose());
    assert!(
        text.lines().any(|l| l.starts_with("╭─ backlog")),
        "board box at column 0: {text}"
    );
    assert!(!text.contains("x side"), "no dock when full screen");
}
