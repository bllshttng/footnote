use std::process::Command;

/// The spawn door runs in the spawner's shell, which carries no worker
/// session id. A target-family command must still read ready there when the
/// machine, lifecycle and provider-goal legs are ready.
#[test]
fn spawn_door_readiness_needs_no_session_env() {
    let output = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .env_remove("FNO_HARNESS_SESSION_ID")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("GEMINI_SESSION_ID")
        .args([
            "loop",
            "readiness",
            "--pre-launch",
            "--harness",
            "claude",
            "--command",
            "/fno:target x-b400",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json["ready"], true, "{stdout}");
    assert_eq!(json["legs"]["stop"]["state"], "ready", "{stdout}");
    assert!(output.status.success(), "{stdout}");
}
