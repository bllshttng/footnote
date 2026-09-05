//! The `fno_id` column's three states (x-b029). The node is the conflation:
//! `fno_id=-` covered both "not an fno pane" and "an fno pane whose id never
//! registered", and 14 of 24 dash rows on the measured mux were the second
//! kind, carrying a populated worker `name` one column over. These tests pin
//! the classification, including the self-contradicting row (a worker name
//! beside `-`) that a two-case fixture would pass against the old code.

use std::ffi::OsString;

use fno::mux_cli::{pane_identity_cell, parse_pane_args, session_id_shaped};
use fno::proto::PaneInfo;

fn pane(fno_id: Option<&str>, name: Option<&str>) -> PaneInfo {
    PaneInfo {
        pane_id: 46,
        squad_id: 0,
        squad_name: None,
        tab_id: 1,
        cwd: "/tmp".into(),
        child_pid: Some(40859),
        title: None,
        pristine_idle_shell: false,
        shell_idle: false,
        tab_name: None,
        tab_ordinal: None,
        fno_id: fno_id.map(str::to_string),
        orphaned_worker: false,
        harness_session_id: None,
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        name: name.map(str::to_string),
    }
}

#[test]
fn orphaned_worker_roundtrips_and_defaults_false() {
    // (v69) The field is additive: present it round-trips, absent it decodes
    // to false, so a v68 payload still reads.
    let mut p = pane(None, None);
    p.orphaned_worker = true;
    let back: PaneInfo = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert!(back.orphaned_worker);
    let mut stripped = serde_json::to_value(&p).unwrap();
    stripped.as_object_mut().unwrap().remove("orphaned_worker");
    let legacy: PaneInfo = serde_json::from_value(stripped).unwrap();
    assert!(!legacy.orphaned_worker);
}

#[test]
fn untracked_sentinel_means_no_fno_evidence_at_all() {
    let (state, cell) = pane_identity_cell(&pane(None, None));
    assert_eq!(state, "untracked");
    assert_eq!(cell, "-");
}

#[test]
fn spawned_name_without_id_never_shares_the_sentinel() {
    // The self-contradicting row this node is about: the same line printed
    // `fno_id=-` and `name=t-6021-roster-gate-gpt`. The spawn captured a
    // worker name, so the pane IS fno's and must not read as foreign.
    let (state, cell) = pane_identity_cell(&pane(None, Some("t-6021-roster-gate-gpt")));
    assert_eq!(state, "unresolved:spawned-name");
    assert!(cell.starts_with("unresolved:"), "{cell}");
    assert_ne!(cell, "-");
}

#[test]
fn worker_name_in_the_id_column_reads_unresolved_not_as_an_id() {
    // Live specimen: `fno_id=t-b783-verb-prefix-agy` beside two session UUIDs
    // in one listing. The column must not carry two kinds of identifier.
    let (state, cell) = pane_identity_cell(&pane(
        Some("t-b783-verb-prefix-agy"),
        Some("t-b783-verb-prefix-agy"),
    ));
    assert_eq!(state, "unresolved:name-as-id");
    assert!(cell.starts_with("unresolved:"), "{cell}");
    assert!(!cell.contains("t-b783"), "{cell}");
}

#[test]
fn claude_and_codex_session_uuids_resolve() {
    // A claude UUIDv4 and a codex UUIDv7 (hex, 8-4-4-4-12) are both session
    // ids; the resolved cell is the id itself.
    let claude = "119e3c52-a4b3-4f7e-8a1c-2d3e4f5a6b7c";
    let (state, cell) = pane_identity_cell(&pane(Some(claude), None));
    assert_eq!(state, "resolved");
    assert_eq!(cell, claude);
    let codex = "01a05fce-0000-7ccc-8000-000000000000";
    let (state, cell) = pane_identity_cell(&pane(Some(codex), Some("t-7e0b-mint-2")));
    assert_eq!(state, "resolved");
    assert_eq!(cell, codex);
}

#[test]
fn session_id_shape_rejects_names_and_short_ids() {
    assert!(session_id_shaped("01a05fce-1234-5678-9abc-def012345678"));
    assert!(session_id_shaped("119E3C52-A4B3-4F7E-8A1C-2D3E4F5A6B7C"));
    // opencode mints `ses_` + alphanumerics, a third session-id shape.
    assert!(session_id_shaped("ses_9f2AbZ01"));
    assert!(!session_id_shaped("ses_"));
    assert!(!session_id_shaped("ses_needs-no-dashes"));
    assert!(!session_id_shaped("t-6021-roster-gate-gpt"));
    assert!(!session_id_shaped("119e3c52"));
    assert!(!session_id_shaped(""));
}

#[test]
fn pane_help_says_identity_not_idleness() {
    // AC6: the help states what the column answers and names the verb that
    // answers idleness, so `pane ls` stops being read as a reuse decision.
    let help = parse_pane_args(&[OsString::from("--help")]).unwrap_err();
    assert!(help.contains("identity"), "{help}");
    assert!(help.contains("not idleness"), "{help}");
    assert!(help.contains("pane wait"), "{help}");
}
