//! The tab-strip context menu: the Split/Join face gate, its send arm,
//! and the headless capture render, moved out of client_tests.rs under
//! the file-budget gate. Helpers stay in the parent and arrive via super::*.
use super::*;

#[tokio::test]
async fn tab_menu_opens_off_a_tab_cell_with_destructive_last() {
    // AC3-HP: right-pressing a tab cell opens the menu pinned to that tab's
    // stable id, with close tab last after a rule.
    let mut v = view_with_agents(vec![]);
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let m = v.row_menu.as_ref().unwrap();
    assert_eq!(m.target, super::MenuTarget::Tab(0));
    assert_eq!(
        m.actions,
        vec![
            super::MenuAction::TabNew,
            super::MenuAction::TabRename,
            super::MenuAction::TabReorder(-1),
            super::MenuAction::TabReorder(1),
            super::MenuAction::TabMoveTo,
            super::MenuAction::TabJoin(Dir::Left),
            super::MenuAction::TabJoin(Dir::Right),
            super::MenuAction::TabJoin(Dir::Up),
            super::MenuAction::TabJoin(Dir::Down),
            super::MenuAction::TabClose,
        ]
    );
    // The destructive item sits last, after a Rule (menu grammar).
    let labels = menu_labels(m);
    // `Close`, not `Close tab`: one shape with the row menu's `✕ Remove`.
    assert_eq!(labels.last().map(String::as_str), Some("Close"));
    assert!(
        m.popup
            .rows
            .iter()
            .rposition(|r| matches!(r, PopupRow::Rule))
            .is_some_and(|rule_at| m.popup.rows.len() - 1 > rule_at),
        "a rule separates close tab from the rest"
    );

    // The other face: the menu's own tab picks the grid. The VIEWED tab's
    // menu offers Split (a tab joining into itself never made sense) and
    // no Join; any other tab's menu offers Join and no Split.
    let mut v = view_with_agents(vec![]);
    let viewed_id = v.active_squad_active_tab_id().expect("a viewed tab");
    // The first tab cell is tab 0; the active tab in this fixture is 1.
    let ((tr, tc), _) = tab_and_new_tab_cells(&v);
    assert!(v.open_tab_menu(tr, tc, Anchor::Center));
    let other = v.row_menu.as_ref().unwrap();
    assert_ne!(other.target, super::MenuTarget::Tab(viewed_id));
    assert!(
        !other
            .actions
            .iter()
            .any(|a| matches!(a, super::MenuAction::TabSplit(_))),
        "another tab's menu lists no Split"
    );
    assert!(
        other
            .actions
            .iter()
            .any(|a| matches!(a, super::MenuAction::TabJoin(_))),
        "another tab's menu lists Join"
    );
    // The viewed tab's cell: walk the strip spans for the cell naming it.
    let mut viewed_cell = None;
    let mut c = v.panel_w() as usize;
    for span in v.tab_bar_window() {
        let w = span.text.chars().count();
        if viewed_cell.is_none() {
            if let Some(super::TabHit::Tab(tid)) = span.hit {
                if tid == viewed_id {
                    viewed_cell = Some((0u16, c as u16));
                }
            }
        }
        c += w;
    }
    let (vr, vc) = viewed_cell.expect("the viewed tab's cell");
    assert!(v.open_tab_menu(vr, vc, Anchor::Center));
    let viewed = v.row_menu.as_ref().unwrap();
    assert!(
        viewed
            .actions
            .iter()
            .any(|a| matches!(a, super::MenuAction::TabSplit(_))),
        "the viewed tab's menu lists Split"
    );
    assert!(
        !viewed
            .actions
            .iter()
            .any(|a| matches!(a, super::MenuAction::TabJoin(_))),
        "the viewed tab's menu lists no Join"
    );
    // The headless visual record: the same opened menu rendered to frame
    // text, written to target/ so the PR attaches the real grid.
    let text = frame_text(&v.compose());
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("menu-capture.txt");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, &text).unwrap();
    assert!(text.contains("Split Up"), "the grid renders");

    // The skew branch of the same family: against a server that never
    // announced (pre-v97 layout; the v95 build among them cannot parse
    // SplitDir and the unknown variant ended the client's session),
    // the Split cell refuses by notice and sends nothing. The edges
    // pin the threshold: v96 is the command's birth, not the
    // announcement's.
    assert!(!server_has_splitdir(None));
    assert!(!server_has_splitdir(Some(95)));
    assert!(server_has_splitdir(Some(96)));
    assert!(server_has_splitdir(Some(97)));
    v.server_proto = None;
    let sel = v
        .row_menu
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|a| matches!(a, super::MenuAction::TabSplit(Dir::Left)))
        .expect("the viewed tab's menu offers the split cell");
    v.row_menu.as_mut().unwrap().popup.sel = sel;
    let mut buf: Vec<u8> = Vec::new();
    row_menu_execute_selected(&mut v, &mut buf).await.unwrap();
    assert!(buf.is_empty(), "an unannounced server gets no split");
    assert!(
        v.notice
            .as_ref()
            .is_some_and(|(s, _)| s.contains("restart the mux server"))
    );
}
