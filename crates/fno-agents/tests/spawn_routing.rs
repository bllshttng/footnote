//! Tests for the new spawn routing and ask-no-create behavior (Task 1.3a).
//!
//! Covers:
//! - ask with unknown name -> exit 16 + unknown-agent stderr (all 3 providers)
//! - dispatch_claude_spawn: collision check, receipt byte shape
//! - dispatch_codex_once / dispatch_gemini_once: teardown, collision, create failure

use fno_agents::paths::AgentsHome;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

// Serialize tests that mutate PATH so they don't race.
static PATH_MUTEX: Mutex<()> = Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-spawn-routing-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Seed registry.json with a single entry (any provider).
fn seed_registry(home: &AgentsHome, name: &str, provider: &str) {
    let session_field = if provider == "claude" {
        r#","short_id":"deadbeef""#.to_string()
    } else if provider == "codex" {
        r#","codex_session_id":"sess-codex-001""#.to_string()
    } else {
        r#","gemini_session_id":"sess-gemini-001""#.to_string()
    };
    let body = format!(
        r#"{{"schema_version":3,"agents":[{{"name":"{name}","provider":"{provider}","cwd":"/tmp","status":"live","created_at":"2026-06-06T00:00:00Z","log_path":null{session_field}}}]}}"#
    );
    let path = home.registry_json();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// Install a fake claude binary that simulates `claude --bg`.
fn install_fake_claude(bin_dir: &Path) {
    let script = r#"#!/bin/sh
name=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--name" ]; then name="$a"; fi
  prev="$a"
done
printf 'backgrounded · 7c5dcf5d · %s\n' "$name"
exit 0
"#;
    let path = bin_dir.join("claude");
    fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Install a fake codex binary that emits a one-shot JSONL session.
fn install_fake_codex(bin_dir: &Path, session_id: &str, reply: &str) {
    let script = format!(
        r#"#!/bin/sh
printf '{{"type":"thread.started","thread_id":"{session_id}"}}\n'
printf '{{"type":"item.completed","item":{{"type":"agent_message","text":"{reply}"}}}}\n'
printf '{{"type":"turn.completed"}}\n'
"#
    );
    let path = bin_dir.join("codex");
    fs::write(&path, &script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Install a fake gemini binary that emits a one-shot JSON response.
///
/// gemini_ask::parse_response expects `{"session_id":..., "response":..., "stats":{}}`.
fn install_fake_gemini(bin_dir: &Path, reply: &str) {
    let script = format!(
        r#"#!/bin/sh
printf '{{"session_id":"gemini-sess-001","response":"{reply}","stats":{{}}}}\n'
"#
    );
    let path = bin_dir.join("gemini");
    fs::write(&path, &script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn path_with(bin_dir: &Path) -> String {
    format!("{}:/usr/bin:/bin", bin_dir.display())
}

// ---------------------------------------------------------------------------
// AC1-HP: ask with unknown name exits 16 (claude path)
// ---------------------------------------------------------------------------

#[test]
fn ask_unknown_name_claude_exits_16() {
    use fno_agents::claude_ask::{dispatch_claude_ask, ClaudeHome};

    let home = AgentsHome::at(tmpdir("ask-unk-cl-home"));
    let ch = ClaudeHome::at(tmpdir("ask-unk-cl-claude"));
    let cwd = tmpdir("ask-unk-cl-cwd");

    // Empty registry: name "ghost" does not exist.
    let out = dispatch_claude_ask(
        &home,
        &ch,
        "ghost",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(3)),
        &[],
    );

    assert_eq!(
        out.exit_code, 16,
        "expected exit 16 for unknown-name ask, got {} (stderr: {})",
        out.exit_code, out.stderr
    );
    assert!(
        out.stderr.contains("unknown agent"),
        "stderr must mention 'unknown agent': {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("ghost"),
        "stderr must mention the agent name: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("spawn"),
        "stderr must mention 'spawn': {}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "stdout must be empty on error");
}

// ---------------------------------------------------------------------------
// AC1-HP: ask with unknown name exits 16 (codex path)
// ---------------------------------------------------------------------------

#[test]
fn ask_unknown_name_codex_exits_16() {
    use fno_agents::codex_ask::dispatch_codex_ask;

    let home = AgentsHome::at(tmpdir("ask-unk-cx-home"));
    let cwd = tmpdir("ask-unk-cx-cwd");

    // Empty registry: name "ghost-codex" does not exist.
    let out = dispatch_codex_ask(
        &home,
        "ghost-codex",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(3)),
    );

    assert_eq!(
        out.exit_code, 16,
        "expected exit 16 for unknown-name codex ask, got {} (stderr: {})",
        out.exit_code, out.stderr
    );
    assert!(
        out.stderr.contains("unknown agent"),
        "stderr must mention 'unknown agent': {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("ghost-codex"),
        "stderr must mention the agent name: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("spawn"),
        "stderr must mention 'spawn': {}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "stdout must be empty on error");
}

// ---------------------------------------------------------------------------
// AC1-HP: ask with unknown name exits 16 (gemini path)
// ---------------------------------------------------------------------------

#[test]
fn ask_unknown_name_gemini_exits_16() {
    use fno_agents::gemini_ask::dispatch_gemini_ask;

    let home = AgentsHome::at(tmpdir("ask-unk-gm-home"));
    let cwd = tmpdir("ask-unk-gm-cwd");

    // Empty registry: name "ghost-gemini" does not exist.
    let out = dispatch_gemini_ask(
        &home,
        "ghost-gemini",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(3)),
    );

    assert_eq!(
        out.exit_code, 16,
        "expected exit 16 for unknown-name gemini ask, got {} (stderr: {})",
        out.exit_code, out.stderr
    );
    assert!(
        out.stderr.contains("unknown agent"),
        "stderr must mention 'unknown agent': {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("ghost-gemini"),
        "stderr must mention the agent name: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("spawn"),
        "stderr must mention 'spawn': {}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "stdout must be empty on error");
}

// ---------------------------------------------------------------------------
// AC2-HP: unknown-name error message is byte-parity with Python
// ---------------------------------------------------------------------------

#[test]
fn ask_unknown_name_stderr_byte_parity() {
    use fno_agents::claude_ask::{dispatch_claude_ask, ClaudeHome};

    let home = AgentsHome::at(tmpdir("ask-parity-home"));
    let ch = ClaudeHome::at(tmpdir("ask-parity-claude"));
    let cwd = tmpdir("ask-parity-cwd");

    let out = dispatch_claude_ask(
        &home,
        &ch,
        "myagent",
        "msg",
        "abilities",
        &cwd,
        false,
        None,
        &[],
    );

    // Python: f"unknown agent {name!r}; spawn it first: fno agents spawn {name} -p <provider>"
    // Rust must match this exactly (py_repr wraps in single quotes).
    let expected =
        "unknown agent 'myagent'; spawn it first: fno agents spawn myagent -p <provider>\n";
    assert_eq!(
        out.stderr, expected,
        "stderr must be byte-for-byte parity with Python"
    );
}

// ---------------------------------------------------------------------------
// AC2-HP: ask still works for KNOWN agents (registry row exists)
// ---------------------------------------------------------------------------

#[test]
fn ask_known_agent_still_routes_to_followup() {
    use fno_agents::claude_ask::{dispatch_claude_ask, ClaudeHome};

    let home = AgentsHome::at(tmpdir("ask-known-home"));
    let ch = ClaudeHome::at(tmpdir("ask-known-claude"));
    let cwd = tmpdir("ask-known-cwd");

    // Seed a known claude agent.
    seed_registry(&home, "alice", "claude");

    // dispatch_claude_ask should attempt followup (not exit 16).
    // It will fail because there's no live socket, but exit code
    // will NOT be 16 (unknown-name).
    let out = dispatch_claude_ask(
        &home,
        &ch,
        "alice",
        "hi",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_millis(200)),
        &[],
    );

    assert_ne!(
        out.exit_code, 16,
        "a known agent must not get exit 16: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// AC3-HP: dispatch_claude_spawn - happy path, receipt byte shape
// ---------------------------------------------------------------------------

#[test]
fn spawn_claude_receipt_byte_shape() {
    use fno_agents::claude_ask::{dispatch_claude_spawn, ClaudeHome};

    let home = AgentsHome::at(tmpdir("spawn-cl-home"));
    let ch = ClaudeHome::at(tmpdir("spawn-cl-claude"));
    let cwd = tmpdir("spawn-cl-cwd");
    let bin = tmpdir("spawn-cl-bin");
    install_fake_claude(&bin);
    let path = path_with(&bin);

    let out = dispatch_claude_spawn(
        &home,
        &ch,
        "myspawn",
        "hello",
        "abilities",
        &cwd,
        false,
        None,
        &[("PATH", path.as_str())],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
        false, // surface_cwd: explicit --cwd, no default move (x-85fe)
    );

    assert_eq!(
        out.exit_code, 0,
        "spawn claude happy path should exit 0, stderr: {}",
        out.stderr
    );
    // Receipt: {"name": "<name>", "short_id": "<8hex>", "provider": "claude", "status": "live"}\n
    let receipt = out.stdout.trim_end_matches('\n');
    assert!(
        receipt.starts_with(r#"{"name": "myspawn", "short_id": "#),
        "receipt must start with name/short_id fields: {}",
        out.stdout
    );
    assert!(
        receipt.ends_with(r#""provider": "claude", "status": "live"}"#),
        "receipt must end with provider/status: {}",
        out.stdout
    );
    // Parse as JSON to verify structure.
    let v: serde_json::Value = serde_json::from_str(receipt).expect("receipt must be valid JSON");
    assert_eq!(v["name"], "myspawn");
    assert_eq!(v["provider"], "claude");
    assert_eq!(v["status"], "live");
    let short_id = v["short_id"].as_str().unwrap();
    assert_eq!(
        short_id.len(),
        8,
        "short_id must be 8 hex chars: {}",
        short_id
    );
    assert!(
        short_id.chars().all(|c| c.is_ascii_hexdigit()),
        "short_id must be hex: {}",
        short_id
    );
}

// x-85fe: surface_cwd=true appends the effective cwd as the LAST key (AC1-HP /
// AC1-UI). Byte-parity with the Python cmd_spawn receipt; the unmoved receipt
// (surface_cwd=false) is proven byte-identical by spawn_claude_receipt_byte_shape.
#[test]
fn spawn_claude_receipt_surfaces_moved_cwd() {
    use fno_agents::claude_ask::{dispatch_claude_spawn, ClaudeHome};

    let home = AgentsHome::at(tmpdir("spawn-mv-home"));
    let ch = ClaudeHome::at(tmpdir("spawn-mv-claude"));
    let cwd = tmpdir("spawn-mv-cwd");
    let bin = tmpdir("spawn-mv-bin");
    install_fake_claude(&bin);
    let path = path_with(&bin);

    let out = dispatch_claude_spawn(
        &home,
        &ch,
        "movedspawn",
        "hello",
        "abilities",
        &cwd,
        false,
        None,
        &[("PATH", path.as_str())],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
        true, // surface_cwd: the default canonical move
    );

    assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
    let receipt = out.stdout.trim_end_matches('\n');
    // cwd is the LAST key.
    assert!(
        receipt.ends_with(&format!(r#""cwd": "{}"}}"#, cwd.display())),
        "receipt must end with the cwd field: {}",
        out.stdout
    );
    let v: serde_json::Value = serde_json::from_str(receipt).expect("receipt must be valid JSON");
    assert_eq!(v["cwd"], cwd.display().to_string());
    assert_eq!(v["status"], "live");
}

// ---------------------------------------------------------------------------
// AC3-ERR: dispatch_claude_spawn - collision check (name already exists)
// ---------------------------------------------------------------------------

#[test]
fn spawn_claude_collision_exits_2() {
    use fno_agents::claude_ask::{dispatch_claude_spawn, ClaudeHome};

    let home = AgentsHome::at(tmpdir("spawn-cl-coll-home"));
    let ch = ClaudeHome::at(tmpdir("spawn-cl-coll-claude"));
    let cwd = tmpdir("spawn-cl-coll-cwd");

    // Seed an existing agent.
    seed_registry(&home, "existing", "claude");

    let out = dispatch_claude_spawn(
        &home,
        &ch,
        "existing",
        "hello",
        "abilities",
        &cwd,
        false,
        None,
        &[],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
        false, // surface_cwd: explicit --cwd, no default move (x-85fe)
    );

    assert_eq!(
        out.exit_code, 2,
        "collision must exit 2, got {}: {}",
        out.exit_code, out.stderr
    );
    assert!(
        out.stderr.contains("already exists"),
        "stderr must say agent already exists: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("existing"),
        "stderr must mention the agent name: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// AC4-HP: dispatch_codex_once - happy path (create + reply + teardown)
// PATH is injected via set_var under the mutex (same pattern as
// codex_ask_dispatch.rs tests).
// ---------------------------------------------------------------------------

#[test]
fn spawn_codex_once_happy_path() {
    use fno_agents::codex_ask::dispatch_codex_once;

    let home = AgentsHome::at(tmpdir("once-cx-home"));
    let cwd = tmpdir("once-cx-cwd");
    let bin = tmpdir("once-cx-bin");
    install_fake_codex(&bin, "aaaa1111-0000-0000-0000-000000000001", "once reply");

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let old_path = std::env::var_os("PATH");
    unsafe { std::env::set_var("PATH", path_with(&bin)) };

    let out = dispatch_codex_once(
        &home,
        "ephemeral-codex",
        "do something once",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        None,
        None,
        None,
    );

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }

    assert_eq!(
        out.exit_code, 0,
        "once happy path must exit 0, stderr: {}",
        out.stderr
    );
    assert_eq!(
        out.stdout, "once reply",
        "stdout must be the reply verbatim"
    );
    // Teardown receipt on stderr.
    assert!(
        out.stderr.contains("once:"),
        "teardown receipt must start with 'once:': {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("ephemeral-codex"),
        "teardown receipt must contain agent name: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("codex"),
        "teardown receipt must mention provider: {}",
        out.stderr
    );

    // Registry row must be removed after teardown.
    let registry_path = home.registry_json();
    if registry_path.exists() {
        let body = fs::read_to_string(&registry_path).unwrap();
        assert!(
            !body.contains("ephemeral-codex"),
            "registry row must be torn down: {}",
            body
        );
    }
}

// ---------------------------------------------------------------------------
// AC4-ERR: dispatch_codex_once - collision check (name already exists)
// ---------------------------------------------------------------------------

#[test]
fn spawn_codex_once_collision_exits_2() {
    use fno_agents::codex_ask::dispatch_codex_once;

    let home = AgentsHome::at(tmpdir("once-cx-coll-home"));
    let cwd = tmpdir("once-cx-coll-cwd");

    // Seed a pre-existing agent.
    seed_registry(&home, "taken", "codex");

    let out = dispatch_codex_once(
        &home,
        "taken",
        "msg",
        "abilities",
        &cwd,
        false,
        None,
        None,
        None,
        None,
    );

    assert_eq!(out.exit_code, 2, "collision must exit 2: {}", out.stderr);
    assert!(
        out.stderr.contains("already exists"),
        "stderr must say already exists: {}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// AC4-ERR: dispatch_codex_once - create failure = no registry row written
// ---------------------------------------------------------------------------

#[test]
fn spawn_codex_once_create_failure_no_registry_entry() {
    use fno_agents::codex_ask::dispatch_codex_once;

    let home = AgentsHome::at(tmpdir("once-cx-fail-home"));
    let cwd = tmpdir("once-cx-fail-cwd");

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let old_path = std::env::var_os("PATH");
    // Empty PATH so codex binary is not found -> exit 14.
    unsafe { std::env::set_var("PATH", "/nonexistent-bin-dir-spawn-routing") };

    let out = dispatch_codex_once(
        &home,
        "will-fail",
        "msg",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(3)),
        None,
        None,
        None,
    );

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }

    // create failed, so no registry row should have been written.
    let registry_path = home.registry_json();
    if registry_path.exists() {
        let body = fs::read_to_string(&registry_path).unwrap();
        assert!(
            !body.contains("will-fail"),
            "no registry entry should exist after create failure: {}",
            body
        );
    }
    assert_ne!(out.exit_code, 0, "create failure must not exit 0");
}

// ---------------------------------------------------------------------------
// AC5-HP: dispatch_gemini_once - happy path (create + reply + teardown)
// ---------------------------------------------------------------------------

#[test]
fn spawn_gemini_once_happy_path() {
    use fno_agents::gemini_ask::dispatch_gemini_once;

    let home = AgentsHome::at(tmpdir("once-gm-home"));
    let cwd = tmpdir("once-gm-cwd");
    let bin = tmpdir("once-gm-bin");
    install_fake_gemini(&bin, "once gemini reply");

    let _guard = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let old_path = std::env::var_os("PATH");
    unsafe { std::env::set_var("PATH", path_with(&bin)) };

    let out = dispatch_gemini_once(
        &home,
        "ephemeral-gemini",
        "do something once",
        "abilities",
        &cwd,
        false,
        Some(Duration::from_secs(10)),
        None,
    );

    match old_path {
        Some(p) => unsafe { std::env::set_var("PATH", p) },
        None => unsafe { std::env::remove_var("PATH") },
    }

    assert_eq!(
        out.exit_code, 0,
        "once happy path must exit 0, stderr: {}",
        out.stderr
    );
    // stdout = reply verbatim.
    assert!(!out.stdout.is_empty(), "stdout must contain the reply");
    // Teardown receipt on stderr.
    assert!(
        out.stderr.contains("once:"),
        "teardown receipt must contain 'once:': {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("gemini"),
        "teardown receipt must mention provider: {}",
        out.stderr
    );

    // Registry row must be removed after teardown.
    let registry_path = home.registry_json();
    if registry_path.exists() {
        let body = fs::read_to_string(&registry_path).unwrap();
        assert!(
            !body.contains("ephemeral-gemini"),
            "registry row must be torn down: {}",
            body
        );
    }
}

// ---------------------------------------------------------------------------
// Client binary integration tests: verify client.rs routing wiring.
// These exercise the thin shell in bin/client.rs that delegates to the lib.
// ---------------------------------------------------------------------------

/// Locate the fno-agents client binary from the build output dir.
fn find_client_bin() -> std::path::PathBuf {
    // `cargo test` sets CARGO_MANIFEST_DIR; binary lands under target/debug.
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // Walk up to workspace root (fno/crates/fno-agents -> abilities/)
    let target = manifest
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target")
        .join("debug")
        .join("fno-agents");
    if target.exists() {
        return target;
    }
    // Fallback: check from manifest dir directly.
    let direct = manifest.join("target").join("debug").join("fno-agents");
    if direct.exists() {
        return direct;
    }
    // Last resort: rely on PATH (headless CI).
    std::path::PathBuf::from("fno-agents")
}

/// AC6-CLIENT: `ask` on an unknown name must exit 16 at the client level
/// (client.rs unknown-name pre-check, Task 1.3a).
#[test]
fn client_ask_unknown_name_exits_16() {
    let home_dir = tmpdir("cli-ask-unk-home");
    let bin = find_client_bin();
    if !bin.exists() {
        // Skip if binary not built yet (will be built by the time this runs
        // in CI via `cargo test`; in a source-only check this is expected).
        eprintln!(
            "skipping client_ask_unknown_name_exits_16: binary not found at {:?}",
            bin
        );
        return;
    }

    let out = std::process::Command::new(&bin)
        .args(["ask", "ghost-client", "hello"])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .output()
        .expect("failed to run fno-agents");

    assert_eq!(
        out.status.code(),
        Some(16),
        "client ask unknown name must exit 16, got {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown agent"),
        "stderr must mention 'unknown agent': {}",
        stderr
    );
    assert!(
        stderr.contains("spawn"),
        "stderr must mention 'spawn': {}",
        stderr
    );
}

/// x-2c27: `spawn --once` is the back-compat alias for `--substrate headless`.
/// For claude that is the `claude -p` one-shot lane - NOT the `--bg` thread.
/// It must NOT exit 2, must NOT print the old "--once not supported" message,
/// and must NOT emit the bg JSON `"short_id"` receipt (which would prove it
/// hit the bg lane instead of the headless passthrough).
#[test]
fn client_spawn_once_claude_is_headless_p_lane_not_bg() {
    let _guard = PATH_MUTEX.lock().unwrap();
    let home_dir = tmpdir("cli-spawn-once-claude-home");
    let bin_dir = tmpdir("cli-spawn-once-claude-bin");
    let cwd = tmpdir("cli-spawn-once-claude-cwd");
    install_fake_claude(&bin_dir);
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_once_claude_is_headless_p_lane_not_bg: binary not found at {:?}",
            bin
        );
        return;
    }

    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "myagent",
            "hello",
            "--provider",
            "claude",
            "--once",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .env("PATH", path_with(&bin_dir))
        .current_dir(&cwd)
        .output()
        .expect("failed to run fno-agents");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(2),
        "claude --once (headless) must not exit 2; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("not supported"),
        "claude --once must not print the old 'not supported' message: {stderr}"
    );
    // The headless `claude -p` lane returns the subprocess output verbatim; it
    // does NOT wrap it in the bg JSON receipt. Absence of `"short_id"` proves we
    // took the -p lane, not the --bg lane.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("\"short_id\""),
        "claude --once (headless) must NOT emit the bg JSON receipt; stdout: {stdout} stderr: {stderr}"
    );
}

/// x-2c27 (codex P2): the claude headless lane honors `--timeout` - a hung
/// `claude -p` is SIGKILLed past the deadline and reported as exit 124, not an
/// indefinite wedge.
#[test]
fn client_spawn_headless_claude_honors_timeout() {
    let _guard = PATH_MUTEX.lock().unwrap();
    let home_dir = tmpdir("cli-spawn-hl-to-home");
    let bin_dir = tmpdir("cli-spawn-hl-to-bin");
    let cwd = tmpdir("cli-spawn-hl-to-cwd");
    // A fake claude that hangs well past the 1s timeout.
    {
        use std::os::unix::fs::PermissionsExt;
        let path = bin_dir.join("claude");
        fs::write(&path, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!("skipping client_spawn_headless_claude_honors_timeout: binary not found");
        return;
    }

    let start = std::time::Instant::now();
    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "wk",
            "hello",
            "--provider",
            "claude",
            "--substrate",
            "headless",
            "--timeout",
            "1",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .env("PATH", path_with(&bin_dir))
        .current_dir(&cwd)
        .output()
        .expect("failed to run fno-agents");
    let elapsed = start.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(124),
        "headless timeout must exit 124; stderr: {stderr}"
    );
    assert!(
        stderr.contains("timed out"),
        "stderr must name the timeout: {stderr}"
    );
    assert!(
        elapsed.as_secs() < 10,
        "must not wait the full 30s sleep; took {elapsed:?}"
    );
}

/// x-2c27: `bg` is claude-only. `--substrate bg --provider codex` must hard-error
/// (exit 2) pointing to headless, never silently fall to another substrate.
#[test]
fn client_spawn_substrate_bg_codex_hard_errors() {
    let home_dir = tmpdir("cli-spawn-bg-codex-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!("skipping client_spawn_substrate_bg_codex_hard_errors: binary not found");
        return;
    }

    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "myagent",
            "hello",
            "--provider",
            "codex",
            "--substrate",
            "bg",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .output()
        .expect("failed to run fno-agents");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "codex --substrate bg must exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("claude-only") && stderr.contains("headless"),
        "codex --substrate bg error must name the claude-only constraint and headless: {stderr}"
    );
}

/// x-dfa4: --permission-mode on a NON-claude bg/headless lane is fail-closed
/// (exit 2, pointing at --substrate pane); the one-shot lanes hardcode their own
/// bypass form and can't honor a mapped mode without a silent downgrade.
#[test]
fn client_spawn_permission_mode_codex_headless_fails_closed() {
    let home_dir = tmpdir("cli-spawn-perm-codex-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_permission_mode_codex_headless_fails_closed: binary not found"
        );
        return;
    }
    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "myagent",
            "hi",
            "--provider",
            "codex",
            "--substrate",
            "headless",
            "--permission-mode",
            "acceptEdits",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1")
        .env("FNO_AGENTS_HOME", &home_dir)
        .output()
        .expect("failed to run fno-agents");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "must exit 2; stderr: {stderr}");
    assert!(
        stderr.contains("permission-mode") && stderr.contains("pane"),
        "error must name --permission-mode and point at pane: {stderr}"
    );
}

/// x-dfa4: --permission-mode and --yolo are one knob at a time on the bg lane
/// too (the pane lane enforces this in Python).
#[test]
fn client_spawn_permission_mode_and_yolo_mutually_exclusive() {
    let home_dir = tmpdir("cli-spawn-perm-mutex-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_permission_mode_and_yolo_mutually_exclusive: binary not found"
        );
        return;
    }
    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "myagent",
            "hi",
            "--provider",
            "claude",
            "--substrate",
            "bg",
            "--yolo",
            "--permission-mode",
            "plan",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1")
        .env("FNO_AGENTS_HOME", &home_dir)
        .output()
        .expect("failed to run fno-agents");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "must exit 2; stderr: {stderr}");
    assert!(
        stderr.contains("mutually exclusive"),
        "error must name the mutual exclusion: {stderr}"
    );
}

/// x-567d: opencode headless is now WIRED (was exit-2 "not wired", x-51f6).
/// `--substrate headless --provider opencode` invokes the `opencode run`
/// one-shot. With no `opencode` on PATH the dispatch surfaces "binary not
/// found" (exit 13) — the proof it reached run_opencode instead of the retired
/// refusal — never exit 2. PATH is isolated so the assertion is deterministic
/// on a dev machine that happens to have opencode installed (no real run).
#[test]
fn client_spawn_substrate_headless_opencode_is_wired() {
    let home_dir = tmpdir("cli-spawn-headless-opencode-home");
    let empty_path = tmpdir("cli-spawn-headless-opencode-emptypath");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!("skipping client_spawn_substrate_headless_opencode_is_wired: binary not found");
        return;
    }

    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "myagent",
            "hello",
            "--provider",
            "opencode",
            "--substrate",
            "headless",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .env("PATH", &empty_path) // isolate: `opencode` is deterministically absent
        .output()
        .expect("failed to run fno-agents");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code(),
        Some(2),
        "opencode --substrate headless must no longer exit 2 'not wired'; stderr: {stderr}"
    );
    assert!(
        stderr.contains("not found"),
        "opencode --substrate headless with no opencode on PATH must surface 'binary not found' \
         (proof it reached run_opencode, i.e. wired): {stderr}"
    );
}

