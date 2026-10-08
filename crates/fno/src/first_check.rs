//! Forward committed births to the agent runtime's one first-check owner.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) fn record(journal: &Path, event_id: &str, envelope: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(envelope).map_err(|e| e.to_string())?;
    if value["type"] != "agent_spawned" || !value["data"]["spawned_by_session"].is_string() {
        return Ok(());
    }
    let journal = journal
        .to_str()
        .ok_or("first-check journal path is not UTF-8")?;
    let request =
        serde_json::json!({"journal": journal, "event_id": event_id, "envelope": envelope});
    let input = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let binary = std::env::var_os("FNO_AGENTS_BIN").unwrap_or_else(|| "fno-agents".into());
    let mut child = Command::new(binary)
        .args(["claim", "birth"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("first-check owner unavailable: {e}"))?;
    let written = child
        .stdin
        .take()
        .ok_or("first-check stdin unavailable")?
        .write_all(&input);
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("first-check request failed: {error}"));
    }
    let started = Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("first-check owner refused birth: {status}")),
            None if started.elapsed() >= Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("first-check owner timed out after 10s".into());
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
