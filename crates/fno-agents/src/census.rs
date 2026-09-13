//! The running-process census (x-f188): one row per long-lived process,
//! answering "is this running fno process an older build than the binary it
//! was launched from?". Python's `update.running_components` is a thin
//! adapter over `fno-agents census --json`; the walking and classifying live
//! here beside the drift classifier they reuse.
//!
//! Classifier sources, no third rule: a build SELF-REPORT (the keeper's
//! Identify reply carries `drift`, computed live from
//! [`crate::drift::self_drift`]) or, for a pre-report build, the process's
//! start time against its executable's mtime. A probe that cannot decide
//! reads `unknown` with the failure named, never `current`.

use crate::drift;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PROBE_BUDGET: Duration = Duration::from_millis(750);

const TAG_IDENTIFY_GRAPH: u8 = 3; // graph_keeper.rs
const TAG_IDENTIFY_PANE: u8 = 4; // pane_keeper.rs Frame::Identify
const TAG_REPLY: u8 = 5; // both keepers

/// (pid, args) for every process `ps` will name, one entry per pid.
fn ps_table() -> Vec<(u32, String)> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,args="])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(' ')?;
            Some((pid.parse().ok()?, rest.trim().to_string()))
        })
        .collect()
}

/// Seconds the process has been alive, from a `ps` etime string
/// ([[dd-]hh:]mm:ss). The day prefix is not a base-60 digit; split it off
/// before the fold or every process older than a day reads unparseable.
fn parse_etime(text: &str) -> Option<f64> {
    let (days, clock) = match text.split_once('-') {
        Some((d, rest)) => (d.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let mut secs = 0.0;
    for part in clock.split(':') {
        secs = secs * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(secs + days * 86_400.0)
}

/// Seconds the process has been alive, from `ps` etime.
fn etime_secs(pid: u32) -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_etime(String::from_utf8_lossy(&out.stdout).trim())
}

fn started_epoch(etime: Option<f64>) -> Option<f64> {
    let etime = etime?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some(now - etime)
}

fn started_before_rewrite(started_at: Option<f64>, exe: Option<&Path>) -> bool {
    let Some(started) = started_at else {
        return false;
    };
    let Some(exe) = exe else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(exe) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let Ok(nanos) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    started < nanos.as_secs_f64()
}

/// One bounded frame round-trip: write `request`, read the reply frame
/// (`reply_tag`), parse its JSON payload. `None` on any failure.
fn frame_round_trip(sock: &Path, request: [u8; 5], reply_tag: u8) -> Option<Value> {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(PROBE_BUDGET)).ok()?;
    stream.set_write_timeout(Some(PROBE_BUDGET)).ok()?;
    stream.write_all(&request).ok()?;
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).ok()?;
    if header[0] != reply_tag {
        return None;
    }
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut payload = vec![0u8; len.min(1 << 20)];
    stream.read_exact(&mut payload).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// One Identify with a short bound; `Some` only when the keeper answers
/// with a parseable IdentifyReply. `identify_tag` selects the lane's frame
/// (the graph and pane keepers use different request tags, the same reply).
fn identify_reply(sock: &Path, identify_tag: u8) -> Option<Value> {
    frame_round_trip(sock, [identify_tag, 0, 0, 0, 0], TAG_REPLY)
}

/// The `on_restart` / `survives` pair, fixed per component so every surface
/// says the same thing.
fn fate(component: &str) -> (&'static str, &'static str) {
    match component {
        "daemon" => ("restarts", "workers and panes"),
        "store-keeper" => ("cycles; the next read respawns it", "the graph on disk"),
        "mux-server" => ("kept; only `--mux` replaces it", "its panes"),
        _ => ("kept", "its pane; current only when that pane ends"),
    }
}

fn row(
    component: &str,
    pid: Option<u32>,
    name: Option<String>,
    exe: Option<String>,
    started_at: Option<f64>,
    verdict: &str,
    evidence: &str,
) -> Value {
    let (on_restart, survives) = fate(component);
    json!({
        "component": component,
        "pid": pid,
        "name": name,
        "exe": exe,
        "started_at": started_at,
        "verdict": verdict,
        "evidence": evidence,
        "on_restart": on_restart,
        "survives": survives,
    })
}

/// Keeper rows off the process table: argv[0] basename `fno-agents-worker`,
/// lane from the argv flag, socket and session from argv. Keepers sharing
/// one socket are listed as duplicates (change 2 retires them).
fn keeper_rows() -> Vec<Value> {
    let mut rows = Vec::new();
    let mut seen: BTreeMap<String, u32> = BTreeMap::new();
    for (pid, args) in ps_table() {
        let argv: Vec<&str> = args.split_whitespace().collect();
        let Some(argv0) = argv.first() else {
            continue;
        };
        if Path::new(argv0)
            .file_name()
            .is_none_or(|n| n != "fno-agents-worker")
        {
            continue;
        }
        let flag = argv
            .iter()
            .find(|a| ["--pane", "--keeper", "--store-keeper"].contains(a));
        let component = match flag {
            Some(&"--store-keeper") => "store-keeper",
            Some(&"--keeper") => "thread-keeper",
            _ => "pane-keeper",
        };
        let sock = argv
            .windows(2)
            .find(|w| w[0] == "--sock")
            .map(|w| PathBuf::from(w[1].clone()));
        let session = argv
            .windows(2)
            .find(|w| w[0] == "--session")
            .map(|w| w[1].to_string());
        let name = session
            .clone()
            .or_else(|| sock.as_ref().map(|s| s.display().to_string()));
        let started = started_epoch(etime_secs(pid));
        let mut store_graph: Option<String> = None;
        let (verdict, evidence) = match sock.as_deref() {
            None => ("unknown", "argv declares no socket"),
            Some(sock) => {
                let tag = if component == "store-keeper" {
                    TAG_IDENTIFY_GRAPH
                } else {
                    TAG_IDENTIFY_PANE
                };
                match identify_reply(sock, tag) {
                    Some(reply) => {
                        if component == "store-keeper" {
                            store_graph =
                                reply.get("graph").and_then(Value::as_str).map(String::from);
                        }
                        match reply.get("drift").and_then(Value::as_str) {
                            Some("drifted") => ("stale", "build self-report"),
                            Some("fresh") => ("current", "build self-report"),
                            _ => {
                                // A reply with no drift key is a keeper built
                                // before the self-report; start time is the only
                                // reading it gives. An unreadable start time is
                                // no verdict, never current.
                                let exe = Path::new(argv0);
                                if started_before_rewrite(started, Some(exe)) {
                                    ("stale", "predates build self-report")
                                } else if started.is_some() {
                                    ("current", "started at-or-after the binary was written")
                                } else {
                                    ("unknown", "no readable start time")
                                }
                            }
                        }
                    }
                    None => ("unknown", "no Identify answer"),
                }
            }
        };
        let mut row = row(
            component,
            Some(pid),
            name,
            Some(argv0.to_string()),
            started,
            verdict,
            evidence,
        );
        if let Some(graph) = store_graph {
            row["graph"] = json!(graph);
        }
        if let Some(sock) = &sock {
            row["sock"] = json!(sock.display().to_string());
            if let Some(holder) = seen.get(&sock.display().to_string()) {
                row["evidence"] = json!(format!(
                    "{}; duplicate keeper on {} (seat held by pid {})",
                    row["evidence"].as_str().unwrap_or_default(),
                    sock.display(),
                    holder
                ));
            } else {
                seen.insert(sock.display().to_string(), pid);
            }
        }
        rows.push(row);
    }
    rows
}

/// The daemon's row, read in-process: one agent.status call feeds both the
/// pid and the drift verdict. A failed call reads unknown, never current.
async fn daemon_row() -> Value {
    use crate::client::{call_if_running, ClientError};
    use crate::protocol::Request;
    let resp = call_if_running(
        &crate::paths::AgentsHome::from_env(),
        &Request::new(1, "agent.status", json!({})),
    )
    .await;
    let pid = resp
        .as_ref()
        .ok()
        .and_then(|r| r.result())
        .and_then(|r| r.pointer("/daemon/pid"))
        .and_then(Value::as_u64)
        .map(|p| p as u32);
    let (verdict, evidence) = match resp {
        Ok(resp) => match resp.result() {
            Some(result) => {
                let drift = crate::client::drift_from_status(result);
                let label = drift::drift_label(&drift);
                match label {
                    "fresh" => ("current".to_string(), "build self-report".to_string()),
                    "drifted" => ("stale".to_string(), "build self-report".to_string()),
                    _ => (
                        "unknown".to_string(),
                        "status reply carries no drift verdict".to_string(),
                    ),
                }
            }
            None => (
                "unknown".to_string(),
                "status reply unparseable".to_string(),
            ),
        },
        Err(ClientError::DaemonNotRunning) => ("current".to_string(), "no daemon running".into()),
        Err(e) => ("unknown".to_string(), format!("status call failed: {e}")),
    };
    row(
        "daemon",
        pid,
        Some("agents home".into()),
        None,
        None,
        &verdict,
        &evidence,
    )
}

#[cfg(test)]
mod etime_tests {
    use super::parse_etime;

    #[test]
    fn parse_etime_reads_every_ps_shape() {
        assert_eq!(parse_etime("30"), Some(30.0));
        assert_eq!(parse_etime("05:30"), Some(330.0));
        assert_eq!(parse_etime("02:03:04"), Some(7384.0));
        assert_eq!(parse_etime("1-02:03:04"), Some(93_784.0));
    }

    #[test]
    fn parse_etime_refuses_junk_and_empty() {
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("not-a-time"), None);
        assert_eq!(parse_etime("x-02:03"), None);
    }
}