/// x-567d: `--substrate bg --provider opencode` still hard-errors (bg is
/// claude-only) but now points at `--substrate headless` (the generic bg arm)
/// - opencode's headless one-shot is wired, so that advice is no longer the
/// dead end the x-51f6 special-case pane arm guarded against.
#[test]
fn client_spawn_substrate_bg_opencode_hard_errors_pointing_to_headless() {
    let home_dir = tmpdir("cli-spawn-bg-opencode-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_substrate_bg_opencode_hard_errors_pointing_to_headless: binary not found"
        );
        return;
    }

    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "myagent",
            "hello",
            "--provider",
            "opencode",
            "--substrate",
            "bg",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .output()
        .expect("failed to run fno-agents");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "opencode --substrate bg must exit 2 (bg is claude-only); stderr: {stderr}"
    );
    assert!(
        stderr.contains("headless"),
        "opencode --substrate bg error must now point to --substrate headless \
         (the wired one-shot, no longer a dead end): {stderr}"
    );
}

/// x-9112: `spawn --substrate bg` without --provider no longer exits 2 with a
/// "provider is required" error - it INFERS the invoking harness, mirroring
/// Python's resolve_dispatch_provider (and the pane arm). Proven deterministically
/// here via a single CODEX_SESSION_ID marker: inference resolves codex, which then
/// hits the bg-is-claude-only guard (exit 2, but for the claude-only reason, not a
/// missing-provider one). Other harness markers are removed so the ambient session
/// running the test suite can't make the env ambiguous.
#[test]
fn client_spawn_bg_no_provider_infers_harness() {
    let home_dir = tmpdir("cli-spawn-noprov-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_bg_no_provider_infers_harness: binary not found at {:?}",
            bin
        );
        return;
    }

    let out = std::process::Command::new(&bin)
        .args(["spawn", "myagent", "hello", "--substrate", "bg"])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .env("CODEX_SESSION_ID", "test-sid") // exactly one marker -> codex inferred
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("GEMINI_SESSION_ID")
        .output()
        .expect("failed to run fno-agents");

    let stderr = String::from_utf8_lossy(&out.stderr);
    // Inference landed on codex -> the bg-claude-only guard, NOT the old
    // "provider is required" rejection.
    assert!(
        stderr.contains("claude-only") && !stderr.contains("provider is required"),
        "missing --provider on bg must infer the harness (codex here), not reject: {stderr}"
    );
}

