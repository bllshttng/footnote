//! An unreadable claims root is an error, never an empty claim list (x-636f).
//!
//! `claims::list`/`list_in` swallowed a failed `read_dir` into
//! `unwrap_or_default()`, so `claim list --json` printed `[]` and exited 0 on
//! a root the process cannot read: a failed read dressed as an empty fleet.
//! A missing directory is a legitimate empty root.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn claims_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("x636f-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn write_live_lock(dir: &std::path::Path) {
    let now = fno_agents::claims::now_ms();
    let yaml = format!(
        "schema_version: 1\nkey: \"node:x-636f\"\nholder: \"target-session:t-636f\"\nacquired_at: {now}\npid: 1\nhost: test-host\nexpires_at: {}\nreason: \"x-636f unreadable-root fixture\"\n",
        now + 900_000
    );
    std::fs::write(dir.join("node%3Ax-636f.lock"), yaml).expect("write lock");
}

fn set_mode(dir: &std::path::Path, mode: u32) {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn an_unreadable_claims_dir_is_an_error_not_an_empty_list() {
    if running_as_root() {
        return; // root reads through mode 000; the assertion cannot fire
    }
    let dir = claims_dir("list");
    write_live_lock(&dir);
    set_mode(&dir, 0o000);
    let read = fno_agents::claims::list_in(&[dir.clone()], None, true);
    set_mode(&dir, 0o755);
    let err = read.expect_err("mode-000 dir must be Err");
    assert!(
        err.contains(&dir.display().to_string()),
        "error names the unreadable dir: {err}"
    );
    // A missing directory is a legitimate empty root, not a fault.
    let missing = dir.parent().expect("parent").join("no-such-claims-dir");
    assert_eq!(
        fno_agents::claims::list_in(&[missing], None, true).expect("missing dir is Ok"),
        Vec::new()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn claim_list_exits_nonzero_on_an_unreadable_root() {
    if running_as_root() {
        return; // root reads through mode 000; the assertion cannot fire
    }
    let root = claims_dir("cli-list");
    let dir = root.join(".fno/claims");
    std::fs::create_dir_all(&dir).expect("mkdir");
    write_live_lock(&dir);
    set_mode(&dir, 0o000);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.args(["claim", "list", "--json", "--include-stale"])
        .env("FNO_CLAIMS_ROOT", &root)
        .env("HOME", &root);
    let out = cmd.output().expect("run claim list");
    set_mode(&dir, 0o755);
    assert!(
        !out.status.success(),
        "claim list must exit non-zero: {}",
        out.status
    );
    assert!(
        out.stdout.is_empty(),
        "no stdout on a failed read: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&root.display().to_string()),
        "stderr names the root: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn claim_sweep_exits_nonzero_on_an_unreadable_dir() {
    if running_as_root() {
        return; // root reads through mode 000; the assertion cannot fire
    }
    let root = claims_dir("cli-sweep");
    let dir = root.join("claims");
    std::fs::create_dir_all(&dir).expect("mkdir");
    write_live_lock(&dir);
    set_mode(&dir, 0o000);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.args(["claim", "sweep", "--json", "--all", "--claims-dir"])
        .arg(&dir)
        .env("HOME", &root);
    let out = cmd.output().expect("run claim sweep");
    set_mode(&dir, 0o755);
    assert!(
        !out.status.success(),
        "claim sweep must exit non-zero: {}",
        out.status
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("\"claims\":[]"),
        "never renders an empty-fleet success: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("claims root"),
        "stderr names the fault: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
