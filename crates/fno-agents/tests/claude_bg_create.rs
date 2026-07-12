//! Integration tests for `claude_ask::bg_create` against a fake `claude` (ab-cc926b4e).
//!
//! Ports the `_fake_claude.py` contract: a script emitting
//! `backgrounded · <8hex> · <name>` and honoring `FAKE_CLAUDE_*` env knobs.
//! `PATH` and the knobs are passed through `bg_create`'s `extra_env` so each
//! spawn is self-contained — no process-global env mutation, no test races.

use fno_agents::claude_ask::{bg_create, AskError};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "abi-ask-create-{}-{}-{}",
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

/// Write a fake `claude` script into `bin_dir` and make it executable.
/// The `·` is a literal U+00B7 so the stdout matches the short-id contract.
fn install_fake_claude(bin_dir: &Path) {
    let script = r#"#!/bin/sh
name=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--name" ]; then name="$a"; fi
  prev="$a"
done
if [ -n "$FAKE_CLAUDE_STDIN_DUMP" ]; then cat > "$FAKE_CLAUDE_STDIN_DUMP"; fi
if [ -n "$FAKE_CLAUDE_STDERR" ]; then printf '%s' "$FAKE_CLAUDE_STDERR" >&2; fi
if [ -n "$FAKE_CLAUDE_STDOUT" ]; then
  printf '%s' "$FAKE_CLAUDE_STDOUT"
else
  printf 'backgrounded · 7c5dcf5d · %s\n' "$name"
fi
exit "${FAKE_CLAUDE_EXIT:-0}"
"#;
    let path = bin_dir.join("claude");
    fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// PATH that finds the fake claude plus the standard tools the script needs.
fn path_with(bin_dir: &Path) -> String {
    format!("{}:/usr/bin:/bin", bin_dir.display())
}

#[test]
fn create_happy_parses_short_id() {
    let bin = tmpdir("happy");
    install_fake_claude(&bin);
    let cwd = tmpdir("happy-cwd");
    let path = path_with(&bin);
    let res = bg_create(
        "alice",
        "hello",
        &cwd,
        None,
        &[("PATH", path.as_str())],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    )
    .unwrap();
    assert_eq!(res.short_id, "7c5dcf5d");
    assert_eq!(res.stdout, "backgrounded \u{b7} 7c5dcf5d \u{b7} alice\n");
}

#[test]
fn create_nonzero_exit_without_confirmation_is_subprocess_error() {
    let bin = tmpdir("nonzero");
    install_fake_claude(&bin);
    let cwd = tmpdir("nonzero-cwd");
    let path = path_with(&bin);
    // A launch that FAILS prints no `backgrounded · <id>` confirmation. Override
    // stdout so the fake emits a non-contract line and exits nonzero: stdout
    // EOFs with no short-id (the NoId path), and bg_create reaps the real exit
    // code + stderr for a precise error.
    let err = bg_create(
        "bob",
        "hi",
        &cwd,
        None,
        &[
            ("PATH", path.as_str()),
            ("FAKE_CLAUDE_STDOUT", "error: failed to background\n"),
            ("FAKE_CLAUDE_EXIT", "3"),
            ("FAKE_CLAUDE_STDERR", "boom"),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    )
    .unwrap_err();
    match err {
        AskError::Subprocess { exit_code, stderr } => {
            assert_eq!(exit_code, 3);
            assert_eq!(stderr, "boom");
        }
        other => panic!("expected subprocess error, got {:?}", other),
    }
}

#[test]
fn create_confirmation_wins_over_late_nonzero_exit() {
    // The contract since PR #544: bg_create returns the instant the confirmation
    // line is seen and never waits for the launcher's exit code. So a launcher
    // that prints `backgrounded · <id>` and THEN exits nonzero is a SUCCESS --
    // the agent is backgrounded and registered, so the parent's late exit is
    // moot. (This is what makes the wait unhangable when the detached agent holds
    // the inherited stdout pipe open.)
    let bin = tmpdir("confirm-then-fail");
    install_fake_claude(&bin);
    let cwd = tmpdir("confirm-then-fail-cwd");
    let path = path_with(&bin);
    let res = bg_create(
        "frank",
        "hi",
        &cwd,
        None,
        &[
            ("PATH", path.as_str()),
            // default stdout = the real confirmation line; just exit nonzero after.
            ("FAKE_CLAUDE_EXIT", "3"),
            ("FAKE_CLAUDE_STDERR", "late-warning"),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    )
    .expect("a printed confirmation must be a success despite a later nonzero exit");
    assert_eq!(res.short_id, "7c5dcf5d");
}

#[test]
fn create_unparseable_stdout_is_parse_error() {
    let bin = tmpdir("parse");
    install_fake_claude(&bin);
    let cwd = tmpdir("parse-cwd");
    let path = path_with(&bin);
    let err = bg_create(
        "carol",
        "hi",
        &cwd,
        None,
        &[
            ("PATH", path.as_str()),
            ("FAKE_CLAUDE_STDOUT", "not the contract\n"),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    )
    .unwrap_err();
    assert!(matches!(err, AskError::Parse { .. }));
}

#[test]
fn create_argv_overflow_sends_message_via_stdin() {
    let bin = tmpdir("overflow");
    install_fake_claude(&bin);
    let cwd = tmpdir("overflow-cwd");
    let dump = cwd.join("stdin_dump.txt");
    let path = path_with(&bin);
    // > 200 KiB forces the stdin path.
    let big = "x".repeat(200 * 1024 + 10);
    let res = bg_create(
        "dave",
        &big,
        &cwd,
        None,
        &[
            ("PATH", path.as_str()),
            ("FAKE_CLAUDE_STDIN_DUMP", dump.to_str().unwrap()),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    )
    .unwrap();
    assert_eq!(res.short_id, "7c5dcf5d");
    let dumped = fs::read_to_string(&dump).unwrap();
    assert_eq!(dumped.len(), big.len());
    assert_eq!(dumped, big);
}

#[test]
fn create_missing_binary_is_127() {
    // PATH that does NOT contain the fake claude.
    let empty_bin = tmpdir("missing-bin");
    let cwd = tmpdir("missing-cwd");
    let path = format!("{}", empty_bin.display()); // no /usr/bin, no claude
    let err = bg_create(
        "erin",
        "hi",
        &cwd,
        Some(Duration::from_secs(5)),
        &[("PATH", path.as_str())],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    )
    .unwrap_err();
    match err {
        AskError::Subprocess { exit_code, .. } => assert_eq!(exit_code, 127),
        other => panic!("expected 127 subprocess error, got {:?}", other),
    }
}
