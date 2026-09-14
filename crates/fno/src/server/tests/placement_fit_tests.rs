//! The `fit` placement tests (v80, x-ae47): where the server commits a
//! server-chosen tab. Parent helpers resolve through the glob.
use super::*;

fn full_tab(id: TabId, leaves: [u64; 4]) -> Tab {
    Tab {
        name: None,
        id,
        root: Node::Branch {
            axis: Axis::Horizontal,
            children: leaves.map(|p| (0.25, Node::Leaf(p))).to_vec(),
        },
        focus: leaves[0],
    }
}

#[test]
fn place_with_fit_births_squad_on_route_miss() {
    // AC1-HP: a fit placement onto a squad-less route births the squad
    // and its first tab, the same path the no-tab placement takes.
    let mut core = empty_core();
    let (sid, tid, fell_back) = core
        .place_with(
            None,
            "/fresh",
            9,
            &PanePlacement {
                fit: true,
                max_panes: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!fell_back);
    let sq = core.session.squad(sid).unwrap();
    assert_eq!(sq.origins, vec!["/fresh".to_string()]);
    assert_eq!(sq.tabs.len(), 1);
    assert_eq!(sq.tabs[0].id, tid);
    assert_eq!(tree::leaves(&sq.tabs[0].root), vec![9]);
}

#[test]
fn place_with_fit_walks_to_first_tab_with_room() {
    // AC2-HP: a full tab is skipped; the pane lands in the first tab
    // below the cap, and the squad's tab count is unchanged.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, full_tab(10, [1, 2, 3, 4]));
    core.session
        .squad_mut(1)
        .unwrap()
        .tabs
        .push(leaf_tab(20, 5));

    let (_sid, tid, fell_back) = core
        .place_with(
            Some(1),
            "/a",
            9,
            &PanePlacement {
                fit: true,
                max_panes: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!fell_back);
    assert_eq!(tid, 20);
    let sq = core.session.squad(1).unwrap();
    assert_eq!(sq.tabs.len(), 2, "fit adds no tab when one has room");
    assert_eq!(tree::leaves(&sq.tabs[0].root), vec![1, 2, 3, 4]);
    assert_eq!(tree::leaves(&sq.tabs[1].root), vec![5, 9]);
}

#[test]
fn place_with_fit_mints_tab_when_every_tab_is_full() {
    // AC3-EDGE: every tab at the cap -> the squad gains a tab with the
    // pane as its lone leaf. No tab refused for size, so no fallback.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, full_tab(10, [1, 2, 3, 4]));

    let (_sid, tid, fell_back) = core
        .place_with(
            Some(1),
            "/a",
            9,
            &PanePlacement {
                fit: true,
                max_panes: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!fell_back);
    let sq = core.session.squad(1).unwrap();
    assert_eq!(sq.tabs.len(), 2);
    assert_eq!(sq.tabs[1].id, tid);
    assert_eq!(tree::leaves(&sq.tabs[0].root), vec![1, 2, 3, 4]);
    assert_eq!(tree::leaves(&sq.tabs[1].root), vec![9]);
}

#[test]
fn place_with_fit_too_small_tab_hands_pane_to_next_with_room() {
    // A tab with room that refuses the split for size does not dead-end:
    // the walk continues to the next tab with room, still no fallback.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(10, 1));
    core.session
        .squad_mut(1)
        .unwrap()
        .tabs
        .push(leaf_tab(20, 2));
    // 3 rows cannot hold two MIN_ROWS(2)-tall halves -> Dir::Down refusal.
    core.tab_areas.insert(10, (3, 80));

    let (_sid, tid, fell_back) = core
        .place_with(
            Some(1),
            "/a",
            9,
            &PanePlacement {
                fit: true,
                max_panes: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!fell_back, "the next tab with room took the pane");
    assert_eq!(tid, 20);
    assert_eq!(
        tree::leaves(&core.session.squad(1).unwrap().tabs[1].root),
        vec![2, 9]
    );
}

#[test]
fn place_with_fit_too_small_everywhere_mints_with_fell_back() {
    // fell_back is the "a tab WITH room refused for size" signal: the
    // pane still lands, as a new tab in the same squad.
    let mut core = empty_core();
    core.session
        .add_squad(1, vec!["/a".into()], None, leaf_tab(10, 1));
    // 3 rows cannot hold two MIN_ROWS(2)-tall halves -> Dir::Down refusal.
    core.tab_areas.insert(10, (3, 80));

    let (_sid, tid, fell_back) = core
        .place_with(
            Some(1),
            "/a",
            9,
            &PanePlacement {
                fit: true,
                max_panes: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(fell_back);
    let sq = core.session.squad(1).unwrap();
    assert_eq!(sq.tabs.len(), 2);
    assert_eq!(sq.tabs[1].id, tid);
    assert_eq!(tree::leaves(&sq.tabs[1].root), vec![9]);
}

#[test]
fn run_pane_refuses_fit_with_explicit_geometry() {
    // AC4-ERR: the server re-validates the CLI gate - fit plus any
    // explicit geometry is BAD_REQUEST before any pane exists.
    let mut core = empty_core();
    for (placement, label) in [
        (
            PanePlacement {
                fit: true,
                tab: Some(TabSel::Id(1)),
                ..Default::default()
            },
            "tab",
        ),
        (
            PanePlacement {
                fit: true,
                at: Some(3),
                ..Default::default()
            },
            "at",
        ),
        (
            PanePlacement {
                fit: true,
                split: Some(Dir::Down),
                ..Default::default()
            },
            "split",
        ),
        (
            PanePlacement {
                fit: true,
                here: true,
                ..Default::default()
            },
            "here",
        ),
    ] {
        let err = core
            .run_pane(
                "/a".into(),
                "/a".into(),
                vec!["true".into()],
                24,
                80,
                false,
                placement,
                None,
            )
            .unwrap_err();
        assert_eq!(err.0, err_code::BAD_REQUEST, "{label}");
        assert_eq!(
            err.1,
            "--fit selects its own tab and cannot be combined with --tab, --at, or --split"
        );
        assert!(core.session.squads.is_empty(), "{label}: nothing placed");
    }
}
