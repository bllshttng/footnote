//! Render, diff and hit tests for the pane frame (AC7-AC10).

use super::*;

/// A PaneMeta row.
fn meta_row(id: u64, label: &str, node: &str, branch: &str, ctx: &str) -> crate::proto::PaneMeta {
    crate::proto::PaneMeta {
        id,
        label: label.into(),
        node: Some(node.into()),
        branch: Some(branch.into()),
        ctx: Some(ctx.into()),
    }
}

/// two_pane_view with content-sized pty frames, PaneMeta rows on the active
/// tab, and an AgentRow (Working, opus-5) on the focused pane 11.
fn framed_pair() -> View {
    let mut v = two_pane_view();
    v.frames.insert(10, text_frame(27, 33, 'a'));
    v.frames.insert(11, text_frame(27, 34, 'b'));
    v.layout.squads[0].tabs[0].panes = vec![
        meta_row(10, "king-5317-succeed-g3", "x-0e67", "main", "49%"),
        meta_row(11, "pane-b", "n-abc12", "feature/pane", "22%"),
    ];
    let mut a = tab_agent(Some(0), Some(AgentBadge::Working), false);
    a.pane_id = Some(11);
    a.model = Some("opus-5".into());
    v.layout.agents.push(a);
    v
}

/// The narrow-neighbour layout for AC7-EDGE: pane 10 is 19 cols (unframed),
/// pane 11 framed.
fn narrow_pair() -> View {
    let mut v = two_pane_view();
    v.set_layout(LayoutView {
        squads: vec![meta(1, "footnote", 2, 1)],
        active_squad: 1,
        panes: vec![
            (
                10,
                Rect {
                    x: 0,
                    y: 0,
                    rows: 29,
                    cols: 19,
                },
            ),
            (
                11,
                Rect {
                    x: 20,
                    y: 0,
                    rows: 29,
                    cols: 36,
                },
            ),
        ],
        focus: 11,
        area: (29, 72),
        agents: vec![],
        focus_node: None,
    });
    v.frames.insert(10, text_frame(29, 19, 'a'));
    v.frames.insert(11, text_frame(27, 34, 'b'));
    v.layout.squads[0].tabs[0].panes =
        vec![meta_row(11, "pane-b", "n-abc12", "feature/pane", "22%")];
    v
}

/// The terminal cell of pane-10's content origin.
const P10_CONTENT: usize = 29;

#[test]
fn pane_border_renders_the_focused_frame() {
    let v = framed_pair();
    let frame = v.compose();
    assert!(frame.geometry_ok());
    let lines: Vec<Vec<char>> = (0..frame.rows as usize)
        .map(|r| {
            (0..frame.cols as usize)
                .map(|c| frame.cells[r * frame.cols as usize + c].c)
                .collect()
        })
        .collect();
    let top = &lines[1];
    let seg = |a: usize, b: usize| top[a..b].iter().collect::<String>();
    // Focused tab with caps, cut to the 14-cell zone beside the grip.
    assert!(seg(64, 99).contains("▐ pane-b-… ▌"), "{top:?}");
    // Status on the right zone.
    assert!(seg(64, 99).contains("● Work"), "{top:?}");
    // Rounded corners.
    assert_eq!(top[64], '╭');
    assert_eq!(top[99], '╮');
    // Unfocused neighbour: caps-less tab, cut to 10 cols of name.
    assert!(seg(28, 63).contains("─ king-5317… ─"), "{top:?}");
    // Content blits at the content origin.
    assert_eq!(lines[2][P10_CONTENT], 'a');
    assert_eq!(lines[2][65], 'b');
    // The gap between the frames is blank.
    assert_eq!(lines[2][63], ' ');
    // Cursor at the focused pane's content origin.
    assert_eq!((frame.cursor_row, frame.cursor_col), (2, 65));
    assert!(frame.cursor_visible);
}