/// Mux server rows from the front door's own `ls --json`; the pid sidecar
/// field (change 4) is what the census classifies.
fn mux_rows() -> Vec<Value> {
    let Some(fno) = resolve_fno() else {
        return Vec::new();
    };
    let Ok(out) = std::process::Command::new(&fno)
        .args(["mux", "ls", "--json"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(rows) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Vec::new();
    };
    let Some(list) = rows.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in list {
        if r.get("state").and_then(Value::as_str) != Some("live") {
            continue;
        }
        let session = r
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or("unnamed");
        let panes = r.get("panes").and_then(Value::as_u64).unwrap_or(0);
        let pid = r.get("pid").and_then(Value::as_u64).map(|p| p as u32);
        let (verdict, evidence) = match pid {
            Some(pid) => {
                let started = started_epoch(etime_secs(pid));
                if started_before_rewrite(started, Some(&fno)) {
                    ("stale", "predates build self-report")
                } else if started.is_some() {
                    ("current", "started at-or-after the binary was written")
                } else {
                    ("unknown", "no readable start time")
                }
            }
            None => ("unknown", "no pid sidecar"),
        };
        let mut row = row(
            "mux-server",
            pid,
            Some(session.to_string()),
            Some(fno.display().to_string()),
            started_epoch(etime_secs(pid.unwrap_or(0))),
            verdict,
            evidence,
        );
        row["on_restart"] = json!(if panes > 0 {
            format!("kept; only `--mux` replaces it, ending {panes} shell(s)")
        } else {
            "kept; auto-restarts (pane-less)".to_string()
        });
        row["survives"] = json!(if panes > 0 {
            format!("{panes} panes")
        } else {
            "no panes".to_string()
        });
        out.push(row);
    }
    out
}

fn resolve_fno() -> Option<PathBuf> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"));
    let candidate = cargo_home.join("bin").join("fno");
    if candidate.is_file() {
        return Some(candidate);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("fno"))
            .find(|p| p.is_file())
    })
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The census: daemon, keepers, mux servers. Bounded: every probe and every
/// subprocess carries a timeout, so a wedged keeper delays one row, never
/// the census.
pub async fn census() -> Vec<Value> {
    let mut rows = vec![daemon_row().await];
    rows.extend(keeper_rows());
    rows.extend(mux_rows());
    rows
}

