//! Wave G2 unit tests for `gemini_ask` pure-fn core (ab-73da4ac2).
//!
//! Tests for: argv builders (create/resume), inject_from_name, sandbox_flag,
//! single-blob `parse_response` + schema-drift guards, and the GeminiAskError
//! exit-code map. All run against fixture data with no subprocess or
//! filesystem dependency. Byte-parity contract is `providers/gemini.py`.

use fno_agents::gemini_ask::{
    build_argv_create, build_argv_resume, inject_from_name, parse_response, sandbox_flag,
    GeminiAskError,
};

// ---------------------------------------------------------------------------
// inject_from_name (Locked Decision 8 parity; identical to codex)
// ---------------------------------------------------------------------------

#[test]
fn inject_from_name_prepends_bracket_prefix() {
    assert_eq!(
        inject_from_name("hello world", "alice"),
        "[from: alice]\n\nhello world"
    );
}

#[test]
fn inject_from_name_empty_prompt() {
    assert_eq!(inject_from_name("", "x"), "[from: x]\n\n");
}

#[test]
fn inject_from_name_no_escaping() {
    assert_eq!(inject_from_name("a&b<c>", "x\"y"), "[from: x\"y]\n\na&b<c>");
}

// ---------------------------------------------------------------------------
// sandbox_flag (bounded-posture amendment: bounded `--approval-mode yolo
// --sandbox` (+ fallback) vs explicit full `--yolo`; same on create+resume)
// ---------------------------------------------------------------------------

#[test]
fn sandbox_flag_bounded_with_provider() {
    assert_eq!(
        sandbox_flag(false, Some(true)),
        vec!["--approval-mode", "yolo", "--sandbox"]
    );
}

#[test]
fn sandbox_flag_bounded_fallback_no_provider_never_prompts() {
    let fb = sandbox_flag(false, Some(false));
    assert_eq!(fb, vec!["--approval-mode", "yolo"]);
    assert!(!fb.iter().any(|t| t == "default" || t == "auto_edit"));
}

#[test]
fn sandbox_flag_full_yolo_is_bare_yolo() {
    assert_eq!(sandbox_flag(true, None), vec!["--yolo"]);
}

// ---------------------------------------------------------------------------
// build_argv_create
// ---------------------------------------------------------------------------

#[test]
fn build_argv_create_default_is_never_prompt() {
    // The bounded default's argv is host-dependent (--sandbox only when a
    // provider exists), so assert the host-INDEPENDENT invariants: the stable
    // prefix + the never-prompt axis (--approval-mode yolo always present) and
    // NEVER a prompting mode (default/auto_edit).
    let argv = build_argv_create("[from: alice]\n\nhello", false, None, None);
    assert_eq!(argv[0], "gemini");
    assert_eq!(argv[1], "--skip-trust");
    assert_eq!(argv[2], "-p");
    assert_eq!(argv[3], "[from: alice]\n\nhello");
    assert_eq!(argv[4], "--output-format");
    assert_eq!(argv[5], "json");
    let pos = argv
        .iter()
        .position(|a| a == "--approval-mode")
        .expect("--approval-mode present");
    assert_eq!(argv[pos + 1], "yolo");
    assert!(!argv.iter().any(|a| a == "default" || a == "auto_edit"));
}

#[test]
fn build_argv_create_forwards_model() {
    // x-c772: an explicit --model reaches `gemini --model <m>`; None/empty = none.
    let argv = build_argv_create("msg", false, None, Some("gemini-3-pro"));
    let i = argv
        .iter()
        .position(|a| a == "--model")
        .expect("--model present");
    assert_eq!(argv[i + 1], "gemini-3-pro");
    let argv_none = build_argv_create("msg", false, None, Some(""));
    assert!(!argv_none.iter().any(|a| a == "--model"));
}

#[test]
fn build_argv_create_no_cd_flag() {
    // gemini pins cwd via Popen(cwd=...), not a -C argv token.
    let argv = build_argv_create("msg", false, None, None);
    assert!(!argv.contains(&"-C".to_string()));
    assert!(!argv.contains(&"--resume".to_string()));
}

