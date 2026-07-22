//! Daemon binary-version drift detection (ab-1891cdff).
//!
//! The `fno-agents` daemon is a long-lived process. A `cargo install` (or any
//! rebuild) replaces the on-disk binary, but the *running* daemon keeps
//! executing its old code until it idle-exits or is killed, silently stranding
//! new features. This module is the drift *signal*: a fingerprint of the
//! executable a process is running, compared against the binary a client would
//! launch right now.
//!
//! The signal is a running-exe fingerprint (canonical path + mtime + size), NOT
//! `CARGO_PKG_VERSION` (Locked Decision #1): the package version rarely bumps in
//! development, where many features land at the same `0.1.0`. The fingerprint
//! catches any reinstall/rebuild, including a same-version dev build.
//!
//! This file holds only the *pure* pieces (fingerprint + classification) so the
//! `DriftState` matrix is unit-testable without a live daemon. The async
//! daemon-status probe that feeds it lives in [`crate::client::check_daemon_drift`].

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// A running-or-on-disk executable's identity, for drift comparison. The path is
/// canonicalized (so symlink vs target, `~/.cargo/bin` vs `target/debug`, are
/// compared apples-to-apples); `mtime_nanos`/`size` are compared only for
/// equality against a fingerprint of the SAME logical binary, so coarse clocks
/// only ever cost an advisory false verdict, never a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeFingerprint {
    /// Canonicalized absolute path of the executable.
    pub path: PathBuf,
    /// File mtime as nanoseconds since the Unix epoch (i64 holds ~year 2262).
    pub mtime_nanos: i64,
    /// File size in bytes.
    pub size: u64,
}

impl ExeFingerprint {
    /// Stat `path` (canonicalizing it) into a fingerprint. Returns `None` on any
    /// error -- a missing file, a stat failure, or an mtime that does not fit an
    /// `i64` of nanoseconds. The caller treats `None` as `Unknown` (silent,
    /// never a false alarm); a drift check must never crash a `status`/`list`.
    pub fn of(path: &Path) -> Option<ExeFingerprint> {
        let canon = std::fs::canonicalize(path).ok()?;
        let meta = std::fs::metadata(&canon).ok()?;
        let mtime = meta.modified().ok()?;
        let nanos = mtime.duration_since(UNIX_EPOCH).ok()?.as_nanos();
        let mtime_nanos = i64::try_from(nanos).ok()?;
        Some(ExeFingerprint {
            path: canon,
            mtime_nanos,
            size: meta.len(),
        })
    }

    /// Fingerprint the current process's own executable. The daemon calls this
    /// once at startup to record what it is running; `None` if `current_exe()`
    /// or the stat fails (the daemon then reports no fingerprint, and every
    /// client check fails safe to `Unknown`).
    pub fn current() -> Option<ExeFingerprint> {
        ExeFingerprint::of(&std::env::current_exe().ok()?)
    }
}

/// The verdict of a drift check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftState {
    /// The running binary matches the binary a client would launch now.
    Fresh,
    /// The running binary differs (path OR content) from the on-disk launch
    /// target -- the daemon is stale and should be restarted.
    Drifted {
        running: ExeFingerprint,
        on_disk: ExeFingerprint,
    },
    /// No daemon is running; nothing can be stale. (Decided by the async wrapper
    /// before [`classify`] is called.)
    DaemonDown,
    /// The check could not be completed: a stat/`current_exe` error, or the
    /// daemon reported no fingerprint. Silent by design -- never a warning, so a
    /// drift-check failure never cries wolf.
    Unknown,
}

/// Pure classification: compare the daemon's reported `running` fingerprint to
/// the client's fresh `on_disk` fingerprint of the binary it would launch now.
///
/// A `None` on either side yields [`DriftState::Unknown`] (fail-safe). Otherwise
/// any difference in canonical path, mtime, or size is [`DriftState::Drifted`].
/// Path drift and content drift are both "drifted" -- the operator's remedy
/// (`fno agents restart`) is the same either way. [`DriftState::DaemonDown`] is
/// never produced here; the async wrapper decides it from the status probe.
pub fn classify(running: Option<&ExeFingerprint>, on_disk: Option<&ExeFingerprint>) -> DriftState {
    match (running, on_disk) {
        (Some(r), Some(d)) => {
            if r.path != d.path || r.mtime_nanos != d.mtime_nanos || r.size != d.size {
                DriftState::Drifted {
                    running: r.clone(),
                    on_disk: d.clone(),
                }
            } else {
                DriftState::Fresh
            }
        }
        // Daemon reported no fingerprint, or the on-disk stat failed: no basis to
        // prove drift, so stay silent rather than warn on a guess.
        _ => DriftState::Unknown,
    }
}

