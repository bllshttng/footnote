//! Advisory cross-process lock serializing the two plan-doc writers, ported
//! 1:1 from `cli/src/fno/plan/locking.py`.
//!
//! The lock is a sidecar flock, NEVER the plan file's own fd: both writers
//! finish with rename, which swaps the plan's inode out from under any lock
//! held on that inode. Keying a separate lockfile on the plan's resolved path
//! avoids that. Advisory only - human editors are unguarded by design.
//!
//! Config-free (`$HOME/.fno/locks`), same as `fno.paths.locks_dir()`, so the
//! Python body-append writer and the Rust writers agree without config.

use sha1::{Digest, Sha1};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `$HOME/.fno/locks`, deliberately config-free.
pub fn locks_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".fno").join("locks")
}

/// `~/.fno/locks/plan-<sha1(resolved path)>.lock`.
pub fn lock_path_for(plan_path: &Path) -> PathBuf {
    let resolved = fs::canonicalize(plan_path).unwrap_or_else(|_| {
        // Python resolve(strict=False): normalize without requiring existence.
        let abs = if plan_path.is_absolute() {
            plan_path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|d| d.join(plan_path))
                .unwrap_or_else(|_| plan_path.to_path_buf())
        };
        let mut out = PathBuf::new();
        for comp in abs.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                comp => out.push(comp.as_os_str()),
            }
        }
        out
    });
    let mut h = Sha1::new();
    h.update(resolved.to_string_lossy().as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    locks_dir().join(format!("plan-{hex}.lock"))
}

/// RAII lock file. Unlock happens on drop; the fd close releases the flock.
pub struct PlanDocLock {
    file: fs::File,
    lock_path: PathBuf,
}

impl PlanDocLock {
    /// Hold an exclusive advisory lock keyed on the plan doc's resolved path,
    /// polling until `timeout`, then an error. The lock is held only for one
    /// whole-file rewrite, so contention past the timeout is not expected.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self, String> {
        let lock_path = lock_path_for(path);
        fs::create_dir_all(locks_dir())
            .map_err(|e| format!("plan_doc_lock: mkdir {}: {e}", locks_dir().display()))?;
        let deadline = Instant::now() + timeout;
        let mut file = None;
        while file.is_none() {
            let opened = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| format!("plan_doc_lock: open {}: {e}", lock_path.display()))?;
            loop {
                match opened.try_lock() {
                    Ok(()) => {
                        file = Some(opened);
                        break;
                    }
                    Err(_) => {
                        if Instant::now() >= deadline {
                            return Err(format!(
                                "plan_doc_lock: {} busy > {}s",
                                lock_path.display(),
                                timeout.as_secs_f64()
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
        let guard = PlanDocLock {
            file: file.unwrap(),
            lock_path: lock_path.clone(),
        };
        if guard.same_lock_inode() {
            Ok(guard)
        } else {
            drop(guard);
            // The lockfile was swapped under us (reap or repair); retry until
            // the deadline, mirroring the Python unlink-then-reacquire loop.
            if Instant::now() >= deadline {
                return Err(format!(
                    "plan_doc_lock: {} busy > {}s",
                    lock_path.display(),
                    timeout.as_secs_f64()
                ));
            }
            Self::acquire(path, deadline.saturating_duration_since(Instant::now()))
        }
    }

    fn same_lock_inode(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let opened = match self.file.metadata() {
            Ok(m) => m,
            Err(_) => return false,
        };
        let current = match fs::metadata(&self.lock_path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        opened.dev() == current.dev() && opened.ino() == current.ino()
    }
}

impl Drop for PlanDocLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-plan-doc-lock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn acquires_and_releases() {
        let dir = tmp_dir("basic");
        let plan = dir.join("plan.md");
        fs::write(&plan, "x").unwrap();
        {
            let _guard = PlanDocLock::acquire(&plan, Duration::from_secs(2)).unwrap();
        }
        // After release, a second acquire succeeds.
        let _again = PlanDocLock::acquire(&plan, Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn nested_acquire_from_same_process_times_out() {
        // flock is per-fd: a second fd on the same lockfile cannot get LOCK_EX.
        let dir = tmp_dir("nested");
        let plan = dir.join("plan.md");
        fs::write(&plan, "x").unwrap();
        let _outer = PlanDocLock::acquire(&plan, Duration::from_secs(2)).unwrap();
        let nested = match PlanDocLock::acquire(&plan, Duration::from_millis(100)) {
            Err(e) => e,
            Ok(_) => panic!("nested acquire should have timed out"),
        };
        assert!(nested.contains("busy"));
    }

    #[test]
    fn lock_path_is_sha1_of_resolved_path() {
        let dir = tmp_dir("path");
        let plan = dir.join("plan.md");
        fs::write(&plan, "x").unwrap();
        let a = lock_path_for(&plan);
        let b = lock_path_for(&plan);
        assert_eq!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("plan-") && name.ends_with(".lock"));
        let hex = &name["plan-".len()..name.len() - ".lock".len()];
        assert_eq!(hex.len(), 40);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
