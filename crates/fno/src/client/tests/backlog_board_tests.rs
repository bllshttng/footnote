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
        json!({"id": "x-1", "status": "ready", "priority": "p1", "title": "First card", "project": "fno",
               "cwd": "/tmp/x519f", "sessions": [
                   {"phase": "execute", "harness": "claude",
                    "session_id": "4d4ea752-1063-4515-8366-b9946ed8f64d"}]}),
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
fn board_render_rows() {
    let b = BoardView::new(0);
    let (lines, follow) = render(&b, 120);
    assert_eq!(lines[0], "reading board...");
    assert!(follow.is_none());

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

    let mut inp = board_inputs();
    inp.flow = json!({"available": false, "reason": "no usable window start"});
    let b = board_with(inp);
    let (lines, _) = render(&b, 200);
    assert!(
        lines[0].contains("flow: no usable window start"),
        "{:?}",
        lines[0]
    );

    let b = board_with(board_inputs());
    let (lines, _) = render(&b, 80);
    let stacked = lines
        .iter()
        .filter(|l| {
            l.starts_with("In Progress  ") || l.starts_with("Now  ") || l.starts_with("Next  ")
        })
        .count();
    assert!(stacked >= 3, "expected stacked headers, got {lines:?}");

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

    // The keyed grammar reaches the board through the Find input; a bad
    // keyed query names the parse error and keeps the previous filter.
    let mut v = key_view(board_with(board_inputs()));
    v.backlog_board.as_mut().expect("board open").input =
        Some((BoardInputKind::Find, "s:ready".into()));
    input_commit(&mut v);
    let b = v.backlog_board.as_ref().expect("board open");
    let total: usize = b
        .body
        .as_ref()
        .unwrap()
        .lanes
        .iter()
        .map(|l| l.cells.iter().map(|c| c.total).sum::<usize>())
        .sum();
    assert_eq!(total, 2, "only the ready cards stay");
    assert_eq!(b.query.q.as_deref(), Some("s:ready"));
    let mut v = key_view(board_with(board_inputs()));
    {
        let b = v.backlog_board.as_mut().expect("board open");
        b.query.q = Some("s:ready".into());
        b.input = Some((BoardInputKind::Find, "stauts:ready".into()));
    }
    input_commit(&mut v);
    let b = v.backlog_board.as_ref().expect("board open");
    assert_eq!(
        b.query.q.as_deref(),
        Some("s:ready"),
        "the previous filter stays"
    );
    let notice = v
        .notice
        .as_ref()
        .map(|(text, _)| text.clone())
        .unwrap_or_default();
    assert!(
        notice.contains("did you mean 'status:'?"),
        "notice: {notice}"
    );
}

// AC4-HP: the stats line counts every column and renders the flow line.

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

// AC7-EDGE: an unavailable flow renders its reason, never numbers.

// AC8-EDGE: below WIDE_CELLS_AT the cells stack with `Now  12` headers.

