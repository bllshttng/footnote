//! The `fno mux ls | kill-server` verbs: plain client-process code over the
//! frozen pre-Attach protocol pair (`Query`/`Info`, `KillServer`). These run
//! and exit - no TUI, no raw mode, no attach - so every probe is bounded by
//! a read/write timeout and a bad session can never hang the listing
//! (AC4-ERR).

use std::path::Path;
use std::time::{Duration, Instant};

use crate::proto::{self, read_msg_sync, write_msg_sync, ClientMsg, ServerMsg, DEFAULT_SESSION};

/// Bound every probe: a wedged server counts as alive-but-unqueryable, never
/// a hang. Generous next to a socket round-trip, tight next to a human.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolve the target session: explicit flag/arg > `FNO_SESSION` (set in
/// every pane the server spawns) > the default. Pure, so precedence is
/// unit-testable (Locked 7).
pub fn resolve_session(explicit: Option<&str>, env: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| env.filter(|s| !s.is_empty()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_SESSION.to_string())
}

/// What one socket probe learned.
enum Probe {
    /// The server answered `Query`.
    Live {
        clients: u32,
        squads: u32,
        panes: u32,
    },
    /// Something accepts connections but never answered a parseable `Info`
    /// (an older build, a wedged server): listed, never unlinked, and one
    /// bad session never breaks the listing (AC4-ERR).
    Unqueryable,
    /// Nothing listens: a leftover socket from a dead server.
    Stale,
    /// The probe itself failed CLIENT-side (fd exhaustion, permissions):
    /// says nothing about the server, so it must never read as `Stale` -
    /// "stale" steers the operator toward kill-server's unlink.
    Unprobeable(String),
}

fn probe(sock: &Path) -> Probe {
    let stream = match std::os::unix::net::UnixStream::connect(sock) {
        Ok(s) => s,
        // Only a refused connection proves nothing listens; every other
        // error (EMFILE, EACCES, ...) is OUR failure, not the server's.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return Probe::Stale,
        Err(e) => return Probe::Unprobeable(e.to_string()),
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return Probe::Unqueryable,
    };
    if write_msg_sync(&mut w, &ClientMsg::Query).is_err() {
        return Probe::Unqueryable;
    }
    let mut r = stream;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    // The server answers Query with exactly one Info then closes; tolerate
    // (skip) anything else a confused peer might emit until the deadline.
    while Instant::now() < deadline {
        match read_msg_sync::<_, ServerMsg>(&mut r) {
            Ok(ServerMsg::Info {
                clients,
                squads,
                panes,
                ..
            }) => {
                return Probe::Live {
                    clients,
                    squads,
                    panes,
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    Probe::Unqueryable
}

/// `fno mux ls`: one row per `*.sock` in the mux dir. Read-only - a stale
/// socket is REPORTED, never unlinked (kill-server owns removal). Exits 0
/// even when every row is stale or unqueryable; only "no sessions" is
/// distinguishable by text, not exit code, so scripts can `grep`.
pub fn ls() -> i32 {
    let dir = proto::mux_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        // A missing dir means no session ever started here. Any OTHER error
        // (permissions, I/O) must not read as an empty listing - a script
        // grepping "no sessions" would get a clean false negative.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no sessions");
            return 0;
        }
        Err(e) => {
            eprintln!("fno: cannot read {}: {e}", dir.display());
            return 1;
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("sock"))
                .then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .flatten()
        })
        .collect();
    if names.is_empty() {
        println!("no sessions");
        return 0;
    }
    names.sort();
    for name in names {
        let sock = dir.join(format!("{name}.sock"));
        match probe(&sock) {
            Probe::Live {
                clients,
                squads,
                panes,
            } => println!("{name}: {clients} clients, {squads} squads, {panes} panes"),
            Probe::Unqueryable => println!("{name}: alive (unqueryable - older server?)"),
            Probe::Stale => println!("{name}: stale"),
            Probe::Unprobeable(e) => println!("{name}: probe failed ({e})"),
        }
    }
    0
}

/// `fno mux kill-server [<name>]`: shut one session down. A live server Byes
/// its clients, kills every pane child, and exits (its SocketGuard unlinks
/// the socket); a stale socket is unlinked here with a message (exit 0); no
/// socket at all is "no server" (exit 1).
pub fn kill_server(session: &str) -> i32 {
    let sock = match proto::socket_path(session) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fno: {e}");
            return 2;
        }
    };
    if !sock.exists() {
        eprintln!("fno: no server for session {session:?}");
        return 1;
    }
    let stream = match std::os::unix::net::UnixStream::connect(&sock) {
        Ok(s) => s,
        // Only a REFUSED connection (or a socket that vanished mid-race)
        // proves the server is dead. Any other connect error is client-side
        // (fd exhaustion, permissions) - unlinking on it would orphan a LIVE
        // server: still running, unreachable by name, invisible to ls.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            // AC4-EDGE: dead server left its socket behind - take it out.
            return match std::fs::remove_file(&sock) {
                Ok(()) => {
                    println!("removed stale socket for session {session:?} (server was dead)");
                    0
                }
                Err(e) => {
                    eprintln!("fno: cannot remove stale socket {}: {e}", sock.display());
                    1
                }
            };
        }
        Err(e) => {
            eprintln!("fno: cannot connect to {}: {e}", sock.display());
            return 1;
        }
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fno: socket setup failed: {e}");
            return 1;
        }
    };
    if write_msg_sync(&mut w, &ClientMsg::KillServer).is_err() {
        eprintln!("fno: could not reach the server for session {session:?}");
        return 1;
    }
    // Drain until the server closes the connection (bounded), then wait for
    // its SocketGuard unlink - the observable proof the process exited.
    let mut r = stream;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while Instant::now() < deadline {
        if read_msg_sync::<_, ServerMsg>(&mut r).is_err() {
            break; // EOF/timeout: the server is going down
        }
    }
    let deadline = Instant::now() + PROBE_TIMEOUT;
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(30));
    }
    if sock.exists() {
        eprintln!("fno: session {session:?} did not shut down in time");
        return 1;
    }
    println!("killed session {session:?}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_session_resolution_flag_beats_env_beats_default() {
        // Locked 7: --session flag > FNO_SESSION env > "main" (AC3-EDGE).
        assert_eq!(resolve_session(Some("other"), Some("work")), "other");
        assert_eq!(resolve_session(None, Some("work")), "work");
        assert_eq!(resolve_session(None, None), DEFAULT_SESSION);
        // An empty env var reads as unset, not as a session named "".
        assert_eq!(resolve_session(None, Some("")), DEFAULT_SESSION);
    }

    #[test]
    fn mux_kill_server_missing_socket_is_no_server_exit_1() {
        // No env manipulation (unit tests share the process): a name no real
        // session uses resolves to a socket that does not exist -> exit 1.
        // The full live/stale matrix runs e2e against FNO_MUX_DIR-scoped
        // servers in 3.6.
        let code = kill_server(&format!("fno-test-absent-{}", std::process::id()));
        assert_eq!(code, 1, "missing socket must exit 1");
    }

    #[test]
    fn mux_kill_server_invalid_name_is_usage_exit_2() {
        assert_eq!(kill_server("../evil"), 2, "validation precedes any I/O");
    }
}
