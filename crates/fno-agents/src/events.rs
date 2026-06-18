//! Operator-facing `events.jsonl` emitter (Wave 3, task 3.2).
//!
//! The boundary (cross-language coupling discipline): `events.jsonl` is
//! **operator-facing** (what an auditor / the stop hook sees); per-agent
//! `timeline.jsonl` is **model-facing** (what the model would see in its
//! transcript). This module owns the operator side.
//!
//! Two invariants are load-bearing and tested here:
//!
//! - **500B payload cap** (Silent-Failure-Hunter finding): a payload whose
//!   serialized JSON object exceeds [`MAX_EVENT_PAYLOAD_BYTES`] is REJECTED at
//!   the write boundary and replaced by a small `event_payload_too_large`
//!   meta-event. An oversized event must never silently truncate or vanish.
//! - **FIFO per-emitter ordering**: each emission is open-`O_APPEND`-write-close.
//!   A single event line stays well under `PIPE_BUF` (4096B; the cap keeps it
//!   under 600B with the `ts`/`kind` framing), so the append is atomic at the
//!   kernel level. Cross-emitter ordering (Python <-> Rust interleaving) is
//!   unspecified by design; consumers filter by `source` when ordering matters.

use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum serialized size (bytes) of an event's payload object. Payloads over
/// this are rejected and replaced by a meta-event. Chosen per the design's
/// Silent-Failure-Hunter table; keeps the final line under `PIPE_BUF`.
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 500;

/// Rotate `events.jsonl` once it exceeds this many bytes. The active file is
/// renamed to `events.jsonl.1` (single generation; older history is the
/// operator's archive concern, not the daemon's).
pub const ROTATE_AT_BYTES: u64 = 8 * 1024 * 1024;

/// Errors the emitter surfaces to its caller. Emission failures are logged by
/// the daemon rather than aborting the operation that triggered them: a missing
/// audit line must not take down a live agent.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error("event io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("event payload was not a JSON object")]
    NotAnObject,
}

/// Appends structured events to a JSONL file. Cheap to clone (just a path); the
/// emitter holds no long-lived file descriptor, so rotation cannot strand a
/// stale fd (Concurrency invariant: "open with `O_APPEND` per emission").
#[derive(Debug, Clone)]
pub struct EventEmitter {
    path: PathBuf,
    source: String,
}

impl EventEmitter {
    /// Construct an emitter writing to `path`, tagging every line with
    /// `source` (e.g. `"daemon"`, `"worker:wkA"`) so consumers can filter by
    /// emitter when cross-emitter ordering matters.
    pub fn new(path: impl Into<PathBuf>, source: impl Into<String>) -> Self {
        EventEmitter {
            path: path.into(),
            source: source.into(),
        }
    }

    /// Emit `kind` with a structured payload. The payload must serialize to a
    /// JSON object; `ts` (wall-clock RFC3339), `kind`, and `source` are merged
    /// in by the emitter and override any same-named payload keys.
    ///
    /// Returns `Ok(())` on a successful append. An oversized payload is NOT an
    /// error to the caller: the meta-event is written and `Ok(())` returned, so
    /// callers cannot accidentally treat "too large" as "not emitted".
    pub fn emit<P: Serialize>(&self, kind: &str, payload: &P) -> Result<(), EmitError> {
        let value = serde_json::to_value(payload).map_err(|_| EmitError::NotAnObject)?;
        let obj = match value {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            _ => return Err(EmitError::NotAnObject),
        };

        // Size the payload object (sans framing) against the cap. Oversized ->
        // substitute a small meta-event that records the intent and size, so an
        // auditor sees that an event was dropped and why, never silence.
        let payload_len = serde_json::to_string(&obj).map(|s| s.len()).unwrap_or(0);
        if payload_len > MAX_EVENT_PAYLOAD_BYTES {
            let mut meta = Map::new();
            meta.insert("intended_kind".into(), Value::String(kind.to_string()));
            meta.insert("size".into(), Value::Number(payload_len.into()));
            return self.write_line("event_payload_too_large", meta);
        }

        self.write_line(kind, obj)
    }

    /// Emit an event whose payload is built ad-hoc as a JSON object. Convenience
    /// for call sites that assemble fields inline rather than via a struct.
    pub fn emit_fields(&self, kind: &str, fields: Map<String, Value>) -> Result<(), EmitError> {
        let payload_len = serde_json::to_string(&fields).map(|s| s.len()).unwrap_or(0);
        if payload_len > MAX_EVENT_PAYLOAD_BYTES {
            let mut meta = Map::new();
            meta.insert("intended_kind".into(), Value::String(kind.to_string()));
            meta.insert("size".into(), Value::Number(payload_len.into()));
            return self.write_line("event_payload_too_large", meta);
        }
        self.write_line(kind, fields)
    }