#[test]
fn build_argv_create_yolo_replaces_approval_mode() {
    let argv = build_argv_create("msg", true, None, None);
    assert!(argv.contains(&"--yolo".to_string()));
    assert!(!argv.contains(&"--approval-mode".to_string()));
}

#[test]
fn build_argv_create_with_session_id_appends_flag() {
    let argv = build_argv_create("msg", false, Some("uuid-abc"), None);
    // --session-id <uuid> appended after the sandbox flags.
    let pos = argv
        .iter()
        .position(|a| a == "--session-id")
        .expect("--session-id present");
    assert_eq!(argv[pos + 1], "uuid-abc");
}

#[test]
fn build_argv_create_empty_session_id_omits_flag() {
    // Python: `if session_id:` is falsy for "" -> no flag.
    let argv = build_argv_create("msg", false, Some(""), None);
    assert!(!argv.contains(&"--session-id".to_string()));
}

// ---------------------------------------------------------------------------
// build_argv_resume
// ---------------------------------------------------------------------------

#[test]
fn build_argv_resume_includes_resume_and_session() {
    let argv = build_argv_resume("sess-uuid-1234", "msg to resume", false);
    assert_eq!(argv[0], "gemini");
    assert_eq!(argv[1], "--skip-trust");
    assert_eq!(argv[2], "-p");
    assert_eq!(argv[3], "msg to resume");
    assert_eq!(argv[4], "--output-format");
    assert_eq!(argv[5], "json");
    let rpos = argv
        .iter()
        .position(|a| a == "--resume")
        .expect("--resume present");
    assert_eq!(argv[rpos + 1], "sess-uuid-1234");
    // never-prompt invariant (host-independent): --approval-mode yolo, never default.
    let apos = argv
        .iter()
        .position(|a| a == "--approval-mode")
        .expect("--approval-mode present");
    assert_eq!(argv[apos + 1], "yolo");
    assert!(!argv.iter().any(|a| a == "default" || a == "auto_edit"));
}

#[test]
fn build_argv_resume_yolo_uses_yolo_flag() {
    // gemini resume uses the SAME sandbox_flag as create (unlike codex).
    let argv = build_argv_resume("s123", "p", true);
    assert!(argv.contains(&"--yolo".to_string()));
    assert!(!argv.contains(&"--approval-mode".to_string()));
}

#[test]
fn build_argv_resume_no_cd_flag() {
    let argv = build_argv_resume("s123", "p", false);
    assert!(!argv.contains(&"-C".to_string()));
}

// ---------------------------------------------------------------------------
// parse_response — happy path + schema-drift guards (gemini._parse_response)
// ---------------------------------------------------------------------------

#[test]
fn parse_response_happy_path() {
    let blob = r#"{"session_id":"uuid-1","response":"hi there","stats":{"turns":1}}"#;
    let (sid, reply) = parse_response(blob).unwrap();
    assert_eq!(sid.as_deref(), Some("uuid-1"));
    assert_eq!(reply, "hi there");
}

#[test]
fn parse_response_null_response_is_empty_string() {
    // model declined to emit text -> "" (distinct from schema drift).
    let blob = r#"{"session_id":"uuid-1","response":null,"stats":{}}"#;
    let (_sid, reply) = parse_response(blob).unwrap();
    assert_eq!(reply, "");
}

#[test]
fn parse_response_empty_response_string() {
    let blob = r#"{"session_id":"uuid-1","response":"","stats":{}}"#;
    let (_sid, reply) = parse_response(blob).unwrap();
    assert_eq!(reply, "");
}

#[test]
fn parse_response_null_session_id_is_none() {
    let blob = r#"{"session_id":null,"response":"x","stats":{}}"#;
    let (sid, reply) = parse_response(blob).unwrap();
    assert!(sid.is_none());
    assert_eq!(reply, "x");
}

