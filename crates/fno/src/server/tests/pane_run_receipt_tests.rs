//! Pane-run placement receipts: where the server committed a selector spawn -
//! moved verbatim out of server.rs (file budget shrink). Parent helpers
//! resolve through the glob.
use super::*;

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
