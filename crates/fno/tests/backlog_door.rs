//! The `fno backlog` door: the front door hands the whole backlog argv to
//! the sibling Rust binary byte-verbatim, and a missing or non-executable
//! sibling refuses with the install remedy instead of provisioning the
//! Python wheel.

use std::path::PathBuf;
use std::process::Command;

/// A stub sibling whose stdout is its argv, one token per line, exit 0. The
/// door must exec it with the UNTOUCHED backlog argv (the sibling's catalog
/// owns the spelling; the front door carries bytes, not meanings).
fn stub_sibling() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let stub = dir.path().join("fno-agents-stub");
    std::fs::write(
        &stub,
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, stub)
}

fn door(argv: &[&str], env: &[(&str, &PathBuf)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno"));
    cmd.args(argv);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("the fno binary runs")
}

#[test]
fn the_door_execs_the_sibling_with_the_backlog_argv_byte_verbatim() {
    let (_dir, stub) = stub_sibling();
    let out = door(
        &["backlog", "done", "x-abc12345", "--json"],
        &[("FNO_AGENTS_BIN", &stub)],
    );
    assert!(out.status.success(), "stub exited 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let tokens: Vec<&str> = stdout.lines().collect();
    assert_eq!(tokens, vec!["backlog", "done", "x-abc12345", "--json"]);
}

#[test]
fn a_missing_sibling_refuses_with_the_install_remedy_and_exits_2() {
    let missing = PathBuf::from("/nonexistent/fno-agents");
    let out = door(&["backlog", "version"], &[("FNO_AGENTS_BIN", &missing)]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "usage exit for a missing sibling"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not be exec'd"),
        "the refusal names the exec failure: {stderr}"
    );
    assert!(
        stderr.contains("FNO_AGENTS_BIN"),
        "the refusal names the override: {stderr}"
    );
    // The backlog door never walks the bootstrap provisioning path: its
    // one-time uv install banner never prints here.
    assert!(
        !stderr.contains("installing the standalone uv"),
        "the backlog door never provisions: {stderr}"
    );
}
