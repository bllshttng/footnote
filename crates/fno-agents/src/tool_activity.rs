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
pub(crate) struct ToolFold {
    pub(crate) offset: u64,
    pub(crate) calls: u64,
    pub(crate) errors: u64,
    /// The last pass that counted this fold: the prune's clock.
    last_seen: Option<Instant>,
}

impl ToolFold {
    /// Absorb the bytes appended to `path` since the last call. Only whole
    /// newline-terminated lines are consumed; a partial tail stays for the
    /// next pass. A file shorter than the remembered offset (rotated,
    /// replaced) resets the fold and recounts once.
    pub(crate) fn absorb(&mut self, path: &Path, harness: &str) {
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
    if last.elapsed() < SCAN_CADENCE || in_flight.swap(true, Ordering::SeqCst) {
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
