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
/// so parallel tests never share a tree. The `fnoec` prefix is disjoint from
/// daemon_e2e's `fnoe` homes: this module's counter is independent of that
/// file's, and same-binary same-path collisions (a schema-24 seed landing in
/// another test's home) are exactly what the prefix prevents.
pub fn short_home() -> fno_agents::paths::AgentsHome {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    fno_agents::paths::AgentsHome::at(std::path::PathBuf::from(format!(
        "/tmp/fnoec{}_{}",
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

// -------------------------------------------------------------------------
// Golden capture: step 2 of the port protocol
// (docs/architecture/dual-implementation-inventory.md), shared by every
// `*_parity.rs` that freezes a Rust port against goldens. Under
// FNO_CAPTURE_GOLDEN=1 the helper runs the OLD leg on the same fixture,
// asserts Rust==old before freezing, and writes the goldens; in normal mode
// the frozen golden IS the contract and the old leg never runs.
// -------------------------------------------------------------------------

/// Slug a case label into a filename-safe golden key (lowercase, every run of
/// non-alphanumeric chars collapses to a single `_`, trimmed).
pub fn slug(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_us = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Directory holding the frozen goldens for one subject
/// (`tests/golden/<subject>/`).
pub fn golden_dir(subject: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(subject)
}

/// Whether to (re)capture goldens from the live old leg this run.
pub fn capture_mode() -> bool {
    std::env::var("FNO_CAPTURE_GOLDEN").is_ok()
}

/// One frozen case: an optional exit code plus ordered text streams.
///
/// The exit freezes as `<key>.exit` (`"<code>\n"`); stream `i` freezes as
/// `<key>.out` then `<key>.err` by position. This is the on-disk format the
/// first two ports froze, so the kill_criteria and verify_evidence goldens
/// stay byte-compatible through the hoist.
pub struct Golden {
    pub exit: Option<i32>,
    pub streams: Vec<String>,
}

/// Assert the Rust outcome for one case against its frozen golden.
///
/// In capture mode `oracle` must carry the OLD leg's outcome on the same
/// fixture (the caller builds it under `capture_mode().then(|| ..)`, so the
/// old leg only runs in capture mode): the helper asserts Rust==old, then
/// freezes the ORACLE's values, so a broken capture is caught at freeze time.
/// In normal mode the oracle is never run and the golden read from disk is
/// the contract. `oracle` is `None` once the old leg is deleted; capture is
/// then a refusal, because a golden can only be captured from a live leg.
pub fn assert_golden(subject: &str, label: &str, rust: &Golden, oracle: Option<Golden>) {
    let key = slug(label);
    let dir = golden_dir(subject);

    if capture_mode() {
        let golden = oracle.unwrap_or_else(|| {
            panic!(
                "[{label}] FNO_CAPTURE_GOLDEN=1 but no old leg was wired for \
                 this case; a golden can only be captured while the old leg runs"
            )
        });
        assert_eq!(
            golden.exit, rust.exit,
            "[{label}] capture: exit differs oracle={:?} rust={:?}",
            golden.exit, rust.exit
        );
        assert_eq!(
            golden.streams.len(),
            rust.streams.len(),
            "[{label}] capture: stream count differs"
        );
        for (i, (o, r)) in golden.streams.iter().zip(rust.streams.iter()).enumerate() {
            assert_eq!(o, r, "[{label}] capture: stream {i} differs\noracle={o:?}\nrust={r:?}");
        }
        fs::create_dir_all(&dir).unwrap();
        if let Some(code) = golden.exit {
            fs::write(dir.join(format!("{key}.exit")), format!("{code}\n")).unwrap();
        }
        let suffixes = ["out", "err"];
        for (i, s) in golden.streams.iter().enumerate() {
            fs::write(dir.join(format!("{key}.{}", suffixes[i])), s).unwrap();
        }
        return;
    }

    let exit_path = dir.join(format!("{key}.exit"));
    let golden_exit = if exit_path.exists() {
        Some(
            fs::read_to_string(&exit_path)
                .unwrap_or_else(|e| panic!("[{label}] missing golden {exit_path:?}: {e}"))
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("[{label}] bad golden exit in {exit_path:?}: {e}")),
        )
    } else {
        None
    };
    let mut golden_streams = vec![
        fs::read_to_string(dir.join(format!("{key}.out")))
            .unwrap_or_else(|e| panic!("[{label}] missing golden {}.out: {e}", key)),
    ];
    let err_path = dir.join(format!("{key}.err"));
    if err_path.exists() {
        golden_streams.push(
            fs::read_to_string(&err_path)
                .unwrap_or_else(|e| panic!("[{label}] missing golden {err_path:?}: {e}")),
        );
    }

    assert_eq!(
        golden_exit, rust.exit,
        "[{label}] exit differs from golden: golden={golden_exit:?} rust={:?}",
        rust.exit
    );
    assert_eq!(
        golden_streams.len(),
        rust.streams.len(),
        "[{label}] stream count differs from golden"
    );
    for (i, (g, r)) in golden_streams.iter().zip(rust.streams.iter()).enumerate() {
        assert_eq!(
            g, r,
            "[{label}] stream {i} differs from golden:\ngolden={g:?}\nrust={r:?}"
        );
    }
}
