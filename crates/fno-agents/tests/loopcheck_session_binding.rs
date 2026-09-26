//! Integration tests for the loop-check session-binding gate (x-511e Change 1).
//!
//! Isolated `FNO_AGENTS_HOME` per test; the live graph store and the shared
//! registry are never opened. gh/git/fno are pointed at nonexistent paths so
//! the engine degrades without network when an owner case reaches it.

use fno_agents::loopcheck::run_loop_check_capture;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Points `FNO_AGENTS_HOME` at a fresh temp dir for one test and removes it
/// afterwards. A static mutex serializes the tests: the env var is
/// process-global and a concurrent test removing it mid-run would send the
/// gate at the LIVE machine registry, which these tests must never open.
struct HomeGuard {
    // Held only for its Drop: while alive it serializes the tests against
    // the process-global FNO_AGENTS_HOME env var. No test reads the lock
    // itself, so the leading underscore (not #[allow(dead_code)]) is the
    // honest marker - a poisoned-lock guard genuinely carries no data.
    _lock: std::sync::MutexGuard<'static, ()>,
    dir: TempDir,
}

impl HomeGuard {
    fn new() -> Self {
        static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let lock = ENV_LOCK.get_or_init(|| std::sync::Mutex::new(()));
        let _lock = lock.lock().unwrap_or_else(|p| p.into_inner());
        let dir = TempDir::new().unwrap();
        std::env::set_var("FNO_AGENTS_HOME", dir.path());
        HomeGuard { _lock, dir }
    }

    fn seed_registry(&self, rows: &[String]) {
        fs::create_dir_all(self.dir.path()).unwrap();
        fs::write(
            self.dir.path().join("registry.json"),
            format!(
                r#"{{"schema_version":{},"agents":[{}]}}"#,
                fno_agents::state::REGISTRY_SCHEMA_VERSION,
                rows.join(",")
            ),
        )
        .unwrap();
    }