#[test]
fn pane_border_keeps_a_divider_beside_an_unframed_pane() {
    // AC7-EDGE: a 19-col pane never frames; its content starts at its rect
    // origin and the seam beside it stays a real divider.
    let v = narrow_pair();
    let frame = v.compose();
    let row1: Vec<char> = (0..frame.cols as usize).map(|c| frame.cells[c].c).collect();
    assert_eq!(row1[28], 'a', "the narrow pane's row 0 at its rect origin");
    assert_eq!(row1[47], '│', "the seam keeps its glyph");
    assert_eq!(row1[48], '╭', "the framed neighbour owns its own border");
}

#[test]
fn pane_border_renders_a_metaless_pane_as_shell() {
    // AC7-ERR: a pane with no PaneMeta row and no AgentRow (Layout still in
    // flight) still frames, names itself `shell`, and carries no fields.
    let v = two_pane_view();
    let frame = v.compose();
    let top: String = (64..99)
        .map(|c| frame.cells[frame.cols as usize + c].c)
        .collect();
    assert!(top.contains("▐ shell ▌"), "{top}");
    assert!(
        !top.contains("49%") && !top.contains("main") && !top.contains("n-abc12"),
        "{top}"
    );
}

#[test]
fn pane_border_diff_only_the_top_edge_moves() {
    // AC8-HP: a badge flip repaints only the focused pane's top-edge row;
    // every content row is identical. Sideline columns are excluded (the
    // flip repaints the agent's sideline row too, a different surface).
    let v1 = framed_pair();
    let mut v2 = framed_pair();
    v2.layout.agents[0].badge = Some(AgentBadge::Blocked);
    let a = v1.compose();
    let b = v2.compose();
    let mut moved = Vec::new();
    for r in 0..a.rows as usize {
        for c in 28..a.cols as usize {
            if a.cells[r * a.cols as usize + c] != b.cells[r * a.cols as usize + c] {
                moved.push(r);
                break;
            }
        }
    }
    assert_eq!(moved, vec![1]);
}

#[test]
fn pane_border_hit_test_answers_content_and_border() {
    // AC10-HP: a click at framed pane 11's content origin answers (11,0,0);
    // the border ring never forwards.
    let v = framed_pair();
    assert_eq!(v.hit_test(2, 65), Some((11, 0, 0)));
    assert_eq!(v.hit_test(1, 64), None, "the border ring never forwards");
    // The gap cell between the two frames forwards to nothing too.
    assert_eq!(v.hit_test(2, 63), None);
}

#[test]
fn pane_border_press_route_names_the_ring() {
    // AC9: the ring answers for a press (the route sends FocusPane); grips
    // and seams win their own cells before this check ever runs.
    let v = framed_pair();
    assert_eq!(v.border_pane_at(1, 64), Some(11));
    // A grip cell is on the ring too, but grip_at wins it in the route.
    assert_eq!(v.border_pane_at(1, 45), Some(10));
    // The gap between frames belongs to no ring.
    assert_eq!(v.border_pane_at(2, 63), None);
}

#[test]
fn pane_border_renders_before_after_dumps() {
    // The PR body's render proof: the branch's framed render of framed_pair,
    // plus one edge layout per drop step (AC2-EDGE).
    let v = framed_pair();
    println!("=== branch render: two framed panes, pane 11 focused ===");
    println!("{}", frame_text(&v.compose()));
    let f = crate::pane_border::EdgeFields {
        name: "king-5317-succeed-g3",
        status: Some(('●', "Work")),
        model: Some("opus-5"),
        node: Some("x-0e67"),
        branch: Some("main"),
        ctx: Some("49%"),
    };
    for w in [100u16, 44, 40, 35, 30, 20] {
        let e = crate::pane_border::edges(
            &f,
            Rect {
                x: 0,
                y: 0,
                rows: 12,
                cols: w,
            },
            w >= 40,
            true,
        );
        println!("=== edges at {w} cols (grip {}) ===", w >= 40);
        println!("{}", e.top.iter().map(|(c, _)| c).collect::<String>());
        println!("{}", e.bottom.iter().map(|(c, _)| c).collect::<String>());
    }
}
