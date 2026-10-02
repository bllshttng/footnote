use super::*;

pub(in crate::client) fn check_fixture(view: &mut View) {
    use serde_json::json;
    let lead = crate::proto::AgentRow {
        name: "finch".into(),
        crown_scope: Some("team".into()),
        crown_level: Some(1),
        ..Default::default()
    };
    let worker = |name: &str, sid: &str, node: &str| crate::proto::AgentRow {
        name: name.into(),
        harness_session_id: Some(sid.into()),
        node: Some(node.into()),
        ..Default::default()
    };
    let inputs = crate::org_model::OrgInputs {
        backlog: crate::backlog_model::Inputs {
            rows: vec![json!({"id":"x-1","status":"ready","title":"First",
                "sessions":[{"session_id":"s1"}]})],
            agents: vec![lead, worker("first", "s1", "x-1")],
            ..Default::default()
        },
        fold: Ok(json!({})),
        measured_at: board_now(),
    };
    let tree = crate::org_model::derive(&inputs, board_now());
    open(view);
    apply_gather(view, 0, Ok(json!({})), tree);
    assert!(view.messages_board.is_some(), "the fixture opens the board");
}

#[test]
fn messages_board_contracts() {
    use super::super::LayoutView;
    use serde_json::json;
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
    check_fixture(&mut view);
    let prefs = tempfile::tempdir().unwrap();
    crate::view_store::set_test_path(prefs.path());
    let b = view.messages_board.as_mut().expect("open");
    b.snapshot.apply(json!({
        "participants": [
            {"key":"s1","name":"first","system":false,"live":true},
            {"key":"s-q","name":"quill","archive_scope":"team","system":false,"live":false},
            {"key":"s-c","name":"candor","system":false,"live":true},
            {"key":"fno/pr-nudge","name":"fno/pr-nudge","system":true,"live":false},
        ],
        "threads": [
            {"chat_id":"chat-a1","participants":["s1","s-c"],
             "rows":[
               {"id":"m1","ts":"2026-10-01T09:00:00Z","from":"candor","from_key":"s-c","to_key":"s1","summary":"Ship it.","body":"Ship it.","system":false},
               {"id":"m2","ts":"2026-10-01T09:05:00Z","from":"first","from_key":"s1","to_key":"s-c","summary":"On it.","body":"On it.","system":false}],
             "last_ts":"2026-10-01T09:05:00Z"}
        ],
        "system": {"s-c": [
            {"id":"m3","ts":"2026-10-01T09:10:00Z","from":"fno/pr-nudge","from_key":"fno/pr-nudge","to_key":"s-c","summary":"Nudge.","body":"Nudge.","system":true}]},
        "channels": [
            {"scope":"fno","rows":[{"id":"m4","ts":"2026-10-01T09:00:00Z","from":"first","from_key":"s1","to":"fleet:fno","summary":"Standup.","body":"Standup.","system":false}]},
        ],
        "announcements": [], "unreadable": 0,
    }));
    // Column 1: the channel, then the lead folder.
    let tree = b.tree_rows();
    assert!(matches!(tree[0], TreeRow::Channel(_)), "{tree:?}");
    assert!(matches!(tree[1], TreeRow::Lead { .. }), "{tree:?}");
    // Column 2: System first, then the partner thread, unread.
    b.sel_agent = Some("s-c".into());
    let partners = b.partner_rows("s-c");
    assert!(
        matches!(partners[0], PartnerRow::System { .. }),
        "{partners:?}"
    );
    let PartnerRow::Thread {
        unread, partner, ..
    } = &partners[1]
    else {
        panic!("thread row: {partners:?}")
    };
    assert!(unread, "no mark reads unread");
    assert_eq!(partner, "first");
    // Column 3: the channel's rows resolve; the System exchange shows
    // the fno/<arm> sender (AC14-HP).
    b.sel_thread = Some("channel:fno".into());
    assert_eq!(b.conversation_rows().len(), 1);
    b.sel_thread = Some("system:s-c".into());
    let rows = b.conversation_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("from").and_then(Value::as_str),
        Some("fno/pr-nudge")
    );
    // The shared row builder: a field no source holds prints nothing.
    let mut rows2 = Vec::new();
    super::super::feed_detail::info_row("x", None, &mut rows2);
    assert!(rows2.is_empty());
    crate::view_store::clear_test_path();
}
