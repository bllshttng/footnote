//! Mux-server build drift: the pure half of quiet retirement.
//!
//! The mux server is a long-lived process. A rebuild of `fno` replaces the
//! on-disk binary, but the running server keeps executing its old code until
//! something restarts it, and only `fno agents restart --mux` reaches it, at
//! the cost of every live pane. This module is the drift *signal*: a
//! fingerprint of the executable the server is running, compared against the
//! binary a fresh attach would spawn now. The async half (capture at startup,
//! check on the 1s core tick, retire through `Flow::Shutdown` only when no
//! pane, client, or connection is live) lives in [`crate::server`].
//!
//! Mirrors `fno-agents`' `drift.rs` (same fingerprint shape, same fail-safe
//! posture: an unreadable exe is `Unknown`, never a false alarm) as a small
//! local copy because the `fno` crate does not depend on `fno-agents`.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

/// A running-or-on-disk executable's identity, for drift comparison. Same
/// shape as `fno-agents`' `ExeFingerprint`: canonical path, mtime nanos, size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExeFingerprint {
    /// Canonicalized absolute path of the executable.
    pub path: PathBuf,
    /// File mtime as nanoseconds since the Unix epoch.
    pub mtime_nanos: i64,
    /// File size in bytes.
    pub size: u64,
}

impl ExeFingerprint {
    /// Stat `path` (canonicalizing it) into a fingerprint. Returns `None` on
    /// any error; the caller treats `None` as `Unknown` (silent, never a
    /// false alarm).
    pub fn of(path: &std::path::Path) -> Option<ExeFingerprint> {
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

    /// Fingerprint this process's own executable. `None` if `current_exe()`
    /// or the stat fails; the server then classifies every later check
    /// `Unknown` and never retires on a guess.
    pub fn current() -> Option<ExeFingerprint> {
        ExeFingerprint::of(&std::env::current_exe().ok()?)
    }
}

/// The verdict of a build-drift check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftState {
    /// The running binary matches what a fresh attach would spawn now.
    Fresh,
    /// The on-disk binary differs (path or content) from the running one.
    Drifted {
        running: ExeFingerprint,
        on_disk: ExeFingerprint,
    },
    /// The check could not be completed (no captured fingerprint, a stat
    /// failure). Silent by design: never retire on a guess.
    Unknown,
}

/// Pure classification: compare the server's startup fingerprint against a
/// fresh stat of the binary it was launched from. A `None` on either side
/// yields `Unknown` (fail-safe); any path/mtime/size difference is `Drifted`.
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
        _ => DriftState::Unknown,
    }
}

/// Self-drift for the running server: startup fingerprint vs its own
/// executable path stat-ed now.
pub fn self_drift(startup: &ExeFingerprint) -> DriftState {
    classify(Some(startup), ExeFingerprint::of(&startup.path).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno_build_drift_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn write_file(path: &std::path::Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn of_missing_path_is_none() {
        assert!(ExeFingerprint::of(&tmp_path("missing")).is_none());
    }

    #[test]
    fn classify_fresh_when_equal_and_drifted_on_size_change() {
        let p = tmp_path("fresh");
        write_file(&p, b"bin");
        let fp = ExeFingerprint::of(&p).unwrap();
        assert_eq!(classify(Some(&fp), Some(&fp)), DriftState::Fresh);
        write_file(&p, b"bin by a newer build");
        match self_drift(&fp) {
            DriftState::Drifted { .. } => {}
            other => panic!("expected Drifted after rewrite, got {other:?}"),
        }
        fs::remove_file(&p).ok();
    }

    #[test]
    fn classify_path_drift_is_drifted() {
        let a = ExeFingerprint {
            path: PathBuf::from("/opt/a/fno"),
            mtime_nanos: 1,
            size: 1,
        };
        let b = ExeFingerprint {
            path: PathBuf::from("/opt/b/fno"),
            mtime_nanos: 1,
            size: 1,
        };
        assert!(matches!(
            classify(Some(&a), Some(&b)),
            DriftState::Drifted { .. }
        ));
    }

    #[test]
    fn classify_unknown_when_either_side_missing() {
        let fp = ExeFingerprint {
            path: PathBuf::from("/x"),
            mtime_nanos: 1,
            size: 1,
        };
        assert_eq!(classify(None, Some(&fp)), DriftState::Unknown);
        assert_eq!(classify(Some(&fp), None), DriftState::Unknown);
        assert_eq!(classify(None, None), DriftState::Unknown);
    }
}
