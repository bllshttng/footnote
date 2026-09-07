//! The paneless route-hint test, extracted from the parent's inline
//! tests under the file-budget gate (the reseat verb lands in a sibling
//! child module and the parent stays shrink-only).
use super::*;

fn paneless_row(name: &str, attach: Option<&str>) -> crate::agents_view::RegistryAgent {
    paneless_row_with_harness(name, attach, None)
}

fn paneless_row_with_harness(
    name: &str,
    attach: Option<&str>,
    harness: Option<&str>,
) -> crate::agents_view::RegistryAgent {
    crate::agents_view::RegistryAgent {
        name: name.into(),
        cwd: "/tmp/seen".into(),
        attach_id: attach.map(str::to_owned),
        harness: harness.map(str::to_owned),
        ..Default::default()
    }
}

#[test]
fn paneless_route_hint_names_both_routes_for_a_drive_tier_row() {
    // AC9-HP: the exit-17 line a paneless row prints names the peek route
    // every row has AND the drive route a live attach-carrying row has -
    // never the bare "hosts no live pane" the incident hit. `where`,
    // `view`, and `focus` share this one builder, so one assertion covers
    // all three doors.
    let drive = paneless_route_hint("fno mux where", &paneless_row("t-live", Some("deadbee1")));
    assert!(drive.contains("fno agents peek t-live --follow"), "{drive}");
    assert!(drive.contains("fno agents attach t-live"), "{drive}");
    assert!(drive.contains("hosts no live pane"), "{drive}");

    // Follow tier (a peek-capable harness, no attach id): the peek route
    // only, still not the bare line.
    let follow = paneless_route_hint(
        "fno mux where",
        &paneless_row_with_harness("t-codex", None, Some("codex")),
    );
    assert!(
        follow.contains("fno agents peek t-codex --follow"),
        "{follow}"
    );
    assert!(!follow.contains("fno agents attach"), "{follow}");
    assert!(follow.contains("hosts no live pane"), "{follow}");

    // Locate tier (no attach id, no peek reader - e.g. gemini): peek
    // --follow is a route guaranteed to fail there, so the hint must
    // name attach instead, never peek.
    let locate = paneless_route_hint(
        "fno mux where",
        &paneless_row_with_harness("t-gemini", None, Some("gemini")),
    );
    assert!(locate.contains("fno agents attach t-gemini"), "{locate}");
    assert!(!locate.contains("--follow"), "{locate}");
    assert!(locate.contains("hosts no live pane"), "{locate}");
}

#[test]
fn paneless_route_hint_names_the_fno_id_state() {
    // (x-e763) The exit-17 line no longer collapses unresolved and absent
    // into one string: the row's fno_id_state rides in the line.
    let mut resolved = paneless_row("t-live", None);
    resolved.session_id = Some("46c2b4a1-6fe2-4d2a-ab0b-b992674f8148".into());
    let line = paneless_route_hint("fno mux where", &resolved);
    assert!(line.contains("(fno_id_state: resolved)"), "{line}");

    let spawned = paneless_row("t-live", None);
    let line = paneless_route_hint("fno mux where", &spawned);
    assert!(
        line.contains("(fno_id_state: unresolved:spawned-name)"),
        "{line}"
    );
}

#[test]
fn scan_panes_joins_a_stale_mux_field_to_its_live_pane() {
    // (x-e763) AC13's join: a registry row with no mux binding but a pane
    // table entry carrying its resolved id. The pane ls output the operator
    // screenshotted is the second input this join reads.
    use crate::server::lifecycle_target::scan_panes;
    let hit: proto::PaneInfo = serde_json::from_value(serde_json::json!({
        "pane_id": 1637,
        "squad_id": 0,
        "tab_id": 0,
        "cwd": "/tmp",
        "fno_id": "46c2b4a1-6fe2-4d2a-ab0b-b992674f8148",
        "name": "t-87fb-mux-notify"
    }))
    .unwrap();
    let panes = vec![hit];
    let row = paneless_row("t-87fb-mux-notify", None);
    let found = scan_panes(&panes, row.effective_identity(), &row.name);
    assert_eq!(found.map(|p| p.pane_id), Some(1637));
}
