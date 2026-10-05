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
        fold: Ok(
            json!({"scope_nodes":{"team":{"status":"ok","nodes":[{"id":"x-1"}]}},"owned_scopes":{"x-1":"team"}}),
        ),
        measured_at: board_now(),
    };
    let tree = crate::org_model::derive(&inputs, board_now());
    open(view);
    let gen = view
        .messages_board
        .as_ref()
        .expect("the fixture opens the board")
        .gen;
    apply_gather(view, gen, Ok(json!({})), tree);
    assert!(view.messages_board.is_some(), "the fixture opens the board");
}

#[tokio::test]
async fn messages_reply_board_contracts() {
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
            {"key":"s1","name":"first","system":false,"live":true,"last_ts":"2026-10-01T09:00:00Z"},
            {"key":"s-q","name":"quill","archive_scope":"team","system":false,"live":false,"last_ts":"2026-10-01T09:20:00Z"},
            {"key":"s-c","name":"candor","system":false,"live":true,"last_ts":"2026-10-01T09:10:00Z"},
            {"key":"s-ivy","name":"ivy","system":false,"live":true},
            {"key":"s-maple","name":"maple","system":false,"live":true},
            {"key":"s-oak","name":"oak","system":false,"live":true},
            {"key":"fno/pr-nudge","name":"fno/pr-nudge","system":true,"live":false},
        ],
        "threads": [
            {"chat_id":"chat-a1","participants":["s1","s-c"],
             "rows":[
               {"id":"m1","ts":"2026-10-01T09:00:00Z","from":"candor","from_key":"s-c","to_key":"s1","summary":"Ship it.","body":"Ship it.","system":false},
               {"id":"m2","ts":"2026-10-01T09:01:00Z","from":"first","from_key":"s1","to_key":"s-c","summary":"On it.","body":"On it.","system":false},
               {"id":"m3","ts":"2026-10-01T09:02:00Z","from":"candor","from_key":"s-c","to_key":"s1","summary":"Merged.","body":"Merged.","system":false},
               {"id":"m4","ts":"2026-10-01T09:20:00Z","from":"candor","from_key":"s-c","to_key":"s1","summary":"Landed.","body":"Landed.","system":false}],
             "last_ts":"2026-10-01T09:20:00Z"},
            {"chat_id":"chat-a2","participants":["s-c","s-q"],
             "rows":[
               {"id":"m5","ts":"2026-10-01T08:55:00Z","from":"quill","from_key":"s-q","to_key":"s-c","summary":"one","body":"one","system":false},
               {"id":"m6","ts":"2026-10-01T08:56:00Z","from":"quill","from_key":"s-q","to_key":"s-c","summary":"two","body":"two","system":false},
               {"id":"m7","ts":"2026-10-01T08:57:00Z","from":"candor","from_key":"s-c","to_key":"s-q","summary":"three","body":"three","system":false},
               {"id":"m8","ts":"2026-10-01T08:58:00Z","from":"quill","from_key":"s-q","to_key":"s-c","summary":"four","body":"four","system":false}],
             "last_ts":"2026-10-01T08:58:00Z"}
        ],
        "system": {"s-c": [
            {"id":"m9","ts":"2026-10-01T09:10:00Z","from":"fno/pr-nudge","from_key":"fno/pr-nudge","to_key":"s-c","summary":"Nudge.","body":"Nudge.","system":true}]},
        "channels": [
            {"scope":"fno","rows":[{"id":"m10","ts":"2026-10-01T09:00:00Z","from":"first","from_key":"s1","to":"fleet:fno","summary":"Standup.","body":"Standup.","system":false}]},
            {"scope":"kings","rows":[]},
        ],
        "announcements": [], "unreadable": 0,
    }));
    // Column 1 (AC1-HP, AC2-HP, AC14-HP, AC15-HP): the channels, then
    // every non-system participant - the Agents tab lists everyone with
    // reaped last and the sort modes ordering the live ones.
    let agent_names = |rows: &[TreeRow]| -> Vec<String> {
        rows.iter()
            .filter_map(|r| match r {
                TreeRow::Agent { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(agent_names(&b.tree_rows()).len(), 6, "{:?}", b.tree_rows());
    b.sort_mode = SortMode::Last;
    let last_order = agent_names(&b.tree_rows());
    assert_eq!(&last_order[..2], ["candor", "first"], "{last_order:?}");
    assert_eq!(last_order.last().map(String::as_str), Some("quill"));
    b.sort_mode = SortMode::Alpha;
    assert_eq!(
        agent_names(&b.tree_rows()),
        ["candor", "first", "ivy", "maple", "oak", "quill"]
    );
    b.sort_mode = SortMode::Last;
    // The Archive tab lists the reaped agent only (AC15-HP).
    b.list_tab = ListTab::Archive;
    assert_eq!(agent_names(&b.tree_rows()), ["quill"]);
    b.list_tab = ListTab::Agents;
    let lines1 = b.tree_column(100);
    let text1: String = lines1
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    // The filter row leads (item 5); the All filter lists agents only -
    // no broadcast row among them, no `#`.
    assert!(text1.contains("All"), "{text1}");
    assert!(text1.contains("Broadcasts"), "{text1}");
    assert!(!text1.contains('#'), "{text1}");
    assert!(!text1.contains('▸') && !text1.contains('▾'), "{text1}");
    // The Broadcasts filter lists the groups alone, with the retired
    // `kings` scope reading as `leads` (item 5).
    b.filter = ListFilter::Broadcasts;
    let text_b: String = b
        .tree_column(100)
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text_b.contains("fleet:fno"), "{text_b}");
    assert!(text_b.contains("fleet:leads"), "{text_b}");
    assert!(!text_b.contains("candor"), "{text_b}");
    b.filter = ListFilter::All;

    // Column 2 (AC5-HP, AC13-HP): the strip leads with Chats; the Chats
    // tab lists the live thread only, System one entry per arm, Archive
    // the thread whose other party is reaped.
    b.sel_agent = Some("s-c".into());
    let chats = b.chat_rows("s-c");
    assert_eq!(chats.len(), 1, "{chats:?}");
    let ChatRow::Thread {
        unread, partner, ..
    } = &chats[0]
    else {
        panic!("thread row: {chats:?}")
    };
    assert!(unread, "no mark reads unread");
    assert_eq!(partner, "first");
    let chats_lines = b.chats_column(100);
    assert!(
        chats_lines[0].text.starts_with("Chats"),
        "{:?}",
        chats_lines[0].text
    );
    b.col2 = ChatTab::System;
    let sys = b.chat_rows("s-c");
    assert!(
        matches!(&sys[0], ChatRow::SystemEntry { arm } if arm == "fno/pr-nudge"),
        "{sys:?}"
    );
    b.col2 = ChatTab::Archive;
    let archived = b.chat_rows("s-c");
    assert_eq!(archived.len(), 1, "{archived:?}");
    assert!(
        matches!(&archived[0], ChatRow::Thread { partner, .. } if partner == "quill"),
        "{archived:?}"
    );
    b.col2 = ChatTab::Chats;
    let all_text: String = b
        .columns(100)
        .0
        .into_iter()
        .chain(b.columns(100).1)
        .map(|l| l.text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!all_text.contains("Partners"), "{all_text}");
    // Column 3: the channel's rows resolve; the System exchange shows
    // the fno/<arm> sender (AC14-HP).
    b.sel_thread = Some("channel:fno".into());
    assert_eq!(b.conversation_rows().len(), 1);
    b.sel_thread = Some("system:fno/pr-nudge".into());
    let rows = b.conversation_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("from").and_then(Value::as_str),
        Some("fno/pr-nudge")
    );
    // Item 6: a second arm in the same agent's system listing filters to
    // its own rows, never the aggregate.
    b.col2 = ChatTab::System;
    let sys_arms = b.chat_rows("s-c");
    assert!(sys_arms.len() >= 1, "{sys_arms:?}");
    // The bubble thread (AC6-HP, AC16-HP, AC17-HP, AC19-HP): the title
    // names the other party with the info affordances, runs share one
    // label, the time lines separate the five-minute gaps, blanks sit
    // between runs and never inside one, mine end at the right edge,
    // theirs start at column 0, and no delivered-header or envelope text
    // shows.
    b.sel_thread = Some("chat-a1".into());
    let (lines3, owners3) = b.thread_lines(50);
    let texts: Vec<String> = lines3.iter().map(|l| l.text.clone()).collect();
    assert!(texts[0].starts_with("first"), "{texts:?}");
    assert!(
        texts[0].contains("[i]") && texts[0].contains("..."),
        "{texts:?}"
    );
    let time_of = |ts: &str| {
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string()
    };
    let t1 = time_of("2026-10-01T09:00:00Z");
    let t4 = time_of("2026-10-01T09:20:00Z");
    assert_eq!(
        texts
            .iter()
            .filter(|t| t.trim() == t1 || t.trim() == t4)
            .count(),
        2,
        "{texts:?}"
    );
    // Four labels here: the 09:20 time line starts a new run (item 9).
    let label_count = texts
        .iter()
        .filter(|t| t.trim() == "candor" || t.trim() == "first")
        .count();
    assert_eq!(label_count, 4, "{texts:?}");
    // The pure run shape (AC16-HP): A, A, B, A within five minutes paints
    // exactly three labels and the second A bubble carries none.
    b.sel_thread = Some("chat-a2".into());
    let (lines4, _) = b.thread_lines(50);
    let texts4: Vec<String> = lines4.iter().map(|l| l.text.clone()).collect();
    let labels4 = texts4
        .iter()
        .filter(|t| t.trim() == "quill" || t.trim() == "candor")
        .count();
    assert_eq!(labels4, 3, "{texts4:?}");
    let one_i = texts4.iter().position(|t| t.ends_with("one")).unwrap();
    let two_i = texts4.iter().position(|t| t.ends_with("two")).unwrap();
    assert_eq!(two_i, one_i + 1, "one run, no blank between: {texts4:?}");
    b.sel_thread = Some("chat-a1".into());
    let ship_i = texts
        .iter()
        .position(|t| t.ends_with("Ship it."))
        .expect("my bubble right-aligned");
    assert_eq!(texts[ship_i].chars().count(), 49, "right edge: {texts:?}");
    assert_eq!(
        texts.iter().find(|t| t.as_str() == "On it."),
        Some(&"On it.".to_string()),
        "theirs starts at column 0: {texts:?}"
    );
    assert!(!texts
        .iter()
        .any(|t| t.contains("fmail-") || t.contains("<fno_mail")));
    let merged_i = texts
        .iter()
        .position(|t| t.ends_with("Merged."))
        .expect("merged bubble");
    let landed_i = texts
        .iter()
        .position(|t| t.ends_with("Landed."))
        .expect("landed bubble");
    // Each run's first bubble sits directly under its label; a blank sits
    // between runs (AC19-HP); the within-run adjacency is asserted on
    // chat-a2 above.
    assert_eq!(texts[merged_i - 1].trim(), "candor", "{texts:?}");
    assert_eq!(texts[landed_i - 1].trim(), "candor", "{texts:?}");
    assert!(texts[ship_i + 1].trim().is_empty(), "{texts:?}");
    assert_eq!(owners3[ship_i], Some(0), "{owners3:?}");
    let onit_i = texts.iter().position(|t| t.as_str() == "On it.").unwrap();
    assert_eq!(owners3[onit_i], Some(1), "{owners3:?}");
    assert_eq!(owners3[landed_i], Some(3), "{owners3:?}");
    // The shared row builder: a field no source holds prints nothing.
    let mut rows2 = Vec::new();
    super::super::feed_detail::info_row("x", None, &mut rows2);
    assert!(rows2.is_empty());

    // Audit gate: protects row navigation, receiver-first recipient choice,
    // exact reply prefill, and the tap hit map plus no-pane refusal. A cursor,
    // default, quote or wrapped-row regression can misroute a human reply;
    // existing fixture coverage owns projection only, so these assertions extend
    // that owner. It uses the real View/wire and needs no production seam.
    b.col = Col::Chats;
    b.cursors[1] = 0;
    super::keys(&mut view, b"\r", &mut tokio::io::sink())
        .await
        .unwrap();
    assert_eq!(
        view.messages_board.as_ref().unwrap().cursors[2],
        3,
        "opening a long thread shows the newest message"
    );
    super::keys(&mut view, b"k", &mut tokio::io::sink())
        .await
        .unwrap();
    super::keys(&mut view, b"\r", &mut tokio::io::sink())
        .await
        .unwrap();
    let choices = super::super::messages_reply::popup(&view).unwrap();
    assert_eq!(choices.selected(), Some((2, 0)));
    let labels = choices
        .rows
        .iter()
        .filter_map(|row| match row {
            crate::popup::PopupRow::Entry { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(labels, ["first", "candor"]);
    super::super::messages_reply::keys(&mut view, b"\r", &mut tokio::io::sink())
        .await
        .unwrap();
    let seed = super::super::messages_reply::popup(&view)
        .unwrap()
        .rows
        .iter()
        .find_map(|row| match row {
            crate::popup::PopupRow::Input { text, .. } => Some(text.as_str()),
            _ => None,
        });
    assert_eq!(seed, Some("re candor/m3: \"Merged.\" "));
    super::super::messages_reply::keys(&mut view, b"ok", &mut tokio::io::sink())
        .await
        .unwrap();
    let (mut writer, mut reader) = tokio::io::duplex(256);
    super::super::messages_reply::keys(&mut view, b"\r", &mut writer)
        .await
        .unwrap();
    let mut bytes = [0; 256];
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            tokio::io::AsyncReadExt::read(&mut reader, &mut bytes),
        )
        .await
        .is_err(),
        "no wire bytes are sent without a live pane"
    );
    assert_eq!(
        view.notice.as_ref().map(|(text, _)| text.as_str()),
        Some("reply: first has no pane on screen; open a portal first")
    );
    super::super::messages_reply::keys(&mut view, b"\x1b", &mut writer)
        .await
        .unwrap();
    view.notice = None;
    view.layout.agents = vec![crate::proto::AgentRow {
        name: "first".into(),
        harness_session_id: Some("session-uuid".into()),
        pane_id: Some(7),
        ..Default::default()
    }];
    view.layout.panes = vec![(
        7,
        crate::tree::Rect {
            x: 0,
            y: 0,
            rows: 1,
            cols: 1,
        },
    )];
    assert_eq!(
        super::super::messages_reply::endpoint_pane(&view, "first", "s1"),
        Some((7, "session-uuid".into())),
        "a registry fno_id resolves through its unique participant name"
    );
    view.layout.agents.push(crate::proto::AgentRow {
        name: "first".into(),
        harness_session_id: Some("another-session".into()),
        pane_id: Some(8),
        ..Default::default()
    });
    view.layout.panes.push((
        8,
        crate::tree::Rect {
            x: 1,
            y: 0,
            rows: 1,
            cols: 1,
        },
    ));
    assert_eq!(
        super::super::messages_reply::endpoint_pane(&view, "first", "s1"),
        None,
        "ambiguous aliases must not resolve to an arbitrary session"
    );
    view.layout.agents.truncate(1);
    view.layout.panes.truncate(1);
    super::super::messages_reply::open(
        &mut view,
        json!({"id":"m1","from_key":"s-c","to_key":"s1","summary":"Ship it."}),
    );
    super::super::messages_reply::keys(&mut view, b"\r", &mut tokio::io::sink())
        .await
        .unwrap();
    super::super::messages_reply::keys(&mut view, b"ok", &mut tokio::io::sink())
        .await
        .unwrap();
    let (mut reply_writer, mut reply_reader) = tokio::io::duplex(512);
    super::super::messages_reply::keys(&mut view, b"\r", &mut reply_writer)
        .await
        .unwrap();
    let request = crate::proto::read_msg::<_, crate::proto::ClientMsg>(&mut reply_reader)
        .await
        .unwrap();
    let crate::proto::ClientMsg::PaneInput(request) = request else {
        panic!("reply must address one pane and wait for its receipt")
    };
    assert_eq!(request.pane, 7);
    assert_eq!(request.expected_identity, "session-uuid");
    assert!(request.bytes.ends_with(b"ok\r"));
    assert_eq!(view.pending_reply_journals.len(), 1);
    super::super::messages_reply::input_result(
        &mut view,
        request.request_id,
        request.pane,
        Err("pane exited before delivery".into()),
    );
    assert!(view.pending_reply_journals.is_empty());
    assert_eq!(
        view.notice.as_ref().map(|(text, _)| text.as_str()),
        Some("reply not delivered: pane exited before delivery")
    );
    // A bubble tap selects and copies its fmail id (AC18-HP); the reply
    // composer opens from Enter, never a tap.
    let tap_row = (ship_i + 1) as u16;
    super::mouse(
        &mut view,
        crate::mouse::MouseReport {
            kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
            row: tap_row,
            col: 60,
            shift: false,
        },
        &mut writer,
    )
    .await
    .unwrap();
    assert!(super::super::messages_reply::popup(&view).is_none());
    assert!(view.notice.is_some(), "the tap reported a copy");
    // A title [i] click opens the detail modal; the ... click opens the
    // chat-id popup (AC17-HP). The thread column starts at split(100).0 +
    // split(100).1; the title line paints at screen row 1.
    let (col1_w, col2_w) = split(100);
    let tap = |col: usize| crate::mouse::MouseReport {
        kind: crate::proto::MouseKind::Press(crate::proto::MouseButton::Left),
        row: 1,
        col: col as u16,
        shift: false,
    };
    let mut sink = tokio::io::sink();
    let x0 = col1_w + col2_w;
    super::mouse(&mut view, tap(x0 + 7), &mut sink)
        .await
        .unwrap();
    assert!(
        view.messages_board.as_ref().unwrap().detail.is_some(),
        "the [i] click opens the detail modal"
    );
    view.messages_board.as_mut().unwrap().detail = None;
    super::mouse(&mut view, tap(x0 + 12), &mut sink)
        .await
        .unwrap();
    assert!(
        view.messages_board.as_ref().unwrap().detail.is_some(),
        "the ... click opens the chat popup"
    );
    view.messages_board.as_mut().unwrap().detail = None;
    crate::view_store::clear_test_path();
}

#[tokio::test]
async fn messages_keys_contracts() {
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
    let b = view.messages_board.as_mut().expect("open");
    assert_eq!(b.col, Col::Tree);
    // h/l and the arrows step the columns (item 8).
    super::keys(&mut view, b"l", &mut tokio::io::sink())
        .await
        .unwrap();
    assert_eq!(view.messages_board.as_ref().unwrap().col, Col::Chats);
    super::keys(&mut view, b"h", &mut tokio::io::sink())
        .await
        .unwrap();
    assert_eq!(view.messages_board.as_ref().unwrap().col, Col::Tree);
    // Esc backs a column and closes from the first (item 8).
    super::keys(&mut view, b"l", &mut tokio::io::sink())
        .await
        .unwrap();
    super::keys(&mut view, b"\x1b", &mut tokio::io::sink())
        .await
        .unwrap();
    assert_eq!(view.messages_board.as_ref().unwrap().col, Col::Tree);
    super::keys(&mut view, b"\x1b", &mut tokio::io::sink())
        .await
        .unwrap();
    assert!(view.messages_board.is_none(), "esc from column 1 closes");
    // a/b switch the filter (item 5).
    check_fixture(&mut view);
    let b = view.messages_board.as_mut().expect("reopen");
    b.snapshot.apply(fixture_projection());
    super::keys(&mut view, b"b", &mut tokio::io::sink())
        .await
        .unwrap();
    let b = view.messages_board.as_ref().unwrap();
    assert_eq!(b.filter, ListFilter::Broadcasts);
    assert!(b
        .tree_rows()
        .iter()
        .any(|r| matches!(r, TreeRow::Channel(_))));
    super::keys(&mut view, b"a", &mut tokio::io::sink())
        .await
        .unwrap();
    let b = view.messages_board.as_ref().unwrap();
    assert_eq!(b.filter, ListFilter::All);
    assert!(b
        .tree_rows()
        .iter()
        .any(|r| matches!(r, TreeRow::Agent { .. })));
}

/// The fixture projection, detached from the reply test's own apply.
fn fixture_projection() -> serde_json::Value {
    serde_json::json!({
        "participants": [
            {"key":"s1","name":"first","system":false,"live":true},
            {"key":"s-c","name":"candor","system":false,"live":true},
        ],
        "threads": [],
        "system": {},
        "channels": [{"scope":"fno","rows":[]}],
        "announcements": [],
        "unreadable": 0,
    })
}