/// Walk the keeper rows synchronously (tests, and callers already holding no
/// daemon context).
pub fn census_blocking() -> Vec<Value> {
    let mut rows = Vec::new();
    rows.extend(keeper_rows());
    rows.extend(mux_rows());
    rows
}

/// One store keeper the restart verb cycled (or spared).
pub struct CycledKeeper {
    pub graph: Option<String>,
    pub old_pid: Option<u32>,
    pub result: String,
}

/// Shutdown the stale store keepers the census found (x-f188 change 6). No
/// respawn is attempted here: the next read respawns each keeper on the
/// installed binary - the same self-heal the fate text promises - and the
/// Python client's spawner owns the launch flags (read_source, events).
/// A keeper answering `busy` keeps its seat and is reported, not forced.
pub async fn cycle_stale_store_keepers() -> (Vec<CycledKeeper>, usize) {
    let rows = keeper_rows();
    let stale_panes = rows
        .iter()
        .filter(|r| {
            matches!(
                r["component"].as_str(),
                Some("pane-keeper") | Some("thread-keeper")
            ) && r["verdict"] == "stale"
        })
        .count();
    let mut out = Vec::new();
    for r in rows.iter() {
        if r["component"] != "store-keeper" || r["verdict"] != "stale" {
            continue;
        }
        let Some(sock) = r["sock"].as_str().map(PathBuf::from) else {
            continue;
        };
        let result = match shutdown_reply(&sock) {
            Some(reply) if reply.get("ok") == Some(&json!(true)) => {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while std::time::Instant::now() < deadline && sock.exists() {
                    std::thread::sleep(Duration::from_millis(50));
                }
                "cycled".to_string()
            }
            Some(reply) => format!(
                "spared: {}",
                reply
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("mutation in flight")
            ),
            None => "spared: no shutdown answer".to_string(),
        };
        out.push(CycledKeeper {
            graph: r["graph"].as_str().map(String::from),
            old_pid: r["pid"].as_u64().map(|p| p as u32),
            result,
        });
    }
    (out, stale_panes)
}

/// Send one Shutdown frame and read the reply (tag 2 out, response tag 4).
fn shutdown_reply(sock: &Path) -> Option<Value> {
    frame_round_trip(sock, [2, 0, 0, 0, 0], 4)
}
