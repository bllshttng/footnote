//! `fno mux pane keeper list`: what survived, in the keeper's own words.
//!
//! Moved out of mux_cli under the file-budget gate, with the code the
//! pane-to-thread change touched: this listing is what the conversion
//! classifier reads to decide whether a keeper sits behind a pane, so it
//! earns a module named by the question it answers.

use super::*;

/// `fno mux pane keeper list`: one row per keeper socket under the panes
/// dir, probed DIRECTLY (connect + Identify, short timeout). Answers "did
/// the keeper survive" with the keeper's own word - its pid, its child's
/// pid, its cwd and argv - never with a process count. A socket nobody
/// lives behind is listed with the reason, because a silent zero is the
/// receipt-can-lie shape. Read-only: this verb never unlinks anything (the
/// server's readopt sweep owns that).
pub(crate) fn pane_keeper_list(json: bool, stale_after: Option<std::time::Duration>) -> i32 {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    // Both lanes. A converted keeper lives under threads/ with the same pid
    // and the same child, so a listing that read only panes/ would answer
    // "gone" about a keeper that is running.
    let mut names: Vec<(std::path::PathBuf, &str)> = Vec::new();
    for (dir, lane) in [
        (crate::pty::keeper_dir(), "pane"),
        (crate::pty::thread_keeper_dir(), "thread"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|x| x == "sock").unwrap_or(false) {
                names.push((path, lane));
            }
        }
    }
    names.sort();
    for (path, lane) in names {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let (session, pane_key) = match stem.rsplit_once('-') {
            Some((s, key)) if key.chars().all(|c| c.is_ascii_digit()) => {
                (s.to_string(), key.to_string())
            }
            _ => (stem.to_string(), String::new()),
        };
        let mut row = serde_json::json!({
            "socket": path.display().to_string(),
            "session": session,
            "pane_key": pane_key,
            "lane": lane,
        });
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Short timeouts everywhere: a wedged keeper must not wedge the read.
        match std::os::unix::net::UnixStream::connect(&path) {
            Err(e) => {
                row["stale"] = serde_json::json!(format!("no listener: {e}"));
            }
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(750)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(750)));
                let identified = (|| -> Option<serde_json::Value> {
                    use crate::pty::{
                        keeper_decode, keeper_frame_identify, KeeperRead, KEEPER_TAG_IDENTIFY_REPLY,
                    };
                    use std::io::{Read as _, Write as _};
                    stream.write_all(&keeper_frame_identify()).ok()?;
                    let mut buf: Vec<u8> = Vec::new();
                    let mut read_buf = [0u8; 4096];
                    loop {
                        loop {
                            match keeper_decode(&buf) {
                                KeeperRead::NeedMore => break,
                                KeeperRead::Frame(tag, payload, used) => {
                                    buf.drain(..used);
                                    if tag == KEEPER_TAG_IDENTIFY_REPLY {
                                        return serde_json::from_slice(&payload).ok();
                                    }
                                }
                            }
                        }
                        match stream.read(&mut read_buf) {
                            Ok(0) | Err(_) => return None,
                            Ok(n) => buf.extend_from_slice(&read_buf[..n]),
                        }
                    }
                })();
                match identified {
                    None => {
                        row["stale"] = serde_json::json!("no identify answer inside the timeout");
                    }
                    Some(reply) => {
                        for field in ["v", "keeper_pid", "child_pid", "cwd", "argv", "started_at"] {
                            row[field] =
                                reply.get(field).cloned().unwrap_or(serde_json::Value::Null);
                        }
                        let child_pid = reply.get("child_pid").and_then(serde_json::Value::as_u64);
                        if let Some(pid) = child_pid {
                            // SAFETY: signal 0 is the existence probe.
                            let hit = unsafe { libc::kill(pid as libc::pid_t, 0) };
                            if hit != 0 {
                                row["stale"] =
                                    serde_json::json!(format!("child pid {pid} is gone"));
                            }
                        }
                        let age = reply
                            .get("started_at")
                            .and_then(serde_json::Value::as_u64)
                            .map(|t| now.saturating_sub(t));
                        if let (Some(age), Some(cap)) = (age, stale_after) {
                            if age > cap.as_secs() {
                                row["stale"] = serde_json::json!(format!(
                                    "aged {age}s (> {}s)",
                                    cap.as_secs()
                                ));
                            }
                        }
                    }
                }
            }
        }
        rows.push(row);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
        );
    } else if rows.is_empty() {
        println!("no keeper panes");
    } else {
        for row in &rows {
            let stale = row.get("stale").and_then(serde_json::Value::as_str);
            let desc = match stale {
                Some(reason) => format!(" (STALE: {reason})"),
                None => String::new(),
            };
            println!(
                "session {} pane {} keeper {} child {} cwd {}{}",
                row.get("session")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                row.get("pane_key")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                row.get("keeper_pid")
                    .and_then(serde_json::Value::as_u64)
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into()),
                row.get("child_pid")
                    .and_then(serde_json::Value::as_u64)
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into()),
                row.get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                desc,
            );
        }
    }
    EXIT_OK
}
