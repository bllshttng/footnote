//! Compaction awareness (wave 1).
//!
//! Before this module, every reader in the outage stack treated a compacting
//! session as an idle session: a claude transcript writes nothing for the
//! whole compaction, so idle-classification and cap-detection both read a
//! mid-compaction worker as stranded. The fix is one positive marker at the
//! START (the `PreCompact` hook calls `fno-agents compaction mark`) and one
//! positive marker at the END (the transcript's own `compact_boundary` line),
//! so `Compacting` is only ever derived from evidence, never from silence.
//!
//! Marker files live at `<agents home>/compacting/<session>.json`,
//! session-keyed, owner and lifetime recorded in
//! `docs/state-root-inventory.md`. A stamp with no later boundary inside the
//! 45-minute ceiling reads `Unknown`, never `NotCompacting`: an expired stamp
//! is an unmeasured state, not a proof the session stopped compacting.
//!
//! A harness with no measured marker shape (codex, opencode, agy, ...) reads
//! `Unknown { reason: "no-compaction-marker-for-harness" }`. The reader never
//! infers `NotCompacting` from an absence.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::AgentsHome;

/// A claude compaction measured to take 257476 ms (transcript 0c3efcb9); the
/// ceiling is ~10x that. Past it a stamp is evidence of a wedged compaction,
/// not of a live one.
pub const COMPACTING_CEILING_SECS: i64 = 45 * 60;

/// What a reader wants to know: is this session compacting right now?
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state")]
pub enum CompactionState {
    /// A fresh stamp with no later boundary line.
    Compacting { since_epoch: i64, since: String },
    /// Either a boundary line newer than the stamp, or no stamp at all.
    /// `last_boundary` is the newest observed boundary timestamp, when one
    /// was readable - it is evidence, never a guess.
    NotCompacting { last_boundary: Option<String> },
    /// An unmeasured state. `reason` names what could not be read.
    Unknown { reason: String },
}

