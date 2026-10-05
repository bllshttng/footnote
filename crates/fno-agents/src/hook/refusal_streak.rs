//! `fno-agents hook posttooluse-bash` + the `refusal-streak` PreToolUse
//! guard: one refusal-streak ledger per session.
//!
//! A lead that hit a refused verb retried it 35 times and mailed 53
//! near-identical status reports in one window (lead comparison 2026-10-03,
//! internal/fno/evals/kings/20261003-lead-comparison.md). The general rule
//! that evaluation asked for: count identical refusals per command shape,
//! and refuse the THIRD attempt with a pointer to park the action and file
//! or mail the blocker.
//!
//! Two halves share one ledger:
//!
//! - The PostToolUse recorder (`posttooluse-bash`) records every finished
//!   Bash call: a nonzero exit whose output digest matches the previous
//!   failure raises that shape's streak, a changed output restarts it at
//!   one, and a zero exit clears it.
//! - The PreToolUse guard (`refusal-streak` in the `pretooluse-bash`
//!   chain) refuses a command whose streak sits at the park threshold
//!   with the park receipt.
//!
//! The ledger is one JSON file per session under the space's
//! `refusal-streak/` directory, capped at 64 shapes with a 10-minute
//! entry TTL: a retry storm lands seconds apart, while a watcher re-armed
//! half an hour later starts fresh. Every read or write failure fails
//! open (allow), the recorder never blocks a finished call, and the post
//! side emits no event row because it runs on every Bash completion.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Identical failures that park the shape: the third unchanged attempt.
const PARK_THRESHOLD: u64 = 2;
/// Shapes one session tracks; the oldest entry by expiry is evicted.
const MAX_SHAPES: usize = 64;
/// A streak older than this reads as fresh: retry storms are seconds
/// apart, a watcher re-arm is tens of minutes apart.
const ENTRY_TTL_SECS: u64 = 600;
/// Failure-output bytes that feed the digest; refusal messages fit.
const OUTPUT_CAP: usize = 512;
/// Command-preview bytes kept in the entry for the park receipt.
const PREVIEW_CAP: usize = 160;

/// The post side: record one finished Bash call, never block, never fail.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
    record(&payload);
    0
}

/// The PreToolUse half: a refusal when this exact shape sits parked.
pub(super) fn judge_pre(payload: &Value) -> Option<String> {
    let (cwd, session, command) = parts(payload)?;
    let key = shape_key(&cwd, &command);
    park_reason(&ledger_path(&cwd, &session), now_secs(), &key)
}

/// The post side against the resolved payload: bump, restart or clear the
/// shape's streak. Best-effort end to end; an unreadable payload records
/// nothing.
fn record(payload: &Value) {
    let Some((cwd, session, command)) = parts(payload) else {
        return;
    };
    let exit = payload
        .get("tool_response")
        .and_then(|r| r.get("exit_code").or_else(|| r.get("exitCode")))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let failed = exit != 0;
    let digest = if failed {
        output_digest(payload)
    } else {
        String::new()
    };
    let key = shape_key(&cwd, &command);
    record_at(
        &ledger_path(&cwd, &session),
        now_secs(),
        &key,
        failed,
        &digest,
        &preview(&command),
    );
}

/// Payload parts every half needs. A non-Bash tool, a blank command or an
/// absent cwd allows/records nothing (fail open).
fn parts(payload: &Value) -> Option<(PathBuf, String, String)> {
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return None;
    }
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)?;
    let command = payload
        .get("tool_input")
        .and_then(|ti| ti.get("command"))
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())?
        .to_string();
    Some((cwd, session_id(payload), command))
}

/// The ledger file for one session. Session ids from the harness are
/// uuid-shaped; anything with characters outside the safe set collapses
/// to a digest so the path stays one file per session.
fn session_id(payload: &Value) -> String {
    let raw = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let safe = !raw.is_empty() && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if safe {
        raw.to_string()
    } else {
        format!("session-{}", digest_hex(raw.as_bytes())[..16].to_owned())
    }
}

