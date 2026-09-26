//! Finished background subagents this session still holds, read from its
//! own claude transcript. Feeds the `subagents` reading in `king_checkin.rs`.
//!
//! A background subagent that finished stays held until its parent stops it
//! with TaskStop. Every existing reader keys on transcript mtime, which
//! cannot tell a held finished agent from a released one (a TaskStopped
//! agent reads idle, measured: `a7d899f10fd8c4186`), so this module reads
//! what actually says it: the parent transcript's `<task-notification>`
//! blocks (one status per task id) and its TaskStop `tool_use` calls.
//! Claude-only, like every reader of the per-session transcript.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One finished background subagent this session still holds.
#[derive(Debug, PartialEq)]
pub struct Held {
    pub agent_id: String,
    pub name: Option<String>,
    pub status: String,
    pub idle_secs: u64,
}

/// The live threshold, the name and default the Python sidechain reader
/// uses (`FNO_SUBAGENT_LIVE_SECONDS`, default 600), so one knob moves both.
pub fn live_threshold() -> u64 {
    threshold_from(std::env::var("FNO_SUBAGENT_LIVE_SECONDS").ok().as_deref())
}

fn threshold_from(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(600)
}

/// The subagents dir of a session: the transcript path minus its `.jsonl`
/// extension, joined with `subagents`.
fn subagents_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("").join("subagents")
}

/// Every `<task-id>`/`<status>` pair in one notification line, in order.
/// A notification arrives as plain text inside the parent transcript, so
/// this is a text scan, not a JSON walk.
fn notification_pairs(
    line: &str,
    id_re: &regex::Regex,
    status_re: &regex::Regex,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for id in id_re.captures_iter(line) {
        let after = &line[id.get(0).unwrap().end()..];
        if let Some(status) = status_re.captures(after) {
            pairs.push((id[1].to_string(), status[1].to_string()));
        }
    }
    pairs
}

/// The `input.task_id` of every TaskStop `tool_use` in one transcript line.
/// The id can be the agent id or its meta `name`, so both are matched.
fn taskstop_ids(line: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_use")
                && block.get("name").and_then(Value::as_str) == Some("TaskStop")
        })
        .filter_map(|block| {
            block
                .pointer("/input/task_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// The subagent's display name from its meta file, when it parses.
fn meta_name(dir: &Path, agent_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(format!("agent-{agent_id}.meta.json"))).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The finished background subagents in `transcript` still not stopped,
/// idle for at least `min_idle_secs`, oldest first.
///
/// An agent is held idle when its last notification status is `completed`
/// or `failed`, no TaskStop names its id or meta name, and its own
/// transcript's mtime is at least `min_idle_secs` old. The mtime is only a
/// freshness guard: it keeps a resumed agent whose old `completed` status
/// predates new writes from reading as held. An agent with no notification
/// is running or foreground, and is never held. A session with no
/// `subagents` dir returns an empty list; an unreadable transcript errors.
pub fn held_idle(
    transcript: &Path,
    now: SystemTime,
    min_idle_secs: u64,
) -> Result<Vec<Held>, String> {
    let text = std::fs::read_to_string(transcript)
        .map_err(|e| format!("transcript {} unreadable: {e}", transcript.display()))?;
    let dir = subagents_dir(transcript);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("subagents dir {} unreadable: {e}", dir.display())),
    };
    let id_re = regex::Regex::new(r"<task-id>([^<]+)</task-id>").unwrap();
    let status_re = regex::Regex::new(r"<status>([^<]+)</status>").unwrap();
    let mut last_status: Vec<(String, String)> = Vec::new();
    let mut stopped: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.contains("<task-notification>") {
            for (id, status) in notification_pairs(line, &id_re, &status_re) {
                match last_status.iter_mut().find(|(known, _)| *known == id) {
                    Some(slot) => slot.1 = status,
                    None => last_status.push((id, status)),
                }
            }
        }
        if line.contains("\"TaskStop\"") {
            stopped.extend(taskstop_ids(line));
        }
    }
    let mut held = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(agent_id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("agent-"))
            .and_then(|n| n.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let name = meta_name(&dir, agent_id);
        let idle_secs = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let Some(status) = last_status
            .iter()
            .rev()
            .find(|(id, _)| id == agent_id)
            .map(|(_, s)| s.clone())
        else {
            continue;
        };
        if status != "completed" && status != "failed" {
            continue;
        }
        if stopped
            .iter()
            .any(|id| id == agent_id || Some(id.as_str()) == name.as_deref())
        {
            continue;
        }
        if idle_secs < min_idle_secs {
            continue;
        }
        held.push(Held {
            agent_id: agent_id.to_string(),
            name,
            status,
            idle_secs,
        });
    }
    held.sort_by(|a, b| b.idle_secs.cmp(&a.idle_secs));
    Ok(held)
}