    fn write_invalid_registry(&self) {
        fs::create_dir_all(self.dir.path()).unwrap();
        fs::write(self.dir.path().join("registry.json"), "not json at all {{{").unwrap();
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
fn crowned_row_routes_a_real_drain_read_and_the_drain_answers() {
    // The Crown route runs the king's own reads under a fire stamped
    // driver=target. The drain reserve now arms on the shared king entry
    // (not the driver string), and the drain answers through a scripted
    // fno-bin after a real sleep - it is never the 250ms floor that made
    // this route's floor kills look like load flakes.
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[row_json_crowned("opencode", "ses_king", cwd)]);
    // A king manifest with a scope: the board reads in-process, then the
    // drain read shells to the scripted binary. created_at is NOW: a stale
    // stamp puts the crown past its 96h default span and the term gate
    // blocks before the drain read ever runs. FNO_HOME carries a seeded
    // graph store and FNO_CONFIG a workspace config: the scope read demands
    // an epic entry and a work.workspaces mapping, and neither may lean on
    // an ambient machine's graph or config.
    let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let king_md = cwd.join("king.md");
    fs::write(
        &king_md,
        format!("---\nfno_id: k-2440\ncreated_at: {created}\nscope: x-2440\n---\n"),
    )
    .unwrap();
    let graph_home = TempDir::new().unwrap();
    let graph = graph_home.path().join("graph.json");
    fno_agents::graph_store::seed_rows(&graph, &[
        json!({"id":"x-2440","slug":"x-2440","title":"scope","type":"epic","priority":"p1","status":"in_progress"})
    ]).unwrap();
    let config = graph_home.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "state_dir = {:?}\n[work.workspaces.test]\nprojects = [{{name = \"fno\"}}]\n",
            graph_home.path().to_string_lossy()
        ),
    )
    .unwrap();
    let saved_config = std::env::var_os("FNO_CONFIG");
    std::env::set_var("FNO_CONFIG", &config);
    std::env::set_var("FNO_HOME", graph_home.path());
    // The board's quiet verdict needs its own sources readable: fno-py and gh
    // answer empty queues from a stub dir on PATH. The scripted drain sleeps
    // 400ms, then answers: a bound at the old 250ms floor would have killed
    // it. It rides --fno-bin, so the PATH stubs never cover it.
    let bin = cwd.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for (name, body) in [
        ("fno-py", "#!/bin/sh\nprintf '[]\\n'\n"),
        ("gh", "#!/bin/sh\nprintf '[]\\n'\n"),
    ] {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let saved_py = std::env::var_os("FNO_PY");
    let saved_path = std::env::var_os("PATH");
    std::env::set_var("FNO_PY", bin.join("fno-py"));
    std::env::set_var(
        "PATH",
        std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
            &saved_path.clone().unwrap_or_default(),
        )))
        .unwrap(),
    );
    let stub = cwd.join("fno-stub.sh");
    fs::write(&stub, "#!/bin/sh\nsleep 0.4\necho '{\"undelivered\": 2}'\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();
    let mut args = base_args(&king_md, &transcript_path, cwd);
    args.push("--fno-bin".into());
    args.push(stub.to_string_lossy().into_owned());
    let (code, out) = run_loop_check_capture(&with_binding(args, "ses_king"));
    match saved_py {
        Some(v) => std::env::set_var("FNO_PY", v),
        None => std::env::remove_var("FNO_PY"),
    }
    if let Some(v) = saved_path {
        std::env::set_var("PATH", v);
    }
    match saved_config {
        Some(v) => std::env::set_var("FNO_CONFIG", v),
        None => std::env::remove_var("FNO_CONFIG"),
    }
    std::env::remove_var("FNO_HOME");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_ne!(v["decision"], "refuse");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("2 driven rows undelivered"),
        "expected the drain's own verdict through the Crown route, got: {out}"
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

// ---------------------------------------------------------------------------
// The manifest-owner arm and the per-harness continuation (pi)
// ---------------------------------------------------------------------------

/// A manifest that binds its own session the way init writes one: harness
/// and harness_session_id named, the manifest living inside its checkout.
fn manifest_bound(harness: &str, sid: &str, input_node: &str) -> String {
    format!(
        "---\nsession_id: fno-own\ncreated_at: 2026-09-18T00:00:00Z\nattended: true\nharness: {harness}\nharness_session_id: {sid}\ninput: \"{input_node}\"\n---\n"
    )
}

fn with_binding_harness(args: Vec<String>, harness: &str, sid: &str) -> Vec<String> {
    let mut args = args;
    args.push("--harness".into());
    args.push(harness.into());
    args.push("--harness-session".into());
    args.push(sid.into());
    args
}

#[test]
fn a_space_manifest_binds_through_its_recorded_owner_cwd() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    // The manifest lives under a DIFFERENT tree (the state space), the way
    // the state verb resolves it in production; it records its checkout.
    let space = TempDir::new().unwrap();
    home.seed_registry(&[]);
    let manifest_path = space.path().join("target-state.md");
    let mut m = manifest_bound("pi", "pi-sess-1", "x-715e");
    m.insert_str(
        m.find("---\n").map(|i| i + 4).unwrap_or(0),
        &format!("owner_cwd: \"{}\"\n", cwd.display()),
    );
    fs::write(&manifest_path, m).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("no promise yet")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding_harness(
        base_args(&manifest_path, &transcript_path, cwd),
        "pi",
        "pi-sess-1",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_ne!(
        v["decision"], "refuse",
        "the recorded checkout must bind: {out}"
    );
    assert_eq!(code, 0);
    let _ = &home;
}

#[test]
fn manifest_bound_pi_session_is_owner_and_gates_unchanged() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    // Empty registry: no row is ever keyed by a pi session id.
    home.seed_registry(&[]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest_bound("pi", "pi-sess-1", "x-715e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("no promise yet")).unwrap();

    let args = with_binding_harness(
        base_args(&manifest_path, &transcript_path, cwd),
        "pi",
        "pi-sess-1",
    );
    let (code, out) = run_loop_check_capture(&args);
    let v: Value = serde_json::from_str(&out).unwrap();
    // The manifest IS the binding: the engine decides exactly as it would
    // without the flags (same manifest, same transcript, same cwd).
    assert_ne!(v["decision"], "refuse", "{out}");
    let (bare_code, bare_out) =
        run_loop_check_capture(&base_args(&manifest_path, &transcript_path, cwd));
    let bare: Value = serde_json::from_str(&bare_out).unwrap();
    assert_eq!(v["decision"], bare["decision"], "{out} vs {bare_out}");
    assert_eq!(code, bare_code);
    let _ = &home;
}

#[test]
fn foreign_pi_session_is_refused_and_names_both_sides() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest_bound("pi", "pi-sess-1", "x-715e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    // A /new, a fork or a foreign session: any id the manifest does not name.
    let (code, out) = run_loop_check_capture(&with_binding_harness(
        base_args(&manifest_path, &transcript_path, cwd),
        "pi",
        "pi-sess-2",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("pi-sess-1"),
        "must name the bound session: {out}"
    );
    assert!(
        msg.contains("pi-sess-2"),
        "must name the asking session: {out}"
    );
    // A refusal prints no continuation.
    assert!(v.get("continuation").is_none(), "{out}");
    let _ = &home;
}

#[test]
fn a_manifest_row_for_another_harness_stays_refused() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[]);

    // The manifest binds a pi session; a claude ask cannot borrow it.
    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest_bound("pi", "pi-sess-1", "x-715e")).unwrap();
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("x")).unwrap();

    let (code, out) = run_loop_check_capture(&with_binding_harness(
        base_args(&manifest_path, &transcript_path, cwd),
        "claude",
        "pi-sess-1",
    ));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(code, 0);
    assert_eq!(v["decision"], "refuse", "{out}");
    let _ = &home;
}

#[test]
fn the_pi_continuation_renders_the_skill_command_form() {
    let home = HomeGuard::new();
    let cwd_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();
    home.seed_registry(&[]);

    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, manifest_bound("pi", "pi-sess-1", "x-715e")).unwrap();
    // A non-terminal message: the engine reads a block decision with a
    // continuation.
    let transcript_path = cwd.join("transcript.jsonl");
    fs::write(&transcript_path, transcript("still working, no promise")).unwrap();

    let args = with_binding_harness(
        base_args(&manifest_path, &transcript_path, cwd),
        "pi",
        "pi-sess-1",
    );
    let (_code, out) = run_loop_check_capture(&args);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["decision"], "block", "{out}");
    assert_eq!(v["continuation"], "/skill:target resume", "{out}");

    // The same ask without pi's flags keeps the legacy literal byte-for-byte
    // (the engine's unbound output, which the shim and the loop driver pin).
    let (_bare_code, bare_out) =
        run_loop_check_capture(&base_args(&manifest_path, &transcript_path, cwd));
    let bare: Value = serde_json::from_str(&bare_out).unwrap();
    assert_eq!(bare["decision"], "block", "{bare_out}");
    assert_eq!(bare["continuation"], "/target --resume", "{bare_out}");
    let _ = &home;
}
