//! Wave B1 unit tests for `codex_ask` pure-fn core.
//!
//! Tests for: argv builders (create/resume), inject_from_name, JSONL line
//! parser, error enum, and exit-code map. All tests run against fixture data
//! and in-process helpers with no subprocess or filesystem dependency.
//!
//! These tests MUST FAIL before codex_ask.rs is created (TDD red phase).

use fno_agents::codex_ask::{
    approval_flag, build_argv_create, build_argv_resume, inject_from_name, parse_jsonl_line,
    sandbox_flag, sandbox_flag_resume, CodexAskError, JsonlEvent,
};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// inject_from_name (Locked Decision 7 parity)
// ---------------------------------------------------------------------------

#[test]
fn inject_from_name_prepends_bracket_prefix() {
    let got = inject_from_name("hello world", "alice");
    assert_eq!(got, "[from: alice]\n\nhello world");
}

#[test]
fn inject_from_name_empty_prompt() {
    assert_eq!(inject_from_name("", "x"), "[from: x]\n\n");
}

#[test]
fn inject_from_name_no_escaping() {
    // Plain concatenation; no HTML or JSON escaping
    let got = inject_from_name("a&b<c>", "x\"y");
    assert_eq!(got, "[from: x\"y]\n\na&b<c>");
}

// ---------------------------------------------------------------------------
// sandbox_flag (create path)
// ---------------------------------------------------------------------------

#[test]
fn sandbox_flag_default_is_bounded() {
    // Sandbox tokens only (workspace sandbox); approval is a separate global
    // flag emitted before `exec` - see approval_flag.
    assert_eq!(sandbox_flag(false), vec!["--sandbox", "workspace-write"]);
}

#[test]
fn sandbox_flag_yolo_is_bypass() {
    assert_eq!(
        sandbox_flag(true),
        vec!["--dangerously-bypass-approvals-and-sandbox"]
    );
}

#[test]
fn sandbox_flag_never_carries_approval_token() {
    // --ask-for-approval is a GLOBAL flag; sandbox_flag tokens are spliced
    // AFTER `exec`, where codex rejects --ask-for-approval. It must never leak
    // into sandbox_flag's output. (Regression: pr704 codex spawn abort.)
    for yolo in [true, false] {
        assert!(!sandbox_flag(yolo).contains(&"--ask-for-approval".to_string()));
    }
}

// ---------------------------------------------------------------------------
// approval_flag (create path - GLOBAL flag, emitted before `exec`)
// ---------------------------------------------------------------------------

#[test]
fn approval_flag_default_is_never_prompt() {
    assert_eq!(approval_flag(false), vec!["--ask-for-approval", "never"]);
}

#[test]
fn approval_flag_yolo_is_empty() {
    // The bypass flag from sandbox_flag already disables approval.
    assert!(approval_flag(true).is_empty());
}

// ---------------------------------------------------------------------------
// sandbox_flag_resume (resume path - restricted flags)
// ---------------------------------------------------------------------------

#[test]
fn sandbox_flag_resume_default_is_empty() {
    assert!(sandbox_flag_resume(false).is_empty());
}

#[test]
fn sandbox_flag_resume_yolo_is_bypass() {
    assert_eq!(
        sandbox_flag_resume(true),
        vec!["--dangerously-bypass-approvals-and-sandbox"]
    );
}

// ---------------------------------------------------------------------------
// build_argv_create
// ---------------------------------------------------------------------------

#[test]
fn build_argv_create_default_sandbox() {
    let cwd = PathBuf::from("/tmp/proj");
    let full_prompt = "[from: alice]\n\nhello";
    let argv = build_argv_create(&cwd, full_prompt, false);
    assert_eq!(
        argv,
        vec![
            "codex",
            "--ask-for-approval",
            "never",
            "exec",
            "--json",
            "-C",
            "/tmp/proj",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
            "[from: alice]\n\nhello",
        ]
    );
}

#[test]
fn build_argv_create_approval_precedes_exec() {
    // Regression (pr704): --ask-for-approval is a GLOBAL flag and MUST come
    // before the `exec` subcommand, or codex aborts with
    // `error: unexpected argument '--ask-for-approval' found`.
    let argv = build_argv_create(&PathBuf::from("/x"), "m", false);
    let approval = argv
        .iter()
        .position(|a| a == "--ask-for-approval")
        .expect("bounded create argv carries --ask-for-approval");
    let exec = argv
        .iter()
        .position(|a| a == "exec")
        .expect("create argv carries exec");
    assert!(approval < exec, "approval flag must precede exec: {argv:?}");
}