// AC11-EDGE: a filter matching nothing totals zero and says so.

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
fn t_key_rows() {
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

// AC11-ERR: a claimed card refuses BEFORE the dock opens; the board stays
// open and the notice names the in-flight case (the plan-refusal wording).

// AC12-HP: `t` inside the drill-down targets the drill-down's node, the
// same prefill as a card press.

// AC13-EDGE: a kept non-empty draft is never overwritten; the dock shows
// it and the notice names the way out.
// ----: the sideline backlog view + full screen ----

// The narrow render is the stacked one-column shape: each column header
// carries its count, card rows beneath - never the six-wide cells.
#[test]
fn board_wide_rows() {
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

    // The ROW rule clips with no marker (d-36438ea4): the stats line loses
    // whole trailing text, never a word-boundary ellipsis.
    assert_eq!(
        crate::chrome::clip(
            "In Progress 1 \u{b7} Now 1 \u{b7} Next 279 \u{b7} Later 30",
            26
        ),
        "In Progress 1 \u{b7} Now 1 \u{b7} Ne"
    );
    assert_eq!(crate::chrome::clip("short", 26), "short");
    assert_eq!(crate::chrome::clip("abcdefgh", 4), "abcd");
}

// `V` cycles the sideline view and the board rides with it: to backlog
// opens the board, back to agents closes it. `x` no longer does anything
// on the board (the dock is gone).
#[test]
fn sideline_toggle_rows() {
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
        crate::view_store::SidelineView::Messages
    ));
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
    assert_eq!(
        serde_json::to_value(v.sideline_view).unwrap(),
        serde_json::json!("org")
    );
    rt.block_on(async {
        cycle_sideline_view(&mut v);
    });
    assert!(matches!(
        v.sideline_view,
        crate::view_store::SidelineView::Agents
    ));
    assert!(v.backlog_board.is_none(), "agents view closes the board");
    crate::client::org_board::check_fixture(&mut v);
    let mut sock: Vec<u8> = Vec::new();
    rt.block_on(async {
        crate::client::org_board::keys(&mut v, b"\t\t\tF", &mut sock)
            .await
            .unwrap();
    });
    assert!(v.board_full);
    assert_eq!(
        v.org_board.as_ref().unwrap().mode,
        crate::view_store::OrgMode::Tree
    );
    assert_eq!(
        v.input_owner(),
        crate::client::region_focus::RegionOwner::Board
    );
    let selected_session = |agent: Option<crate::proto::AgentRow>| {
        crate::client::org_board::Selected::Session(crate::org_model::OrgSession {
            view: backlog_model::SessionView {
                phase: Some("execute".into()),
                harness: Some("claude".into()),
                session_id: Some("session-live-full".into()),
                model: None,
                started_at: None,
                ended_at: None,
                agent: Some("worker".into()),
                action: "attach".into(),
                reason: None,
                command: Some("fno agents attach worker".into()),
            },
            agent,
        })
    };
    let agent = crate::proto::AgentRow {
        name: "worker".into(),
        harness: Some("claude".into()),
        harness_session_id: Some("session-live-full".into()),
        pane_id: Some(44),
        context_used_pct: Some(26),
        context_tokens: Some((258687, 1000000)),
        node: Some("x-1".into()),
        model: Some("requested-model".into()),
        started_at: Some(crate::digest_overlay::now_secs() - 10800),
        ..Default::default()
    };
    let mut second_seat = agent.clone();
    second_seat.pane_id = Some(45);
    v.layout.agents = vec![agent.clone(), second_seat.clone()];
    assert_eq!(
        crate::client::node_detail::resolve_session(
            &v,
            Some("session-live-full"),
            Some("claude"),
            Some(&second_seat)
        )
        .unwrap()
        .pane_id,
        Some(45)
    );
    assert!(crate::client::node_detail::resolve_session(
        &v,
        Some("session-live-full"),
        Some("claude"),
        None
    )
    .is_none());
    let old_bare = crate::proto::AgentRow {
        name: "before rename".into(),
        pane_id: Some(66),
        ..Default::default()
    };
    v.layout.agents = vec![crate::proto::AgentRow {
        name: "after rename".into(),
        pane_id: Some(66),
        ..Default::default()
    }];
    assert_eq!(
        crate::client::node_detail::resolve_session(&v, None, None, Some(&old_bare))
            .unwrap()
            .pane_id,
        Some(66)
    );
    v.layout.agents = vec![agent.clone()];
    sock.clear();
    rt.block_on(async {
        crate::client::org_board::dispatch(
            &mut v,
            selected_session(Some(agent.clone())),
            b'\r',
            &mut sock,
        )
        .await
        .unwrap();
    });
    let mut expected = Vec::new();
    rt.block_on(async {
        crate::proto::write_msg(
            &mut expected,
            &crate::proto::ClientMsg::Command(crate::proto::Command::FocusPane(44)),
        )
        .await
        .unwrap();
    });
    assert_eq!(sock, expected, "Org Enter uses the existing focus command");
    assert!(v.org_board.is_none());
    assert_eq!(
        v.input_owner(),
        crate::client::region_focus::RegionOwner::Pane
    );
    crate::client::org_board::open(&mut v);
    v.layout.agents[0].harness_session_id = Some("successor-full-session".into());
    sock.clear();
    rt.block_on(async {
        crate::client::org_board::dispatch(&mut v, selected_session(None), b'\r', &mut sock)
            .await
            .unwrap();
    });
    assert!(
        sock.is_empty(),
        "former identity never selects a same-name successor"
    );
    assert_eq!(
        v.notice.as_ref().map(|(text, _)| text.as_str()),
        Some("no registry row")
    );
    v.layout.agents = vec![agent.clone()];
    rt.block_on(async {
        crate::client::org_board::dispatch(
            &mut v,
            selected_session(Some(agent.clone())),
            b'd',
            &mut sock,
        )
        .await
        .unwrap();
    });
    let detail = v.org_board.as_ref().unwrap().detail.as_ref().unwrap();
    let request = detail.request;
    let identity = detail.identity.clone();
    let gen = v.org_generation;
    let message = format!("{} END-FULL-ROSTER-MESSAGE", "full text ".repeat(35));
    let roster = serde_json::to_vec(&json!({"agents":[{
        "name":"worker", "harness":"claude", "harness_session_id":"session-live-full",
        "observed_model":{"model":"actual-model", "kind":"transcript"},
        "model":"requested-model", "model_basis":"spawn", "effort":"high", "effort_basis":"spawn",
        "status":"working", "status_basis":"transcript", "progress":"awaiting-operator", "progress_basis":"assistant",
        "last_message":message,
        "last_event_at":"2026-09-30T12:34:56Z", "last_activity_basis":"transcript",
    }]})).unwrap();
    let payload = crate::client::org_detail::select_roster(&roster, &agent).unwrap();
    let changed = |request, identity, result| crate::client::org_detail::OrgMsg::Detail {
        request,
        identity,
        result,
    };
    crate::client::org_detail::apply(
        &mut v,
        gen.wrapping_sub(1),
        changed(request, identity.clone(), Ok(payload.clone())),
    );
    let rendered = |v: &View| {
        v.org_board
            .as_ref()
            .unwrap()
            .detail
            .as_ref()
            .unwrap()
            .lines(200)
            .into_iter()
            .map(|l| l.text)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !rendered(&v).contains("actual-model"),
        "stale view generation ignored"
    );
    crate::client::org_detail::apply(
        &mut v,
        gen,
        changed(request, "other identity".into(), Ok(payload.clone())),
    );
    assert!(
        !rendered(&v).contains("actual-model"),
        "changed selection ignored"
    );
    crate::client::org_detail::apply(
        &mut v,
        gen,
        changed(request + 1, identity.clone(), Ok(payload.clone())),
    );
    assert!(
        !rendered(&v).contains("actual-model"),
        "different request ignored"
    );
    crate::client::org_detail::apply(&mut v, gen, changed(request, identity, Ok(payload)));
    let text = rendered(&v);
    for expected in [
        "26% used",
        "258,687 of 1,000,000",
        "actual-model (transcript)",
        "requested-model (spawn)",
        "3h",
        "x-1",
        "END-FULL-ROSTER-MESSAGE",
        "Last activity: 2026-09-30T12:34:56Z (transcript)",
        "Needs-you:",
    ] {
        assert!(text.contains(expected), "detail omitted {expected}: {text}");
    }
    assert!(crate::client::org_detail::select_roster(b"{}", &agent).is_err());
    let ambiguous = serde_json::to_vec(&json!([{"harness":"claude","session_id":"session-live-full"},{"harness":"claude","session_id":"session-live-full"}])).unwrap();
    assert!(crate::client::org_detail::select_roster(&ambiguous, &agent)
        .unwrap_err()
        .contains("ambiguous"));
    rt.block_on(async {
        crate::client::org_board::keys(&mut v, &[27], &mut sock)
            .await
            .unwrap();
    });
    assert!(v.org_board.as_ref().unwrap().detail.is_none());
    let mut short = agent.clone();
    short.harness_session_id = Some("abcdef01-first".into());
    v.layout.agents = vec![short.clone()];
    assert!(crate::client::node_detail::resolve_session(
        &v,
        Some("abcdef01"),
        Some("claude"),
        None
    )
    .is_some());
    short.harness_session_id = Some("abcdef01-second".into());
    v.layout.agents.push(short);
    assert!(crate::client::node_detail::resolve_session(
        &v,
        Some("abcdef01"),
        Some("claude"),
        None
    )
    .is_none());
    assert!(crate::client::node_detail::resolve_session(&v, None, None, None).is_none());
    let inputs = board_inputs();
    let node = backlog_model::node(&inputs, "x-1").unwrap();
    v.org_board.as_mut().unwrap().inputs = Some(inputs);
    v.experimental_backlog = false;
    rt.block_on(async {
        crate::client::org_board::dispatch(
            &mut v,
            crate::client::org_board::Selected::Node(node),
            b'\r',
            &mut sock,
        )
        .await
        .unwrap();
    });
    assert!(v.org_board.is_none());
    assert_eq!(
        v.backlog_board
            .as_ref()
            .unwrap()
            .detail
            .as_ref()
            .unwrap()
            .node_id,
        "x-1"
    );
    assert_eq!(v.sideline_view, crate::view_store::SidelineView::Backlog);

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

