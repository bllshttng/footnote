//! The sideline activity ramp's source: incremental tool-call counting over
//! session transcripts. One pass per daemon tick (5s ceiling) reads only the
//! bytes appended since the last pass - a per-session fold holds the byte
//! offset at the last complete line, so a multi-megabyte transcript costs a
//! read of its tail, never a re-read of the whole file. A daemon restart
//! re-reads its transcripts once, the same trade [`RunningCost`] makes.
//!
//! Counts land on the registry row (the same channel the context readings
//! ride, stamp-gated on the server's 1s poll); the fold itself stays
//! in-memory, keyed by `harness_session_id` (law d-e952ed19).
//!
//! A failed call is read the way each transcript states it: a claude
//! `tool_result` carries its own `is_error` flag; a codex rollout has no
//! flag, so the refusal buckets that price `refusal_rate` read the output
//! text - the same read the lead check-in uses.

use std::collections::HashMap;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// The per-session fold: byte offset plus the cumulative counts.
#[derive(Default)]
struct ToolFold {
    offset: u64,
    calls: u64,
    errors: u64,
    /// The last pass that counted this fold: the prune's clock.
    last_seen: Option<Instant>,
}

impl ToolFold {
    /// Absorb the bytes appended to `path` since the last call. Only whole
    /// newline-terminated lines are consumed; a partial tail stays for the
    /// next pass. A file shorter than the remembered offset (rotated,
    /// replaced) resets the fold and recounts once.
    fn absorb(&mut self, path: &Path, harness: &str) {
        let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
            return;
        };
        if len < self.offset {
            *self = Self::default();
        }
        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        let mut reader = std::io::BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(n) => {
                    if line.last() != Some(&b'\n') {
                        break; // partial tail stays for the next pass
                    }
                    self.offset += n as u64;
                    if let Ok(text) = std::str::from_utf8(&line) {
                        self.row_line(text.trim_end_matches('\n'), harness);
                    }
                }
                Err(_) => break,
            }
        }
    }

    fn row_line(&mut self, line: &str, harness: &str) {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if harness == "codex" {
            self.codex_row(&row);
        } else {
            self.claude_row(&row);
        }
    }

    /// A claude transcript row: tool_use blocks are calls; a tool_result
    /// with the row's own `is_error` flag is a failure.
    fn claude_row(&mut self, row: &serde_json::Value) {
        let Some(content) = row.pointer("/message/content").and_then(|v| v.as_array()) else {
            return;
        };
        for block in content {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("tool_use") => self.calls += 1,
                Some("tool_result") => {
                    if block.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                        self.errors += 1;
                    }
                }
                _ => {}
            }
        }
    }

    /// A codex rollout row: call rows are calls; an output whose text
    /// matches a refusal bucket is a failure (no structural flag exists).
    fn codex_row(&mut self, row: &serde_json::Value) {
        let Some(payload) = row.get("payload") else {
            return;
        };
        match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "function_call" | "custom_tool_call" | "local_shell_call" => self.calls += 1,
            "custom_tool_call_output" | "function_call_output" => {
                let text = crate::session_activity::codex_output_text(payload);
                if crate::refusal_rate::buckets_of(&text).next().is_some() {
                    self.errors += 1;
                }
            }
            _ => {}
        }
    }
}

static FOLDS: OnceLock<Mutex<HashMap<String, ToolFold>>> = OnceLock::new();

/// Scan every counted row once: absorb appends, write the cumulative pair
/// onto the registry row. Runs off the daemon loop on the blocking pool.
fn scan_pass(home: &crate::paths::AgentsHome) {
    let transcripts = crate::context_run::SessionTranscripts::default();
    let mut folds = match FOLDS.get_or_init(Default::default).lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let now = Instant::now();
    let mut seen: Vec<String> = Vec::new();
    let _ = crate::state::update_registry(&home.registry_json(), |registry| {
        for entry in &mut registry.entries {
            if entry.status == crate::AgentStatus::Exited {
                continue;
            }
            let harness = entry.harness_name();
            if harness != "claude" && harness != "codex" {
                continue;
            }
            let Some(sid) = entry.harness_session_id.clone() else {
                continue;
            };
            let Some(path) = transcripts.find(&sid, harness) else {
                continue;
            };
            let fold = folds.entry(sid.clone()).or_default();
            fold.absorb(&path, harness);
            fold.last_seen = Some(now);
            entry.tool_calls = Some(fold.calls);
            entry.tool_errors = Some(fold.errors);
            seen.push(sid);
        }
    });
    // A fold whose row stopped counting (exited, reaped, transcript gone)
    // prunes after a day, the same retention the cost folds keep.
    folds.retain(|sid, fold| {
        seen.contains(sid)
            || fold
                .last_seen
                .map_or(true, |t| now.duration_since(t) < FOLD_TTL)
    });
}

