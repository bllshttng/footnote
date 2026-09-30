//! Unit tests for the `agy_ask` pure-fn core (Phase C, agy harness).
//!
//! Covers: `inject_from_name`, the one-shot argv builder (-p LAST, prompt as
//! value, always `--dangerously-skip-permissions`), the PLAIN-TEXT
//! `parse_response` (no JSON), stderr-based failure `classify_failure`, and the
//! `AgyAskError` exit-code map (ported from `agy-delegate.sh`:
//! 2/3/10/11/12/13/130). No subprocess or filesystem dependency.

use std::path::Path;

use fno_agents::agy_ask::{
    build_argv_once, build_argv_once_with_effort, classify_failure, inject_from_name,
    parse_response, AgyAskError,
};

// ---------------------------------------------------------------------------
// inject_from_name (identical contract to the sibling asks)
// ---------------------------------------------------------------------------

#[test]
fn inject_rows() {
    assert_eq!(
        inject_from_name("hello world", "alice"),
        "[from: alice]\n\nhello world"
    );

    assert_eq!(inject_from_name("a&b<c>", "x\"y"), "[from: x\"y]\n\na&b<c>");
}

// ---------------------------------------------------------------------------
// build_argv_once: -p LAST with prompt as its value; cwd as --add-dir; always
// --dangerously-skip-permissions (headless never-prompt); optional --model.
// ---------------------------------------------------------------------------

#[test]
fn argv_rows() {
    let argv = build_argv_once("do the thing", Path::new("/tmp/repo"), None, None);
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

    let argv = build_argv_once("hi", Path::new("/r"), Some("Gemini 3.5 Flash (High)"), None);
    let m_idx = argv
        .iter()
        .position(|a| a == "--model")
        .expect("has --model");
    assert_eq!(argv[m_idx + 1], "Gemini 3.5 Flash (High)");
    // --model precedes -p (which stays last).
    assert!(m_idx < argv.iter().position(|a| a == "-p").unwrap());

    let argv = build_argv_once_with_effort("hi", Path::new("/r"), None, Some("high"), None, &[]);
    let effort = argv.iter().position(|arg| arg == "--effort").unwrap();
    assert_eq!(&argv[effort..effort + 2], ["--effort", "high"]);

    let argv = build_argv_once("hi", Path::new("/r"), Some(""), None);
    assert!(!argv.iter().any(|a| a == "--model"));

    let argv = build_argv_once("hi", Path::new("/repo"), None, Some("/extra"));
    let dirs: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--add-dir")
        .map(|(i, _)| &argv[i + 1])
        .collect();
    assert!(
        dirs.iter().any(|d| d.as_str() == "/repo"),
        "internal cwd kept"
    );
    assert!(
        dirs.iter().any(|d| d.as_str() == "/extra"),
        "user dir added"
    );
    assert_eq!(dirs.len(), 2, "exactly the two --add-dir values");
    // Empty user value is unset: only the internal cwd injection survives.
    let bare = build_argv_once("hi", Path::new("/repo"), None, Some(""));
    let n = bare.iter().filter(|a| a.as_str() == "--add-dir").count();
    assert_eq!(n, 1);
}

// x-b6e2 (US5, AC1-EDGE): a user --add-dir is ADDITIVE - both the internal cwd
// injection and the user dir appear as --add-dir values; the internal one is
// never replaced. Empty user value = unchanged argv.

// ---------------------------------------------------------------------------
// parse_response: plain text in, plain text out (trimmed); empty -> Empty.
// ---------------------------------------------------------------------------

#[test]
fn parse_rows() {
    assert_eq!(
        parse_response("the answer is 42\n").unwrap(),
        "the answer is 42"
    );

    let out = "  line one\nline two  \n";
    assert_eq!(parse_response(out).unwrap(), "line one\nline two");

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
fn classify_rows() {
    assert!(matches!(
        classify_failure("Error: resource exhausted (quota)", 1),
        AgyAskError::Quota
    ));
    assert!(matches!(
        classify_failure("hit RATE LIMIT", 1),
        AgyAskError::Quota
    ));

    assert!(matches!(
        classify_failure("UNAUTHENTICATED: please sign in", 1),
        AgyAskError::Auth
    ));

    assert!(matches!(
        classify_failure("context deadline exceeded", 1),
        AgyAskError::Timeout { .. }
    ));

    match classify_failure("some other failure", 7) {
        AgyAskError::Invocation { exit_code } => assert_eq!(exit_code, 7),
        other => panic!("expected Invocation, got {other:?}"),
    }

    // A benign stderr must NOT be misclassified just because a trigger word
    // could appear in a model reply — classify only ever sees stderr.
    assert!(matches!(
        classify_failure("ripgrep warning: skipped a dir", 2),
        AgyAskError::Invocation { .. }
    ));

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

// ---------------------------------------------------------------------------
// AgyAskError exit-code map (ported from agy-delegate.sh).
// ---------------------------------------------------------------------------
