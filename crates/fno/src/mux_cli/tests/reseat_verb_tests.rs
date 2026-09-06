//! The registry half of `fno mux thread reseat`: row resolution and the
//! mux-ref flip, pure or on a scratch registry, no server in the loop (the
//! topology half is covered by the server reseat tests).
use super::*;
use crate::mux_cli::reseat_verb::{clear_mux_refs, resolve_reseat_target, ReseatFail};

const REGISTRY: &str = r#"{
  "schema_version": 3,
  "agents": [
    {
      "name": "w1",
      "harness_session_id": "sess-w1-uuid",
      "status": "idle",
      "mux": {"session": "main", "pane_id": 7}
    },
    {
      "name": "w2",
      "harness_session_id": "sess-w2-uuid",
      "status": "idle"
    }
  ]
}"#;

fn scratch_registry(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("fno-reseat-verb-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("registry.json");
    std::fs::write(&path, REGISTRY).unwrap();
    path
}

#[test]
fn resolves_a_row_by_name_and_by_session_id() {
    let by_name = resolve_reseat_target("w1", REGISTRY).unwrap();
    assert_eq!(by_name.name, "w1");
    assert_eq!(by_name.session, "main");
    assert_eq!(by_name.pane, 7);
    let by_session = resolve_reseat_target("sess-w1-uuid", REGISTRY).unwrap();
    assert_eq!(by_session.pane, 7);
}

#[test]
fn a_thread_row_and_an_unknown_name_are_typed_refusals() {
    match resolve_reseat_target("w2", REGISTRY) {
        Err(ReseatFail::NotPaneHosted(name)) => assert_eq!(name, "w2"),
        other => panic!("expected NotPaneHosted, got {other:?}"),
    }
    match resolve_reseat_target("ghost", REGISTRY) {
        Err(ReseatFail::UnknownRow(token)) => assert_eq!(token, "ghost"),
        other => panic!("expected UnknownRow, got {other:?}"),
    }
    assert!(matches!(
        resolve_reseat_target("w1", "{ not json"),
        Err(ReseatFail::RegistryUnreadable(_))
    ));
}

#[test]
fn the_flip_clears_the_named_row_and_preserves_the_rest() {
    let path = scratch_registry("flip");
    let cleared = clear_mux_refs(&path, &|row| {
        row.get("name").and_then(|v| v.as_str()) == Some("w1")
    })
    .unwrap();
    assert_eq!(cleared, 1);
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(doc["agents"][0]["mux"].is_null());
    // The untouched row keeps its shape; unknown fields and the schema stamp
    // pass through: a raw round-trip, never a typed row.
    assert!(doc["agents"][1].get("mux").is_none());
    assert_eq!(doc["schema_version"], 3);
    assert_eq!(doc["agents"][0]["harness_session_id"], "sess-w1-uuid");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_flip_is_idempotent_and_matches_by_mux_pair() {
    let path = scratch_registry("pair");
    let pair = |row: &serde_json::Value| {
        row.get("mux")
            .and_then(|m| m.get("session").and_then(|v| v.as_str()))
            == Some("main")
            && row
                .get("mux")
                .and_then(|m| m.get("pane_id").and_then(|v| v.as_u64()))
                == Some(7)
    };
    assert_eq!(clear_mux_refs(&path, &pair).unwrap(), 1);
    // A re-run after the move answers zero, not an error: both halves of the
    // reseat converge.
    assert_eq!(clear_mux_refs(&path, &pair).unwrap(), 0);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn help_answers_ok_and_bad_args_refuse_with_usage() {
    assert_eq!(reseat(&[OsString::from("--help")], None), EXIT_OK);
    assert_eq!(reseat(&[OsString::from("-h")], None), EXIT_OK);
    assert_eq!(reseat(&[], None), EXIT_USAGE);
    assert_eq!(reseat(&[OsString::from("--nope")], None), EXIT_USAGE);
}
