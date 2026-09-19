//! Stale store-socket hygiene: the store keeper unlinks its socket
//! on every clean exit, so a socket file nobody answers is a kill -9
//! leftover. The graph client self-heals a dead socket (its
//! connect-before-bind removes the stale file and rebinds), so this walk is
//! tidiness plus an honest dead count, never liveness authority: a socket
//! with a live listener is left exactly as found, and an unreadable one is
//! left for the process-table reaper (keeper_lane) rather than guessed at.
//!
//! A state-root SIBLING socket is unlinked only when its graph file is gone
//! too: a rebind requires a client, a client requires the graph, so with the
//! graph absent no keeper can ever be behind the path and the probe-then-
//! unlink race with a self-healing client cannot happen. A sibling whose
//! graph still lives stays for the client's own connect-before-bind. The
//! hashed temp root is different: its contents are ours by construction and
//! its graph names are hashed away, so the probe alone decides. There the
//! real names come from `graph_keeper::store_socket_for`: `<16hex>.sock`,
//! so every `*.sock` entry in the root is a store socket, and a
//! `<x>.sock.lock` whose `<x>.sock` is gone is an orphaned seat lock the
//! keeper's clean exit never removed - unlinked while its flock is held so
//! a keeper racing `take_seat` reads `WouldBlock`, never a recreated file.

use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use serde_json::json;

/// What one sweep pass unlinked, for the `keeper_sweep_done` event.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StoreSweepReport {
    /// Dead store sockets unlinked.
    pub sockets: usize,
    /// Orphaned `<sock>.lock` seat locks unlinked (no socket behind them).
    pub locks: usize,
}

pub(crate) fn store_socket_sweep(home: &AgentsHome, emitter: &EventEmitter) -> StoreSweepReport {
    // SAFETY: getuid reads a per-process kernel value; it cannot fail or race.
    let uid = unsafe { libc::getuid() };
    store_socket_sweep_in(
        home,
        std::env::temp_dir().join(format!("fno-store-{uid}")),
        emitter,
    )
}

/// The parameterized core, so tests point the hashed root at their own tree
/// instead of sweeping the machine's real one.
pub(crate) fn store_socket_sweep_in(
    home: &AgentsHome,
    temp_root: std::path::PathBuf,
    emitter: &EventEmitter,
) -> StoreSweepReport {
    let state_root = home.root().parent().unwrap_or(home.root()).to_path_buf();
    let dirs = vec![state_root, temp_root.clone()];
    let mut report = StoreSweepReport::default();
    for dir in dirs {
        let in_temp_root = dir == temp_root;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if in_temp_root && name.ends_with(".sock.lock") {
                sweep_seat_lock(&dir, &name, entry.path(), emitter, &mut report);
                continue;
            }
            let is_store_sock = if in_temp_root {
                // The hashed root is ours by construction: every .sock in it
                // is a store socket.
                name.ends_with(".sock")
            } else {
                name.ends_with(".store.sock")
            };
            if !is_store_sock {
                continue;
            }
            let path = entry.path();
            // Sibling ownership rule: `<name>.store.sock` is only ours to
            // unlink when `<name>` (its graph) is gone. With the graph
            // present, a client rebind is always one connection away and
            // unlinking here could steal a socket a keeper just bound.
            if !in_temp_root {
                let graph = path.with_file_name(
                    path.file_name()
                        .map(|n| {
                            n.to_string_lossy()
                                .trim_end_matches(".store.sock")
                                .to_string()
                        })
                        .unwrap_or_default(),
                );
                if graph.exists() {
                    continue;
                }
            }
            let dead = match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => {
                    // A live keeper is behind it: leave the socket alone.
                    drop(stream);
                    false
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        || e.kind() == std::io::ErrorKind::NotFound
                        // macOS answers ENOTSOCK when the path is not a
                        // socket at all (Linux says ECONNREFUSED); either
                        // way nothing can ever be listening behind it, so
                        // the litter is safe to unlink.
                        || e.raw_os_error() == Some(libc::ENOTSOCK) =>
                {
                    true
                }
                Err(_) => false, // unreadable is not dead; the reaper owns that verdict
            };
            if dead && std::fs::remove_file(&path).is_ok() {
                report.sockets += 1;
                let _ = emitter.emit(
                    "store_socket_unlinked",
                    &json!({"path": path.to_string_lossy()}),
                );
            }
        }
    }
    report
}

/// An orphaned seat lock in the hashed root: open it, `try_lock` it (the
/// same flock `take_seat` holds for the keeper's life), and unlink it while
/// the lock is held. A lock another process still holds reads `WouldBlock`
/// and stays. Opened without `create`: a lock that vanishes mid-sweep is
/// simply left for the next pass.
fn sweep_seat_lock(
    dir: &std::path::Path,
    name: &str,
    path: std::path::PathBuf,
    emitter: &EventEmitter,
    report: &mut StoreSweepReport,
) {
    let sock = dir.join(name.trim_end_matches(".lock"));
    if sock.exists() {
        // The seat's socket is still there; the lock is not ours to judge.
        return;
    }
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) else {
        return;
    };
    if file.try_lock().is_err() {
        return; // held by a live keeper (or unprobeable): stays
    }
    if std::fs::remove_file(&path).is_ok() {
        report.locks += 1;
        let _ = emitter.emit(
            "store_seat_lock_unlinked",
            &json!({"path": path.to_string_lossy()}),
        );
    }
    drop(file); // the flock releases only after the unlink
}