/// Pane parity with the optional-provider Python resolver: `spawn` on the pane
/// substrate (the default) without --provider is NO LONGER a client-side exit 2.
/// It falls through to the Python re-exec, which resolves the provider from the
/// invoking harness. With no `fno` front door on PATH the re-exec fails (127),
/// but the point is the client no longer rejects a missing provider on this arm.
#[test]
fn client_spawn_pane_no_provider_falls_through() {
    let home_dir = tmpdir("cli-spawn-pane-noprov-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_pane_no_provider_falls_through: binary not found at {:?}",
            bin
        );
        return;
    }

    let out = std::process::Command::new(&bin)
        .args(["spawn", "myagent", "hello"]) // pane is the default substrate
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        // Ensure the `fno` front door cannot be found, so the re-exec fails
        // deterministically (127) rather than launching a real pane.
        .env("PATH", "/nonexistent")
        .output()
        .expect("failed to run fno-agents");

    assert_ne!(
        out.status.code(),
        Some(2),
        "pane spawn without --provider must NOT be a client-side exit 2; got {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("provider is required to spawn a new agent"),
        "pane arm must no longer emit the client-side provider-required error: {}",
        stderr
    );
}

/// Q1 (sigma-review): the HAPPY PATH of the CLIENT-SIDE claude `--bg` spawn lane
/// through the real client binary. Post-x-2c27 that lane is reached via
/// `--substrate bg` (the detached `claude --bg` thread); a plain `spawn -p
/// claude` (pane) routes through the daemon's owned-interactive lane instead
/// (covered by build_request's `spawn_claude_default_is_pty_lane_with_minted_session`
/// + the daemon e2e suite, which spawn_routing.rs deliberately does NOT start a
/// daemon for). The bg receipt byte shape stays pinned end-to-end: callers parse
/// `.short_id` off it.
#[test]
fn client_spawn_bg_claude_happy_path_prints_receipt() {
    let home_dir = tmpdir("cli-spawn-claude-hp-home");
    let bin_dir = tmpdir("cli-spawn-claude-hp-bin");
    let cwd = tmpdir("cli-spawn-claude-hp-cwd");
    install_fake_claude(&bin_dir);
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_spawn_once_claude_happy_path_prints_receipt: binary not found at {:?}",
            bin
        );
        return;
    }

    let out = std::process::Command::new(&bin)
        .args([
            "spawn",
            "hp-agent",
            "hello there",
            "--provider",
            "claude",
            "--substrate",
            "bg",
        ])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .env("PATH", path_with(&bin_dir))
        .current_dir(&cwd)
        .output()
        .expect("failed to run fno-agents");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "claude spawn happy path must exit 0; stderr: {stderr}"
    );
    // Receipt is exactly one compact JSON line (the contract the shell
    // callers' `grep -F '"short_id"' | jq -r .short_id` parse relies on).
    let expected = "{\"name\": \"hp-agent\", \"short_id\": \"7c5dcf5d\", \"provider\": \"claude\", \"status\": \"live\"}\n";
    assert_eq!(
        stdout, expected,
        "claude spawn receipt must be the exact compact JSON line"
    );
    // And the registry row landed under the temp home.
    let registry_raw = fs::read_to_string(home_dir.join("registry.json")).unwrap_or_default();
    assert!(
        registry_raw.contains("\"hp-agent\"") && registry_raw.contains("7c5dcf5d"),
        "registry must carry the spawned row: {registry_raw}"
    );
}

