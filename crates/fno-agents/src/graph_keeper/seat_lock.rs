//! The keeper seat: the exclusive `<sock>.lock` flock a store keeper holds
//! for its life. Split from graph_keeper.rs for the file budget: the file
//! crossed the shrink-only line on main, and this change touched the
//! protocol, so the protocol moved with it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Seat-ladder pacing, mirroring daemon.rs's LOCK_ACQUIRE_* shape: a probe
/// holds the seat lock for microseconds, an incumbent for life, and only
/// duration separates them.
const SEAT_LOCK_ATTEMPTS: usize = 6;
const SEAT_LOCK_RETRY: Duration = Duration::from_millis(25);

fn seat_lock_path(sock: &Path) -> PathBuf {
    let mut s = sock.as_os_str().to_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

/// Take the exclusive seat flock on `<sock>.lock`, held for the process
/// life (the returned File keeps it). `None` = the seat is owned: the
/// daemon's bind_supervisor_socket rule, applied to the store.
pub(super) fn take_seat(sock: &Path) -> Option<std::fs::File> {
    let lock_path = seat_lock_path(sock);
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Two open-and-lock rounds: the daemon's startup sweep unlinks an
    // orphaned lock while holding its own flock, so a keeper that opened
    // the file just before that unlink would otherwise succeed on an
    // unnamed inode and share the seat with a replacement. Re-stat the
    // path after locking: anything but the exact inode we hold means the
    // name moved, and one fresh reopen settles it; a second mismatch
    // means the name is being cycled faster than we can claim it, which
    // reads as the seat being owned.
    for _round in 0..2 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .ok()?;
        let mut name_moved = false;
        for attempt in 0..SEAT_LOCK_ATTEMPTS {
            match file.try_lock() {
                Ok(()) => {
                    use std::os::unix::fs::MetadataExt;
                    let named_ok = std::fs::metadata(&lock_path)
                        .ok()
                        .zip(file.metadata().ok())
                        .is_some_and(|(named, held)| {
                            named.dev() == held.dev() && named.ino() == held.ino()
                        });
                    if named_ok {
                        return Some(file);
                    }
                    name_moved = true;
                    break;
                }
                Err(e) => {
                    let io_err: std::io::Error = e.into();
                    if io_err.kind() != std::io::ErrorKind::WouldBlock {
                        return None;
                    }
                    if attempt + 1 < SEAT_LOCK_ATTEMPTS {
                        std::thread::sleep(SEAT_LOCK_RETRY * (attempt as u32 + 1));
                    }
                }
            }
        }
        if !name_moved {
            return None; // the lock never came free: the seat is owned
        }
    }
    None
}
