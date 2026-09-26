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
// ----: the sideline backlog view + full screen ----

// The narrow render is the stacked one-column shape: each column header
// carries its count, card rows beneath - never the six-wide cells.
#[test]
fn render_at_column_width_groups_by_column_with_counts() {
    let b = board_with(board_inputs());
    let text_w = 34;
    assert!(text_w < WIDE_CELLS_AT, "the column renders stacked");
    let (lines, follow) = render(&b, text_w);
    assert!(follow.is_some(), "the cursor card is the follow line");
    assert!(
        lines.iter().any(|l| l.starts_with("In Progress")),
        "column group headers: {lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("First card")));
}

// `V` cycles the sideline view and the board rides with it: to backlog
// opens the board, back to agents closes it. `x` no longer does anything
// on the board (the dock is gone).
#[test]
fn v_key_cycles_the_sideline_view() {
    let mut v = key_view(board_with(board_inputs()));
    v.backlog_board = None;
    v.experimental_backlog = true;
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(v.backlog_board.is_none(), "agents view starts closed");
    rt.block_on(async {
        cycle_sideline_view(&mut v);
    });
    assert!(matches!(
        v.sideline_view,
        crate::view_store::SidelineView::Backlog
    ));
    assert!(v.backlog_board.is_some(), "backlog view opens the board");
    rt.block_on(async {
        cycle_sideline_view(&mut v);
    });
    assert!(matches!(
        v.sideline_view,
        crate::view_store::SidelineView::Agents
    ));
    assert!(v.backlog_board.is_none(), "agents view closes the board");
}

// `F` toggles the full-screen board and back; the first Esc folds the
// full screen back to the column, the second closes the board.
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
        board_keys(&mut v, b"\x1b", &mut sock)
            .await
            .expect("esc folds");
    });
    assert!(!v.board_full, "esc unfulls first");
    assert!(v.backlog_board.is_some(), "still the backlog column");
    rt.block_on(async {
        board_keys(&mut v, b"\x1b", &mut sock)
            .await
            .expect("esc folds");
    });
    assert!(
        matches!(v.sideline_view, crate::view_store::SidelineView::Agents),
        "second esc returns the sideline to agents"
    );
}

// The composed frame paints the backlog inside the sideline column with
// NO second border: the board's text starts at column 0 and no chrome box
// wraps it.
#[test]
fn compose_paints_the_backlog_inside_the_sideline_column() {
    let mut v = key_view(board_with(board_inputs()));
    v.experimental_backlog = true;
    v.sideline_view = crate::view_store::SidelineView::Backlog;
    let text = crate::vt::frame_text(&v.compose());
    assert!(text.contains("In Progress"), "column header: {text}");
    assert!(text.contains("First card"), "card row: {text}");
    for line in text.lines() {
        assert!(!line.starts_with('┌'), "no second border anywhere: {text}");
    }
}

// Full screen paints the board box at column 0 and lists the edit keys in
// the footer (D6).
#[test]
fn compose_full_screen_board_fills_the_terminal() {
    let mut v = key_view(board_with(board_inputs()));
    v.experimental_backlog = true;
    v.sideline_view = crate::view_store::SidelineView::Backlog;
    v.board_full = true;
    let text = crate::vt::frame_text(&v.compose());
    assert!(
        text.lines().any(|l| l.starts_with("┌─ backlog")),
        "board box at column 0: {text}"
    );
    assert!(
        text.contains("e/p/s/S/D/N/E"),
        "footer lists the edit keys: {text}"
    );
}

// ----: D1/D2 proof - distinct attributes, no INVERSE, real frames ----

/// A view whose sideline shows the fixture backlog, like the operator's.
fn sideline_backlog_view() -> View {
    let (rows, cols) = (24u16, 100u16);
    let mut view = View::new(
        (rows, cols),
        "main".into(),
        LayoutView {
            squads: vec![SquadMeta {
                id: 1,
                name: "main".into(),
                canonical_cwd: "/code/main".into(),
                tabs: vec![TabMeta {
                    id: 0,
                    name: "0".into(),
                    named: false,
                    panes: Vec::new(),
                }],
                active_tab: 0,
                panes: 1,
            }],
            active_squad: 1,
            panes: vec![(
                10,
                Rect {
                    x: 0,
                    y: 0,
                    rows: rows - 1,
                    cols: cols - 28,
                },
            )],
            focus: 10,
            area: (rows - 1, cols - 28),
            agents: vec![],
            focus_node: None,
        },
    );
    view.experimental_backlog = true;
    view.sideline_view = crate::view_store::SidelineView::Backlog;
    view.backlog_board = Some(board_with(board_inputs()));
    view
}