// `F` toggles the full-screen board and back; the first Esc folds the
// full screen back to the column, the second closes the board.

// The docked board's column owns no agents rows. A press anywhere in it
// resolves no sideline row, no drag source, and no chrome hit - the board is
// keyboard-driven, and a click must never act on a phantom agent row.
#[test]
fn board_column_resolves_no_agents_rows_or_chrome_hits() {
    let v = sideline_backlog_view();
    // A cell well inside the board column (panel 28 wide), below the strip.
    assert_eq!(v.sideline_row_at(10, 14), None);
    assert!(v.row_drag_source_at(10, 14).is_none());
    assert!(v.press_hold_row_at(10, 14).is_none());
    assert!(v.chrome_hit(10, 14).is_none());
}

// the board is a modal like the composer - prefix chords still
// resolve while it holds the keyboard (which-key parity). Before the fix the
// prefix byte fell into the board's byte catch-all: `^B C` toggled nothing
// and `^B ?` opened the board's own keys overlay instead of the keybinds.
#[test]
fn chord_rows() {
    let mut v = key_view(board_with(board_inputs()));
    // "Holds the keyboard" is now explicit: a windowed board owns the input
    // only after the operator opened or clicked it, which in the real flow
    // also points the sideline at the backlog view.
    v.region_owner = crate::client::region_focus::RegionOwner::Board;
    v.sideline_view = crate::view_store::SidelineView::Backlog;
    let mut scanner = crate::keys::Scanner::default();
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let flow =
            super::super::overlay_keys::route(&mut v, &mut scanner, &[0x02, b'C'], &mut sock)
                .await
                .expect("the board owns the chunk")
                .expect("route runs");
        assert!(matches!(flow, StdinFlow::Continue));
    });
    assert!(v.org.is_expanded(), "^B C toggled the org fold");
    assert!(v.backlog_board.is_some(), "the board survives the chord");
    // `^B ?` opens the GLOBAL keybinds, never the board's own keys overlay
    // (that stays on the bare key).
    let mut scanner = crate::keys::Scanner::default();
    rt.block_on(async {
        super::super::overlay_keys::route(&mut v, &mut scanner, &[0x02, b'?'], &mut sock)
            .await
            .expect("the board owns the chunk")
            .expect("route runs");
    });
    assert!(
        v.keys_modal.is_some(),
        "^B ? opened the global keybinds modal"
    );
    assert!(
        v.backlog_board
            .as_ref()
            .map(|b| !b.keys_overlay)
            .unwrap_or(false),
        "the board's keys overlay did not arm behind the chord"
    );

    let mut v = key_view(board_with(board_inputs()));
    // The board holds the keyboard until the composer chord hands it over;
    // the sideline rides the backlog view as every real open does.
    v.region_owner = crate::client::region_focus::RegionOwner::Board;
    v.sideline_view = crate::view_store::SidelineView::Backlog;
    let mut scanner = crate::keys::Scanner::default();
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        super::super::overlay_keys::route(&mut v, &mut scanner, &[0x02, b'i'], &mut sock)
            .await
            .expect("the board owns the chunk")
            .expect("route runs");
    });
    assert!(v.launcher.is_some(), "^B i opened the composer");
    assert!(v.backlog_board.is_some(), "the board stays docked");
    let mut scanner = crate::keys::Scanner::default();
    rt.block_on(async {
        super::super::overlay_keys::route(&mut v, &mut scanner, &[b'\t'], &mut sock)
            .await
            .expect("the composer owns the chunk")
            .expect("route runs");
    });
    let l = v.launcher.as_ref().expect("the composer is still open");
    assert_ne!(
        l.focus,
        super::agent_launcher::Focus::Harness,
        "Tab cycled the composer's tab; the byte reached the composer, not the board"
    );
    assert!(v.backlog_board.is_some(), "the board stays docked");
}