/// One day: the fold retention the cost folds keep.
const FOLD_TTL: std::time::Duration = std::time::Duration::from_secs(86_400);

/// The daemon tick's tool-scan arm: throttled to [`SCAN_CADENCE`], one
/// in-flight pass behind `in_flight`, run off the loop. Same shape as
/// `liveness_sweep::maybe_sweep`.
pub(crate) fn maybe_scan(
    last: &mut Instant,
    in_flight: &std::sync::Arc<AtomicBool>,
    home: crate::paths::AgentsHome,
) {
    if last.elapsed() < SCAN_CADENCE || !in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    *last = Instant::now();
    let flag = std::sync::Arc::clone(in_flight);
    tokio::task::spawn_blocking(move || {
        scan_pass(&home);
        flag.store(false, Ordering::SeqCst);
    });
}

/// The counting ceiling the user set: at most one parse pass per 5s.
const SCAN_CADENCE: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_file(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn claude_pair(id: &str, is_error: bool) -> String {
        let call = serde_json::json!({"message": {"content": [
            {"type": "tool_use", "id": id, "name": "Bash"},
        ]}});
        let result = serde_json::json!({"message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": "out"},
        ]}});
        call.to_string() + "\n" + &result.to_string() + "\n"
    }

    fn codex_row(ptype: &str, call_id: &str, text: &str) -> String {
        let mut payload = serde_json::json!({"type": ptype});
        if !call_id.is_empty() {
            payload["call_id"] = serde_json::json!(call_id);
        }
        if !text.is_empty() {
            payload["output"] = serde_json::json!(text);
        }
        serde_json::json!({"payload": payload}).to_string() + "\n"
    }

    /// The fold reads only appended bytes: a first pass over a clean pair
    /// counts once, a second pass over unchanged bytes counts nothing, and
    /// a partial (newline-less) tail is held until it completes.
    #[test]
    fn absorb_reads_only_appended_bytes_and_holds_a_partial_tail() {
        let dir = std::env::temp_dir().join("fno-tool-activity-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hold.jsonl");
        write_file(&path, "");
        let mut fold = ToolFold::default();
        fold.absorb(&path, "claude");
        assert_eq!((fold.calls, fold.errors), (0, 0));
        write_file(&path, &claude_pair("t1", false));
        fold.absorb(&path, "claude");
        assert_eq!((fold.calls, fold.errors), (1, 0));
        let base = std::fs::read_to_string(&path).unwrap();
        // A complete pair appends and is counted once.
        write_file(&path, &(base.clone() + &claude_pair("t2", true)));
        fold.absorb(&path, "claude");
        assert_eq!((fold.calls, fold.errors), (2, 1));
        // An incomplete tail is held: nothing parses, nothing advances.
        write_file(
            &path,
            &(base.clone() + &claude_pair("t2", true) + "{\"partial"),
        );
        let held_offset = fold.offset;
        fold.absorb(&path, "claude");
        assert_eq!(
            (fold.calls, fold.errors),
            (2, 1),
            "the held tail never parses half a row"
        );
        assert_eq!(fold.offset, held_offset);
        // Completing the held line consumes it exactly once (the row is
        // not a countable event, but the bytes must clear cleanly).
        write_file(&path, &(base + &claude_pair("t2", true) + "{\"partial\n"));
        fold.absorb(&path, "claude");
        assert_eq!(fold.offset, held_offset + "{\"partial\n".len() as u64);
    }

    /// A file that shrank under the fold's offset (rotated, replaced)
    /// resets the fold and recounts.
    #[test]
    fn a_rotated_transcript_resets_and_recounts() {
        let dir = std::env::temp_dir().join("fno-tool-activity-rotate");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rotate.jsonl");
        write_file(&path, &claude_pair("t1", false));
        let mut fold = ToolFold::default();
        fold.absorb(&path, "claude");
        assert_eq!(fold.calls, 1);
        write_file(&path, "");
        fold.absorb(&path, "claude");
        assert_eq!((fold.calls, fold.errors, fold.offset), (0, 0, 0));
    }

    /// Codex: call rows count; outputs grade by the refusal buckets - the
    /// same failed-read the lead check-in prices refusal_rate with.
    #[test]
    fn codex_counts_calls_and_bucket_failures() {
        let dir = std::env::temp_dir().join("fno-tool-activity-codex");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("codex.jsonl");
        let text = [
            codex_row("function_call", "c1", ""),
            codex_row("function_call_output", "c1", "Usage: bad args"),
            codex_row("function_call", "c2", ""),
            codex_row("function_call_output", "c2", "all good"),
        ]
        .join("");
        write_file(&path, &text);
        let mut fold = ToolFold::default();
        fold.absorb(&path, "codex");
        assert_eq!(fold.calls, 2);
        assert_eq!(fold.errors, 1);
    }
}