/// The command shape: cwd plus the command with whitespace runs collapsed,
/// digested. A shape is what "identical retry" means.
fn shape_key(cwd: &Path, command: &str) -> String {
    let mut normalized = String::with_capacity(command.len());
    let mut in_space = false;
    for c in command.chars() {
        if c.is_whitespace() {
            in_space = true;
        } else {
            if in_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            in_space = false;
            normalized.push(c);
        }
    }
    let mut material = cwd.to_string_lossy().into_owned();
    material.push('\u{1}');
    material.push_str(&normalized);
    digest_hex(material.as_bytes())
}

fn output_digest(payload: &Value) -> String {
    let response = payload.get("tool_response");
    let text: String = ["stderr", "output", "stdout"]
        .iter()
        .filter_map(|field| response.and_then(|r| r.get(*field)).and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join("\u{1}");
    let capped: String = text.chars().take(OUTPUT_CAP).collect();
    digest_hex(capped.as_bytes())
}

fn preview(command: &str) -> String {
    command.chars().take(PREVIEW_CAP).collect()
}

/// FNV-1a is overkill to depend on and blake3 is already in the tree: one
/// stable content digest for both the shape key and the output digest.
fn digest_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The ledger path for one session under the space the hooks key their
/// state off (`events_space`, so a pinned test sees a pinned space).
fn ledger_path(cwd: &Path, session: &str) -> PathBuf {
    super::events_space(cwd)
        .join("refusal-streak")
        .join(format!("{session}.json"))
}

fn load(path: &Path) -> BTreeMap<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    value
        .get("entries")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
}

fn save(path: &Path, entries: &BTreeMap<String, Value>) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let document = json!({"version": 1, "entries": entries});
    let body = document.to_string();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// One outcome against the ledger: a failure with the same digest and a
/// live entry raises the streak, a changed digest restarts it at one, a
/// success clears the shape. Saving also prunes expired entries and
/// evicts the oldest past the cap.
fn record_at(path: &Path, now: u64, key: &str, failed: bool, digest: &str, preview_text: &str) {
    let mut entries = load(path);
    if failed {
        let old = entries.get(key);
        let live = old
            .and_then(|e| e.get("expires_at"))
            .and_then(Value::as_u64)
            .is_some_and(|expires| now < expires);
        let same = live
            && old
                .and_then(|e| e.get("digest"))
                .and_then(Value::as_str)
                .is_some_and(|d| d == digest);
        let count = if same {
            old.and_then(|e| e.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                + 1
        } else {
            1
        };
        entries.insert(
            key.to_string(),
            json!({
                "digest": digest,
                "count": count,
                "preview": preview_text,
                "expires_at": now.saturating_add(ENTRY_TTL_SECS),
            }),
        );
    } else {
        entries.remove(key);
    }
    entries.retain(|_, e| {
        e.get("expires_at")
            .and_then(Value::as_u64)
            .is_some_and(|expires| now < expires)
    });
    while entries.len() > MAX_SHAPES {
        let oldest = entries
            .iter()
            .min_by_key(|(_, e)| e.get("expires_at").and_then(Value::as_u64).unwrap_or(0))
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                entries.remove(&k);
            }
            None => break,
        }
    }
    save(path, &entries);
}