// a chord that opens a lower-priority modal over the docked board hands the
// keyboard to that modal: `^B i` opens the composer, and a Tab then cycles
// the composer's tab - it never falls through to the board's folder.

// The composed frame paints the backlog inside the sideline column: the
// filter bar, the board pane and the detail pane are the column's
// content, title rows carrying the focus mark instead of frames.
#[tokio::test]
async fn compose_rows() {
    let mut v = key_view(board_with(board_inputs()));
    v.experimental_backlog = true;
    v.sideline_view = crate::view_store::SidelineView::Backlog;
    let text = crate::vt::frame_text(&v.compose());
    assert!(text.contains("Search:"), "filter bar: {text}");
    assert!(text.contains("In Progress"), "column header: {text}");
    assert!(text.contains("mux card"), "card row: {text}");

    let mut v = key_view(board_with(board_inputs()));
    v.experimental_backlog = true;
    v.sideline_view = crate::view_store::SidelineView::Backlog;
    v.board_full = true;
    let text = crate::vt::frame_text(&v.compose());
    assert!(
        text.lines().any(|l| l.starts_with("Search:")),
        "filter bar at column 0: {text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("backlog \u{b7} kanban")),
        "board title at column 0: {text}"
    );
    assert!(
        text.lines().any(|l| l.contains("details \u{b7} x-")),
        "detail title paints: {text}"
    );
    assert!(
        !text.contains('\u{256d}'),
        "no box-drawing frame glyph anywhere: {text}"
    );
    assert!(
        !text.contains("e/p/s/S edit") && text.contains("c cols"),
        "hint carries the column key and no edit keys: {text}"
    );
    // One esc chip on the full board (the filter bar's, top right); a tap
    // returns it to the docked column, and a tap on the column's chip,
    // keyboard elsewhere, closes the column. The chip rides no frame now,
    // so the tap goes through the real press path at the recorded span.
    let chip = tap_recorded_chip(&mut v).await;
    assert_eq!(chip, 1, "one chip on the full board");
    assert!(
        !v.board_full && v.backlog_board.is_some(),
        "full returns to docked"
    );
    v.region_owner = crate::client::region_focus::RegionOwner::Pane;
    let chip = tap_recorded_chip(&mut v).await;
    assert_eq!(chip, 1, "one chip on the docked column");
    assert!(v.backlog_board.is_none(), "the docked column closes");
}

/// Press the topmost recorded esc chip through the real mouse path: the
/// press lands on the recorded span, `chip_at` routes it as Esc.
async fn tap_recorded_chip(v: &mut View) -> usize {
    v.compose();
    let chips: Vec<crate::chrome::CloseSpan> = v
        .close_chips
        .borrow()
        .iter()
        .copied()
        .filter(|s| s.len == 3)
        .collect();
    let Some(s) = chips.last() else {
        return 0;
    };
    let (row, col) = (s.row as u16, s.col as u16 + 1);
    let sock: Vec<u8> = Vec::new();
    let mut sock = sock;
    let mut scanner = crate::keys::Scanner::default();
    for kind in [
        crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        crate::proto::MouseKind::Release(crate::proto::MouseButton::Left),
    ] {
        let rep = crate::mouse::MouseReport {
            kind,
            row,
            col,
            shift: false,
        };
        crate::client::region_focus::mouse_pre_pass(v, &mut scanner, vec![rep], &mut sock)
            .await
            .unwrap();
    }
    chips.len()
}