fn cell_at(frame: &crate::proto::Frame, r: usize, c: usize, cols: usize) -> crate::proto::Cell {
    frame.cells[r * cols + c]
}

// The hierarchy proof: in the composed backlog column, a column header
// cell, a card-id cell, a meta cell and a body cell carry DISTINCT
// attribute sets, no cell in the column carries INVERSE, and the cursor
// row wears the explicit band pair.
#[test]
fn backlog_panel_cells_carry_distinct_attributes() {
    let view = sideline_backlog_view();
    let frame = view.compose();
    let cols = 100usize;
    let text = crate::vt::frame_text(&frame);
    let find_row = |needle: &str| {
        text.lines()
            .position(|l| l.starts_with(needle))
            .unwrap_or_else(|| panic!("no row starts with {needle:?}: {text}"))
    };
    // The stats line leads with the same words, so the column head is the
    // LAST row that starts with one.
    let meta_row = find_row("In Progress");
    let head_row = text
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with("In Progress"))
        .map(|(i, _)| i)
        .last()
        .expect("no column head row");
    let body_row = text
        .lines()
        .position(|l| l.contains("x-2"))
        .expect("no card row for x-2");
    // A column header cell is BOLD.
    let head = cell_at(&frame, head_row, 2, cols);
    assert!(
        head.flags & crate::proto::cell_flags::BOLD != 0,
        "header bold"
    );
    // A meta (summary/counts) cell is the dim slot, no INVERSE.
    let meta = cell_at(&frame, meta_row, 2, cols);
    assert_eq!(meta.fg, crate::proto::Color::Indexed(8), "meta dim slot");
    // The CURSOR row (x-2) wears the explicit band pair across its full
    // width, id included: the band is the one place a color pair is legal.
    let band = cell_at(&frame, body_row, 1, cols);
    assert_eq!(band.bg, crate::proto::Color::Indexed(0), "band surface");
    assert_eq!(band.fg, crate::proto::Color::Indexed(3), "band accent text");
    // A NON-cursor card id takes the accent slot, and its title is plain.
    let id_row = text
        .lines()
        .position(|l| l.contains("x-1"))
        .expect("no card row for x-1");
    let line = text.lines().nth(id_row).expect("id row");
    let id_col = line.find("x-1").expect("id on its row") + 1;
    let id_cell = cell_at(&frame, id_row, id_col + 1, cols);
    assert_eq!(
        id_cell.fg,
        crate::proto::Color::Indexed(3),
        "id accent slot"
    );
    let title_col = line.find("First card").expect("title on its row") + 1;
    let title_cell = cell_at(&frame, id_row, title_col + 1, cols);
    assert_eq!(title_cell.fg, crate::proto::Color::Default, "title plain");
    assert_eq!(title_cell.flags, 0, "title plain");
    // Nothing in the painted column is inverse, and no second border.
    for r in 0..24usize {
        for c in 0..27usize {
            let cell = cell_at(&frame, r, c, cols);
            assert!(
                cell.flags & crate::proto::cell_flags::INVERSE == 0,
                "INVERSE at {r},{c}: {text}"
            );
        }
        assert!(
            !text
                .lines()
                .nth(r)
                .map(|l| l.starts_with('┌'))
                .unwrap_or(false),
            "no second border: {text}"
        );
    }
}

// The full-screen board keeps the hierarchy on the terminal's own bg.
#[test]
fn full_board_panel_cells_match_the_theme_bg() {
    let mut view = sideline_backlog_view();
    view.board_full = true;
    let frame = view.compose();
    let text = crate::vt::frame_text(&frame);
    assert!(
        text.lines()
            .any(|l| l.starts_with("\u{250c}\u{2500} backlog")),
        "board box at column 0: {text}"
    );
    assert!(text.contains("In Progress"), "{text}");
}

// Evidence shots (FNO_UX_SHOTS): the sideline column, the full board and
// the node detail, each composed for real.
#[test]
fn ux_shot_backlog_sideline_column() {
    use crate::frame_html::write_shot;
    let view = sideline_backlog_view();
    let frame = view.compose();
    write_shot(
        &frame,
        "ux-shot-backlog-sideline",
        "the backlog as a sideline view",
    );
}

#[test]
fn ux_shot_backlog_full_board() {
    use crate::frame_html::write_shot;
    let mut view = sideline_backlog_view();
    view.board_full = true;
    let frame = view.compose();
    write_shot(
        &frame,
        "ux-shot-backlog-full-board",
        "the full-screen backlog board",
    );
}