/// Format the operator-facing drift warning, or `None` when there is nothing to
/// warn about (`Fresh`/`DaemonDown`/`Unknown`). The message is advisory and
/// names the exact remedy verb. The caller routes it to **stderr** only, so a
/// `--json` stdout consumer is never contaminated (Locked Decision #5).
///
/// `pid` is the running daemon's pid when the caller has it (the `status`
/// surface does); it is woven into the message for a more actionable warning and
/// omitted otherwise.
pub fn drift_warning(state: &DriftState, pid: Option<u32>) -> Option<String> {
    match state {
        DriftState::Drifted { .. } => {
            let who = match pid {
                Some(p) => format!("the running daemon (pid {p})"),
                None => "the running daemon".to_string(),
            };
            Some(format!(
                "fno agents: {who} is an older build than the installed binary; \
                 run `fno agents restart` to pick up the new build."
            ))
        }
        DriftState::Fresh | DriftState::DaemonDown | DriftState::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("fno_drift_{}_{}_{tag}", std::process::id(), {
            use std::sync::atomic::{AtomicU32, Ordering};
            static C: AtomicU32 = AtomicU32::new(0);
            C.fetch_add(1, Ordering::Relaxed)
        }));
        p
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn of_missing_path_is_none() {
        // AC1-ERR: a stat that cannot resolve the file fails safe to None
        // (mapped to Unknown by classify), never a panic.
        let p = tmp_path("missing");
        assert!(ExeFingerprint::of(&p).is_none());
    }

    #[test]
    fn of_roundtrips_and_tracks_size_change() {
        let p = tmp_path("rt");
        write_file(&p, b"hello");
        let a = ExeFingerprint::of(&p).expect("fingerprint");
        let b = ExeFingerprint::of(&p).expect("fingerprint again");
        assert_eq!(a, b, "same file fingerprints equal");
        assert_eq!(a.size, 5);

        // A larger rewrite changes the size -> a distinct fingerprint, even if
        // the coarse mtime did not advance.
        write_file(&p, b"hello world!!");
        let c = ExeFingerprint::of(&p).expect("fingerprint after grow");
        assert_ne!(a, c, "size change yields a different fingerprint");
        assert_eq!(c.size, 13);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn classify_fresh_when_equal() {
        // AC1-FR: identical running/on-disk fingerprint -> Fresh, no warning.
        let p = tmp_path("fresh");
        write_file(&p, b"bin");
        let fp = ExeFingerprint::of(&p).unwrap();
        assert_eq!(classify(Some(&fp), Some(&fp)), DriftState::Fresh);
        assert_eq!(drift_warning(&DriftState::Fresh, Some(1)), None);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn classify_content_drift_when_size_differs() {
        // AC1-HP (classification half): same path, different content -> Drifted.
        let p = tmp_path("content");
        write_file(&p, b"old");
        let running = ExeFingerprint::of(&p).unwrap();
        let on_disk = ExeFingerprint {
            size: running.size + 7,
            ..running.clone()
        };
        match classify(Some(&running), Some(&on_disk)) {
            DriftState::Drifted { .. } => {}
            other => panic!("expected Drifted, got {other:?}"),
        }
        fs::remove_file(&p).ok();
    }

    #[test]
    fn classify_path_drift_when_path_differs() {
        // AC1-EDGE: running from a different path than we would launch -> Drifted.
        let a = ExeFingerprint {
            path: PathBuf::from("/opt/a/fno-agents-daemon"),
            mtime_nanos: 100,
            size: 10,
        };
        let b = ExeFingerprint {
            path: PathBuf::from("/home/u/.cargo/bin/fno-agents-daemon"),
            mtime_nanos: 100,
            size: 10,
        };
        match classify(Some(&a), Some(&b)) {
            DriftState::Drifted { .. } => {}
            other => panic!("expected Drifted, got {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_when_either_missing() {
        // AC1-ERR: a None on either side is Unknown (silent), never Drifted.
        let fp = ExeFingerprint {
            path: PathBuf::from("/x"),
            mtime_nanos: 1,
            size: 1,
        };
        assert_eq!(classify(None, Some(&fp)), DriftState::Unknown);
        assert_eq!(classify(Some(&fp), None), DriftState::Unknown);
        assert_eq!(classify(None, None), DriftState::Unknown);
        // And Unknown never warns.
        assert_eq!(drift_warning(&DriftState::Unknown, None), None);
        assert_eq!(drift_warning(&DriftState::DaemonDown, None), None);
    }

    #[test]
    fn drift_warning_names_restart_verb() {
        // AC1-HP (message half): a Drifted state warns, names the restart verb,
        // and weaves in the pid when present.
        let fp = ExeFingerprint {
            path: PathBuf::from("/x"),
            mtime_nanos: 1,
            size: 1,
        };
        let state = DriftState::Drifted {
            running: fp.clone(),
            on_disk: fp,
        };
        let msg = drift_warning(&state, Some(91627)).expect("warns on drift");
        assert!(msg.contains("fno agents restart"), "names the remedy verb");
        assert!(msg.contains("build"), "describes a build mismatch");
        assert!(msg.contains("91627"), "names the pid when known");
    }
}