// Full screen paints the filter bar, the board pane and the two-row hint;
// the hint carries the edit keys (the footer's replacement).

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

// The hierarchy proof: in the composed full-screen board, a column header
// cell, a card-id cell, a meta cell and the cursor band carry DISTINCT
// attribute sets, and no cell carries INVERSE.
#[test]
fn board_cells_rows() {
    let mut view = sideline_backlog_view();
    view.board_full = true;
    let frame = view.compose();
    let cols = 100usize;
    let text = crate::vt::frame_text(&frame);
    // The cursor card row and the stacked column header directly above
    // it (the panes share rows, so anchors stay within the board pane).
    let band_row = text
        .lines()
        .enumerate()
        .find(|(_, l)| {
            // Pad-width agnostic: the frame's side padding may grow.
            let body = l.trim_start_matches('│').trim_start();
            body.starts_with("▸●") && l.contains("x-2")
        })
        .expect("cursor card row")
        .0;
    let head_row = band_row - 1;
    let head_line = text.lines().nth(head_row).expect("head line");
    assert!(
        head_line
            .trim_start_matches('│')
            .trim_start()
            .starts_with("In Progress"),
        "column header above the cursor row: {head_line}"
    );
    let head_col = head_line.find("In Progress").expect("head text") + 1;
    let head = cell_at(&frame, head_row, head_col, cols);
    assert!(
        head.flags & crate::proto::cell_flags::BOLD != 0,
        "header bold at row {head_row}: {text}"
    );
    // The stats line (contains `·`) reads dim.
    let meta_row = text
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("In Progress 1 \u{b7}"))
        .expect("stats line")
        .0;
    let meta = cell_at(&frame, meta_row, 2, cols);
    assert_eq!(meta.fg, crate::proto::Color::Indexed(8), "meta dim slot");
    let band_line = text.lines().nth(band_row).expect("band line");
    let id_col = band_line.find("x-2").expect("id on the cursor row");
    let band = cell_at(&frame, band_row, id_col, cols);
    assert_eq!(band.bg, view.theme.sel, "band surface");
    // x-b5b8: the cursor band text is the neutral stamp pair, like every
    // selection band - the brand never fills a banded row's text.
    assert_eq!(band.fg, view.theme.stamp, "band accent text");
    // A non-cursor card id keeps the accent slot and a plain title.
    let id_row = text
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("x-1") && l.contains("First card"))
        .expect("x-1 card row")
        .0;
    let line = text.lines().nth(id_row).expect("id row");
    let id_col = line.find("x-1").expect("id on its row");
    let id_cell = cell_at(&frame, id_row, id_col, cols);
    assert_eq!(
        id_cell.fg,
        crate::proto::Color::Indexed(3),
        "id accent slot"
    );
    let title_col = line.find("First card").expect("title on its row");
    let title_cell = cell_at(&frame, id_row, title_col, cols);
    assert_eq!(title_cell.fg, crate::proto::Color::Default, "title plain");
    // Nothing in the frame is inverse.
    for r in 0..24usize {
        for c in 0..cols {
            let cell = cell_at(&frame, r, c, cols);
            assert!(
                cell.flags & crate::proto::cell_flags::INVERSE == 0,
                "INVERSE at {r},{c}: {text}"
            );
        }
    }

    let mut view = sideline_backlog_view();
    view.board_full = true;
    let frame = view.compose();
    let text = crate::vt::frame_text(&frame);
    assert!(
        text.lines().any(|l| l.starts_with("backlog \u{b7} kanban")),
        "board title at column 0: {text}"
    );
    assert!(text.contains("In Progress"), "{text}");
}

// The full-screen board keeps the hierarchy on the terminal's own bg.

// Evidence shots (FNO_UX_SHOTS): the sideline column, the full board and
// the node detail, each composed for real.
#[test]
fn ux_shot_rows() {
    use crate::frame_html::write_shot;
    let view = sideline_backlog_view();
    let frame = view.compose();
    write_shot(
        &frame,
        "ux-shot-backlog-sideline",
        "the backlog as a sideline view",
    );

    let mut view = sideline_backlog_view();
    view.board_full = true;
    let frame = view.compose();
    write_shot(
        &frame,
        "ux-shot-backlog-full-board",
        "the full-screen backlog board",
    );

    let mut view = sideline_backlog_view();
    if let Some(b) = view.backlog_board.as_mut() {
        b.detail = Some(node_detail::NodeDetailOverlay {
            node_id: "x-1".into(),
            trail: vec![],
            sel: 0,
            scroll: 0,
        });
    }
    let frame = view.compose();
    write_shot(&frame, "ux-shot-backlog-detail", "the node detail overlay");
}

// The same frames under the user's theme (Catppuccin) were evidence shots
// with no assertion behind them; the palette contract lives in
// `backlog_panel_cells_carry_distinct_attributes` and the theme's own
// tests. Removed under the shrink-only test cap: the two variants guarded
// no contract of their own.

