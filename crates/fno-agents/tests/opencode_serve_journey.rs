//! The unattended journey that EARNS opencode's `bg = true` capability bit
//! (x-d9f9; the rule at harness_map.py:123 - a true value must be backed by an
//! unattended journey test for THIS harness, never inherited from another).
//!
//! Opt-in: runs only when `FNO_JOURNEY_OPENCODE=1` AND an `opencode` binary is
//! on PATH (CI has neither - x-9a96). The live run against opencode 1.14.50 +
//! the machine's default agent config is the recorded evidence for the flip;
//! commit-time evidence lives in the x-d9f9 decision record.
//!
//! The journey is the whole worker path with no human: boot the shared serve,
//! dispatch a worker (session mint, permission grant, registry row, detached
//! `run --attach` writer), wait for the turn to complete by polling the
//! structured message readback, and assert the receipt's claims against the
//! server. Cleanup deletes the session and stops the private serve.

use fno_agents::opencode_serve::{
    delete_session, dispatch_opencode_serve, ensure_serve, fetch_messages, fetch_session,
};
use fno_agents::paths::AgentsHome;
use fno_agents::state::load_registry;

use std::path::Path;
use std::time::{Duration, Instant};

fn journey_enabled() -> bool {
    if std::env::var("FNO_JOURNEY_OPENCODE").ok().as_deref() != Some("1") {
        return false;
    }
    // PATH probe, not a version pin (x-9a96 covers the pin question).
    std::process::Command::new("opencode")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The last assistant text part in the message readback, if any.
fn last_assistant_text(messages: &serde_json::Value) -> Option<String> {
    let arr = messages.as_array()?;
    let mut out = None;
    for msg in arr {
        let info = msg.get("info")?;
        if info.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(parts) = msg.get("parts").and_then(|p| p.as_array()) {
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        out = Some(text.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Kills the private serve pid on drop, so a FAILED assertion (which panics
/// past the inline cleanup) still leaves no orphan node process pointing at a
/// deleted tempdir.
struct ServeReaper(u32);
impl Drop for ServeReaper {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0 as i32, libc::SIGTERM) };
    }
}

#[test]
fn opencode_bg_worker_completes_unattended_over_serve() {
    if !journey_enabled() {
        eprintln!(
            "skipping: set FNO_JOURNEY_OPENCODE=1 with opencode on PATH to run the live journey"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = AgentsHome::at(dir.path().to_path_buf());
    let cwd = dir.path().join("worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    // A granted state dir, so the journey also proves the writable-dirs grant
    // lands on the session (the double-writer blocker's fix).
    let state_dir = dir.path().join("state-root");
    std::fs::create_dir_all(&state_dir).unwrap();

    let serve = ensure_serve(&home).expect("serve boots unattended");
    let _reaper = ServeReaper(serve.pid);
    // Boot proof: the generated config carries the unattended permission posture.
    let config = std::fs::read_to_string(home.root().join("opencode-serve-config.json")).unwrap();
    assert!(
        config.contains("allow") && config.contains("*"),
        "config: {config}"
    );

    // The dispatch under test: the public entry, so the env-read dirs path is
    // the real one.
    std::env::set_var("FNO_WORKER_ADD_DIRS", state_dir.to_string_lossy().as_ref());
    let outcome = dispatch_opencode_serve(
        &home,
        "wk-journey",
        "Reply with exactly: journey-ok",
        "journey",
        &cwd,
        None,
    );
    std::env::remove_var("FNO_WORKER_ADD_DIRS");
    assert_eq!(outcome.exit_code, 0, "dispatch failed: {}", outcome.stderr);
    let receipt: serde_json::Value = serde_json::from_str(outcome.stdout.trim()).unwrap();
    let session_id = receipt["session_id"].as_str().unwrap().to_string();
    assert!(
        session_id.starts_with("ses_"),
        "session id shape: {session_id}"
    );

    // Fire-and-forget proof: the spawn returned; the TURN runs on the serve.
    // Wait for the assistant reply by polling the structured readback.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut reply = None;
    while Instant::now() < deadline {
        if let Ok(messages) = fetch_messages(&serve.base_url, &session_id) {
            if let Some(text) = last_assistant_text(&messages) {
                reply = Some(text);
                break;
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    let reply = reply.expect("no assistant reply within 180s");
    assert!(
        reply.contains("journey-ok"),
        "worker answered {reply:?}, expected journey-ok"
    );

    // The writable-dirs grant is ON the session (scoped external_directory
    // allow rule for the granted root).
    let session = fetch_session(&serve.base_url, &session_id).expect("session readback");
    let rules = session
        .get("permission")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let granted = rules.iter().any(|r| {
        r.get("permission").and_then(|p| p.as_str()) == Some("external_directory")
            && r.get("action").and_then(|a| a.as_str()) == Some("allow")
            && r.get("pattern")
                .and_then(|p| p.as_str())
                .map(|p| p.starts_with(state_dir.to_string_lossy().as_ref()))
                .unwrap_or(false)
    });
    assert!(
        granted,
        "no external_directory allow for the state root: {rules:?}"
    );

    // The registry row binds harness + session.
    let reg = load_registry(&home.registry_json()).unwrap();
    let row = reg.find("wk-journey").expect("registry row");
    assert_eq!(row.harness.as_deref(), Some("opencode"));
    assert_eq!(row.harness_session_id.as_deref(), Some(session_id.as_str()));

    // Structured capture is the SERVE's message readback, not the writer's
    // stdout (the attach writer prints nothing - verified live). The readback
    // above already proved role/text; pin the structured fields a pane scrape
    // could never give: model identity and message ids.
    {
        let messages = fetch_messages(&serve.base_url, &session_id).expect("readback for capture");
        let arr = messages.as_array().expect("messages array");
        let assistant = arr
            .iter()
            .find(|m| {
                m.get("info")
                    .and_then(|i| i.get("role"))
                    .and_then(|r| r.as_str())
                    == Some("assistant")
            })
            .expect("assistant message in readback");
        let info = assistant.get("info").unwrap();
        assert!(
            info.get("modelID")
                .and_then(|m| m.as_str())
                .is_some_and(|m| !m.is_empty()),
            "structured model identity on the readback: {info}"
        );
        assert!(
            info.get("id")
                .and_then(|i| i.as_str())
                .is_some_and(|i| i.starts_with("msg_")),
            "structured message id on the readback: {info}"
        );
    }

    // The writer's log is the diagnostics trail (its stderr; stdout is empty
    // on the attach path). Existence is the claim - non-emptiness is not,
    // because a quiet run writes nothing.
    assert!(
        home.root()
            .join("agents")
            .join("logs")
            .join("wk-journey.jsonl")
            .exists(),
        "writer log file missing"
    );

    // Cleanup: session deleted; the reaper drops the serve pid on exit.
    delete_session(&serve.base_url, &session_id).expect("session teardown");
}

// tempfile + libc are dev-dependencies of the crate; serde_json is a main dep.
