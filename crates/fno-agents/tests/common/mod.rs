//! Shared helpers for the review-coverage integration tests. A directory
//! module, not a test target of its own.

use std::fs;
use std::path::{Path, PathBuf};

/// Write an executable stub script.
///
/// Published atomically: the body is written to a temp sibling, chmod'd,
/// then renamed onto the final path (same fix as tests/loop_check.rs). The
/// published path is complete and closed from birth, so an exec can never
/// hit ETXTBSY (needs a write-open fd on the inode, including via a fork
/// that inherited one) or a partial file - no probe-exec loop required.
pub fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&tmp).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&tmp, perms).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

// -------------------------------------------------------------------------
// x-a879 repro helpers: a daemon under a throwaway home, and the
// loss-shaped registry the restart tests seed. The home is always a fresh
// short /tmp tree; the live `~/.fno/agents` is never touched.
// -------------------------------------------------------------------------

pub const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-daemon");

/// Short home root (Unix-socket `sun_path` is ~104 bytes), pid+counter keyed
/// so parallel tests never share a tree.
pub fn short_home() -> fno_agents::paths::AgentsHome {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    fno_agents::paths::AgentsHome::at(std::path::PathBuf::from(format!(
        "/tmp/fnoe{}_{}",
        std::process::id(),
        n
    )))
}

/// A daemon started by a test, killed and reaped when dropped.
pub struct DaemonChild(pub std::process::Child);

impl Drop for DaemonChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait until `path` exists (the daemon's supervisor socket).
pub fn wait_for_path(path: &Path, budget: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("path never appeared within {budget:?}: {}", path.display());
}

/// Wait until the daemon's event log carries `needle`.
pub fn wait_for_event(
    home: &fno_agents::paths::AgentsHome,
    needle: &str,
    budget: std::time::Duration,
) {
    let start = std::time::Instant::now();
    while start.elapsed() < budget {
        if fs::read_to_string(home.events_jsonl())
            .unwrap_or_default()
            .contains(needle)
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("event never appeared within {budget:?}: {needle}");
}

/// Spawn the daemon under `home` and wait for socket + start event. Idle-exit
/// is disabled so a daemon outliving its test cannot strand the suite.
pub fn start_daemon(home: &fno_agents::paths::AgentsHome) -> DaemonChild {
    let mut cmd = std::process::Command::new(DAEMON_BIN);
    cmd.env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600");
    let child = cmd.spawn().expect("daemon spawns");
    wait_for_path(&home.supervisor_sock(), std::time::Duration::from_secs(10));
    wait_for_event(home, "daemon_started", std::time::Duration::from_secs(10));
    DaemonChild(child)
}

/// 29 rows shaped like the 26 the 2026-09-01 registry loss took: claude and
/// codex rows, with and without `harness_session_id`, pane rows carrying a
/// `pane_id`, created_at stamped before the loss window. The count the file
/// held before the loss.
pub fn loss_shaped_rows() -> Vec<String> {
    (0..29)
        .map(|i| {
            let harness = if i % 3 == 0 { "codex" } else { "claude" };
            let sid = if i % 5 == 4 {
                "null".to_string()
            } else {
                format!("\"sess-{i:04}\"")
            };
            let pane = if i % 7 == 0 {
                format!(r#""pane_id":{i},"#)
            } else {
                String::new()
            };
            format!(
                r#"{{"name":"row-{i:02}","short_id":"row-{i:02}-id","harness":"{harness}","harness_session_id":{sid},{pane}"cwd":"/tmp/loss/row-{i:02}","log_path":"/tmp/loss/row-{i:02}.log","created_at":"2026-09-01T15:{i:02}:00Z","status":"live"}}"#
            )
        })
        .collect()
}

/// Write the loss-shaped registry under `home` and return its path.
///
/// Seeded at the CURRENT schema: a source-run daemon (this test build) is
/// refused any schema bump of its own FNO_AGENTS_HOME store, so a v20 seed
/// would fail the sweep's first write - the guard firing, not the incident
/// replaying. The old-version refusal has its own test below.
pub fn seed_loss_shaped_registry(home: &fno_agents::paths::AgentsHome) -> PathBuf {
    fs::create_dir_all(home.root()).unwrap();
    let path = home.registry_json();
    fs::write(
        &path,
        format!(
            r#"{{"schema_version":{},"agents":[{}]}}"#,
            fno_agents::state::REGISTRY_SCHEMA_VERSION,
            loss_shaped_rows().join(",")
        ),
    )
    .unwrap();
    path
}
