//! The `fno mux web reap` verb: the corpse sweep for the `--web` bridge marker
//! (`web-<session>.json`, written and Drop-removed by `mux serve --web`,
//! crates/fno/src/web.rs). A bridge killed by anything but its own Drop -
//! SIGKILL, a crash, a machine reboot - leaves the file and its 64-hex token
//! on disk forever. Stopping a live bridge is `fno mux serve --web --stop`;
//! this verb removes what a death without Drop left behind: every marker whose
//! port refuses.

use std::ffi::OsString;
use std::net::TcpStream;
use std::time::Duration;

use super::{EXIT_OK, EXIT_USAGE};
use crate::proto;

pub fn web(args: &[OsString], _env_session: Option<&str>) -> i32 {
    let verb = args.first().and_then(|a| a.to_str()).unwrap_or("");
    match verb {
        "reap" => web_reap(&args[1..]),
        "-h" | "--help" | "" => {
            eprintln!(
                "usage: fno mux web reap [--json] (stopping a live bridge is: fno mux serve --web --stop)"
            );
            EXIT_USAGE
        }
        other => {
            eprintln!("fno mux web: unknown verb {other:?} (verb: reap)");
            EXIT_USAGE
        }
    }
}

/// Can anything accept a TCP connection on (bind, port) right now? One probe
/// per resolved address, 300 ms each, first answer wins: startup's bind tries
/// every resolved address until one succeeds, so a live bridge can sit on the
/// second of them.
fn bridge_alive(bind: &str, port: u16) -> bool {
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

    fn write_marker(session: &str, body: &str) {
        let dir = proto::mux_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("web-{session}.json"));
        std::fs::write(path, body).unwrap();
    }

    fn marker_path(session: &str) -> std::path::PathBuf {
        proto::mux_dir().join(format!("web-{session}.json"))
    }

    #[test]
    fn unknown_verb_and_bare_web_are_usage() {
        assert_eq!(web(&[], None), EXIT_USAGE);
        assert_eq!(web(&[OsString::from("explode")], None), EXIT_USAGE);
        assert_eq!(web(&[OsString::from("--help")], None), EXIT_USAGE);
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
