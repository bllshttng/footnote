#![cfg(test)]

//! `client_verbs` resume-argv parity test, file-backed to keep the host small.

use super::*;

#[test]
fn session_id_field_and_resume_argv_contract() {
    // Pin ambient dispatch dirs empty; they fold into the codex grant and
    // break the byte-identity argv pinned below.
    std::env::remove_var("FNO_WORKER_ADD_DIRS");
    assert_eq!(session_id_field("claude"), Some("short_id"));
    assert_eq!(session_id_field("codex"), Some("harness_session_id"));
    assert_eq!(session_id_field("gemini"), Some("harness_session_id"));
    assert_eq!(session_id_field("agy"), Some("harness_session_id"));
    assert_eq!(session_id_field("opencode"), Some("harness_session_id"));
    assert_eq!(session_id_field("cursor-agent"), Some("harness_session_id"));
    assert_eq!(session_id_field("unknown"), None);

    // --cd lands the resume in the row's own tree instead of the session
    // directory codex defaults to. It sits BEFORE the subcommand, where
    // codex's globals go. The raw spliced argv then composes with the
    // declared pre_exec (the shared-daemon ownership assertion), so the
    // rendered shape is one `sh -c` whose script runs the daemon start and
    // execs the filled resume.
    //
    // The `-c sandbox_workspace_write.writable_roots` grant is GONE from
    // this argv on purpose: codex 0.156.1 refuses that override paired with
    // `--remote` ("Configure additional workspace roots on the server"),
    // and the declared codex resume form carries `--remote unix://`, so the
    // pair failed every interactive resume of a reaped row. The roots still
    // reach the thread through the turn carrier (pinned below).
    let composed = |argv: &[&str]| -> Vec<String> {
        let script = format!(
            "'codex' 'app-server' 'daemon' 'start'; exec {}",
            argv.iter()
                .map(|t| format!("'{t}'"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        vec!["sh".into(), "-c".into(), script]
    };
    assert_eq!(
        build_resume_argv("codex", "uuid-1", Some("/path/that/does/not/exist")),
        Some(composed(&[
            "codex",
            "--cd",
            "/path/that/does/not/exist",
            "resume",
            "uuid-1",
            "--remote",
            "unix://",
        ]))
    );
    // The roots the dropped override would have carried still resolve, and
    // the delivery lane widens a resolved workspaceWrite posture with them
    // on every turn (`codex_inject::inject` -> `sandbox_policy_with_roots`)
    // - that is how they reach the thread without the refused argv pair.
    let roots = crate::provider::codex_writable_roots(Path::new("/path/that/does/not/exist"));
    assert!(
        roots
            .iter()
            .any(|r| r == "/path/that/does/not/exist/.fno/plans"),
        "plan root resolves for the turn carrier: {roots:?}"
    );
    let policy = crate::codex_thread::sandbox_policy_with_roots(
        &serde_json::json!({"type": "workspaceWrite", "writableRoots": []}),
        &roots,
    );
    assert_eq!(policy["writableRoots"], serde_json::json!(roots));
    // No cwd means no --cd: a bare flag fails parsing, and inventing a
    // directory is the wrong-tree failure this exists to prevent.
    assert_eq!(
        build_resume_argv("codex", "uuid-2", None),
        Some(composed(&[
            "codex", "resume", "uuid-2", "--remote", "unix://"
        ]))
    );
    // An EMPTY cwd is absent too, which is what Python's `if cwd` does.
    // Pinned here because nothing else is: drop the `.filter` and this is
    // the only assertion that fails, instead of a bare `--cd ""` reaching
    // codex, which cannot start on it.
    assert_eq!(
        build_resume_argv("codex", "uuid-3", Some("")),
        Some(composed(&[
            "codex", "resume", "uuid-3", "--remote", "unix://"
        ])),
        "empty cwd must be treated as absent, matching the Python twin"
    );
    assert_eq!(
        build_resume_argv("claude", "abc123", None),
        Some(vec!["claude".into(), "--resume".into(), "abc123".into()])
    );
    assert_eq!(
        build_resume_argv("gemini", "g-1", None),
        Some(vec!["gemini".into(), "--resume".into(), "g-1".into()])
    );
    assert_eq!(
        build_resume_argv("opencode", "ses_1", None),
        Some(vec!["opencode".into(), "--session".into(), "ses_1".into()])
    );
    // The measured primitive (fresh process quoting an earlier turn's
    // token over `--conversation`, 2026-08-26): the argv renders, and a
    // row carrying a canonical harness_session_id reaches it. No spawn
    // lane records one yet, so `resume` on today's agy rows still stops
    // at the missing-session-id refusal.
    assert_eq!(
        build_resume_argv("agy", "x", None),
        Some(vec!["agy".into(), "--conversation".into(), "x".into()])
    );
}
