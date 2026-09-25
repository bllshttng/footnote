use super::*;

// The x-9fd0 test family: the portal-placement picker (`P`) and the
// `l`/Right reach on an agent row. Lives in its own module; client_tests.rs
// is shrink-only under the file-budget gate.

#[tokio::test]
async fn selector_shift_p_opens_the_portal_picker_without_sending() {
    // (x-9fd0) `P` no longer allocates on press: it opens the portal picker
    // over the open portals plus a new-portal row, PRE-SELECTING the
    // new-portal row. The wire gesture the old bare `P` sent is what
    // `P` Enter now sends (see portal_pick_p_then_enter_equals_old_bare_p).
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"P", &mut buf).await.unwrap();
    let pick = v.portal_pick.as_ref().expect("the picker opens");
    assert_eq!(pick.id, "c19cd2c3");
    assert_eq!(
        pick.cursor,
        v.open_portal_rows().len(),
        "the new-portal row is pre-selected"
    );
    assert!(buf.is_empty(), "opening the picker sends nothing");
    assert_eq!(v.selector, None, "the picker replaces the selector");
    assert!(v.attach_place.is_none(), "the sibling picker stays closed");
}

/// A view with three OPEN portals (indices 0-2, projected exactly as the
/// server does: `portal: Some(n)` on a pane-hosted row) plus the paneless
/// live attachable bg row the `P` gesture focuses. nairobi/kigali sit on
/// squad 1's unnamed tabs 0/1, lagos on squad 2's tab 0.
fn portal_pick_view() -> View {
    let agent = |squad: Option<u64>, name: &str, pane_id, tab, portal: Option<u8>| AgentRow {
        spawned_by_name: None,
        lineage_reason: None,
        portal,
        harness: None,
        model: None,
        route: None,
        reach: Reach::Locate,
        spawned_by_session: None,
        lineage_kind: None,
        harness_session_id: None,
        squad,
        name: name.into(),
        pane_id,
        badge: None,
        reason: None,
        exited: false,
        dnd: false,
        unmeasured: false,
        liveness_measured_at: None,
        harness_title: None,
        answerable: None,
        attach_id: None,
        external: false,
        seen: false,
        cwd_base: None,
        tombstone: false,
        subline: None,
        tab,
        account: None,
        updated_at: None,
        pr: None,
        pr_session_short: None,
        tail: None,
        crown_level: None,
        crown_scope: None,
        crown_name: None,
        basis: None,
        last_activity_age_s: None,
        resumable: false,
        no_pane_reason: None,
        pane_activity: None,
    };
    let mut v = view_with_agents(vec![
        agent(Some(1), "nairobi", Some(10), Some(0), Some(0)),
        agent(Some(1), "kigali", Some(11), Some(1), Some(1)),
        agent(Some(2), "lagos", Some(12), Some(0), Some(2)),
        agent(None, "bg-claude", None, None, None),
    ]);
    v.layout.agents[3].attach_id = Some("c19cd2c3".into());
    v.section_view
        .insert(SectionKey::Elsewhere, SectionView::Expanded);
    v
}

/// Open the portal picker the way the `P` key does: on the paneless live
/// attachable row, with nothing sent.
async fn open_portal_pick_by_key(v: &mut View) {
    let cur = v
        .display_rows()
        .iter()
        .position(|r| {
            matches!(r, DisplayRow::Agent(a)
                if a.pane_id.is_none() && !a.exited && a.attach_id.is_some())
        })
        .expect("a paneless live attachable row");
    v.selector = Some(cur);
    let mut buf = Vec::new();
    selector_keys(v, b"P", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "opening the picker sends nothing");
    assert!(v.portal_pick.is_some(), "the picker is open");
}

