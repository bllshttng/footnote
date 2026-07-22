//! Cross-language advisory-lock interop (Wave 3, US6.12 — the load-bearing kill
//! criterion `cross_language_flock_test`).
//!
//! Rust's `File::lock` (std, stable since 1.89) and Python's `fcntl.flock` both
//! wrap `flock(2)` (BSD advisory locks). If they did NOT interoperate, the coupling
//! discipline's "one writer per file" rule would be unenforceable across the
//! Python `fno` <-> Rust `fno-agents` boundary. This test proves they do:
//!
//! - AC12-HP: Python acquires, Rust waits and succeeds on release.
//! - AC12-ERR: Rust holds LOCK_EX, Python's non-blocking acquire fails.
//!
//! Skips cleanly (does not fail) when `python3` is unavailable, so a Rust-only
//! dev box still passes; CI for this Python-first project always has python3.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

fn python3() -> Option<String> {
    for cand in ["python3", "python"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(cand.to_string());
        }
    }
    None
}

fn short_lock_path() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!("/tmp/fnoflk{}_{}.lock", std::process::id(), n))
}

/// Run a python snippet that tries to flock `path` non-blocking, printing
/// `GOT` on success or `BLOCKED` on `BlockingIOError`.
fn python_try_lock(py: &str, path: &std::path::Path, blocking: bool) -> String {
    let mode = if blocking {
        "fcntl.LOCK_EX"
    } else {
        "fcntl.LOCK_EX | fcntl.LOCK_NB"
    };
    let script = format!(
        r#"
import fcntl, sys
f = open({path:?}, "a+")
try:
    fcntl.flock(f.fileno(), {mode})
    print("GOT")
    fcntl.flock(f.fileno(), fcntl.LOCK_UN)
except BlockingIOError:
    print("BLOCKED")
"#,
        path = path.to_string_lossy(),
        mode = mode,
    );
    let out = Command::new(py)
        .arg("-c")
        .arg(script)
        .output()
        .expect("python runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn rust_lock_blocks_python_then_releases() {
    let py = match python3() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: python3 not found; cross-language flock interop not verified here");
            return;
        }
    };
    let path = short_lock_path();
    // Touch the file so both sides open the same inode.
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let _ = f.write_all(b"");
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();

    // Rust holds LOCK_EX.
    file.lock().unwrap();

    // AC12-ERR: Python's non-blocking acquire must fail while Rust holds it.
    let while_held = python_try_lock(&py, &path, false);
    assert_eq!(
        while_held, "BLOCKED",
        "Python acquired a lock Rust was holding — flock interop is BROKEN"
    );

    // AC12-HP: release (unlock + close the fd, the unambiguous release), then
    // Python (blocking) acquires successfully. We close the fd because a release
    // seen cross-language is the invariant that matters; an `unlock` that lingers
    // until close on some kernels would still be a valid release at close.
    file.unlock().unwrap();
    drop(file);
    std::thread::sleep(Duration::from_millis(50));
    let after_release = python_try_lock(&py, &path, true);
    assert_eq!(
        after_release, "GOT",
        "Python could not acquire after Rust released — flock interop is BROKEN"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn python_lock_blocks_rust_nonblocking() {
    let py = match python3() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: python3 not found");
            return;
        }
    };
    let path = short_lock_path();
    {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
    }

    // Python holds LOCK_EX for ~1s in a background process.
    let script = format!(
        r#"
import fcntl, time
f = open({path:?}, "a+")
fcntl.flock(f.fileno(), fcntl.LOCK_EX)
time.sleep(1.0)
fcntl.flock(f.fileno(), fcntl.LOCK_UN)
"#,
        path = path.to_string_lossy()
    );
    let mut child = Command::new(&py).arg("-c").arg(script).spawn().unwrap();
    // Give python time to acquire.
    std::thread::sleep(Duration::from_millis(300));

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    // Rust's non-blocking acquire must fail while Python holds the lock.
    // std's `try_lock` returns `Err(TryLockError::WouldBlock)` when held, so
    // `is_err()` is the same contended-acquire signal fs2's variant gave.
    let got = file.try_lock();
    assert!(
        got.is_err(),
        "Rust acquired a lock Python was holding — flock interop is BROKEN"
    );

    // After python releases, Rust acquires (blocking) successfully.
    let _ = child.wait();
    file.lock()
        .expect("Rust should acquire after Python releases");
    let _ = file.unlock();

    std::fs::remove_file(&path).ok();
}