/// AC5-EDGE (x-f54c): `host` was retired at G4 (interactive daemon PTY hosting
/// moved to the mux). The binary must intercept it with a one-line mux pointer
/// and exit non-zero - never reach the daemon, never a silent no-op.
#[test]
fn client_host_retired_prints_mux_pointer() {
    let home_dir = tmpdir("cli-host-retired-home");
    let bin = find_client_bin();
    if !bin.exists() {
        eprintln!(
            "skipping client_host_retired_prints_mux_pointer: binary not found at {:?}",
            bin
        );
        return;
    }
    let home = AgentsHome::at(home_dir.clone());
    seed_registry(&home, "host-collide", "codex");

    let out = std::process::Command::new(&bin)
        .args(["host", "host-collide", "--provider", "codex"])
        .env("FNO_SPAWN_GATE", "0")
        .env("FNO_E2E", "1") // test context: the spawn-cap auto-emit must NOT fire (x-91b5 AC1-EDGE)
        .env("FNO_AGENTS_HOME", &home_dir)
        .env("FNO_AGENTS_DAEMON_BIN", "/usr/bin/false")
        .output()
        .expect("failed to run fno-agents");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "retired host must exit 2; stderr: {stderr}"
    );
    assert!(
        stderr.contains("retired at G4") && stderr.contains("fno agents spawn"),
        "host must print the mux pointer: {stderr}"
    );
    // It must NOT have taken the daemon path.
    assert!(
        !stderr.contains("lazy-starting"),
        "retired host must not reach the daemon: {stderr}"
    );
}