#[test]
fn build_argv_create_yolo() {
    let cwd = PathBuf::from("/work");
    let argv = build_argv_create(&cwd, "msg", true);
    // --dangerously-bypass-approvals-and-sandbox replaces --sandbox workspace-write
    assert!(argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    assert!(!argv.contains(&"--sandbox".to_string()));
    assert!(!argv.contains(&"workspace-write".to_string()));
}

#[test]
fn build_argv_create_no_resume_subcommand() {
    let argv = build_argv_create(&PathBuf::from("/x"), "m", false);
    // Should be `codex --ask-for-approval never exec --json -C ...`, not
    // `codex exec resume`. The exec subcommand follows the global approval flag.
    assert_eq!(argv[0], "codex");
    assert_eq!(argv[1], "--ask-for-approval");
    assert_eq!(argv[2], "never");
    assert_eq!(argv[3], "exec");
    assert_eq!(argv[4], "--json");
    assert!(!argv.contains(&"resume".to_string()));
}

// ---------------------------------------------------------------------------
// build_argv_resume
// ---------------------------------------------------------------------------

#[test]
fn build_argv_resume_default_no_sandbox_flag() {
    let argv = build_argv_resume("sess-uuid-1234", "msg to resume", false);
    // codex exec resume <session_id> --json --skip-git-repo-check <prompt>
    assert_eq!(argv[0], "codex");
    assert_eq!(argv[1], "exec");
    assert_eq!(argv[2], "resume");
    assert_eq!(argv[3], "sess-uuid-1234");
    assert!(argv.contains(&"--json".to_string()));
    assert!(argv.contains(&"--skip-git-repo-check".to_string()));
    // No --sandbox or --dangerously-bypass on default
    assert!(!argv.contains(&"--sandbox".to_string()));
    assert!(!argv.contains(&"workspace-write".to_string()));
    assert!(!argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    // Prompt is last
    assert_eq!(argv.last().unwrap(), "msg to resume");
}

#[test]
fn build_argv_resume_yolo_adds_bypass() {
    let argv = build_argv_resume("s123", "p", true);
    assert!(argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
}

#[test]
fn build_argv_resume_no_cd_flag() {
    // Resume sets cwd via Popen(cwd=...) not -C argv token
    let argv = build_argv_resume("s123", "p", false);
    assert!(!argv.contains(&"-C".to_string()));
}

// ---------------------------------------------------------------------------
// parse_jsonl_line
// ---------------------------------------------------------------------------

#[test]
fn parse_jsonl_line_thread_started() {
    let line = r#"{"type":"thread.started","thread_id":"aaaabbbb-1234-5678-9abc-def012345678"}"#;
    let ev = parse_jsonl_line(line).unwrap();
    match ev {
        JsonlEvent::ThreadStarted { thread_id } => {
            assert_eq!(thread_id, "aaaabbbb-1234-5678-9abc-def012345678");
        }
        other => panic!("unexpected: {:?}", other),
    }
}

#[test]
fn parse_jsonl_line_item_completed_agent_message() {
    let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello world"}}"#;
    let ev = parse_jsonl_line(line).unwrap();
    match ev {
        JsonlEvent::AgentMessage { text } => assert_eq!(text, "hello world"),
        other => panic!("unexpected: {:?}", other),
    }
}

#[test]
fn parse_jsonl_line_item_completed_error() {
    let line = r#"{"type":"item.completed","item":{"type":"error","message":"something failed"}}"#;
    let ev = parse_jsonl_line(line).unwrap();
    match ev {
        JsonlEvent::SoftError { message } => assert_eq!(message, "something failed"),
        other => panic!("unexpected: {:?}", other),
    }
}

#[test]
fn parse_jsonl_line_turn_completed() {
    let line = r#"{"type":"turn.completed"}"#;
    let ev = parse_jsonl_line(line).unwrap();
    assert!(matches!(ev, JsonlEvent::TurnCompleted));
}

#[test]
fn parse_jsonl_line_non_json_returns_none() {
    // Non-JSON banner lines are skipped
    assert!(parse_jsonl_line("Reading additional input from stdin...").is_none());
    assert!(parse_jsonl_line("").is_none());
    assert!(parse_jsonl_line("   ").is_none());
}

#[test]
fn parse_jsonl_line_non_object_json_returns_none() {
    assert!(parse_jsonl_line("[1,2,3]").is_none());
    assert!(parse_jsonl_line("\"hello\"").is_none());
}

#[test]
fn parse_jsonl_line_unknown_type_returns_other() {
    let line = r#"{"type":"some.future.event","data":42}"#;
    let ev = parse_jsonl_line(line).unwrap();
    assert!(matches!(ev, JsonlEvent::Other { .. }));
}

#[test]
fn parse_jsonl_line_item_completed_unknown_item_type_returns_other() {
    // item.completed with unknown item type -> Other (not panic)
    let line = r#"{"type":"item.completed","item":{"type":"future_type","x":1}}"#;
    let ev = parse_jsonl_line(line).unwrap();
    assert!(matches!(ev, JsonlEvent::Other { .. }));
}

#[test]
fn parse_jsonl_line_thread_started_missing_thread_id_returns_other_or_none() {
    // thread.started without thread_id -> None or Other (can't extract session_id)
    // The key requirement is: no panic, and no ThreadStarted with a junk id.
    let line = r#"{"type":"thread.started"}"#;
    let ev = parse_jsonl_line(line);
    // None (the ? operator returned early) or Some(Other) are both acceptable.
    if let Some(ev) = ev {
        match ev {
            JsonlEvent::ThreadStarted { .. } => {
                panic!("should not parse a missing thread_id as valid ThreadStarted")
            }
            _ => {} // Other is fine
        }
    }
    // None is also fine
}

// ---------------------------------------------------------------------------
// CodexAskError display / exit-code mapping
// ---------------------------------------------------------------------------

#[test]
fn error_no_session_id_exit_code_11() {
    let err = CodexAskError::NoSessionId {
        types_seen: vec!["turn.completed".to_string()],
    };
    assert_eq!(err.exit_code(), 11);
    let msg = err.to_string();
    assert!(msg.contains("session id"), "msg: {}", msg);
}

#[test]
fn error_not_found_maps_to_provider_unavailable() {
    // CodexAskError::NotFound represents "codex binary not on PATH"; per
    // Python parity (dispatch.py maps codex's FileNotFoundError(127) chain
    // to exit 14 "provider unavailable"), exit_code() returns 14 directly.
    // Both dispatch paths (create and resume) inherit this without an
    // inline remap (sigma-review type-design HIGH).
    let err = CodexAskError::NotFound;
    assert_eq!(err.exit_code(), 14);
}

#[test]
fn error_invocation_exit_code_propagated() {
    let err = CodexAskError::Invocation {
        exit_code: 5,
        message: "err".into(),
    };
    assert_eq!(err.exit_code(), 5);
}

#[test]
fn error_tee_open_exit_code_12() {
    let err = CodexAskError::TeeOpen {
        message: "perm denied".into(),
    };
    assert_eq!(err.exit_code(), 12);
}

#[test]
fn error_timeout_exit_code_15() {
    let err = CodexAskError::Timeout { timeout_sec: 60.0 };
    assert_eq!(err.exit_code(), 15);
    let msg = err.to_string();
    assert!(msg.contains("60"), "msg: {}", msg);
}

#[test]
fn error_sigkill_escalated_exit_code_1() {
    let err = CodexAskError::SigkillEscalated {
        partial_exit_code: 0,
    };
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn error_os_error_exit_code_1() {
    let err = CodexAskError::OsError {
        message: "ENOMEM".into(),
    };
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn error_interrupted_exit_code_130() {
    // ab-e7fdbcb6: operator Ctrl-C maps to exit 130 (128 + SIGINT), matching
    // Python's KeyboardInterrupt -> CPython exit 130.
    let err = CodexAskError::Interrupted;
    assert_eq!(err.exit_code(), 130);
    let msg = err.to_string();
    assert!(
        msg.contains("SIGINT") || msg.contains("Ctrl-C"),
        "msg: {}",
        msg
    );
}
