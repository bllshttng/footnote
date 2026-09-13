//! The `fno mux web stop|reap` verbs: the off switch and corpse sweep for the
//! `--web` bridge marker (`web-<session>.json`, written and Drop-removed by
//! `mux serve --web`, crates/fno/src/web.rs). A bridge killed by anything but
//! its own Drop - SIGKILL, a crash, a machine reboot - leaves the file and its
//! 64-hex token on disk forever. `stop` ends the bridge a session named; `reap`
//! removes every marker whose port refuses.

use std::ffi::OsString;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::server_axis::resolve_session;
use super::{EXIT_ERROR, EXIT_OK, EXIT_USAGE};
use crate::proto;

pub fn web(args: &[OsString], env_session: Option<&str>) -> i32 {
    let verb = args.first().and_then(|a| a.to_str()).unwrap_or("");
    match verb {
        "stop" => web_stop(&args[1..], env_session),
        "reap" => web_reap(&args[1..]),
        "-h" | "--help" | "" => {
            eprintln!("usage: fno mux web stop [<session>] [--json] | fno mux web reap [--json]");
            EXIT_USAGE
        }
        other => {
            eprintln!("fno mux web: unknown verb {other:?} (verbs: stop, reap)");
            EXIT_USAGE
        }
    }
}

/// Can anything accept a TCP connection on (bind, port) right now? One probe
/// per resolved address, 300 ms each, first answer wins: startup's bind tries
/// every resolved address until one succeeds, so a live bridge can sit on the
/// second of them. Shared with `print_pane_url`'s liveness check so the URL
/// door and the reap verdict read the same world.
pub(crate) fn bridge_alive(bind: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(addrs) = (bind, port).to_socket_addrs() else {
        return false;
    };
    for addr in addrs.take(8) {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return true;
        }
    }
    false
}

/// The marker's ownership fold, shared with `WebStateFile::drop`: a file that
/// does not name `pid` belongs to a newer bridge and must survive.
fn marker_names_pid(raw: &str, pid: u32) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("pid").and_then(|p| p.as_u64()))
        == Some(pid as u64)
}

/// Same ownership rule as the bridge's own Drop: remove only a marker that
/// still names `pid`. A newer bridge for the session overwrote the file at its
/// own bind, and deleting that one would make a live bridge read as absent.
fn remove_marker_if_still_ours(path: &std::path::Path, pid: u32) -> bool {
    std::fs::read_to_string(path)
        .map(|raw| marker_names_pid(&raw, pid) && std::fs::remove_file(path).is_ok())
        .unwrap_or(false)
}

/// A zombie child reads alive to `kill(pid, 0)`; `waitpid(WNOHANG)` reaps it.
/// A pid that was never ours (the production case) answers ECHILD, so the
/// signal probe decides.
fn pid_alive(pid: u32) -> bool {
    unsafe {
        let waited = libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG);
        if waited > 0 {
            return false;
        }
        if waited == 0 {
            return true;
        }
        libc::kill(pid as libc::pid_t, 0) == 0
    }
}

fn wait_gone(pid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    !pid_alive(pid)
}

