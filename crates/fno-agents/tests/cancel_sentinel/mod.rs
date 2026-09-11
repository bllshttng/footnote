//! When a cancel sentinel fires, what the termination says, and what happens
//! to the file afterward: attribution in the Interrupted line and event, and
//! one-shot consumption so a cancel terminates one run, not every later stop
//! of a session that recovers.

use super::*;

/// Cancel sentinel present (mtime >= created_at) -> Interrupted, and the
/// fired sentinel is consumed: a cancel terminates one run, not every later
/// stop of a session that recovers.
#[test]
fn cancel_sentinel_interrupted() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let sentinel_path = cwd.join(".fno/.target-cancelled");

    fs::write(
        &manifest_path,
        new_manifest("sess-cancel", "2026-06-04T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    fs::write(&sentinel_path, "").unwrap(); // mtime = now, after created_at

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
    assert!(
        !sentinel_path.exists(),
        "a fired sentinel must be consumed, not left to re-fire on every later stop"
    );
}

/// Verify-by-marker: an attributed sentinel makes the Interrupted line NAME
/// who cancelled and why; asserting only that the run stops would prove the
/// mechanism that already worked. The sentinel is still consumed.
#[test]
fn cancel_sentinel_interrupted_names_author_and_reason() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let sentinel_path = cwd.join(".fno/.target-cancelled");

    fs::write(
        &manifest_path,
        new_manifest("sess-cancel-attributed", "2026-06-04T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    fs::write(
        &sentinel_path,
        "author: operator\nreason: wrong direction\n",
    )
    .unwrap(); // mtime = now, after created_at

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.decision, "allow");
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
    assert!(
        d.message.contains("author: operator") && d.message.contains("reason: wrong direction"),
        "the Interrupted line must name who cancelled and why; got: {}",
        d.message
    );
    assert!(
        !sentinel_path.exists(),
        "an attributed fired sentinel is consumed like any other"
    );

    let events_content = fs::read_to_string(project_events(&cwd)).unwrap();
    assert!(
        events_content.contains("\"cancel_author\":\"operator\"")
            && events_content.contains("\"cancel_reason\":\"wrong direction\""),
        "termination event must carry the attribution"
    );
}

/// An empty (bare-touch) sentinel still terminates but reads as
/// unattributed: the line says no author is recorded rather than inventing one.
#[test]
fn cancel_sentinel_empty_reads_unattributed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let sentinel_path = cwd.join(".fno/.target-cancelled");

    fs::write(
        &manifest_path,
        new_manifest("sess-cancel-bare", "2026-06-04T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    fs::write(&sentinel_path, "").unwrap();

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
    assert!(
        d.message.contains("no author recorded"),
        "an empty sentinel must read as unattributed; got: {}",
        d.message
    );
}

/// The tombstone is never consumed by loop-check: it is session-keyed and
/// cleared by init, so a tombstone-fired cancel must survive the stop.
#[test]
fn cancel_tombstone_fires_but_is_not_consumed() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let tombstone_path = cwd.join(".fno/.target-cancelled-final");

    fs::write(
        &manifest_path,
        new_manifest("sess-cancel-tombstone", "2026-06-04T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    fs::write(&tombstone_path, "author: operator\n").unwrap();

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_eq!(d.termination_reason.as_deref(), Some("Interrupted"));
    assert!(
        tombstone_path.exists(),
        "the tombstone is cleared by init, never by the loop-check reader"
    );
}

/// A stale sentinel (mtime older than created_at) neither fires nor is
/// consumed - a newer session must not eat a cancel aimed past it.
#[test]
fn stale_sentinel_ignored_and_left_in_place() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let manifest_path = cwd.join("target-state.md");
    let transcript_path = cwd.join("transcript.jsonl");
    let sentinel_path = cwd.join(".fno/.target-cancelled");

    fs::write(
        &manifest_path,
        new_manifest("sess-stale", "2099-01-01T00:00:00Z", true),
    )
    .unwrap();
    fs::write(&transcript_path, transcript_empty()).unwrap();
    fs::write(&sentinel_path, "author: operator\n").unwrap();
    // The sentinel's real mtime (now) is older than the manifest's created_at
    // fixture above, which is exactly the stale-sentinel shape.

    let mock = MockBins::green();

    let (code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        transcript_path.to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T01:00:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);

    assert_eq!(code, 0);
    assert_ne!(
        d.termination_reason.as_deref(),
        Some("Interrupted"),
        "a stale sentinel must not cancel a fresh session"
    );
    assert!(
        sentinel_path.exists(),
        "an unfired sentinel is left for the session it was written for"
    );
}