#[test]
fn ux_shot_backlog_node_detail() {
    use crate::frame_html::write_shot;
    let mut view = sideline_backlog_view();
    if let Some(b) = view.backlog_board.as_mut() {
        b.detail = Some(node_detail::NodeDetailOverlay {
            node_id: "x-1".into(),
            trail: vec![],
            sel: 0,
            details_open: false,
        });
    }
    let frame = view.compose();
    write_shot(&frame, "ux-shot-backlog-detail", "the node detail overlay");
}

// The same three frames under the user's theme (Catppuccin): the panel
// must stay on the theme's bg with its palette slots - the pale-fill
// regression shot.
#[test]
fn ux_shot_backlog_sideline_column_catppuccin() {
    use crate::frame_html::write_shot;
    let mut view = sideline_backlog_view();
    view.theme = crate::theme::Theme::from_name("catppuccin").0;
    let frame = view.compose();
    write_shot(
        &frame,
        "ux-shot-backlog-sideline-catppuccin",
        "the backlog sideline, catppuccin",
    );
}

#[test]
fn ux_shot_backlog_node_detail_catppuccin() {
    use crate::frame_html::write_shot;
    let mut view = sideline_backlog_view();
    view.theme = crate::theme::Theme::from_name("catppuccin").0;
    if let Some(b) = view.backlog_board.as_mut() {
        b.detail = Some(node_detail::NodeDetailOverlay {
            node_id: "x-1".into(),
            trail: vec![],
            sel: 0,
            details_open: false,
        });
    }
    let frame = view.compose();
    write_shot(
        &frame,
        "ux-shot-backlog-detail-catppuccin",
        "the node detail overlay, catppuccin",
    );
}

// ----: D6 proof - every edit key works from the board AND the detail ----

/// The edit keys both surfaces accept: key, the input kind or write it
/// must queue.
fn edit_key_opens_input(key: &[u8], kind: BoardInputKind, from_detail: bool) {
    let mut v = key_view(board_with(board_inputs()));
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    if from_detail {
        if let Some(b) = v.backlog_board.as_mut() {
            b.detail = Some(node_detail::NodeDetailOverlay {
                node_id: "x-1".into(),
                trail: vec![],
                sel: 0,
                details_open: false,
            });
        }
    }
    if let Some(b) = v.backlog_board.as_mut() {
        focus_card(b, Some("x-1"));
    }
    {
        let b = v.backlog_board.as_ref().expect("board");
        edit_target(b).expect("a target card");
    }
    rt.block_on(async {
        if from_detail {
            node_detail::detail_keys(&mut v, key, &mut sock)
                .await
                .expect("key folds");
        } else {
            board_keys(&mut v, key, &mut sock).await.expect("key folds");
        }
    });
    let b = v.backlog_board.as_ref().expect("board open");
    assert_eq!(
        b.input.as_ref().map(|(k, _)| *k),
        Some(kind),
        "key {key:?} from detail={from_detail} opens its input"
    );
}

#[test]
fn board_edit_keys_open_their_inputs() {
    edit_key_opens_input(b"e", BoardInputKind::Title, false);
    edit_key_opens_input(b"D", BoardInputKind::Append, false);
    edit_key_opens_input(b"N", BoardInputKind::Note, false);
}

#[test]
fn detail_edit_keys_open_their_inputs() {
    edit_key_opens_input(b"e", BoardInputKind::Title, true);
    edit_key_opens_input(b"D", BoardInputKind::Append, true);
    edit_key_opens_input(b"N", BoardInputKind::Note, true);
}

// p/s/S open their pickers from both surfaces; the target follows the
// detail's node when it is open.
#[test]
fn field_pickers_open_from_board_and_detail() {
    for (key, from_detail) in [
        (b'p', false),
        (b's', false),
        (b'S', false),
        (b'p', true),
        (b's', true),
        (b'S', true),
    ] {
        let mut v = key_view(board_with(board_inputs()));
        let mut sock: Vec<u8> = Vec::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        if from_detail {
            if let Some(b) = v.backlog_board.as_mut() {
                b.detail = Some(node_detail::NodeDetailOverlay {
                    node_id: "x-1".into(),
                    trail: vec![],
                    sel: 0,
                    details_open: false,
                });
            }
        }
        rt.block_on(async {
            board_keys(&mut v, &[key], &mut sock)
                .await
                .expect("key folds");
        });
        let b = v.backlog_board.as_ref().expect("board open");
        assert!(
            b.pick.is_some(),
            "{key} from detail={from_detail} opens the picker"
        );
    }
}