fn stop_ladder(pid: u32) -> bool {
    // SIGINT first: the bridge's ctrl_c handler is its designed shutdown, so
    // its own Drop removes the marker. Escalate only when it hangs. After
    // SIGKILL the process is terminally dead; a lingering kill(0) hit is a
    // zombie awaiting its parent's reap, not a running bridge, so no probe
    // runs after the KILL.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }
    if wait_gone(pid, Duration::from_secs(2)) {
        return true;
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if wait_gone(pid, Duration::from_secs(1)) {
        return true;
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(100));
    true
}

fn web_stop(args: &[OsString], env_session: Option<&str>) -> i32 {
    let mut session_arg = None;
    let mut json = false;
    let mut end_of_flags = false;
    for a in args {
        let Some(s) = a.to_str() else {
            eprintln!("fno mux web stop: non-UTF-8 argument");
            return EXIT_USAGE;
        };
        if end_of_flags {
            // After `--` everything is the session name, even one that starts
            // with a dash: `mux serve --web --server <name>` accepts such a
            // name, so its bridge must stay stoppable.
            if session_arg.is_some() {
                eprintln!("fno mux web stop: at most one session name");
                return EXIT_USAGE;
            }
            session_arg = Some(s.to_string());
            continue;
        }
        match s {
            "--" => end_of_flags = true,
            "--json" if json => {
                eprintln!("fno mux web stop: --json given twice");
                return EXIT_USAGE;
            }
            "--json" => json = true,
            f if f.starts_with("--") => {
                eprintln!("fno mux web stop: unknown flag {f:?}");
                return EXIT_USAGE;
            }
            name if session_arg.is_some() => {
                eprintln!("fno mux web stop: at most one session name");
                return EXIT_USAGE;
            }
            name => session_arg = Some(name.to_string()),
        }
    }
    let session = resolve_session(session_arg.as_deref(), env_session);
    let verb = "fno mux web stop";
    let path = proto::mux_dir().join(format!("web-{session}.json"));
    let hint = format!(
        "no web bridge for session {session}; start one with: fno mux serve --web --session {session}"
    );
    let state: serde_json::Value = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    {
        Some(v) => v,
        None => {
            eprintln!("{verb}: {hint}");
            return EXIT_ERROR;
        }
    };
    let pid = match state.get("pid").and_then(|v| v.as_u64()) {
        Some(p) if p > 0 && p <= u32::MAX as u64 => p as u32,
        _ => {
            eprintln!(
                "{verb}: marker {} carries no usable pid; remove it by hand",
                path.display()
            );
            return EXIT_ERROR;
        }
    };
    let port = state
        .get("port")
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok());
    let bind = state
        .get("bind")
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1");
    // The pid alone does not name the bridge: a corpse marker's number can be
    // recycled by an innocent process. The marker's own port is the identity
    // check - the marker is written only after the listener binds, so a
    // refused port means the bridge is gone and the pid belongs to someone
    // else. Signal only a pid whose port still answers.
    let already_dead = !pid_alive(pid)
        || match port {
            Some(p) => !bridge_alive(bind, p),
            None => false,
        };
    if !already_dead {
        stop_ladder(pid);
    }
    // A graceful exit already removed the marker; a kill could not.
    let marker_removed = remove_marker_if_still_ours(&path, pid);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "session": session,
                "pid": pid,
                "port": port,
                "already_dead": already_dead,
                "marker_removed": marker_removed,
            })
        );
    } else if already_dead {
        println!("fno mux web: bridge for session {session} was already dead (pid {pid})");
    } else {
        println!(
            "fno mux web: stopped bridge for session {session} (pid {pid}, port {})",
            port.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
        );
    }
    EXIT_OK
}

/// Scan `dir` for `web-*.json` markers and partition them by liveness:
/// (reaped, live, unreadable). Reaping happens here: a marker whose port
/// refuses is removed on the spot; unreadable markers are named and left.
fn reap_partition(dir: &std::path::Path) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("web-") && n.ends_with(".json"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    let session_of = |p: &std::path::Path| -> String {
        // Exactly one strip of each: a session may itself be named `web-x`
        // or `a.json`, and a repeated trim would misname the receipt.
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("web-"))
            .and_then(|n| n.strip_suffix(".json"))
            .unwrap_or_default()
            .to_string()
    };
    let (mut reaped, mut live, mut unreadable) = (Vec::new(), Vec::new(), Vec::new());
    for path in paths {
        let session = session_of(&path);
        let Some(raw) = std::fs::read_to_string(&path).ok() else {
            unreadable.push(session);
            continue;
        };
        let Ok(state) = serde_json::from_str::<serde_json::Value>(&raw) else {
            unreadable.push(session);
            continue;
        };
        let bind = state
            .get("bind")
            .and_then(|v| v.as_str())
            .unwrap_or("127.0.0.1");
        let Some(port) = state
            .get("port")
            .and_then(|v| v.as_u64())
            .and_then(|p| u16::try_from(p).ok())
        else {
            unreadable.push(session);
            continue;
        };
        // The marker is written only after the listener binds, so a refused
        // port means the listener is gone: this is a corpse. Remove it only
        // while the file still holds the bytes this verdict came from: a
        // replacement bridge that overwrote the path between read and unlink
        // must not lose its marker to the sweep.
        if bridge_alive(bind, port) {
            live.push(session);
        } else if std::fs::read_to_string(&path)
            .map(|now| now == raw)
            .unwrap_or(false)
            && std::fs::remove_file(&path).is_ok()
        {
            reaped.push(session);
        } else {
            unreadable.push(session);
        }
    }
    (reaped, live, unreadable)
}