    fn write_line(&self, kind: &str, mut obj: Map<String, Value>) -> Result<(), EmitError> {
        obj.insert("ts".into(), Value::String(now_rfc3339()));
        obj.insert("kind".into(), Value::String(kind.to_string()));
        obj.insert("source".into(), Value::String(self.source.clone()));
        let mut line = serde_json::to_string(&Value::Object(obj))
            .map_err(|e| EmitError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        line.push('\n');

        self.maybe_rotate()?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // O_APPEND open-write-close: the append is atomic for a sub-PIPE_BUF
        // line, so concurrent emitters never interleave a single line.
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Rename the active file aside once it grows past [`ROTATE_AT_BYTES`].
    /// Best-effort: a rotation race (two emitters both seeing the file large)
    /// is harmless because the rename is idempotent at the path level and the
    /// next `open(..., append)` recreates the active file.
    fn maybe_rotate(&self) -> Result<(), EmitError> {
        let size = match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(_) => return Ok(()), // not yet created; nothing to rotate
        };
        if size <= ROTATE_AT_BYTES {
            return Ok(());
        }
        let rotated = rotated_path(&self.path);
        // Ignore a rename failure (another emitter already rotated): the goal is
        // bounded file size, not exclusive rotation ownership.
        let _ = std::fs::rename(&self.path, rotated);
        Ok(())
    }

    /// Path this emitter writes to (test/inspection helper).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

/// Wall-clock timestamp in RFC3339 with millisecond precision and a `Z` suffix.
/// Event `ts` is wall-clock for human audit (drive-window math uses the
/// monotonic clock instead; LD17). Implemented without `chrono` to keep the
/// dependency surface minimal.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let (year, month, day, hour, min, sec) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

/// Convert unix seconds (UTC) to civil (Y, M, D, h, m, s). Uses Howard Hinnant's
/// days_from_civil inverse; correct for all dates this daemon will ever stamp.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;

    // days since 1970-01-01 -> civil date (Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_events_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-events-test-{}-{}-{}.jsonl",
            tag,
            std::process::id(),
            // nanos for uniqueness across same-pid tests
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).expect("each line is valid json"))
            .collect()
    }

    #[test]
    fn emits_line_with_ts_kind_source() {
        let path = temp_events_path("basic");
        let em = EventEmitter::new(&path, "daemon");
        em.emit("daemon_started", &json!({"pid": 4242, "version": "0.1.0"}))
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!(l["kind"], "daemon_started");
        assert_eq!(l["source"], "daemon");
        assert_eq!(l["pid"], 4242);
        assert!(l["ts"].as_str().unwrap().ends_with('Z'));
        assert!(l["ts"].as_str().unwrap().starts_with("20"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn oversized_payload_becomes_meta_event_not_silence() {
        let path = temp_events_path("oversize");
        let em = EventEmitter::new(&path, "daemon");
        let huge = "x".repeat(2000);
        em.emit("agent_spawned", &json!({"blob": huge})).unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1, "exactly one line: the meta-event");
        let l = &lines[0];
        assert_eq!(l["kind"], "event_payload_too_large");
        assert_eq!(l["intended_kind"], "agent_spawned");
        assert!(l["size"].as_u64().unwrap() > MAX_EVENT_PAYLOAD_BYTES as u64);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn appends_preserve_fifo_order() {
        let path = temp_events_path("fifo");
        let em = EventEmitter::new(&path, "daemon");
        for i in 0..10 {
            em.emit("tick", &json!({"seq": i})).unwrap();
        }
        let lines = read_lines(&path);
        let seqs: Vec<u64> = lines.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, (0..10).collect::<Vec<_>>());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn null_payload_is_allowed_as_empty_object() {
        let path = temp_events_path("null");
        let em = EventEmitter::new(&path, "worker:wkA");
        em.emit("heartbeat", &Value::Null).unwrap();
        let lines = read_lines(&path);
        assert_eq!(lines[0]["kind"], "heartbeat");
        assert_eq!(lines[0]["source"], "worker:wkA");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn civil_date_matches_known_epoch_points() {
        // 0 -> 1970-01-01T00:00:00
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 1700000000 -> 2023-11-14T22:13:20 UTC (known fixture)
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }
}
