//! Integration tests for the loop-check session-binding gate (x-511e Change 1).
//!
//! Isolated `FNO_AGENTS_HOME` per test; the live graph store and the shared
//! registry are never opened. gh/git/fno are pointed at nonexistent paths so
//! the engine degrades without network when an owner case reaches it.

use fno_agents::loopcheck::run_loop_check_capture;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Points `FNO_AGENTS_HOME` at a fresh temp dir for one test and removes it
/// afterwards. A static mutex serializes the tests: the env var is
/// process-global and a concurrent test removing it mid-run would send the
/// gate at the LIVE machine registry, which these tests must never open.
struct HomeGuard(std::sync::MutexGuard<'static, ()>, TempDir);

impl HomeGuard {
    fn new() -> Self {
        static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let lock = ENV_LOCK.get_or_init(|| std::sync::Mutex::new(()));
        let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        let dir = TempDir::new().unwrap();
        std::env::set_var("FNO_AGENTS_HOME", dir.path());
        HomeGuard(guard, dir)
    }

    fn seed_registry(&self, rows: &[String]) {
        fs::create_dir_all(self.1.path()).unwrap();
        fs::write(
            self.1.path().join("registry.json"),
            format!(
                r#"{{"schema_version":{},"agents":[{}]}}"#,
                fno_agents::state::REGISTRY_SCHEMA_VERSION,
                rows.join(",")
            ),
        )
        .unwrap();
    }

    fn write_invalid_registry(&self) {
        fs::create_dir_all(self.1.path()).unwrap();
        fs::write(self.1.path().join("registry.json"), "not json at all {{{").unwrap();
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        std::env::remove_var("FNO_AGENTS_HOME");
    }
}

fn row_json(harness: &str, sid: &str, cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy().into_owned();
    format!(
        r#"{{"name":"w-{sid}","short_id":"w-{sid}","harness":"{harness}","harness_session_id":"{sid}","cwd":"{cwd}","log_path":"/tmp/{sid}.log","created_at":"2026-09-18T00:00:00Z","status":"live"}}"#
    )
}

fn row_json_with_node(harness: &str, sid: &str, cwd: &Path, node: &str) -> String {
    let mut row = row_json(harness, sid, cwd);
    row.insert(row.len() - 1, ',');
    row.insert_str(row.len() - 1, &format!(r#""node":"{node}""#));
    row
}

fn row_json_crowned(harness: &str, sid: &str, cwd: &Path) -> String {
    let mut row = row_json(harness, sid, cwd);
    row.insert(row.len() - 1, ',');
    row.insert_str(row.len() - 1, r#""crown_level":1"#);
    row
}

fn manifest(input_node: &str) -> String {
    format!(
        "---\nsession_id: fno-own\ncreated_at: 2026-09-18T00:00:00Z\nattended: true\ninput: \"{input_node}\"\n---\n"
    )
}

fn transcript(text: &str) -> String {
    format!("{{\"message\":{{\"role\":\"assistant\",\"content\":\"{text}\"}}}}\n")
}

fn base_args(state: &Path, transcript_path: &Path, cwd: &Path) -> Vec<String> {
    vec![
        "loop-check".into(),
        "--state".into(),
        state.to_string_lossy().into_owned(),
        "--transcript".into(),
        transcript_path.to_string_lossy().into_owned(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--gh-bin".into(),
        "/nonexistent-gh".into(),
        "--git-bin".into(),
        "/nonexistent-git".into(),
        "--fno-bin".into(),
        "/nonexistent-fno".into(),
    ]
}

fn with_binding(args: Vec<String>, sid: &str) -> Vec<String> {
    let mut args = args;
    args.push("--harness".into());
    args.push("opencode".into());
    args.push("--harness-session".into());
    args.push(sid.into());
    args
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn owner_session_proceeds_into_the_engine_unchanged() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[row_json("opencode", "ses_owner", cwd)]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest("x-511e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("no promise yet")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&manifest_path, &transcript_path, cwd),
        "ses_owner",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    // The gate let the owner through: whatever the engine decides with
    // nonexistent gh/git, it is the engine's own decision, not a refusal.
    assert_ne!(v["decision"], "refuse");
    assert!(code == 0 || code == 1);
    // A block names its continuation; an allow carries none.
    if v["decision"] == "block" {
        assert_eq!(v["continuation"], "/target --resume");
    } else {
        assert!(v.get("continuation").is_none());
    }
    let _ = &home;
}

#[test]
fn unrelated_session_is_refused_by_name_exit_zero() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[row_json_with_node("opencode", "ses_owner", cwd, "x-511e")]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest("x-511e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("no promise yet")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&manifest_path, &transcript_path, cwd),
        "ses_unrelated",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("ses_owner"),
        "refusal must name the bound session: {out}"
    );
    assert!(
        msg.contains("ses_unrelated"),
        "refusal must name the asking session: {out}"
    );
}

#[test]
fn absent_row_is_refused_not_headroom() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest("x-511e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&manifest_path, &transcript_path, cwd),
        "ses_stranger",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse");
    assert!(v["message"].as_str().unwrap().contains("no registry row"));
}

#[test]
fn unreadable_registry_is_refused_not_headroom() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.write_invalid_registry();

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest("x-511e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&manifest_path, &transcript_path, cwd),
        "ses_x",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse");
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("registry unreadable"));
}

#[test]
fn cwd_mismatch_is_refused_with_both_paths() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[row_json(
        "opencode",
        "ses_owner",
        Path::new("/tmp/other-project"),
    )]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest("x-511e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&manifest_path, &transcript_path, cwd),
        "ses_owner",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse");
    assert!(v["message"].as_str().unwrap().contains("cwd mismatch"));
}

#[test]
fn node_mismatch_is_refused_with_both_nodes() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[row_json_with_node("opencode", "ses_owner", cwd, "x-aaaa")]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest("x-bbbb")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&manifest_path, &transcript_path, cwd),
        "ses_owner",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse");
    assert!(v["message"].as_str().unwrap().contains("node mismatch"));
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("x-aaaa"), "must name the row node: {out}");
    assert!(msg.contains("x-bbbb"), "must name the manifest node: {out}");
}

#[test]
fn crowned_row_without_node_routes_to_the_king_path() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[row_json_crowned("opencode", "ses_king", cwd)]);

    // No target manifest at all: the king path answers on its own evidence.
    let missing = cwd.join("no-such-target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding(
        base_args(&missing, &transcript_path, cwd),
        "ses_king",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_ne!(v["decision"], "refuse");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("no king manifest"),
        "expected the king path's own allow, got: {out}"
    );
}

#[test]
fn flags_absent_gate_never_runs_and_engine_is_unchanged() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    // Registry seeded with a WRONG session on purpose: with no binding flags
    // the gate must not even read it, so the engine answers as it always has.
    home.seed_registry(&[row_json("claude", "ses_other", cwd)]);

    // No manifest at --state: the engine's long-standing missing-manifest
    // allow is the unchanged behavior this test pins.
    let missing = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&base_args(&missing, &transcript_path, cwd));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "allow");
    let _ = &home;
}