fn web_reap(args: &[OsString]) -> i32 {
    let mut json = false;
    for a in args {
        let Some(s) = a.to_str() else {
            eprintln!("fno mux web reap: non-UTF-8 argument");
            return EXIT_USAGE;
        };
        match s {
            "--json" if json => {
                eprintln!("fno mux web reap: --json given twice");
                return EXIT_USAGE;
            }
            "--json" => json = true,
            f => {
                eprintln!("fno mux web reap: unexpected argument {f:?}");
                return EXIT_USAGE;
            }
        }
    }
    let (reaped, live, unreadable) = reap_partition(&proto::mux_dir());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "reaped": reaped,
                "live": live,
                "unreadable": unreadable,
            })
        );
    } else if reaped.is_empty() {
        println!(
            "fno mux web reap: no corpse markers ({} live, {} unreadable left alone)",
            live.len(),
            unreadable.len()
        );
    } else {
        println!(
            "fno mux web reap: removed {} corpse marker(s): {} ({} live, {} unreadable left alone)",
            reaped.len(),
            reaped.join(", "),
            live.len(),
            unreadable.len()
        );
    }
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn write_marker(session: &str, body: &str) {
        let dir = proto::mux_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("web-{session}.json"));
        std::fs::write(path, body).unwrap();
    }

    fn marker_path(session: &str) -> std::path::PathBuf {
        proto::mux_dir().join(format!("web-{session}.json"))
    }

    fn dead_pid() -> u32 {
        let mut child = Command::new("/bin/sleep").arg("0").spawn().unwrap();
        child.wait().unwrap();
        child.id()
    }

    #[test]
    fn unknown_verb_and_bare_web_are_usage() {
        assert_eq!(web(&[], None), EXIT_USAGE);
        assert_eq!(web(&[OsString::from("explode")], None), EXIT_USAGE);
        assert_eq!(web(&[OsString::from("--help")], None), EXIT_USAGE);
    }

    #[test]
    fn stop_without_a_marker_is_an_error_naming_the_start_hint() {
        assert_eq!(
            web(&[OsString::from("stop"), OsString::from("nosuch")], None),
            EXIT_ERROR
        );
        assert!(!marker_path("nosuch").exists());
    }

    #[test]
    fn stop_removes_a_corpse_marker_without_signalling() {
        let pid = dead_pid();
        write_marker(
            "corpse",
            &format!(
                "{{\"bind\":\"127.0.0.1\",\"port\":1,\"token\":\"{}\",\"pid\":{pid}}}",
                "a".repeat(64)
            ),
        );
        let code = web(&[OsString::from("stop"), OsString::from("corpse")], None);
        assert_eq!(code, EXIT_OK);
        assert!(!marker_path("corpse").exists(), "corpse marker must go");
    }

    #[test]
    fn stop_signals_a_live_bridge_and_removes_the_marker() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = listener.local_addr().unwrap().port();
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        write_marker(
            "livebridge",
            &format!(
                "{{\"bind\":\"127.0.0.1\",\"port\":{live_port},\"token\":\"a\",\"pid\":{}}}",
                child.id()
            ),
        );
        let code = web(
            &[OsString::from("stop"), OsString::from("livebridge")],
            None,
        );
        assert_eq!(code, EXIT_OK);
        assert!(!marker_path("livebridge").exists());
        // The port answered and the pid answered, so the ladder ran: the
        // stand-in bridge must be dead. web_stop's liveness probe reaped it
        // through waitpid, so std's Child no longer owns it; the signal
        // probe is the death proof.
        assert_ne!(
            unsafe { libc::kill(child.id() as libc::pid_t, 0) },
            0,
            "a live pid with a live port gets signaled"
        );
    }

    #[test]
    fn stop_spares_a_recycled_pid_when_the_port_refuses() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        write_marker(
            "recycled",
            &format!(
                "{{\"bind\":\"127.0.0.1\",\"port\":{dead_port},\"token\":\"a\",\"pid\":{}}}",
                child.id()
            ),
        );
        let code = web(&[OsString::from("stop"), OsString::from("recycled")], None);
        assert_eq!(code, EXIT_OK);
        assert!(
            child.try_wait().unwrap().is_none(),
            "a live pid behind a refused port is a recycled number, not the bridge"
        );
        assert!(!marker_path("recycled").exists());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn stop_honors_end_of_options() {
        assert_eq!(
            web(
                &[
                    OsString::from("stop"),
                    OsString::from("--"),
                    OsString::from("--weird")
                ],
                None
            ),
            EXIT_ERROR,
            "-- names session --weird; no such marker is the error path"
        );
    }

    #[test]
    fn marker_removal_respects_ownership() {
        let dir = proto::mux_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web-own.json");
        std::fs::write(&path, "{\"pid\":42}").unwrap();
        assert!(
            !remove_marker_if_still_ours(&path, 7),
            "a marker naming another pid belongs to a newer bridge"
        );
        assert!(path.exists());
        assert!(remove_marker_if_still_ours(&path, 42));
        assert!(!path.exists());
    }

    #[test]
    fn marker_names_pid_is_exact() {
        assert!(marker_names_pid("{\"pid\":7}", 7));
        assert!(!marker_names_pid("{\"pid\":8}", 7));
        assert!(!marker_names_pid("not json", 7));
        assert!(!marker_names_pid("{}", 7));
    }

    #[test]
    fn reap_removes_only_markers_whose_port_refuses() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = listener.local_addr().unwrap().port();
        write_marker(
            "reaplive",
            &format!("{{\"bind\":\"127.0.0.1\",\"port\":{live_port},\"token\":\"a\",\"pid\":1}}"),
        );
        write_marker(
            "reapdead",
            "{\"bind\":\"127.0.0.1\",\"port\":1,\"token\":\"b\",\"pid\":1}",
        );
        std::fs::write(proto::mux_dir().join("web-reapjunk.json"), "not json").unwrap();

        assert_eq!(web(&[OsString::from("reap")], None), EXIT_OK);
        assert!(!marker_path("reapdead").exists(), "refused port = corpse");
        assert!(marker_path("reaplive").exists(), "live port = keep");
        assert!(
            marker_path("reapjunk").exists(),
            "unreadable markers are named, never removed"
        );
    }

    #[test]
    fn reap_partition_splits_live_from_corpse_markers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = listener.local_addr().unwrap().port();
        write_marker(
            "jsonlive",
            &format!("{{\"bind\":\"127.0.0.1\",\"port\":{live_port},\"token\":\"a\",\"pid\":1}}"),
        );
        write_marker(
            "jsondead",
            "{\"bind\":\"127.0.0.1\",\"port\":1,\"token\":\"b\",\"pid\":1}",
        );
        std::fs::write(proto::mux_dir().join("web-jsonjunk.json"), "not json").unwrap();
        let (reaped, live, unreadable) = reap_partition(&proto::mux_dir());
        assert_eq!(reaped, vec!["jsondead".to_string()]);
        assert_eq!(live, vec!["jsonlive".to_string()]);
        assert_eq!(unreadable, vec!["jsonjunk".to_string()]);
        assert!(
            !marker_path("jsondead").exists(),
            "the corpse was removed during the partition"
        );
    }
}
