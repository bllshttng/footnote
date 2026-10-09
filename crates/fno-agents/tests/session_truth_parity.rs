//! Golden parity: the native truth reader answers exactly what the Python
//! `resolve_session_truth` answered over the committed fixture transcripts,
//! field for field, with `now_s` pinned, on a cold read and again on warm
//! cursors. The golden set carries the unknown handle (AC1, AC4).
//!
//! parity-stage: differential
//! parity-oracle: fno.agents.session_truth.resolve_session_truth

use fno_agents::session_truth::cursor::TruthCursors;
use fno_agents::session_truth::{resolve_payload, Stores};
use fno_agents::state::RegistryEntry;
use serde_json::Value;

const NOW_S: f64 = 1791460800.0; // 2026-10-08T12:00:00Z, pinned in the goldens

fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session_truth")
}

fn goldens() -> Value {
    let path = fixtures_dir().join("goldens.json");
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}")),
    )
    .expect("goldens parse")
}

fn row_for(case: &str, golden: &Value) -> Option<RegistryEntry> {
    if golden["state"] == "unknown" && golden["reason"] == "not-found" {
        return None;
    }
    let mut row = RegistryEntry::default();
    row.name = case.to_string();
    let sid = golden["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    row.harness_session_id = Some(sid);
    row.cwd = "/fixture/project".to_string();
    if let Some(agent) = golden_handle_agent(case) {
        row.harness = Some(agent);
    }
    if case.starts_with("claude_") || case.starts_with("codex_") {
        row.transcript_path = Some(
            fixtures_dir()
                .join(format!("{case}.jsonl"))
                .to_string_lossy()
                .to_string(),
        );
    }
    Some(row)
}

fn golden_handle_agent(case: &str) -> Option<String> {
    match case {
        c if c.starts_with("claude_") => Some("claude".into()),
        c if c.starts_with("codex_") => Some("codex".into()),
        c if c.starts_with("opencode_") => Some("opencode".into()),
        _ => None,
    }
}

fn stores() -> Stores {
    Stores {
        projects_root: fixtures_dir().join("no-projects"),
        account_projects_roots: Vec::new(),
        codex_sessions_dir: None,
        opencode_db: fixtures_dir().join("opencode.db"),
    }
}

fn assert_matches(case: &str, payload: &Value, golden: &Value) {
    for key in [
        "handle",
        "state",
        "reason",
        "last_activity_age_s",
        "last_event_at",
        "last_activity_basis",
        "last_message",
        "provider_refusal",
        "session_id",
        "observed_model",
        "harness_title",
        "suggestions",
        "reachability",
        "basis",
        "falsifier_error",
    ] {
        assert_eq!(
            payload.get(key),
            golden.get(key),
            "{case}: key {key} diverged from the Python golden"
        );
    }
}

#[test]
fn every_fixture_answers_its_python_golden() {
    let goldens = goldens();
    let stores = stores();
    let mut cursors = TruthCursors::new();
    let cases = goldens["cases"].as_object().expect("cases object");
    assert!(cases.len() >= 10, "the fixture set is present");
    for (case, golden) in cases {
        let rows = row_for(case, golden).map(|r| vec![r]);
        let payload = resolve_payload(rows.as_deref(), case, NOW_S, &stores, &mut cursors);
        assert_matches(case, &payload, golden);

        // The incremental path (the daemon's steady state) reads the same
        // rows through warm cursors and must answer identically.
        if rows.is_some() {
            let warm = resolve_payload(rows.as_deref(), case, NOW_S, &stores, &mut cursors);
            assert_matches(case, &warm, golden);
        }
    }
}

#[test]
fn a_stamp_free_transcript_falls_back_to_mtime() {
    // No record carries a timestamp, so the age leg falls to the file stat
    // (basis `mtime`); the checkout-touched file reads fresh, so the row
    // answers `working`, never stalled (truth never falsely asserts
    // silence). AC parity for the non-`last-entry` basis word.
    let dir = std::env::temp_dir().join(format!("truth-parity-mtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("no-stamps.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"assistant","message":{"role":"assistant","content":"reading the ledger"}}"#,
            "\n"
        ),
    )
    .unwrap();
    let mut row = RegistryEntry::default();
    row.name = "claude_mtime_fallback".to_string();
    row.harness = Some("claude".into());
    row.harness_session_id = Some("sid-mtime-0001".into());
    row.cwd = "/fixture/project".to_string();
    row.transcript_path = Some(path.to_string_lossy().to_string());
    let rows = vec![row];
    let stores = stores();
    let mut cursors = TruthCursors::new();
    let payload = resolve_payload(
        Some(&rows),
        "claude_mtime_fallback",
        NOW_S,
        &stores,
        &mut cursors,
    );
    assert_eq!(payload["state"], "working");
    assert_eq!(payload["last_activity_basis"], "mtime");
    let _ = std::fs::remove_dir_all(&dir);
}
