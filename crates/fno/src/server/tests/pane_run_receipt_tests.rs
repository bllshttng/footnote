//! Pane-run placement: the receipts naming where the server committed a
//! selector spawn, the child cwd, and create-if-absent squads - moved
//! verbatim out of the over-budget test files. Parent helpers resolve
//! through the glob.
use super::*;

#[test]
fn run_pane_places_at_named_tab_and_anchor() {
    // AC2-HP: --tab <id> --at <pane> --split down lands below the anchor in
    // that exact tab; a bad anchor is BAD_REQUEST with no orphan pane.
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let before_panes = core.panes.len();
    let pid = core
        .run_pane(
            "/a".into(),
            "/a".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                view: false,
                from: None,
                portal_new: false,
                portal: None,
                target: PaneTarget::SquadId(1),
                split: Some(Dir::Down),
                here: false,
                tab: Some(TabSel::Id(10)),
                at: Some(2),
                fallback: PlacementFallback::NewTab,
                max_panes: None,
                thread_pane: false,
                fit: false,
            },
            None,
        )
        .unwrap();
    let tab = core
        .session
        .squad(1)
        .unwrap()
        .tabs
        .iter()
        .find(|t| t.id == 10)
        .unwrap();
    assert!(tree::leaves(&tab.root).contains(&pid), "landed in tab 10");
    core.reap_pane(pid);

    // Bad anchor: pane 999 is not in tab 10 -> BAD_REQUEST, no orphan pane.
    let panes_now = core.panes.len();
    let err = core
        .run_pane(
            "/a".into(),
            "/a".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                view: false,
                from: None,
                portal_new: false,
                portal: None,
                target: PaneTarget::SquadId(1),
                split: Some(Dir::Down),
                here: false,
                tab: Some(TabSel::Id(10)),
                at: Some(999),
                fallback: PlacementFallback::NewTab,
                max_panes: None,
                thread_pane: false,
                fit: false,
            },
            None,
        )
        .unwrap_err();
    assert_eq!(err.0, err_code::BAD_REQUEST);
    assert_eq!(
        core.panes.len(),
        panes_now,
        "a bad anchor reaps the pre-spawned pane (no orphan)"
    );
    let _ = before_panes;
}

#[test]
fn pane_run_receipt_reports_committed_tab_for_selector_placements() {
    // (x-18c4) The bounded pane lane verifies placement against this
    // receipt, so ANY selector placement must answer where the pane
    // actually landed - not only `--at current`.
    let mut core = two_tab_core();
    core.shells = vec!["/bin/cat".into()];
    let (tx, mut rx) = oneshot::channel();
    core.handle_msg(CoreMsg::PaneRun {
        squad_key: "/a".into(),
        cwd: "/a".into(),
        argv: vec!["/bin/cat".into()],
        cols: Some(80),
        rows: Some(24),
        claim: false,
        placement: PanePlacement {
            view: false,
            from: None,
            portal_new: false,
            portal: None,
            target: PaneTarget::SquadId(1),
            split: None,
            here: false,
            tab: Some(TabSel::Id(20)),
            at: None,
            fallback: PlacementFallback::NewTab,
            max_panes: None,
            thread_pane: false,
            fit: false,
        },
        worker: None,
        reply: tx,
    });
    match rx.try_recv().unwrap() {
        ServerMsg::PaneSpawned { placement, .. } => {
            let rp = placement.expect("selector placement carries the receipt");
            assert_eq!(rp.tab, 20, "receipt names the committed tab");
            assert_eq!(rp.squad, 1);
            assert_eq!(rp.tab_name.as_deref(), Some("bee"));
            assert_eq!(rp.tab_ordinal, Some(2));
        }
        other => panic!("expected PaneSpawned, got {other:?}"),
    }

    // The legacy no-selector path keeps `placement: None`.
    let (tx, mut rx) = oneshot::channel();
    core.handle_msg(CoreMsg::PaneRun {
        squad_key: "/a".into(),
        cwd: "/a".into(),
        argv: vec!["/bin/cat".into()],
        cols: Some(80),
        rows: Some(24),
        claim: false,
        placement: PanePlacement {
            view: false,
            from: None,
            portal_new: false,
            portal: None,
            target: PaneTarget::SquadId(1),
            split: None,
            here: false,
            tab: None,
            at: None,
            fallback: PlacementFallback::NewTab,
            max_panes: None,
            thread_pane: false,
            fit: false,
        },
        worker: None,
        reply: tx,
    });
    match rx.try_recv().unwrap() {
        ServerMsg::PaneSpawned { placement, .. } => {
            assert!(placement.is_none(), "no-selector path stays receipt-less");
        }
        other => panic!("expected PaneSpawned, got {other:?}"),
    }
    let spawned: Vec<u64> = core.session.squads[0]
        .tabs
        .iter()
        .flat_map(|t| tree::leaves(&t.root))
        .filter(|pane| *pane >= 100)
        .collect();
    for pane in spawned {
        core.reap_pane(pane);
    }
}

