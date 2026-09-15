//! Operator-facing `events.jsonl` emitter (Wave 3, task 3.2).
//!
//! The boundary (cross-language coupling discipline): `events.jsonl` is
//! **operator-facing** (what an auditor / the stop hook sees); per-agent
//! `timeline.jsonl` is **model-facing** (what the model would see in its
//! transcript). This module owns the operator side.
//!
//! Two invariants are load-bearing and tested here:
//!
//! - **Payload cap**: a payload whose serialized JSON object exceeds the
//!   schema's `limits.max_data_bytes` (65536, from `events_limits`) is
//!   REJECTED at the write boundary and replaced by a small
//!   `event_payload_too_large` meta-event. An oversized event must never
//!   silently truncate or vanish. (Measured 2026-09-15: the richest real
//!   check-in carries 646 characters in its `change` field alone, so the old
//!   500-byte literal dropped the most valuable rows, not noise.)
//! - **FIFO per-emitter ordering**: each emission is open-`O_APPEND`-write-close.
//!   A single event line is written atomically line-at-a-time; cross-emitter
//!   ordering (Python <-> Rust interleaving) is unspecified by design;
//!   consumers filter by `source` when ordering matters.
//! - **No rotation destroys ingested history**: a durable journal past
//!   [`ROTATE_AT_BYTES`] is ingested into its `events.db` (`events_store`)
//!   BEFORE the rename, so one generation on disk still means no lost rows.
//!
//! Envelope (x-2901): the unified line is `{ts, type, source, data:{...}}` -
//! the same shape the Python/fno emitter and the Rust loop runtime already
//! write. The retired `{ts, kind, <flat fields>}` shape is read-tolerated by
//! `subscribe`/`digest` during the mixed-binary window; nothing emits it here.