// The `c` column picker: opening, hiding the focus column, and the focus
// width clamp (25..=75), each persisted.
#[test]
fn colpick_hides_and_rewides_the_focus_column() {
    let mut v = key_view(board_with(board_inputs()));
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        board_keys(&mut v, b"c", &mut sock).await.expect("c folds");
    });
    assert!(
        v.backlog_board.as_ref().expect("board").colpick.is_some(),
        "c opens the picker"
    );
    // Enter on the focus column hides it; the cursor clamps to the new
    // last shown column.
    rt.block_on(async {
        colpick_keys(&mut v, b"\r");
    });
    let b = v.backlog_board.as_ref().expect("board");
    assert_eq!(b.layout.columns.len(), 5, "a column was hidden");
    assert!(b.col < b.layout.columns.len(), "cursor clamped");
    // Widen past the clamp; 75 holds.
    for _ in 0..10 {
        rt.block_on(async {
            colpick_keys(&mut v, b"+");
        });
    }
    let b = v.backlog_board.as_ref().expect("board");
    assert_eq!(b.layout.focus_pct, 75, "focus clamps at 75");
}

// The crown's finding: a wide row merged its columns' role walks out of
// lockstep, so a header's style landed mid-word (`No|w`). Each header word
// carries exactly one style.
#[test]
fn wide_cell_headers_carry_one_style_per_header() {
    let b = board_with(board_inputs());
    let (lines, _) = render(&b, 200);
    // The stats and flow lines also name every column but carry `·`; the
    // merged wide header row does not.
    let header = lines
        .iter()
        .filter(|l| !l.contains('\u{b7}'))
        .find(|l| l.starts_with("In Progress") && l.contains("Triage"))
        .expect("the wide row merges the cell headers onto one line");
    for word in ["In Progress", "Now", "Next", "Later", "Triage"] {
        let at = header.find(word).expect(word);
        let roles = &header.roles[at..at + word.len()];
        assert!(
            roles.iter().all(|&r| r == roles[0]),
            "{word} must carry one style, got {roles:?}"
        );
    }
}

// The wide layout keeps every shown column inside the row width: at the
// WIDE_CELLS_AT threshold with the six default columns, the last column's
// header still paints (the 12-column floors never overrun `w`).
#[test]
fn wide_layout_fits_every_shown_column_at_the_threshold() {
    let b = board_with(board_inputs());
    let (lines, _) = render(&b, WIDE_CELLS_AT);
    let header = lines
        .iter()
        .filter(|l| !l.contains('\u{b7}'))
        .find(|l| l.starts_with("In Progress") && l.contains("Triage"))
        .expect("the wide header row renders at the threshold");
    assert!(
        header.contains("Done"),
        "last column survives the cut: {header}"
    );
}

// The crown's finding: a summary cut mid-word (`Nex`) reads as a broken
// word; the cut lands after a whole word and carries an ellipsis.
#[test]
fn summary_lines_elide_at_a_word_with_an_ellipsis() {
    assert_eq!(
        elide_words(
            "In Progress 1 \u{b7} Now 1 \u{b7} Next 279 \u{b7} Later 30",
            26
        ),
        "In Progress 1 \u{b7} Now 1 \u{b7}\u{2026}"
    );
    assert_eq!(elide_words("short", 26), "short");
    // One long word: no boundary exists, so the ellipsis follows a hard cut.
    assert_eq!(elide_words("abcdefgh", 4), "abc\u{2026}");
}

// D5: a detail field's label reads dim and its value stays normal.
#[test]
fn detail_field_labels_go_dim_and_values_stay_normal() {
    use crate::client::backlog_style::BRole;
    let mut v = key_view(board_with(board_inputs()));
    focus_card(&mut v.backlog_board.as_mut().expect("board"), Some("x-1"));
    open_detail(&mut v);
    let b = v.backlog_board.as_ref().expect("detail opened");
    let (lines, _) = node_detail::overlay_lines(b, 120);
    let field = lines
        .iter()
        .find(|l| l.starts_with("kind:"))
        .expect("the kind field line");
    let colon = field.find(':').expect("label ends with a colon");
    assert!(
        field.roles[..=colon].iter().all(|&r| r == BRole::Meta),
        "label chars go dim, got {:?}",
        &field.roles[..=colon]
    );
    assert_eq!(field.roles[colon + 2], BRole::Body, "value stays normal");
}