/// The park verdict: a receipt when the shape's live streak sits at the
/// threshold, otherwise None.
fn park_reason(path: &Path, now: u64, key: &str) -> Option<String> {
    let entries = load(path);
    let entry = entries.get(key)?;
    let count = entry.get("count").and_then(Value::as_u64)?;
    let expires = entry.get("expires_at").and_then(Value::as_u64)?;
    if now >= expires || count < PARK_THRESHOLD {
        return None;
    }
    let preview_text = entry
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(format!(
        "[fno refusal-streak] `{preview_text}` already failed with the identical result {count} times in this session; the third unchanged attempt is parked. Retrying it cannot succeed while the answer stays identical. Park the action: file the blocker (`fno backlog idea \"<what is blocked and why>\"`) or mail your lead (`fno agents mail send <lead>`), then move to other work. A changed command is judged fresh."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streak_parks_the_third_identical_failure_and_clears_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.json");
        let key = "shape";
        // One refusal parks nothing.
        record_at(&path, 1_000, key, true, "d1", "cmd");
        assert!(park_reason(&path, 1_001, key).is_none());
        // The second identical refusal parks the third attempt.
        record_at(&path, 1_002, key, true, "d1", "cmd");
        let parked = park_reason(&path, 1_003, key).unwrap();
        assert!(parked.contains("parked"), "{parked}");
        assert!(parked.contains("fno backlog idea"), "{parked}");
        // A changed answer restarts the streak at one.
        record_at(&path, 1_004, key, true, "d2", "cmd");
        assert!(park_reason(&path, 1_005, key).is_none());
        // Identical twice again reaches the threshold from the restart.
        record_at(&path, 1_006, key, true, "d2", "cmd");
        record_at(&path, 1_007, key, true, "d2", "cmd");
        assert!(park_reason(&path, 1_008, key).is_some());
        // A success clears the shape entirely.
        record_at(&path, 1_009, key, false, "", "cmd");
        assert!(park_reason(&path, 1_010, key).is_none());
        // The TTL reads a stale streak as fresh.
        record_at(&path, 2_000, key, true, "d3", "cmd");
        record_at(&path, 2_001, key, true, "d3", "cmd");
        assert!(park_reason(&path, 2_002, key).is_some());
        assert!(park_reason(&path, 2_000 + ENTRY_TTL_SECS + 1, key).is_none());
        // Whitespace runs collapse inside one shape; a different command
        // is a different shape.
        let cwd = Path::new("/repo");
        assert_eq!(
            shape_key(cwd, "fno  backlog   get x"),
            shape_key(cwd, "fno backlog get x")
        );
        assert_ne!(
            shape_key(cwd, "fno backlog get x"),
            shape_key(cwd, "fno backlog get y")
        );
        // Both exit-key spellings read, and the recorder material parks on
        // the same digest path the payload flow uses.
        let payload_failed = serde_json::json!({
            "tool_name": "Bash", "cwd": "/repo", "session_id": "s-1",
            "tool_input": {"command": "fno king escalate"},
            "tool_response": {"exitCode": 1, "stderr": "refused: the stalled set is empty"}
        });
        let payload_ok = serde_json::json!({
            "tool_name": "Bash", "cwd": "/repo", "session_id": "s-1",
            "tool_input": {"command": "fno king escalate"},
            "tool_response": {"exit_code": 0}
        });
        assert_eq!(session_id(&payload_failed), "s-1");
        let exit_failed = payload_failed
            .get("tool_response")
            .and_then(|r| r.get("exit_code").or_else(|| r.get("exitCode")))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        assert_eq!(exit_failed, 1);
        let exit_ok = payload_ok
            .get("tool_response")
            .and_then(|r| r.get("exit_code").or_else(|| r.get("exitCode")))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        assert_eq!(exit_ok, 0);
        let target = dir.path().join("payloads.json");
        let key2 = shape_key(Path::new("/repo"), "fno king escalate");
        record_at(
            &target,
            3_000,
            &key2,
            true,
            &output_digest(&payload_failed),
            "fno king escalate",
        );
        record_at(
            &target,
            3_001,
            &key2,
            true,
            &output_digest(&payload_failed),
            "fno king escalate",
        );
        assert!(park_reason(&target, 3_002, &key2).is_some());
        // A success payload's digest reads empty and the clear path runs.
        record_at(&target, 3_003, &key2, false, "", "fno king escalate");
        assert!(park_reason(&target, 3_004, &key2).is_none());
    }
}
