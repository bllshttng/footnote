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

/// Serializes the tests that mutate process env. The lib's
/// `claims::test_env_lock` is #[cfg(test)]-gated, so an integration binary
/// carries its own.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "fno-ask-create-{}-{}-{}",
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
if [ -n "$FAKE_CLAUDE_ARGV_DUMP" ]; then printf '%s\n' "$@" > "$FAKE_CLAUDE_ARGV_DUMP"; fi
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
    // Name the variant we got. This assertion used to discard `err`, so when it
    // failed once under a loaded CI runner the log could only say "not Parse" -
    // and Subprocess{127} (the fake claude failed to exec) reads nothing like
    // Parse but is indistinguishable from it in a bare `matches!`.
    assert!(
        matches!(err, AskError::Parse { .. }),
        "expected a parse error, got: {err}"
    );
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

#[test]
fn create_unrouted_child_floors_the_inherited_route_stamp() {
    // AC7-HP: ambient stamp, no route in the overlay. The serving
    // session forks with the SUPERVISOR's env, so only a settings file
    // reaches it: the argv must splice a --settings floor that empties the
    // stamp for this one session.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let stamp = fno_agents::codex_route::ROUTE_PROVIDER_ENV;
    let prior = std::env::var(stamp).ok();
    std::env::set_var(stamp, "zai");
    let bin = tmpdir("stamp-floor");
    install_fake_claude(&bin);
    let cwd = tmpdir("stamp-floor-cwd");
    let dump = cwd.join("argv_dump.txt");
    let path = path_with(&bin);
    let res = bg_create(
        "gina",
        "hi",
        &cwd,
        None,
        &[
            ("PATH", path.as_str()),
            ("FAKE_CLAUDE_ARGV_DUMP", dump.to_str().unwrap()),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    );
    match prior {
        Some(v) => std::env::set_var(stamp, v),
        None => std::env::remove_var(stamp),
    }
    let res = res.expect("bg_create succeeds under the floor");
    assert_eq!(res.short_id, "7c5dcf5d");
    let argv = fs::read_to_string(&dump).unwrap();
    let mut it = argv.lines();
    let mut settings = None;
    while let Some(a) = it.next() {
        if a == "--settings" {
            settings = it.next();
        }
    }
    let settings = settings.unwrap_or_else(|| panic!("no --settings in argv: {argv}"));
    let raw = fs::read_to_string(settings).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        v["env"][stamp], "",
        "the floor must empty the inherited stamp: {raw}"
    );
}

#[test]
fn create_routed_overlay_keeps_its_stamp() {
    // AC8-EDGE: an overlay carrying the endpoint is a route; it owns the
    // slot, so no floor file empties its stamp.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let stamp = fno_agents::codex_route::ROUTE_PROVIDER_ENV;
    let prior = std::env::var(stamp).ok();
    std::env::set_var(stamp, "zai");
    let bin = tmpdir("stamp-routed");
    install_fake_claude(&bin);
    let cwd = tmpdir("stamp-routed-cwd");
    let dump = cwd.join("argv_dump.txt");
    let path = path_with(&bin);
    let res = bg_create(
        "hank",
        "hi",
        &cwd,
        None,
        &[
            ("PATH", path.as_str()),
            ("FAKE_CLAUDE_ARGV_DUMP", dump.to_str().unwrap()),
            ("ANTHROPIC_BASE_URL", "https://api.z.ai/api/anthropic"),
            (stamp, "zai"),
        ],
        None,
        None,
        None,
        fno_agents::claude_ask::HarnessFlags::default(),
    );
    match prior {
        Some(v) => std::env::set_var(stamp, v),
        None => std::env::remove_var(stamp),
    }
    let res = res.expect("bg_create succeeds on the routed lane");
    assert_eq!(res.short_id, "7c5dcf5d");
    let argv = fs::read_to_string(&dump).unwrap();
    assert!(
        !argv.contains("--settings"),
        "a routed overlay must not get a scrub floor: {argv}"
    );
}
