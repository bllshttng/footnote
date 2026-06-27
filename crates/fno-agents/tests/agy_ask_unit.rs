//! Unit tests for the `agy_ask` pure-fn core (Phase C, agy harness).
//!
//! Covers: `inject_from_name`, the one-shot argv builder (-p LAST, prompt as
//! value, always `--dangerously-skip-permissions`), the PLAIN-TEXT
//! `parse_response` (no JSON), stderr-based failure `classify_failure`, and the
//! `AgyAskError` exit-code map (ported from `agy-delegate.sh`:
//! 2/3/10/11/12/13/130). No subprocess or filesystem dependency.

use std::path::Path;

use fno_agents::agy_ask::{
    build_argv_once, classify_failure, inject_from_name, parse_response, AgyAskError,
};

// ---------------------------------------------------------------------------
// inject_from_name (identical contract to the sibling asks)
// ---------------------------------------------------------------------------

#[test]
fn inject_from_name_prepends_bracket_prefix() {
    assert_eq!(
        inject_from_name("hello world", "alice"),
        "[from: alice]\n\nhello world"
    );
}

#[test]
fn inject_from_name_no_escaping() {
    assert_eq!(inject_from_name("a&b<c>", "x\"y"), "[from: x\"y]\n\na&b<c>");
}

// ---------------------------------------------------------------------------
// build_argv_once: -p LAST with prompt as its value; cwd as --add-dir; always
// --dangerously-skip-permissions (headless never-prompt); optional --model.
// ---------------------------------------------------------------------------

#[test]
fn argv_once_basic_shape() {
    let argv = build_argv_once("do the thing", Path::new("/tmp/repo"), None);
    assert_eq!(argv[0], "agy");
    // -p is the LAST flag and its value is the prompt (the wrapper's ordering rule).
    let p_idx = argv.iter().position(|a| a == "-p").expect("has -p");
    assert_eq!(p_idx, argv.len() - 2, "-p must be second-to-last");
    assert_eq!(argv[argv.len() - 1], "do the thing");
    // never-prompt posture is always present on the headless one-shot.
    assert!(argv.iter().any(|a| a == "--dangerously-skip-permissions"));
    // cwd is passed as the agy workspace.
    let d_idx = argv
        .iter()
        .position(|a| a == "--add-dir")
        .expect("has --add-dir");
    assert_eq!(argv[d_idx + 1], "/tmp/repo");
    // no --model when not requested.
    assert!(!argv.iter().any(|a| a == "--model"));
}

#[test]
fn argv_once_with_model() {
    let argv = build_argv_once("hi", Path::new("/r"), Some("Gemini 3.5 Flash (High)"));
    let m_idx = argv
        .iter()
        .position(|a| a == "--model")
        .expect("has --model");
    assert_eq!(argv[m_idx + 1], "Gemini 3.5 Flash (High)");
    // --model precedes -p (which stays last).
    assert!(m_idx < argv.iter().position(|a| a == "-p").unwrap());
}

#[test]
fn argv_once_empty_model_is_omitted() {
    let argv = build_argv_once("hi", Path::new("/r"), Some(""));
    assert!(!argv.iter().any(|a| a == "--model"));
}

// ---------------------------------------------------------------------------
// parse_response: plain text in, plain text out (trimmed); empty -> Empty.
// ---------------------------------------------------------------------------

#[test]
fn parse_plain_text_reply() {
    assert_eq!(
        parse_response("the answer is 42\n").unwrap(),
        "the answer is 42"
    );
}

#[test]
fn parse_multiline_reply_trims_edges_only() {
    let out = "  line one\nline two  \n";
    assert_eq!(parse_response(out).unwrap(), "line one\nline two");
}

#[test]
fn parse_empty_is_empty_error() {
    assert!(matches!(parse_response(""), Err(AgyAskError::Empty { .. })));
    assert!(matches!(
        parse_response("   \n\t  "),
        Err(AgyAskError::Empty { .. })
    ));
}

// ---------------------------------------------------------------------------
// classify_failure: stderr scan -> Quota / Auth / Timeout / Invocation.
// ---------------------------------------------------------------------------

#[test]
fn classify_quota() {
    assert!(matches!(
        classify_failure("Error: resource exhausted (quota)", 1),
        AgyAskError::Quota
    ));
    assert!(matches!(
        classify_failure("hit RATE LIMIT", 1),
        AgyAskError::Quota
    ));
}

#[test]
fn classify_auth() {
    assert!(matches!(
        classify_failure("UNAUTHENTICATED: please sign in", 1),
        AgyAskError::Auth
    ));
}

#[test]
fn classify_timeout() {
    assert!(matches!(
        classify_failure("context deadline exceeded", 1),
        AgyAskError::Timeout { .. }
    ));
}

#[test]
fn classify_unknown_is_invocation() {
    match classify_failure("some other failure", 7) {
        AgyAskError::Invocation { exit_code } => assert_eq!(exit_code, 7),
        other => panic!("expected Invocation, got {other:?}"),
    }
}

#[test]
fn classify_scans_stderr_not_stdout_triggers() {
    // A benign stderr must NOT be misclassified just because a trigger word
    // could appear in a model reply — classify only ever sees stderr.
    assert!(matches!(
        classify_failure("ripgrep warning: skipped a dir", 2),
        AgyAskError::Invocation { .. }
    ));
}

// ---------------------------------------------------------------------------
// AgyAskError exit-code map (ported from agy-delegate.sh).
// ---------------------------------------------------------------------------

#[test]
fn exit_code_map_matches_wrapper() {
    assert_eq!(AgyAskError::NotFound.exit_code(), 13);
    assert_eq!(
        AgyAskError::Empty {
            raw_head: String::new()
        }
        .exit_code(),
        3
    );
    assert_eq!(AgyAskError::Quota.exit_code(), 10);
    assert_eq!(AgyAskError::Auth.exit_code(), 11);
    assert_eq!(AgyAskError::Timeout { timeout_sec: 5.0 }.exit_code(), 12);
    assert_eq!(AgyAskError::Invocation { exit_code: 4 }.exit_code(), 2);
    // a folded-zero invocation maps to 1 (never 0).
    assert_eq!(AgyAskError::Invocation { exit_code: 0 }.exit_code(), 1);
    assert_eq!(
        AgyAskError::OsError {
            message: String::new()
        }
        .exit_code(),
        1
    );
    assert_eq!(AgyAskError::Interrupted.exit_code(), 130);
}