#[test]
fn pane_placement_target_does_not_replace_child_cwd() {
    let mut core = placement_core();
    let root = std::env::temp_dir().join(format!("fno-placement-cwd-{}", std::process::id()));
    let child_cwd = root.join("child");
    std::fs::create_dir_all(&child_cwd).unwrap();
    let marker = child_cwd.join("cwd.txt");
    let pid = core
        .run_pane(
            "/repo/default".into(),
            child_cwd.to_string_lossy().into_owned(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "pwd > cwd.txt; sleep 30".into(),
            ],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

    // A loaded CI runner can take several seconds just to spawn the PTY +
    // start the shell; 15s matches the PTY-wait convention elsewhere and
    // keeps this off the flake list. Readiness is NON-EMPTY CONTENT, not
    // existence: `pwd > cwd.txt` creates the file on redirect, BEFORE pwd
    // writes into it, so an exists() gate can hand the read an empty string
    // and the canonicalize below then fails as a confusing NotFound.
    let content = || {
        std::fs::read_to_string(&marker)
            .ok()
            .filter(|s| !s.trim().is_empty())
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    while content().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let reported =
        content().expect("pane shell never wrote cwd.txt within 15s (spawn slow or failed)");
    assert_eq!(
        std::fs::canonicalize(reported.trim()).unwrap(),
        std::fs::canonicalize(&child_cwd).unwrap()
    );
    let (sid, _) = core.session.find_pane(pid).unwrap();
    assert_eq!(sid, 7);

    core.reap_pane(pid);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn run_pane_create_if_absent_mints_persisted_named_squad() {
    // AC2-HP (x-9f75): a `pane run --squad <name>` naming no existing squad mints a persisted named squad
    // (origins = the spawn's repo root) and lands the pane as its first tab. A second run with the same
    // name joins it - no duplicate mint.
    let _s = StoreScratch::new("run-create-if-absent");
    let mut core = empty_core();
    let run = |core: &mut Core| {
        core.run_pane(
            "/repo/proj".into(),
            "/repo/proj".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("readyrule".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };
    let pid = run(&mut core);
    let (sid, _) = core.session.find_pane(pid).unwrap();
    let sq = core.session.squad(sid).unwrap();
    assert_eq!(sq.name.as_deref(), Some("readyrule"));
    assert_eq!(sq.origins, vec!["/repo/proj".to_string()]);
    assert_eq!(tree::leaves(&sq.tabs[0].root), vec![pid]);
    assert!(
        crate::squad_store::load()
            .squads
            .iter()
            .any(|s| s.name == "readyrule"),
        "the named squad is persisted (write-through)"
    );

    let pid2 = run(&mut core);
    let (sid2, _) = core.session.find_pane(pid2).unwrap();
    assert_eq!(sid2, sid, "the second run joins the existing named squad");
    assert_eq!(
        core.session
            .squads
            .iter()
            .filter(|s| s.name.as_deref() == Some("readyrule"))
            .count(),
        1,
        "no duplicate squad minted"
    );

    core.reap_pane(pid);
    core.reap_pane(pid2);
}

#[test]
fn run_pane_create_if_absent_rejects_blank_name_before_spawn() {
    // A blank/whitespace SquadName is still refused (never a minted squad),
    // and no pane is spawned - fail-closed, mirroring resolve_placement.
    let _s = StoreScratch::new("run-create-blank");
    let mut core = empty_core();
    let before = core.panes.len();
    let err = core
        .run_pane(
            "/repo/proj".into(),
            "/repo/proj".into(),
            vec!["/bin/cat".into()],
            24,
            80,
            false,
            PanePlacement {
                target: PaneTarget::SquadName("   ".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert!(err.1.contains("blank"), "{err:?}");
    assert_eq!(err.0, err_code::BAD_REQUEST, "blank name is a bad request");
    assert_eq!(core.panes.len(), before, "no pane spawned on a blank name");
    assert!(core.session.squads.is_empty(), "no squad minted");
}