impl CompactionState {
    /// A compacting session - or one whose compaction state could not be
    /// measured while a stamp is live - is never acted on. `Unknown` with a
    /// live stamp is deliberately NOT false here: between `PreCompact` and
    /// the boundary line the session is structurally idle, and trap 5 of
    /// forbids rendering an unmeasured state as a healthy one.
    pub fn possibly_compacting(&self) -> bool {
        match self {
            CompactionState::Compacting { .. } => true,
            CompactionState::Unknown { reason } => reason == "stamp-past-ceiling",
            CompactionState::NotCompacting { .. } => false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CompactionStamp {
    session: String,
    since_epoch: i64,
    since: String,
    marked_by: String,
}

/// `<home>/compacting/<session>.json`, the session-keyed marker folder.
pub fn mark_path(home: &AgentsHome, session: &str) -> PathBuf {
    home.root()
        .join("compacting")
        .join(format!("{session}.json"))
}

/// The `PreCompact` hook's write: stamp "this session started compacting".
/// Atomic (tmp + rename) so a reader mid-compaction never sees a torn file.
pub fn mark(home: &AgentsHome, session: &str, now_epoch: i64) -> Result<PathBuf, String> {
    let session = session.trim();
    if session.is_empty() {
        return Err("a nonblank --session is required".to_string());
    }
    let path = mark_path(home, session);
    let dir = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let stamp = CompactionStamp {
        session: session.to_string(),
        since_epoch: now_epoch,
        since: epoch_to_rfc3339(now_epoch),
        marked_by: "precompact-hook".to_string(),
    };
    let body =
        serde_json::to_string_pretty(&stamp).map_err(|e| format!("stamp encode failed: {e}"))?;
    let tmp = dir.join(format!(".{}.tmp-{}", session, std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot rename into place: {e}"))?;
    Ok(path)
}

fn read_stamp(path: &Path) -> Result<Option<CompactionStamp>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("stamp unreadable: {e}")),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("stamp unparseable: {e}"))
}

fn rfc3339_to_epoch(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp())
}

fn epoch_to_rfc3339(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// One JSONL transcript scan. Returns the FIRST `"subtype":"compact_boundary"`
/// entry whose timestamp is strictly newer than `after_epoch`, mirroring the
/// end marker the claude CLI itself writes (`type:"system"` with
/// `compactMetadata`, measured 2026-09-08).
fn first_boundary_after(transcript: &Path, after_epoch: i64) -> Result<Option<String>, String> {
    let raw =
        std::fs::read_to_string(transcript).map_err(|e| format!("transcript unreadable: {e}"))?;
    for line in raw.lines() {
        if !line.contains("compact_boundary") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("subtype").and_then(|v| v.as_str()) != Some("compact_boundary") {
            continue;
        }
        let Some(ts) = row.get("timestamp").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(epoch) = rfc3339_to_epoch(ts) {
            if epoch > after_epoch {
                return Ok(Some(ts.to_string()));
            }
        }
    }
    Ok(None)
}

/// The timestamp of the newest `type:"assistant"` entry. User, system and
/// summary lines keep transcript mtime warm without meaning the worker spoke
/// (trap 4: one session read 0.4h fresh by mtime while its newest assistant
/// entry was 10.5h old), so liveness reads THIS, never mtime.
pub fn newest_assistant_ts(transcript: &Path) -> Result<Option<String>, String> {
    let raw =
        std::fs::read_to_string(transcript).map_err(|e| format!("transcript unreadable: {e}"))?;
    let mut newest: Option<(i64, String)> = None;
    for line in raw.lines() {
        // Cheap prefilter before the JSON parse: only assistant entries can win.
        if !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(ts) = row.get("timestamp").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(epoch) = rfc3339_to_epoch(ts) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(e, _)| epoch > *e) {
            newest = Some((epoch, ts.to_string()));
        }
    }
    Ok(newest.map(|(_, ts)| ts))
}

/// The full reader. `transcript` is the claude JSONL path when known; a
/// `None` transcript with a live stamp still answers from the stamp alone.
pub fn compaction_state(
    home: &AgentsHome,
    harness: &str,
    session_id: &str,
    transcript: Option<&Path>,
    now_epoch: i64,
) -> CompactionState {
    if harness != "claude" {
        // No marker shape measured for codex/opencode/agy transcripts; the
        // reader never infers NotCompacting from an absence.
        return CompactionState::Unknown {
            reason: "no-compaction-marker-for-harness".to_string(),
        };
    }
    let stamp = match read_stamp(&mark_path(home, session_id)) {
        Ok(Some(stamp)) => stamp,
        Ok(None) => {
            let last_boundary = transcript.and_then(|t| newest_boundary(t).ok().flatten());
            return CompactionState::NotCompacting { last_boundary };
        }
        Err(reason) => return CompactionState::Unknown { reason },
    };
    match transcript {
        Some(t) => match first_boundary_after(t, stamp.since_epoch) {
            Ok(Some(ts)) => CompactionState::NotCompacting {
                last_boundary: Some(ts),
            },
            Ok(None) => past_ceiling_or_compacting(&stamp, now_epoch),
            Err(reason) => CompactionState::Unknown {
                reason: format!("boundary-scan-failed: {reason}"),
            },
        },
        None => past_ceiling_or_compacting(&stamp, now_epoch),
    }
}

fn past_ceiling_or_compacting(stamp: &CompactionStamp, now_epoch: i64) -> CompactionState {
    if now_epoch - stamp.since_epoch > COMPACTING_CEILING_SECS {
        CompactionState::Unknown {
            reason: "stamp-past-ceiling".to_string(),
        }
    } else {
        CompactionState::Compacting {
            since_epoch: stamp.since_epoch,
            since: stamp.since.clone(),
        }
    }
}

/// The newest boundary in a transcript with no stamp to compare against.
fn newest_boundary(transcript: &Path) -> Result<Option<String>, String> {
    let raw =
        std::fs::read_to_string(transcript).map_err(|e| format!("transcript unreadable: {e}"))?;
    let mut newest: Option<(i64, String)> = None;
    for line in raw.lines() {
        if !line.contains("compact_boundary") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if row.get("subtype").and_then(|v| v.as_str()) != Some("compact_boundary") {
            continue;
        }
        let Some(ts) = row.get("timestamp").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(epoch) = rfc3339_to_epoch(ts) {
            if newest.as_ref().is_none_or(|(e, _)| epoch > *e) {
                newest = Some((epoch, ts.to_string()));
            }
        }
    }
    Ok(newest.map(|(_, ts)| ts))
}

/// Count the compactions a transcript records at or after `since_epoch`.
/// The Claude CLI writes `compact_boundary` itself, so this counts what a
/// hook-fed journal row can miss. An empty or unparseable `since` counts every
/// boundary, the same conservative direction `in_tenure` takes.
pub fn count_boundaries_since(transcript: &Path, since_epoch: Option<i64>) -> Result<u64, String> {
    let file =
        std::fs::File::open(transcript).map_err(|e| format!("transcript unreadable: {e}"))?;
    let mut count = 0;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| format!("transcript unreadable: {e}"))?;
        if !line.contains("compact_boundary") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if row.get("subtype").and_then(|v| v.as_str()) != Some("compact_boundary") {
            continue;
        }
        let Some(ts) = row.get("timestamp").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(epoch) = rfc3339_to_epoch(ts) else {
            continue;
        };
        if since_epoch.is_none_or(|since| epoch >= since) {
            count += 1;
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------------
// CLI verb: `fno-agents compaction mark --session <id>` (the PreCompact hook
// leg) and `status` (read-side debug surface; the daemon and provider_cap call
// the library in process).
// ---------------------------------------------------------------------------

pub fn run_compaction(args: &[String]) -> i32 {
    let home = AgentsHome::from_env();
    match args.split_first() {
        Some((action, rest)) if action == "mark" => compaction_mark(&home, rest),
        Some((action, rest)) if action == "status" => compaction_status(&home, rest),
        Some((action, rest)) if action == "operator-turns" => crate::operator_turns::run(rest),
        _ => {
            eprintln!("usage: compaction mark --session <id> | status --harness <h> --session <id> [--transcript <path>] [--json] | operator-turns --session <id> --transcript <path> --capture-dir <dir>");
            2
        }
    }
}

pub(crate) fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn compaction_mark(home: &AgentsHome, args: &[String]) -> i32 {
    let Some(session) = flag_value(args, "--session") else {
        eprintln!("compaction mark: --session <id> is required");
        return 2;
    };
    match mark(home, &session, now_epoch_secs()) {
        Ok(path) => {
            println!("marked: {}", path.display());
            0
        }
        Err(reason) => {
            eprintln!("compaction mark failed: {reason}");
            1
        }
    }
}

fn compaction_status(home: &AgentsHome, args: &[String]) -> i32 {
    let json = crate::json_output::requested(args);
    let Some(session) = flag_value(args, "--session") else {
        eprintln!("compaction status: --session <id> is required");
        return 2;
    };
    let harness = flag_value(args, "--harness").unwrap_or_else(|| "claude".to_string());
    let transcript = flag_value(args, "--transcript").map(PathBuf::from);
    let state = compaction_state(
        home,
        &harness,
        &session,
        transcript.as_deref(),
        now_epoch_secs(),
    );
    if json {
        println!("{}", serde_json::to_string(&state).unwrap_or_default());
    } else {
        match &state {
            CompactionState::Compacting { since, .. } => println!("compacting since {since}"),
            CompactionState::NotCompacting { last_boundary } => match last_boundary {
                Some(ts) => println!("not compacting (last boundary {ts})"),
                None => println!("not compacting"),
            },
            CompactionState::Unknown { reason } => println!("unknown ({reason})"),
        }
    }
    0
}

pub fn now_epoch_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// Tests. Fixtures quote the REAL transcript shapes measured on this machine:
// the assistant 429 tail (worktree transcript, 2026-08-17) and the
// system compact_boundary line (worktree transcript, 2026-09-08).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> AgentsHome {
        let dir =
            std::env::temp_dir().join(format!("fno-compaction-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        AgentsHome::at(dir)
    }

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    // Real assistant 429 tail, byte-shape from the wild (timestamps kept).
    const FOUR29_LINE: &str = r#"{"parentUuid":"p1","isSidechain":false,"type":"assistant","timestamp":"2026-08-17T17:29:20.381Z","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"API Error: Request rejected (429) · [1308][Usage limit reached for 5 hour. Your limit will reset at 2026-08-18 02:03:44][20260818012920fc56663065714c5e]"}]}}"#;

    // Real compact_boundary entry, byte-shape from the wild.
    const BOUNDARY_LINE: &str = r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-09-08T17:51:01.245Z","uuid":"edeae8ce-8036-4160-88ab-62c458aa3173","compactMetadata":{"trigger":"auto","preTokens":86169,"postTokens":38476,"durationMs":216579}}"#;

    // A user entry 1 minute old that keeps mtime warm (trap 4 fixture).
    fn user_line(ts: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"text","text":"keep-alive"}}]}}}}"#
        )
    }

    #[test]
    fn ac1_hp_a_fresh_stamp_without_a_boundary_reads_compacting_then_not() {
        let home = tmp_home("hp");
        let session = "s-hp";
        let now = now_epoch_secs();
        mark(&home, session, now).unwrap();
        let transcript = home.root().join("t.jsonl");
        write_file(&transcript, &format!("{FOUR29_LINE}\n"));

        let state = compaction_state(&home, "claude", session, Some(&transcript), now + 600);
        assert_eq!(
            state,
            CompactionState::Compacting {
                since_epoch: now,
                since: epoch_to_rfc3339(now),
            }
        );

        // The boundary line lands at T+5m: the compaction is over.
        let mut body = std::fs::read_to_string(&transcript).unwrap();
        body.push_str(&format!(
            "{}\n",
            BOUNDARY_LINE.replace("2026-09-08T17:51:01.245Z", &epoch_to_rfc3339(now + 300))
        ));
        write_file(&transcript, &body);

        let state = compaction_state(&home, "claude", session, Some(&transcript), now + 600);
        assert!(matches!(state, CompactionState::NotCompacting { .. }));
    }

    #[test]
    fn count_boundaries_since_streams_and_applies_the_inclusive_start() {
        let home = tmp_home("count");
        let transcript = home.root().join("t.jsonl");
        let body = [
            BOUNDARY_LINE.replace("2026-09-08T17:51:01.245Z", "2026-09-01T00:00:00Z"),
            BOUNDARY_LINE.replace("2026-09-08T17:51:01.245Z", "2026-09-10T00:00:00Z"),
            BOUNDARY_LINE.replace("2026-09-08T17:51:01.245Z", "2026-09-11T00:00:00Z"),
            BOUNDARY_LINE.replace("2026-09-08T17:51:01.245Z", "2026-09-12T00:00:00Z"),
        ]
        .join("\n");
        write_file(&transcript, &body);

        assert_eq!(
            count_boundaries_since(
                &transcript,
                Some(
                    chrono::DateTime::parse_from_rfc3339("2026-09-10T00:00:00Z")
                        .unwrap()
                        .timestamp(),
                )
            )
            .unwrap(),
            3
        );
        assert_eq!(count_boundaries_since(&transcript, None).unwrap(), 4);
    }

    #[test]
    fn ac1_err_codex_reads_unknown_and_a_stale_stamp_never_reads_not_compacting() {
        let home = tmp_home("err");
        // Codex: no measured marker shape.
        let state = compaction_state(&home, "codex", "s-codex", None, now_epoch_secs());
        assert_eq!(
            state,
            CompactionState::Unknown {
                reason: "no-compaction-marker-for-harness".to_string()
            }
        );

        // Claude with a stamp older than the ceiling and no boundary.
        let session = "s-stale";
        let now = now_epoch_secs();
        mark(&home, session, now - COMPACTING_CEILING_SECS - 60).unwrap();
        let state = compaction_state(&home, "claude", session, None, now);
        assert_eq!(
            state,
            CompactionState::Unknown {
                reason: "stamp-past-ceiling".to_string()
            }
        );
        // And a caller must never treat that as idle.
        assert!(state.possibly_compacting());
    }

    #[test]
    fn ac1_ts_newest_assistant_ts_ignores_mtime_warming_user_lines() {
        let dir = std::env::temp_dir().join(format!("fno-compaction-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("t.jsonl");
        // Newest assistant entry: 10 hours ago. Newest line: a user entry 1
        // minute ago. The answer is the 10-hour-old assistant timestamp.
        write_file(
            &transcript,
            &format!("{FOUR29_LINE}\n{}\n", user_line("2026-09-13T16:59:00.000Z")),
        );
        let ts = newest_assistant_ts(&transcript).unwrap();
        assert_eq!(ts.as_deref(), Some("2026-08-17T17:29:20.381Z"));
    }

    #[test]
    fn a_stale_marker_directory_never_blocks_a_clean_read() {
        let home = tmp_home("no-stamp");
        let state = compaction_state(&home, "claude", "absent", None, now_epoch_secs());
        assert_eq!(
            state,
            CompactionState::NotCompacting {
                last_boundary: None
            }
        );
        assert!(!state.possibly_compacting());
    }
}
