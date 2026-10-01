use super::*;

pub(in crate::client) fn check_fixture(view: &mut View) {
    use serde_json::json;
    let lead = AgentRow {
        name: "finch".into(),
        crown_scope: Some("team".into()),
        crown_level: Some(1),
        ..Default::default()
    };
    let worker = |name: &str, sid: &str, node: &str| AgentRow {
        name: name.into(),
        harness_session_id: Some(sid.into()),
        node: Some(node.into()),
        ..Default::default()
    };
    let backlog = crate::backlog_model::Inputs {
        rows: vec![
            json!({"id":"x-1","status":"ready","title":"First","sessions":[{"session_id":"s1"},{"session_id":"old1"},{"session_id":"old2"}]}),
            json!({"id":"x-2","status":"ready","title":"Second","blocked_by":["x-1"],"sessions":[{"session_id":"s2"},{"session_id":"s3"}]}),
        ],
        agents: vec![
            lead,
            worker("first", "s1", "x-1"),
            worker("second", "s2", "x-2"),
            worker("third", "s3", "x-2"),
        ],
        ..Default::default()
    };
    open(view);
    let gen = view.org_generation;
    apply_fold(
        view,
        gen,
        OrgInputs {
            backlog,
            fold: Ok(
                json!({"scope_nodes":{"team":{"status":"ok","nodes":[{"id":"x-1"},{"id":"x-2"}]}},"owned_scopes":{"x-1":"team","x-2":"team"}}),
            ),
            measured_at: now(),
        },
    );
    let b = view.org_board.as_mut().unwrap();
    b.mode = OrgMode::Tree;
    b.filter = OrgSessions::Current;
    let texts = b
        .lines(60, 24)
        .into_iter()
        .map(|l| l.text)
        .collect::<Vec<_>>();
    assert!(texts[1].contains("finch"));
    assert!(texts[2].contains("x-1"));
    assert!(texts[3].contains("first"));
    assert!(texts[4].contains("x-2"));
    assert!(b.footer().starts_with("leads 1 · current 3"));
    assert!(texts[0].starts_with("Tree │ Table │ Graph · current"));
    assert!(
        texts[3].starts_with("│ └ "),
        "a worker hangs under its node"
    );
    assert!(
        texts[4].starts_with("└ ▾ x-2"),
        "the last node closes the branch"
    );
    assert!(
        !texts
            .iter()
            .any(|t| ["{", "claim:", " Q", "age ", "PR-", "unobserved"]
                .iter()
                .any(|j| t.contains(j))),
        "rows read in words: {texts:?}"
    );
    assert_eq!(
        counts_line(&json!({"ready": 3, "done": 0, "in_review": 2, "in_progress": 4})),
        "4 working · 2 in review · 3 ready"
    );
    assert!(b.footer_hints(false).starts_with("tap to focus · j/k move"));
    assert!(b.footer_hints(true).starts_with("j/k move"));
    b.filter = OrgSessions::Former;
    assert_eq!(
        b.rows()
            .iter()
            .filter(|r| matches!(&r.selected, Selected::Session(_)))
            .count(),
        2
    );
    b.filter = OrgSessions::All;
    assert_eq!(
        b.rows()
            .iter()
            .filter(|r| matches!(&r.selected, Selected::Session(_)))
            .count(),
        5
    );
    b.filter = OrgSessions::Current;
    b.mode = OrgMode::Graph;
    let first = b.graph_lines(100, 20);
    let allocation = b.graph.borrow().as_ref().unwrap().4.boxes.as_ptr();
    assert_eq!(first, b.graph_lines(100, 20));
    assert_eq!(
        allocation,
        b.graph.borrow().as_ref().unwrap().4.boxes.as_ptr(),
        "same frame reuses layout storage"
    );
    assert!(
        first.iter().any(|l| l.contains('◀')),
        "dependency edge has a visible endpoint"
    );
    assert!(first[0].contains('▶'), "selected Lead is marked");
    b.move_graph_cursor(true);
    let moved = b.graph_lines(100, 20);
    assert!(!moved[0].contains('▶'));
    assert!(moved.iter().any(|line| line.contains('▶')));
    assert_eq!(
        allocation,
        b.graph.borrow().as_ref().unwrap().4.boxes.as_ptr()
    );
    b.cursor = 0;
    let bare = |pane| AgentRow {
        name: "shared label".into(),
        pane_id: Some(pane),
        ..Default::default()
    };
    b.snapshot.tree.as_mut().unwrap().unowned = vec![bare(44), bare(45)];
    *b.graph.borrow_mut() = None;
    b.pan = (88, 0);
    b.graph_lines(100, 20);
    let keys = b
        .graph
        .borrow()
        .as_ref()
        .unwrap()
        .4
        .boxes
        .iter()
        .filter(|p| p.key.starts_with("unowned:"))
        .map(|p| p.key.clone())
        .collect::<Vec<_>>();
    assert_eq!(keys.len(), 2);
    assert_ne!(
        keys[0], keys[1],
        "equal labels retain different pane identities"
    );
    let saved_term = view.term;
    let saved_full = view.board_full;
    view.term = (24, 100);
    view.board_full = true;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (mut socket, _receiver) = tokio::io::duplex(4096);
        mouse(
            view,
            crate::mouse::MouseReport {
                row: 3,
                col: 12,
                kind: MouseKind::Press(MouseButton::Left),
                shift: false,
            },
            &mut socket,
        )
        .await
        .unwrap();
    });
    assert!(
        matches!(view.org_board.as_ref().unwrap().selected(), Some(Selected::Session(s)) if s.agent.as_ref().unwrap().pane_id == Some(45))
    );
    view.term = saved_term;
    view.board_full = saved_full;
    let b = view.org_board.as_mut().unwrap();
    b.snapshot.tree.as_mut().unwrap().unowned.clear();
    b.pan = (0, 0);
    b.cursor = 0;
    *b.graph.borrow_mut() = None;
    b.query = "first".into();
    let graph = b.graph_lines(100, 20);
    assert!(!graph.iter().any(|l| l.contains("second")));
    b.query.clear();
    b.mode = OrgMode::Tree;
    let mut departed = b.snapshot.tree.as_ref().unwrap().leads[0].nodes[0].clone();
    departed.view.card.id = "departed-node".into();
    departed.current.clear();
    departed.former.truncate(1);
    departed.former[0].view.session_id = Some("last-run".into());
    b.snapshot.tree.as_mut().unwrap().leads[0]
        .left
        .push(departed);
    assert!(b
        .rows()
        .iter()
        .any(|r| r.text.contains("left the team (24h): 1")));
    assert!(!b.rows().iter().any(|r| r.text.contains("last-run")));
    b.departures.insert("team".into());
    assert!(b.rows().iter().any(|r| r.text.contains("last-run")));
    b.departures.clear();
    assert!(!b.rows().iter().any(|r| r.text.contains("last-run")));
    b.cursor = 0;
    let prefs = tempfile::tempdir().unwrap();
    crate::view_store::set_test_path(prefs.path());
    view.term = (24, 100);
    view.board_full = true;
    let tap = |row, col| crate::mouse::MouseReport {
        row,
        col,
        kind: MouseKind::Press(MouseButton::Left),
        shift: false,
    };
    runtime.block_on(async {
        let (mut socket, _receiver) = tokio::io::duplex(4096);
        mouse(view, tap(3, 4), &mut socket).await.unwrap();
        assert_eq!(view.org_board.as_ref().unwrap().cursor, 2, "a tap selects");
        mouse(view, tap(0, 8), &mut socket).await.unwrap();
        assert_eq!(
            view.org_board.as_ref().unwrap().mode,
            OrgMode::Table,
            "a tab tap switches mode"
        );
        view.org_board.as_mut().unwrap().mode = OrgMode::Tree;
        mouse(view, tap(2, 4), &mut socket).await.unwrap();
        assert!(view.org_board.is_some(), "a tap on a node only selects it");
        mouse(view, tap(2, 4), &mut socket).await.unwrap();
        assert!(view.org_board.is_none(), "a second tap acts like Enter");
    });
    crate::view_store::clear_test_path();
    view.term = saved_term;
    view.board_full = saved_full;
    let generation = view.org_generation;
    view.org_board = None;
    open(view);
    assert!(view.org_generation > generation);
    let mut stale = crate::org_model::OrgInputs {
        backlog: Default::default(),
        fold: Err("stale".into()),
        measured_at: 0,
    };
    apply_fold(view, generation, stale.clone());
    assert!(view.org_board.as_ref().unwrap().snapshot.error.is_none());
    stale.fold = Err("fresh failure".into());
    let generation = view.org_generation;
    apply_fold(view, generation, stale);
    assert_eq!(
        view.org_board.as_ref().unwrap().snapshot.error.as_deref(),
        Some("fresh failure")
    );
}
