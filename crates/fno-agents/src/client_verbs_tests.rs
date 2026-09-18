#![cfg(test)]

//! `client_verbs` resume-argv parity test, file-backed to keep the host small.

use super::*;

#[test]
fn session_id_field_and_resume_argv_match_python() {
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
    // directory codex defaults to. It sits with the -c grant BEFORE the
    // subcommand, where codex's globals go. The raw spliced argv then
    // composes with the declared pre_exec (the shared-daemon ownership
    // assertion), so the rendered shape is one `sh -c` whose script runs
    // the daemon start and execs the filled resume. The grant folds
    // FNO_WORKER_ADD_DIRS when the invoking environment carries one, so
    // the expectation reads the same ambient var instead of pinning a
    // roots list the machine is free to extend.
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
    let mut roots = vec!["/path/that/does/not/exist/.fno/plans".to_string()];
    for extra in crate::claude_ask::state_dirs_from_env() {
        if !extra.is_empty() && !roots.contains(&extra) {
            roots.push(extra);
        }
    }
    let encoded = serde_json::to_string(&roots).unwrap();
    let grant = format!("sandbox_workspace_write.writable_roots={encoded}");
    assert_eq!(
        build_resume_argv("codex", "uuid-1", Some("/path/that/does/not/exist")),
        Some(composed(&[
            "codex",
            "-c",
            grant.as_str(),
            "--cd",
            "/path/that/does/not/exist",
            "resume",
            "uuid-1",
            "--remote",
            "unix://",
        ]))
    );
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