#[test]
fn comment_key_opens_input_and_dead_edit_keys() {
    // c opens the comment input from the detail pane; the seven dead edit
    // keys open nothing from either surface.
    let mut v = key_view(board_with(board_inputs()));
    let mut sock: Vec<u8> = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    if let Some(b) = v.backlog_board.as_mut() {
        b.detail = Some(node_detail::NodeDetailOverlay {
            node_id: "x-1".into(),
            trail: vec![],
            sel: 0,
            scroll: 0,
        });
    }
    if let Some(b) = v.backlog_board.as_mut() {
        focus_card(b, Some("x-1"));
    }
    rt.block_on(async {
        node_detail::detail_keys(&mut v, b"c", &mut sock)
            .await
            .expect("c folds");
    });
    assert_eq!(
        v.backlog_board
            .as_ref()
            .expect("board")
            .input
            .as_ref()
            .map(|(k, _)| *k),
        Some(BoardInputKind::Comment),
        "c from the detail opens the comment input"
    );
    for key in [b'e', b'p', b's', b'S', b'D', b'N', b'E'] {
        let mut v = key_view(board_with(board_inputs()));
        if let Some(b) = v.backlog_board.as_mut() {
            b.detail = Some(node_detail::NodeDetailOverlay {
                node_id: "x-1".into(),
                trail: vec![],
                sel: 0,
                scroll: 0,
            });
        }
        if let Some(b) = v.backlog_board.as_mut() {
            focus_card(b, Some("x-1"));
        }
        let mut sock: Vec<u8> = Vec::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = node_detail::detail_keys(&mut v, &[key], &mut sock).await;
            let _ = board_keys(&mut v, &[key], &mut sock).await;
        });
        let b = v.backlog_board.as_ref().expect("board");
        assert!(
            b.input.is_none(),
            "dead key {key} opens nothing: {:?}",
            b.input
        );
    }
}

#[test]
fn comment_thread_renders_in_order_with_marks() {
    let mut inp = board_inputs();
    if let Some(r) = inp.rows.get_mut(0) {
        r["progress_notes"] = json!([
            {"ts": "2026-10-02T10:00:00+00:00", "text": "rename the flag", "kind": "comment",
             "author": "user", "comment_id": "c-abc123", "state": "done", "state_ref": "PR 2951"},
            {"ts": "2026-10-02T10:01:00+00:00", "text": "landed", "kind": "reply",
             "reply_to": "c-abc123", "author": "agent"},
        ]);
    }
    let mut v = key_view(board_with(inp));
    focus_card(&mut v.backlog_board.as_mut().expect("board"), Some("x-1"));
    open_detail(&mut v);
    let b = v.backlog_board.as_ref().expect("detail opened");
    let (lines, _) = node_detail::pane_lines(b, "x-1", Some(0), 120);
    let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
    let joined = texts.join("\n");
    assert!(
        joined.contains("comments (2, 0 open)"),
        "the thread header counts rows and open asks: {joined}"
    );
    let ask = texts
        .iter()
        .position(|t| t.contains("rename the flag"))
        .expect("the ask row");
    let reply = texts
        .iter()
        .position(|t| t.contains("landed"))
        .expect("the reply row");
    assert!(ask < reply, "the comment reads before its reply");
    assert!(
        texts[ask].contains("\u{2713} user"),
        "the done mark and author paint"
    );
    assert!(
        texts[reply].starts_with("  ") && texts[reply].contains("agent"),
        "the reply indents under its head"
    );
}

// The `c` column picker: opening, hiding the focus column, and the focus
// width clamp (25..=75), each persisted.

// The team's finding: a wide row merged its columns' role walks out of
// lockstep, so a header's style landed mid-word (`No|w`). Each header word
// carries exactly one style.

// The wide layout keeps every shown column inside the row width: at the
// WIDE_CELLS_AT threshold with the six default columns, the last column's
// header still paints (the 12-column floors never overrun `w`).

// The team's finding: a summary cut mid-word (`Nex`) reads as a broken
// word; the cut lands after a whole word and carries an ellipsis.

// D5: a detail field's label reads dim and its value stays normal.
#[test]
fn detail_field_labels_go_dim_and_values_stay_normal() {
    use crate::client::backlog_style::BRole;
    let mut v = key_view(board_with(board_inputs()));
    focus_card(&mut v.backlog_board.as_mut().expect("board"), Some("x-1"));
    open_detail(&mut v);
    let b = v.backlog_board.as_ref().expect("detail opened");
    let (lines, _) = node_detail::pane_lines(b, "x-1", Some(0), 120);
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
    let sid = "4d4ea752-1063-4515-8366-b9946ed8f64d";
    let adopt = format!("$ fno agents adopt {sid} --cross-project");
    assert!(
        lines.iter().any(|l| l.trim() == sid),
        "the session row shows the full id"
    );
    assert!(
        lines.iter().any(|l| l.starts_with(&adopt)),
        "the session row shows the adopt command"
    );
    let nv = crate::backlog_model::node(b.inputs.as_ref().expect("inputs"), "x-1")
        .expect("the fixture node resolves");
    assert_eq!(
        node_detail::copy_target(&nv, "x-1", 0, false).as_deref(),
        Some(sid)
    );
    assert_eq!(
        node_detail::copy_target(&nv, "x-1", 0, true).as_deref(),
        Some(&adopt[2..])
    );
    // A link row copies the linked id and carries no command (AC4-EDGE).
    let mut linked = board_inputs();
    linked.rows[0]["blocked_by"] = json!(["x-2"]);
    let nv2 = crate::backlog_model::node(&linked, "x-1").expect("the linked node resolves");
    assert_eq!(
        node_detail::copy_target(&nv2, "x-1", 0, false).as_deref(),
        Some("x-2")
    );
    assert_eq!(node_detail::copy_target(&nv2, "x-1", 0, true), None);
}