#[tokio::test]
async fn portal_pick_p_then_enter_equals_old_bare_p() {
    // AC3-REG: the gesture the operator already has in their fingers - P then
    // Enter - must send exactly what bare `P` sent before the picker existed:
    // portal_new with NO index, the server allocates.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(v.portal_pick.is_none(), "the commit closes the picker");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert!(placement.portal_new, "asks the server to allocate");
            assert_eq!(
                placement.portal, None,
                "and names no index of its own - that is the whole point"
            );
            assert!(placement.wants_portal(), "still routes as a portal reach");
            assert!(placement.split.is_none() && !placement.here && placement.at.is_none());
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn portal_pick_lists_open_portals_and_preselects_new() {
    // AC1-HP: three open portals listed with index, tab and occupant, plus
    // the new-portal row, with the NEW row selected.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    let pick = v.portal_pick.as_ref().unwrap();
    assert_eq!(pick.cursor, 3, "the new-portal row is pre-selected");
    let overlay = v.portal_pick_lines(pick).join("\n");
    for label in [
        "◫0",
        "◫1",
        "◫2", //
        "nairobi",
        "kigali",
        "lagos", //
        "tab 1",
        "tab 2",      //
        "new portal", //
        "hjkl/arrows move",
        "1-9 jump",
        "enter place",
        "esc/q cancel",
        "shift+HJKL split",
        "t new tab",
    ] {
        assert!(overlay.contains(label), "missing {label}: {overlay}");
    }
    for line in v.portal_pick_lines(&v.portal_pick.as_ref().unwrap()) {
        assert!(!line.contains('…'), "an ellipsis: {line}");
    }
}

#[tokio::test]
async fn portal_pick_digit_places_into_that_portal() {
    // AC2-HP: `2` then Enter shows the row through the picker's SECOND list
    // row - portal 1 - and allocates nothing.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"2\r", &mut buf).await.unwrap();
    assert!(v.portal_pick.is_none(), "the commit closes the picker");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert_eq!(placement.portal, Some(1), "the second list row's index");
            assert!(!placement.portal_new, "no new portal is allocated");
            assert!(placement.wants_portal());
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn portal_pick_out_of_range_digit_drops_the_rest_of_the_read() {
    // AC4-EDGE: `9` on a four-row list BELs, notices, and DROPS the trailing
    // bytes of the same read - a fast `9j` must never move onto a row the
    // operator never chose.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"9j", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "nothing after the bad digit commits");
    let pick = v.portal_pick.as_ref().expect("the picker stays open");
    assert_eq!(pick.cursor, 3, "the trailing j never moves the cursor");
    let (notice, _) = v.notice.expect("the miss is named on screen");
    assert!(notice.contains("no portal at that number"), "{notice}");
}

#[tokio::test]
async fn portal_pick_commit_resolves_against_the_current_portal_set() {
    // AC5-EDGE: the last portal closes underneath the open picker (cursor
    // resting on the new-portal row, which the close moved); the commit
    // resolves against the CURRENT rows, finds the cursor past even the
    // new-portal row, and refuses with a notice instead of acting on the
    // stale list.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await; // cursor 3, four rows
    v.layout.agents.retain(|a| a.portal.is_none()); // every portal closes
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"\r", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a stale commit sends nothing");
    assert!(v.portal_pick.is_none(), "the refusal closes the picker");
    let (notice, _) = v.notice.expect("the staleness is named on screen");
    assert!(notice.contains("no longer available"), "{notice}");
}

#[tokio::test]
async fn portal_pick_digit_keeps_pace_with_portals_closing_under_it() {
    // The cursor is a LIST POSITION, re-resolved per key against the live
    // rows: digit 3 on [p0 p1 p2 +new] points at ◫2; after p2 closes the
    // SAME position names +new, and the commit allocates - never a portal
    // index the list no longer shows.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    portal_pick_keys(&mut v, b"3", &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(v.portal_pick.as_ref().unwrap().cursor, 2);
    v.layout.agents.retain(|a| a.portal != Some(2));
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert!(
                placement.portal_new,
                "the drawn row at the cursor is now the new-portal row"
            );
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn portal_pick_with_no_open_portals_still_offers_new() {
    // First-use shape: zero open portals leaves a one-row picker whose Enter
    // is the old bare `P` exactly.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf = Vec::new();
    selector_keys(&mut v, b"P", &mut buf).await.unwrap();
    assert_eq!(v.portal_pick.as_ref().unwrap().cursor, 0);
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"\r", &mut buf).await.unwrap();
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert!(placement.portal_new);
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn portal_pick_hjkl_move_and_q_esc_cancel() {
    // The vocabulary is the attach picker's: hjkl/arrows move, esc/q cancel.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await; // cursor 3
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"kk", &mut buf).await.unwrap();
    assert_eq!(v.portal_pick.as_ref().unwrap().cursor, 1, "k moves up");
    portal_pick_keys(&mut v, b"j", &mut buf).await.unwrap();
    assert_eq!(v.portal_pick.as_ref().unwrap().cursor, 2, "j moves down");
    portal_pick_keys(&mut v, b"1", &mut buf).await.unwrap();
    assert_eq!(v.portal_pick.as_ref().unwrap().cursor, 0, "digit jumps");
    portal_pick_keys(&mut v, b"q", &mut buf).await.unwrap();
    assert!(v.portal_pick.is_none(), "q cancels");
    assert!(buf.is_empty(), "motion and cancel never send");
}

#[tokio::test]
async fn portal_pick_shift_l_on_new_row_sends_a_right_split() {
    // AC1-HP: on the + row, shift+L asks for the new portal as a RIGHT
    // split of the pane the operator is looking at, and the picker closes.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"L", &mut buf).await.unwrap();
    assert!(v.portal_pick.is_none(), "the commit closes the picker");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert!(placement.portal_new, "asks the server to allocate");
            assert_eq!(placement.portal, None, "no index is named");
            assert_eq!(placement.split, Some(Dir::Right));
            assert_eq!(
                placement.target,
                PaneTarget::SquadId(v.layout.active_squad),
                "the split lands in the workspace the operator views"
            );
            assert!(!placement.here);
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn portal_pick_split_on_an_open_portal_row_sends_nothing() {
    // AC1-ERR: shift+HJKL on an OPEN-portal row cannot commit (a repoint
    // keeps its geometry by design), so nothing is sent, the cursor never
    // moves, the rest of the read is dropped, and the notice names the + row.
    let mut v = portal_pick_view();
    open_portal_pick_by_key(&mut v).await;
    portal_pick_keys(&mut v, b"2", &mut Vec::new())
        .await
        .unwrap(); // cursor to list row 1
    let mut buf = Vec::new();
    portal_pick_keys(&mut v, b"Lj", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "nothing commits");
    let pick = v.portal_pick.as_ref().expect("the picker stays open");
    assert_eq!(pick.cursor, 1, "the trailing j never moves the cursor");
    let (notice, _) = v.notice.expect("the miss is named on screen");
    assert!(
        notice.contains("move to the + row"),
        "the + row is named: {notice}"
    );
}

#[tokio::test]
async fn portal_pick_t_on_new_row_equals_enter() {
    // AC1-EDGE: `t` is Enter's keyboard-only alias, byte for byte.
    let mut v1 = portal_pick_view();
    open_portal_pick_by_key(&mut v1).await;
    let mut b1 = Vec::new();
    portal_pick_keys(&mut v1, b"\r", &mut b1).await.unwrap();
    let mut v2 = portal_pick_view();
    open_portal_pick_by_key(&mut v2).await;
    let mut b2 = Vec::new();
    portal_pick_keys(&mut v2, b"t", &mut b2).await.unwrap();
    assert_eq!(b1, b2, "t sends exactly what Enter sends");
}

// ---- task 1.2: `l`/Right on an agent row reaches portal 0 ----------------

#[tokio::test]
async fn selector_l_on_agent_row_reaches_portal_zero() {
    // AC6-HP (half 1): `l` on a paneless live agent row takes the SAME
    // row_action path Enter takes - the reach through portal 0 - matching the
    // peek overlay, where `l` and Enter already agree.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"l", &mut buf).await.unwrap();
    assert_eq!(v.selector, None, "the reach closes the selector");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { id, placement }) => {
            assert_eq!(id, "c19cd2c3");
            assert_eq!(
                placement.portal_target(),
                Some(0),
                "the same portal Enter reaches"
            );
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn selector_right_arrow_on_agent_row_reaches_portal_zero() {
    // AC6-HP (half 2): a COMPLETE Right sequence is stripped by the pre-pass
    // before the fold, so it needs its own agent-row branch - today it said
    // "only a workspace row has a caret", the dead-keybind shape this closes.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"\x1b[C", &mut buf).await.unwrap();
    assert_eq!(v.selector, None, "the reach closes the selector");
    let mut cur = std::io::Cursor::new(&buf);
    match crate::proto::read_msg_sync::<_, ClientMsg>(&mut cur).unwrap() {
        ClientMsg::Command(Command::AttachAgent { placement, .. }) => {
            assert_eq!(placement.portal_target(), Some(0));
        }
        other => panic!("expected AttachAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn selector_h_on_agent_row_stays_inert() {
    // The plan's explicit leave-alone: collapse has no meaning for an agent
    // row, so `h` neither reaches nor closes nor sends.
    let mut v = unified_rows_view();
    v.selector = Some(8); // bg-claude
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"h", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "h sends nothing");
    assert_eq!(v.selector, Some(8), "h neither reaches nor closes");
}

// ---- task 1.1: the `P` refusal names the row's state -----------------------

/// The display position of the named agent row.
fn cursor_on(v: &View, name: &str) -> usize {
    v.display_rows()
        .iter()
        .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == name))
        .unwrap_or_else(|| panic!("row {name} is on screen"))
}

#[tokio::test]
async fn portal_pick_refusal_names_the_portal_state_and_reseat() {
    // AC1-HP: `P` on a row already shown through a portal names the row,
    // its portal and tab, and the verb that moves it - not the old
    // state-free rule sentence that contradicted the picker listing.
    let mut v = portal_pick_view();
    let cur = cursor_on(&v, "nairobi");
    v.selector = Some(cur);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"P", &mut buf).await.unwrap();
    assert!(buf.is_empty(), "a refusal sends nothing");
    assert!(v.portal_pick.is_none(), "the picker never opens");
    let (notice, _) = v.notice.expect("the state is named on screen");
    assert!(notice.contains("nairobi"), "{notice}");
    assert!(notice.contains("portal 0"), "{notice}");
    assert!(notice.contains("tab 1"), "{notice}");
    assert!(notice.contains("reseat"), "{notice}");
}

#[tokio::test]
async fn portal_pick_refusal_names_the_pane_state_and_reseat() {
    // A pane-hosted row that is no portal seat still refuses - the gate
    // does not move - but the refusal names the pane state it observes.
    let mut v = portal_pick_view();
    v.layout.agents[0].portal = None; // nairobi keeps pane 10, loses portal 0
    let cur = cursor_on(&v, "nairobi");
    v.selector = Some(cur);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"P", &mut buf).await.unwrap();
    assert!(
        v.portal_pick.is_none(),
        "the gate holds: the picker stays shut"
    );
    let (notice, _) = v.notice.expect("the state is named on screen");
    assert!(notice.contains("nairobi"), "{notice}");
    assert!(notice.contains("already has a pane"), "{notice}");
    assert!(notice.contains("reseat"), "{notice}");
    assert!(notice.contains("tab 1"), "{notice}");
}

#[tokio::test]
async fn portal_pick_refusal_on_an_exited_row_names_the_exit_not_reseat() {
    // AC1-ERR: an exited row gets its name and its exit. `reseat` moves a
    // LIVE worker, so the exited arm must not offer it.
    let mut v = portal_pick_view();
    v.layout.agents[3].attach_id = None;
    v.layout.agents[3].exited = true; // bg-claude exits
    let cur = cursor_on(&v, "bg-claude");
    v.selector = Some(cur);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"P", &mut buf).await.unwrap();
    let (notice, _) = v.notice.expect("the exit is named on screen");
    assert!(notice.contains("bg-claude"), "{notice}");
    assert!(notice.contains("exited"), "{notice}");
    assert!(
        !notice.contains("reseat"),
        "no reseat for the dead: {notice}"
    );
}

#[tokio::test]
async fn portal_pick_refusal_on_a_non_agent_row_states_the_rule() {
    // AC1-EDGE (half 1): no row in hand means no state to name, so this is
    // the one arm allowed to state the rule.
    let mut v = portal_pick_view();
    let cur = v
        .display_rows()
        .iter()
        .position(|r| !matches!(r, DisplayRow::Agent(_)))
        .expect("a non-agent row");
    v.selector = Some(cur);
    let mut buf: Vec<u8> = Vec::new();
    selector_keys(&mut v, b"P", &mut buf).await.unwrap();
    let (notice, _) = v.notice.expect("the rule is named on screen");
    assert_eq!(notice, "a portal shows an agent row");
}
