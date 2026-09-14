//! The `pane run --fit` parse and wire-shape tests (x-ae47), moved out of
//! mux_cli.rs (file budget shrink). Parent helpers resolve through the glob.
use super::*;

#[test]
fn mux_pane_parse_run_fit_selects_server_tab() {
    // AC4-ERR + AC6-HP (client half): bare --fit parses into the
    // placement; any explicit geometry refuses before a command exists.
    let p = parse_pane_args(&os(&["run", "--fit", "--", "sleep", "300"])).unwrap();
    assert!(matches!(
        p.cmd,
        PaneCmd::Run {
            placement: PanePlacement { fit: true, .. },
            ref argv,
            ..
        } if argv == &["sleep", "300"]
    ));
    for combo in [
        vec!["run", "--fit", "--tab", "id:1", "--", "true"],
        vec!["run", "--fit", "--at", "3", "--", "true"],
        vec!["run", "--fit", "--split", "down", "--", "true"],
        vec!["run", "--fit", "at", "3", "--", "true"],
    ] {
        let err = parse_pane_args(&os(&combo)).unwrap_err();
        assert_eq!(
            err,
            "--fit selects its own tab and cannot be combined with --tab, --at, or --split"
        );
    }
}

#[test]
fn pane_placement_fit_is_serde_default_and_skipped_when_false() {
    // AC5-EDGE: a v79 payload with no fit key decodes fit == false, and a
    // false fit stays off the wire, so the floor does not move.
    let older: PanePlacement = serde_json::from_str(
            r#"{"target":"CurrentRoute","split":null,"here":false,"tab":null,"at":null,"fallback":"new_tab","max_panes":null,"thread_pane":false,"portal":null,"portal_new":false}"#,
        )
        .unwrap();
    assert!(!older.fit);
    let wire = serde_json::to_string(&PanePlacement {
        fit: false,
        ..Default::default()
    })
    .unwrap();
    assert!(
        !wire.contains("fit"),
        "false fit stays off the wire: {wire}"
    );
    let with_fit = serde_json::to_string(&PanePlacement {
        fit: true,
        ..Default::default()
    })
    .unwrap();
    assert!(with_fit.contains("\"fit\":true"));
}