// AC4-HP: Space on value rows builds a multi-select set; the board keeps
// cards in either status; `any` clears the set.
#[test]
fn facet_rows() {
    let mut inp = board_inputs();
    if let Some(r) = inp.rows.get_mut(2) {
        r["status"] = json!("done");
    }
    let mut v = key_view(board_with(inp));
    // The status facet's value list sorts: any, done, in_progress, ready.
    if let Some(b) = v.backlog_board.as_mut() {
        b.facet = Some(FacetPick {
            facet: 2,
            sel: 0,
            value_sel: Some(3), // ready
        });
    }
    facet_toggle(&mut v);
    if let Some(b) = v.backlog_board.as_mut() {
        b.facet = Some(FacetPick {
            facet: 2,
            sel: 0,
            value_sel: Some(2), // in_progress
        });
    }
    facet_toggle(&mut v);
    let b = v.backlog_board.as_ref().expect("board open");
    let set = b.query.sets.get("status").expect("status set built");
    assert!(set.contains(&"ready".to_string()) && set.contains(&"in_progress".to_string()));
    let kept: Vec<&str> = b
        .body
        .as_ref()
        .unwrap()
        .lanes
        .iter()
        .flat_map(|l| l.cells.iter())
        .flat_map(|c| c.cards.iter())
        .map(|c| c.id.as_str())
        .collect();
    let mut sorted = kept.clone();
    sorted.sort();
    assert_eq!(sorted, ["x-1", "x-2"], "either status stays, done drops");
    // Both rows carry the ticked mark.
    let b = v.backlog_board.as_mut().expect("board open");
    b.facet = Some(FacetPick {
        facet: 2,
        sel: 0,
        value_sel: Some(0),
    });
    let popup = facet_popup(b).expect("value popup");
    let marks: Vec<&str> = popup
        .rows
        .iter()
        .filter_map(|r| match r {
            PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .filter(|l| l.starts_with('['))
        .collect();
    assert_eq!(
        marks,
        vec!["[ ] done", "[x] in_progress", "[x] ready"],
        "{marks:?}"
    );
    // `any` clears the set.
    facet_toggle(&mut v);
    let b = v.backlog_board.as_ref().expect("board open");
    assert!(b.query.sets.get("status").is_none(), "any clears the set");

    let b = board_with(board_inputs());
    let board = b.body.as_ref().unwrap();
    let lines = filter_bar_lines(&b, board, 120);
    assert!(
        lines
            .iter()
            .any(|l| l.text.contains("Labels: none on any node")),
        "{lines:?}"
    );

    let b = board_with(board_inputs());
    let names: Vec<&str> = visible_facets(b.body.as_ref().unwrap())
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert!(!names.contains(&"tag"), "empty tag facet hides: {names:?}");
    let mut inp = board_inputs();
    if let Some(r) = inp.rows.get_mut(0) {
        r["tags"] = json!(["infra"]);
    }
    let mut b = board_with(inp);
    rederive(&mut b);
    let names: Vec<&str> = visible_facets(b.body.as_ref().unwrap())
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert!(names.contains(&"tag"), "a tagged row reveals the facet");
}

// AC3-EDGE bar half: with no tags anywhere, the bar names the hidden facet.

// AC3-EDGE picker half: the tag facet hides while empty and shows once a
// row carries one.

// The paint memos must rebuild only when their key moves. A build
// counter makes the contract mechanical: same key, one build; any key
// field, a rebuild. Both slots answer to the same contract, so one test
// walks both.
#[test]
fn memo_rebuilds_only_when_the_key_moves() {
    let b = board_with(board_inputs());
    let builds = std::cell::Cell::new(0);
    let body_key = |gen: u64, row: usize| crate::client::backlog_board::BodyKey {
        gen,
        lane: 0,
        col: 0,
        row,
        w: 120,
        list: false,
        query: None,
        errors: 0,
        columns: vec!["ready".into()],
    };
    let detail_key =
        |mtime: Option<std::time::SystemTime>| crate::client::backlog_board::DetailKey {
            gen: 1,
            node: "x-1".into(),
            sel: None,
            w: 80,
            doc: Some(("x-1".into(), "/plans/x-1.md".into(), mtime, String::new())),
        };
    let build = |builds: &std::cell::Cell<usize>| {
        builds.set(builds.get() + 1);
        (Vec::new(), None)
    };
    let _ = b.board_body_cached(body_key(0, 0), || build(&builds));
    let _ = b.board_body_cached(body_key(0, 0), || build(&builds));
    assert_eq!(builds.get(), 1, "same key reads the memo");
    let _ = b.board_body_cached(body_key(0, 1), || build(&builds));
    assert_eq!(builds.get(), 2, "a moved cursor rebuilds");
    let _ = b.board_body_cached(body_key(1, 1), || build(&builds));
    assert_eq!(builds.get(), 3, "a new read rebuilds");
    // The detail slot keys on the document identity: a doc re-read (mtime
    // or node move) rebuilds even with the same node and read.
    let _ = b.detail_lines_cached(detail_key(None), || build(&builds));
    let _ = b.detail_lines_cached(detail_key(None), || build(&builds));
    assert_eq!(builds.get(), 4, "same identity reads the memo");
    let later = std::time::SystemTime::now();
    let _ = b.detail_lines_cached(detail_key(Some(later)), || build(&builds));
    assert_eq!(builds.get(), 5, "a re-read doc rebuilds");
}

// The cheap reading. The window flushes on 30s and the line names
// count, avg and max, so a paint-cost regression moves a readable number.
#[test]
fn paint_stats_flush_reports_count_avg_max() {
    let mut s = crate::client::backlog_board::PaintStats::new();
    assert!(!s.due(), "an empty window never flushes");
    s.record(1500);
    s.record(2500);
    assert!(!s.due(), "inside the window holds");
    let line = s.take_line();
    assert!(line.contains("2 paints"), "{line}");
    assert!(line.contains("avg 2.0ms"), "{line}");
    assert!(line.contains("max 2.5ms"), "{line}");
    assert_eq!(s.count, 0, "take resets the window");
}

/// x-5926: the board remembers its state. Set a filter, a search text, the
/// list view and a selection, close, reopen: the fresh board holds the same
/// query, and its first gather parks the cursor back on the saved card.
#[test]
fn board_remembers_filters_search_view_and_selection_across_reopen() {
    let prefs = tempfile::tempdir().expect("tempdir");
    crate::view_store::set_test_path(prefs.path());
    // Session one: filter to priority p2, search "mux", list view, one
    // lane, cursor on x-2.
    let mut b = board_with(board_inputs());
    b.query.sets.insert("priority", vec!["p2".into()]);
    b.query.q = Some("mux".into());
    b.query.view = backlog_model::View::List;
    b.query.lanes = backlog_model::LanesBy::None;
    rederive(&mut b);
    focus_card(&mut b, Some("x-2"));
    assert_eq!(cursor_card_id(&b).as_deref(), Some("x-2"), "fixture cursor");
    save_board_prefs(&b);
    drop(b);
    // Close, reopen through the real open path: the query comes back, the
    // saved card rides along as the pending focus.
    let mut v = key_view(board_with(board_inputs()));
    v.backlog_board = None;
    backlog_board_open_fresh(&mut v);
    {
        let b = v.backlog_board.as_ref().expect("reopen opens the board");
        assert_eq!(
            b.query.sets.get("priority").map(Vec::as_slice),
            Some(&["p2".to_string()][..])
        );
        assert_eq!(b.query.q.as_deref(), Some("mux"));
        assert_eq!(b.query.view, backlog_model::View::List);
        assert_eq!(b.query.lanes, backlog_model::LanesBy::None);
        assert_eq!(b.pending_focus.as_deref(), Some("x-2"));
    }
    // The first gather parks the cursor on the saved card (x-2 is the one
    // card the restored filter set keeps).
    let gen = v.backlog_board.as_ref().expect("open").gen;
    apply_fold(
        &mut v,
        gen,
        BoardMsg::Gathered {
            inputs: board_inputs(),
        },
    );
    let b = v.backlog_board.as_ref().expect("gather keeps the board");
    assert_eq!(
        cursor_card_id(b).as_deref(),
        Some("x-2"),
        "selection restored"
    );
}

/// x-5926: `x` (reset filters) returns the query to `any` and the store's
/// memory to the defaults, so the next open starts clean too.
#[test]
fn reset_filters_returns_every_filter_to_any_and_clears_the_memory() {
    let prefs = tempfile::tempdir().expect("tempdir");
    crate::view_store::set_test_path(prefs.path());
    let mut v = key_view(board_with(board_inputs()));
    {
        let b = v.backlog_board.as_mut().expect("fixture board");
        b.query.sets.insert("status", vec!["ready".into()]);
        b.query.q = Some("mux".into());
        save_board_prefs(b);
    }
    reset_filters(&mut v);
    let b = v.backlog_board.as_ref().expect("reset keeps the board");
    assert!(b.query.sets.is_empty());
    assert_eq!(b.query.q, None);
    assert_eq!(b.query.view, backlog_model::View::Kanban);
    assert_eq!(b.query.lanes, backlog_model::LanesBy::Project);
    assert_eq!(b.pending_focus, None);
    let remembered = crate::view_store::load_board_query().expect("reset saves the defaults");
    assert_eq!(remembered.sets.len(), 0);
    assert_eq!(remembered.q, None);
}
