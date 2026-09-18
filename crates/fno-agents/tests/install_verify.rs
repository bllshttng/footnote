//! Probe classification, the poisoned-path inode repair, and its one-attempt
//! policy - AC1-HP through AC1-CLEANUP of x-6d44.

use fno_agents::install_verify::{probe_exec, repair_inode, verify_and_repair, Probe};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

const SHORT: Duration = Duration::from_millis(1500);
const TINY: Duration = Duration::from_millis(400);

/// Write `body` at `path` with mode `mode`, published atomically (temp
/// sibling + rename) so a probe never execs a partial file.
fn write_file(path: &Path, body: &str, mode: u32) {
    use std::fs;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body).unwrap();
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).unwrap();
    fs::rename(&tmp, path).unwrap();
}

fn version_script(rev: &str) -> String {
    format!("#!/bin/sh\necho '{{\"crates_rev\": \"{rev}\", \"dirty\": false}}'\n")
}

fn no_siblings_left(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .all(|e| !e.file_name().to_string_lossy().contains("inode-repair"))
}

fn ino(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().ino()
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions; it reads the real effective uid.
    unsafe { libc::geteuid() == 0 }
}

// ---- probe_exec classification ----

#[test]
#[cfg(unix)]
fn ok_probe_reports_rev_and_repairs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, &version_script("aabbcc"), 0o755);
    let before = ino(&bin);
    let out = verify_and_repair(&bin, SHORT, probe_exec);
    assert_eq!(out.rev.as_deref(), Some("aabbcc"));
    assert_eq!(out.instrument_error, None);
    assert_eq!(out.repaired_after, None);
    // AC1-HP: the file's inode and mode are unchanged.
    let meta = std::fs::metadata(&bin).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o755);
    assert_eq!(ino(&bin), before);
    assert!(no_siblings_left(dir.path()));
}

#[test]
#[cfg(unix)]
fn mode_644_script_spawn_fails_then_repair_execs_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, &version_script("cafe01"), 0o644);
    let before = ino(&bin);
    let out = verify_and_repair(&bin, SHORT, probe_exec);
    // AC1-REPAIR via the spawn_failed flavor: repair ran, re-probe answered.
    assert_eq!(out.rev.as_deref(), Some("cafe01"));
    assert_eq!(out.repaired_after.as_deref(), Some("spawn_failed"));
    assert_eq!(out.instrument_error, None);
    assert_ne!(ino(&bin), before);
    assert!(no_siblings_left(dir.path()));
}

#[test]
#[cfg(unix)]
fn killed_script_repairs_then_names_both_classifications_when_still_broken() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, "#!/bin/sh\nkill -9 $$\n", 0o755);
    let before = ino(&bin);
    let out = verify_and_repair(&bin, SHORT, probe_exec);
    // The repair RAN (fresh inode) and the re-probe still died by signal:
    // the error names both classifications, and the row may not read fresh.
    assert_ne!(ino(&bin), before);
    let err = out.instrument_error.as_deref().unwrap();
    assert!(err.contains("killed by signal 9"), "{err}");
    assert!(err.contains("after inode repair for signal(9)"), "{err}");
    assert_eq!(out.rev, None);
    assert!(no_siblings_left(dir.path()));
}

#[test]
#[cfg(unix)]
fn exit_137_earns_one_repair_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, "#!/bin/sh\nexit 137\n", 0o755);
    let before = ino(&bin);
    let out = verify_and_repair(&bin, SHORT, probe_exec);
    // The repair RAN (fresh inode) and the re-probe still failed clean:
    // both classifications are named, and the verdict may not read fresh.
    assert_ne!(ino(&bin), before);
    let err = out.instrument_error.as_deref().unwrap();
    assert!(err.contains("exited 137"), "{err}");
    assert!(err.contains("after inode repair for exit(137)"), "{err}");
    assert_eq!(out.rev, None);
    assert!(no_siblings_left(dir.path()));
}

#[test]
#[cfg(unix)]
fn timeout_earns_no_repair() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, "#!/bin/sh\nsleep 5\n", 0o755);
    let before = ino(&bin);
    let out = verify_and_repair(&bin, TINY, probe_exec);
    let err = out.instrument_error.as_deref().unwrap();
    assert!(err.contains("hung on `version --json`"), "{err}");
    assert_eq!(out.rev, None);
    assert_eq!(out.repaired_after, None);
    assert_eq!(ino(&bin), before);
    assert!(no_siblings_left(dir.path()));
}

#[test]
#[cfg(unix)]
fn clean_nonzero_exit_with_output_earns_no_repair() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, "#!/bin/sh\necho hello\nexit 5\n", 0o755);
    let before = ino(&bin);
    let out = verify_and_repair(&bin, SHORT, probe_exec);
    let err = out.instrument_error.as_deref().unwrap();
    assert!(err.contains("exited 5"), "{err}");
    assert_eq!(out.repaired_after, None);
    assert_eq!(ino(&bin), before);
    assert!(no_siblings_left(dir.path()));
}

#[test]
#[cfg(unix)]
fn repair_against_the_running_executable_succeeds() {
    use std::cell::Cell;
    let bin = std::env::current_exe().unwrap();
    let before = ino(&bin);
    // Fabricate the kill-shaped first classification; the re-probe really
    // execs the (test) binary, which answers nothing - the assertion is
    // that copy+rename succeed against the live path and the re-probe runs.
    let calls = Cell::new(0usize);
    let out = verify_and_repair(&bin, SHORT, |p, t| {
        if calls.replace(calls.get() + 1) == 0 {
            Probe::Signal(9)
        } else {
            probe_exec(p, t)
        }
    });
    assert_ne!(ino(&bin), before);
    assert!(out.instrument_error.is_some()); // test binary speaks no version --json
    assert!(!out
        .instrument_error
        .as_deref()
        .unwrap()
        .contains("inode repair failed"));
    // The test process itself is unaffected: getting here proves it.
}

#[test]
#[cfg(unix)]
fn repair_failure_in_a_read_only_dir_unlinks_the_sibling_and_names_the_io_error() {
    if running_as_root() {
        // Root ignores directory permissions, so the failure is unreachable.
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fno-agents");
    write_file(&bin, &version_script("deadbe"), 0o755);
    let before = ino(&bin);
    let before_bytes = std::fs::read(&bin).unwrap();
    let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(dir.path(), perms).unwrap();
    let err = repair_inode(&bin).unwrap_err();
    // Restore the dir so tempdir's drop can clean up.
    let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(dir.path(), perms).unwrap();
    assert!(err.contains("for inode repair failed"), "{err}");
    assert_eq!(ino(&bin), before);
    assert_eq!(std::fs::read(&bin).unwrap(), before_bytes);
    assert!(no_siblings_left(dir.path()));
}