/// The reading the check-in journals: the held rows behind a count.
pub fn reading(transcript: &Path, now: SystemTime, min_idle_secs: u64) -> Result<Value, String> {
    let held = held_idle(transcript, now, min_idle_secs)?;
    Ok(json!({
        "held_idle": held.len(),
        "held": held
            .iter()
            .map(|h| json!({
                "id": h.agent_id,
                "name": h.name,
                "status": h.status,
                "idle_secs": h.idle_secs,
            }))
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// One parent-transcript row carrying a task notification.
    fn notify(id: &str, status: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"text\",\
             \"text\":\"<task-notification><task-id>{id}</task-id>\
             <status>{status}</status></task-notification>\"}}]}}}}"
        )
    }

    /// One parent-transcript row carrying a TaskStop tool call.
    fn stop_call(task_id: &str) -> String {
        json!({"message": {"content": [{
            "type": "tool_use", "name": "TaskStop",
            "input": {"task_id": task_id},
        }]}})
        .to_string()
    }

    /// A session dir with a parent transcript and per-agent transcript
    /// files; returns (transcript path, subagents dir).
    fn fixture(dir: &Path, transcript_lines: &[String], agents: &[&str]) -> (PathBuf, PathBuf) {
        let session = dir.join("914bce77-test.jsonl");
        let mut file = std::fs::File::create(&session).unwrap();
        for line in transcript_lines {
            writeln!(file, "{line}").unwrap();
        }
        drop(file);
        let subagents = session.with_extension("").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        for name in agents {
            std::fs::File::create(subagents.join(format!("agent-{name}.jsonl"))).unwrap();
        }
        (session, subagents)
    }

    /// Backdate a file's mtime so `now - mtime` reads as `secs` seconds.
    fn backdate(path: &Path, secs: u64) {
        let file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let past = SystemTime::now() - std::time::Duration::from_secs(secs);
        file.set_times(std::fs::FileTimes::new().set_modified(past))
            .unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn only_a_finished_unstopped_old_agent_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let (session, subagents) = fixture(
            dir.path(),
            &[
                notify("aaa", "completed"),
                notify("bbb", "completed"),
                stop_call("bbb"),
                notify("ccc", "killed"),
            ],
            &["aaa", "bbb", "ccc", "ddd"],
        );
        for id in ["aaa", "bbb", "ccc"] {
            backdate(&subagents.join(format!("agent-{id}.jsonl")), 7200);
        }
        let held = held_idle(&session, SystemTime::now(), 600).unwrap();
        assert_eq!(
            held.len(),
            1,
            "A held; B TaskStopped; C killed; D never notified"
        );
        assert_eq!(held[0].agent_id, "aaa");
        assert_eq!(held[0].status, "completed");
    }

    #[test]
    fn missing_transcript_errors_and_missing_dir_is_empty() {
        let err = held_idle(
            Path::new("/nonexistent/session.jsonl"),
            SystemTime::now(),
            600,
        )
        .unwrap_err();
        assert!(
            err.contains("/nonexistent/session.jsonl"),
            "names the path: {err}"
        );
        let dir = tempfile::tempdir().unwrap();
        let (session, _subagents) = fixture(dir.path(), &[], &[]);
        assert!(held_idle(&session, SystemTime::now(), 600)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_recently_written_completed_agent_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let (session, subagents) = fixture(dir.path(), &[notify("ddd", "completed")], &["ddd"]);
        backdate(&subagents.join("agent-ddd.jsonl"), 30);
        assert!(
            held_idle(&session, SystemTime::now(), 600)
                .unwrap()
                .is_empty(),
            "resumed 30s ago: the old completed status is stale"
        );
    }

    #[test]
    fn a_taskstop_by_meta_name_releases_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let (session, subagents) = fixture(
            dir.path(),
            &[notify("eee", "completed"), stop_call("bp-e")],
            &["eee"],
        );
        std::fs::write(
            subagents.join("agent-eee.meta.json"),
            json!({"name": "bp-e"}).to_string(),
        )
        .unwrap();
        backdate(&subagents.join("agent-eee.jsonl"), 7200);
        let held = held_idle(&session, SystemTime::now(), 600).unwrap();
        assert!(held.is_empty(), "TaskStop by meta name releases the agent");
    }

    #[test]
    fn last_status_wins_and_rows_sort_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let (session, subagents) = fixture(
            dir.path(),
            &[
                notify("fff", "completed"),
                notify("fff", "stopped"),
                notify("ggg", "failed"),
            ],
            &["fff", "ggg"],
        );
        backdate(&subagents.join("agent-fff.jsonl"), 3600);
        backdate(&subagents.join("agent-ggg.jsonl"), 7200);
        let held = held_idle(&session, SystemTime::now(), 600).unwrap();
        assert_eq!(
            held.iter().map(|h| h.agent_id.as_str()).collect::<Vec<_>>(),
            vec!["ggg"],
            "fff's last status is stopped; ggg failed and sorted first at 7200s"
        );
    }

    #[test]
    fn the_reading_wraps_held_rows_behind_a_count() {
        let dir = tempfile::tempdir().unwrap();
        let (session, subagents) = fixture(dir.path(), &[notify("aaa", "completed")], &["aaa"]);
        backdate(&subagents.join("agent-aaa.jsonl"), 7200);
        let value = reading(&session, SystemTime::now(), 600).unwrap();
        assert_eq!(value["held_idle"], 1);
        assert_eq!(value["held"][0]["id"], "aaa");
        assert_eq!(value["held"][0]["idle_secs"], 7200);
    }

    #[test]
    fn the_threshold_reads_the_python_knob_and_defaults_to_600() {
        assert_eq!(threshold_from(None), 600);
        assert_eq!(threshold_from(Some("")), 600);
        assert_eq!(threshold_from(Some("junk")), 600);
        assert_eq!(threshold_from(Some("0")), 600);
        assert_eq!(threshold_from(Some("120")), 120);
    }
}