#[test]
fn parse_response_missing_session_id_key_is_none() {
    let blob = r#"{"response":"x","stats":{}}"#;
    let (sid, _reply) = parse_response(blob).unwrap();
    assert!(sid.is_none());
}

#[test]
fn parse_response_empty_stdout_is_parse_error() {
    assert!(matches!(
        parse_response(""),
        Err(GeminiAskError::Parse { .. })
    ));
    assert!(matches!(
        parse_response("   \n "),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_malformed_json_is_parse_error() {
    assert!(matches!(
        parse_response("not json at all"),
        Err(GeminiAskError::Parse { .. })
    ));
    assert!(matches!(
        parse_response("{broken"),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_non_object_is_parse_error() {
    assert!(matches!(
        parse_response("[1,2,3]"),
        Err(GeminiAskError::Parse { .. })
    ));
    assert!(matches!(
        parse_response("\"a string\""),
        Err(GeminiAskError::Parse { .. })
    ));
    assert!(matches!(
        parse_response("42"),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_missing_response_key_is_drift() {
    // present session_id + stats but no response -> schema drift (exit 11).
    let blob = r#"{"session_id":"u","stats":{}}"#;
    assert!(matches!(
        parse_response(blob),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_missing_stats_key_is_drift() {
    // Codex P2 PR #317: a missing `stats` is drift, not a silent empty reply.
    let blob = r#"{"session_id":"u","response":"hi"}"#;
    assert!(matches!(
        parse_response(blob),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_non_string_session_id_is_drift() {
    let blob = r#"{"session_id":123,"response":"x","stats":{}}"#;
    assert!(matches!(
        parse_response(blob),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_non_string_response_is_drift() {
    let blob = r#"{"session_id":"u","response":42,"stats":{}}"#;
    assert!(matches!(
        parse_response(blob),
        Err(GeminiAskError::Parse { .. })
    ));
}

#[test]
fn parse_response_raw_head_truncates_to_200_chars() {
    // The raw_head carried for forensics is the first 200 chars of the blob.
    let long = "x".repeat(500);
    match parse_response(&long) {
        Err(GeminiAskError::Parse { raw_head }) => {
            assert_eq!(raw_head.chars().count(), 200);
        }
        other => panic!("expected Parse error, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// GeminiAskError exit-code map (dispatch.py _gemini_*_path failure->exit)
// ---------------------------------------------------------------------------

#[test]
fn error_not_found_exit_14() {
    assert_eq!(GeminiAskError::NotFound.exit_code(), 14);
}

#[test]
fn error_parse_exit_11() {
    let e = GeminiAskError::Parse {
        raw_head: "garbage".into(),
    };
    assert_eq!(e.exit_code(), 11);
    assert!(e.to_string().contains("parse"));
}

#[test]
fn error_tee_open_exit_12() {
    assert_eq!(
        GeminiAskError::TeeOpen {
            message: "perm".into()
        }
        .exit_code(),
        12
    );
}

#[test]
fn error_timeout_exit_15() {
    let e = GeminiAskError::Timeout { timeout_sec: 60.0 };
    assert_eq!(e.exit_code(), 15);
    assert!(e.to_string().contains("60"));
}

#[test]
fn error_invocation_nonzero_propagates() {
    assert_eq!(GeminiAskError::Invocation { exit_code: 5 }.exit_code(), 5);
    // sigkill escalation folds into Invocation with the partial code; a 0
    // partial maps to 1 (a partial reply + kill is never a success).
    assert_eq!(GeminiAskError::Invocation { exit_code: 0 }.exit_code(), 1);
}

#[test]
fn error_os_error_exit_1() {
    assert_eq!(
        GeminiAskError::OsError {
            message: "EMFILE".into()
        }
        .exit_code(),
        1
    );
}

#[test]
fn error_interrupted_exit_130() {
    let e = GeminiAskError::Interrupted;
    assert_eq!(e.exit_code(), 130);
    assert!(e.to_string().contains("SIGINT") || e.to_string().contains("Ctrl-C"));
}