use serde::Serialize;
use serde_json::{Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Rotate `events.jsonl` once it exceeds this many bytes. The active file is
/// renamed to `events.jsonl.1` and one generation stays on disk; every
/// durable row of the renamed file is already ingested into its `events.db`
/// first, so the rename never destroys history.
pub const ROTATE_AT_BYTES: u64 = 8 * 1024 * 1024;

/// Sibling journal suffix for ephemeral-class rows (x-add3). The Python
/// `fno.events` module declares the same string; a parity test
/// (`cli/tests/events/test_ephemeral_set_parity.py`) holds the two equal so
/// both languages write the same sibling file.
pub const EPHEMERAL_SUFFIX: &str = ".ephemeral";

/// Event types the schema declares `retention: ephemeral` (x-add3). These are
/// routed to the `.ephemeral` sibling journal at the write boundary so a
/// high-cadence gauge (mux_pane_counters: 30s samples, ~5KB a row) cannot
/// consume the durable journal's rotation budget. Kept equal to
/// `cli/src/fno/events/schema.yaml` by the same parity test; flip the class in
/// both places or the two write boundaries route differently.
pub const EPHEMERAL_EVENT_TYPES: &[&str] = &[
    "claim_acquired",
    "claim_clock_skew_rejected",
    "claim_force_overridden",
    "claim_idempotent_reacquired",
    "claim_rebound",
    "claim_refreshed",
    "claim_released",
    "claim_stale_reclaimed",
    "graph_tx_conflict",
    "human_touch",
    "mux_pane_counters",
    "orphan_reap_sweep",
    "single_flight_gate",
];

/// Whether an event kind belongs to the schema-declared ephemeral class.
pub fn is_ephemeral_event(kind: &str) -> bool {
    EPHEMERAL_EVENT_TYPES.contains(&kind)
}

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
    /// JSON object; the emitter frames it as `{ts, type: kind, source, data}`
    /// (wall-clock RFC3339 `ts`), so payload keys live under `data` and never
    /// collide with the envelope fields.
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
        if payload_len > crate::events_limits::max_data_bytes() {
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
        if payload_len > crate::events_limits::max_data_bytes() {
            let mut meta = Map::new();
            meta.insert("intended_kind".into(), Value::String(kind.to_string()));
            meta.insert("size".into(), Value::Number(payload_len.into()));
            return self.write_line("event_payload_too_large", meta);
        }
        self.write_line(kind, fields)
    }

    fn write_line(&self, event_type: &str, payload: Map<String, Value>) -> Result<(), EmitError> {
        // Unified envelope (x-2901): the payload nests under `data`, the kind is
        // stamped as `type`. The schema cap is measured on `payload` before
        // this framing (in emit/emit_fields), so nesting never changes which
        // events are dropped.
        let mut obj = Map::new();
        obj.insert("ts".into(), Value::String(now_rfc3339()));
        obj.insert("type".into(), Value::String(event_type.to_string()));
        obj.insert("source".into(), Value::String(self.source.clone()));
        obj.insert("data".into(), Value::Object(payload));
        let mut line = serde_json::to_string(&Value::Object(obj))
            .map_err(|e| EmitError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        line.push('\n');

        // Honor the declared retention class (x-add3): an ephemeral row goes to
        // the sibling journal beside this emitter's file, rotating on its own
        // size; every other class keeps the emitter's path. The sibling shares
        // the emitter's directory, so the append/rotate machinery below runs
        // unchanged against whichever file the row targets.
        let ephemeral_target = is_ephemeral_event(event_type).then(|| {
            // Follow a journal symlink before deriving the sibling (a worktree
            // journal linked into the repo space routes its ephemeral rows to
            // that space's sibling, mirroring the Python writer's resolve-then-
            // derive order). A journal that does not exist yet has no symlink
            // to follow, so the unresolved path is the right base there.
            let base = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
            ephemeral_path(&base)
        });
        let target: &Path = ephemeral_target.as_deref().unwrap_or(&self.path);

        // The retention class is also the durability boundary: a durable
        // target's rotation must ingest first (x-1e71); an ephemeral target's
        // rotation stays ingest-free, those rows are disposable by design.
        self.maybe_rotate(target, ephemeral_target.is_none())?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // O_APPEND open-write-close: the append is atomic for a sub-PIPE_BUF
        // line, so concurrent emitters never interleave a single line.
        let mut f = OpenOptions::new().create(true).append(true).open(target)?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Rename the active file aside once it grows past [`ROTATE_AT_BYTES`].
    /// Best-effort: a rotation race (two emitters both seeing the file large)
    /// is harmless because the rename is idempotent at the path level and the
    /// next `open(..., append)` recreates the active file.
    ///
    /// A durable target syncs its store FIRST: a rename whose rows the store
    /// does not hold would destroy history, so a failed ingest leaves the
    /// journal growing past the threshold instead (the sync retries on the
    /// next emit past 8 MiB). After a successful sync the file is re-stat'd,
    /// so a second emitter that raced the ingest is not stripped of a fresh
    /// file it just recreated.
    fn maybe_rotate(&self, path: &Path, durable: bool) -> Result<(), EmitError> {
        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return Ok(()), // not yet created; nothing to rotate
        };
        if size <= ROTATE_AT_BYTES {
            return Ok(());
        }
        if durable {
            if let Err(e) = crate::events_store::sync(path) {
                eprintln!(
                    "events: rotation deferred, store ingest failed ({}: {e})",
                    crate::events_store::store_path(path).display()
                );
                return Ok(());
            }
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            if size <= ROTATE_AT_BYTES {
                return Ok(());
            }
        }
        let rotated = rotated_path(path);
        // Ignore a rename failure (another emitter already rotated): the goal is
        // bounded file size, not exclusive rotation ownership.
        let _ = std::fs::rename(path, rotated);
        Ok(())
    }

    /// Path this emitter writes to (test/inspection helper).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

/// The sibling journal an ephemeral-class row is routed to (same directory,
/// same stem, the [`EPHEMERAL_SUFFIX`] tail). Shared with the claims audit
/// writer so every Rust journal writer routes identically.
pub(crate) fn ephemeral_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(EPHEMERAL_SUFFIX);
    PathBuf::from(s)
}

/// Wall-clock timestamp in RFC3339 with millisecond precision and a `Z` suffix.
/// Event `ts` is wall-clock for human audit (drive-window math uses the
/// monotonic clock instead; LD17). Implemented without `chrono` to keep the
/// dependency surface minimal.
pub(crate) fn now_rfc3339() -> String {
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
    fn emits_line_with_ts_type_source_data() {
        let path = temp_events_path("basic");
        let em = EventEmitter::new(&path, "daemon");
        em.emit("daemon_started", &json!({"pid": 4242, "version": "0.1.0"}))
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!(l["type"], "daemon_started");
        assert_eq!(l["source"], "daemon");
        assert_eq!(l["data"]["pid"], 4242);
        assert!(l.get("kind").is_none(), "no legacy kind field");
        assert!(l["ts"].as_str().unwrap().ends_with('Z'));
        assert!(l["ts"].as_str().unwrap().starts_with("20"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn oversized_payload_becomes_meta_event_not_silence() {
        let path = temp_events_path("oversize");
        let em = EventEmitter::new(&path, "daemon");
        let huge = "x".repeat(crate::events_limits::max_data_bytes() + 1);
        em.emit("agent_spawned", &json!({"blob": huge})).unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1, "exactly one line: the meta-event");
        let l = &lines[0];
        assert_eq!(l["type"], "event_payload_too_large");
        assert_eq!(l["data"]["intended_kind"], "agent_spawned");
        assert!(
            l["data"]["size"].as_u64().unwrap() > crate::events_limits::max_data_bytes() as u64
        );
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
        let seqs: Vec<u64> = lines
            .iter()
            .map(|l| l["data"]["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, (0..10).collect::<Vec<_>>());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn null_payload_is_allowed_as_empty_object() {
        let path = temp_events_path("null");
        let em = EventEmitter::new(&path, "worker:wkA");
        em.emit("heartbeat", &Value::Null).unwrap();
        let lines = read_lines(&path);
        assert_eq!(lines[0]["type"], "heartbeat");
        assert_eq!(lines[0]["source"], "worker:wkA");
        assert_eq!(lines[0]["data"], json!({}));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn civil_date_matches_known_epoch_points() {
        // 0 -> 1970-01-01T00:00:00
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 1700000000 -> 2023-11-14T22:13:20 UTC (known fixture)
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn ephemeral_kind_lands_in_sibling_never_main() {
        let path = temp_events_path("ephemeral");
        let em = EventEmitter::new(&path, "daemon");
        em.emit("mux_pane_counters", &json!({"session": "s1", "panes": []}))
            .unwrap();

        assert!(
            read_lines(&path).is_empty(),
            "ephemeral row reached the durable journal"
        );
        let sibling = ephemeral_path(&path);
        let rows = read_lines(&sibling);
        assert_eq!(
            rows.len(),
            1,
            "the gauge must be provably alive in the sibling"
        );
        assert_eq!(rows[0]["type"], "mux_pane_counters");
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&sibling).ok();
    }

    #[test]
    fn durable_kind_keeps_emitter_path() {
        let path = temp_events_path("durable");
        let em = EventEmitter::new(&path, "daemon");
        em.emit(
            "operator_decision",
            &json!({"decision_id": "d-1", "decision": "x"}),
        )
        .unwrap();

        let rows = read_lines(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["type"], "operator_decision");
        assert!(
            !ephemeral_path(&path).exists(),
            "non-ephemeral emit created the sibling"
        );
        std::fs::remove_file(&path).ok();
    }

    /// Fill a journal past [`ROTATE_AT_BYTES`] with generation-marked rows.
    fn fill_past_threshold(path: &Path, gen: u64) {
        let mut fh = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        let blob = "x".repeat(4096);
        for n in 0..2100 {
            writeln!(
                fh,
                "{}",
                json!({"ts": "2026-09-10T00:00:00Z", "type": "generation_marker",
                       "source": "test", "data": {"gen": gen, "n": n, "blob": blob}})
            )
            .unwrap();
        }
    }

    fn store_count(store: &Path, event_type: &str) -> u64 {
        crate::events_store::open_read(store)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM events WHERE type = ?1",
                [&event_type],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as u64
    }

    #[test]
    fn durable_rotation_ingests_before_rename() {
        // AC2-HP: two rotations later, the store answers for every
        // generation, not just the one still on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fill_past_threshold(&path, 1);
        let em = EventEmitter::new(&path, "daemon");
        em.emit("operator_decision", &json!({"decision_id": "d-1"}))
            .unwrap();
        assert!(
            path.metadata().unwrap().len() < super::ROTATE_AT_BYTES,
            "the emitter rotated the full journal"
        );
        fill_past_threshold(&path, 2);
        em.emit("operator_decision", &json!({"decision_id": "d-2"}))
            .unwrap();
        let store = crate::events_store::store_path(&path);
        assert_eq!(store_count(&store, "generation_marker"), 4200);
        // The tail row of the live file ingests lazily: it is IN the next
        // sync (rotation or history read), never after. d-1 was ingested by
        // the second rotation; d-2 is the live tail.
        assert_eq!(store_count(&store, "operator_decision"), 1);
    }

    #[test]
    fn blocked_store_defers_the_rotation() {
        // AC2-ERR: a store that cannot be created stops the rename; the
        // journal keeps every row rather than dropping them unstored.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        fill_past_threshold(&path, 1);
        std::fs::create_dir(dir.path().join("events.db")).unwrap();
        let em = EventEmitter::new(&path, "daemon");
        em.emit("operator_decision", &json!({"decision_id": "d-1"}))
            .unwrap();
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines > 2100, "the journal kept its rows (got {lines})");
        assert!(
            !dir.path().join("events.jsonl.1").exists(),
            "no rotation happened"
        );
    }
}
