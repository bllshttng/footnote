//! The footnote harness's file contract, shared by the process that writes a
//! session (the `footnote` binary, through the fno crate's generated copy)
//! and the supervisor that launches it and reads it back. The record is
//! `<fno_id>.jsonl` beside the `<fno_id>/` sidecar dir. Field lists live in
//! `docs/architecture/footnote-transcript-schema.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const SCHEMA_V: u64 = 1;

/// Every record type a reader understands. An unknown type without
/// `ignorable` refuses the session: guessing at it could hide an effect.
pub const KNOWN_TYPES: &[&str] = &[
    "header",
    "turn_context",
    "user_input",
    "model_request",
    "model_attempt",
    "model_response",
    "usage",
    "tool_call",
    "effect_decision",
    "tool_start",
    "tool_result",
    "hook_decision",
    "compaction",
    "terminal",
];

/// The launch spec's version. The binary refuses any other, so a stale
/// `footnote` beside a newer supervisor fails loudly instead of misreading.
pub const LAUNCH_SPEC_V: u64 = 1;

/// The record: `<fno_id>.jsonl` beside the sidecar dir.
pub fn transcript_file(dir: &Path, fno_id: &str) -> PathBuf {
    dir.parent().unwrap_or(dir).join(format!("{fno_id}.jsonl"))
}

/// A top-level record file: `<fno_id>.jsonl` directly under a project
/// slug. Sidecar dirs carry no extension, and child transcripts live one
/// level down, inside them.
pub fn is_record_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// Read every record, refusing an unknown non-ignorable type or a bad line.
pub fn read_records(path: &Path) -> Result<Vec<Value>, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: Value = serde_json::from_str(&line)
            .map_err(|e| format!("{}:{}: not JSON: {e}", path.display(), n + 1))?;
        let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
        let ignorable = rec.get("ignorable").and_then(Value::as_bool) == Some(true);
        if !KNOWN_TYPES.contains(&ty) && !ignorable {
            return Err(format!(
                "{}:{}: unknown record type {ty:?}",
                path.display(),
                n + 1
            ));
        }
        out.push(rec);
    }
    Ok(out)
}

/// The borrowed model endpoint, resolved by the supervisor from its route
/// env and config. It rides stdin, so the key never reaches argv or env.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointSpec {
    pub base_url: String,
    pub key: String,
    pub bearer: bool,
    /// `anthropic` or `openai`.
    pub wire: String,
    pub provider_id: Option<String>,
    /// `env` or `config`: where the supervisor found the endpoint.
    pub route: String,
}

/// One launch of the `footnote` binary, written by the supervisor to the
/// child's stdin. Everything the loop would otherwise ask the supervisor
/// for is resolved here, before the child starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchSpec {
    pub v: u64,
    /// `create` mints nothing: the supervisor minted `fno_id`. `resume`
    /// reopens the session in `session_dir`.
    pub mode: String,
    pub fno_id: String,
    pub session_dir: PathBuf,
    pub cwd: PathBuf,
    pub model: String,
    pub message: String,
    pub timeout_secs: Option<u64>,
    pub node: Option<String>,
    pub plan_path: Option<String>,
    pub cost_cap_usd: Option<f64>,
    pub wall_cap_minutes: Option<u64>,
    pub plugin_root: Option<PathBuf>,
    pub parent_session_id: Option<String>,
    pub price_cache: PathBuf,
    /// The model's context window in tokens; compaction fires at 80%.
    pub context_window: u64,
    pub endpoint: EndpointSpec,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provenance::TranscriptSource;
    use serde_json::json;
    use std::io::Write as _;

    fn line(f: &mut File, seq: u64, ty: &str, data: Value, ignorable: bool) {
        let mut rec = json!({"v": SCHEMA_V, "seq": seq, "id": format!("r{seq}"),
            "ts": "2026-10-09T00:00:00.000Z", "session_id": "s1", "type": ty, "data": data});
        if ignorable {
            rec["ignorable"] = json!(true);
        }
        writeln!(f, "{rec}").unwrap();
    }

    /// The intel source lists a footnote session with its turns and calls; an
    /// unknown non-ignorable record type refuses the read.
    #[test]
    fn readers_see_the_transcript_and_refuse_unknown_types() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let dir = root.join("_none").join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = tmp.path().join("cwd");
        let path = transcript_file(&dir, "s1");
        let mut f = File::create(&path).unwrap();
        line(&mut f, 0, "header", json!({"cwd": cwd}), false);
        line(
            &mut f,
            1,
            "user_input",
            json!({"text": "hi", "origin": "operator"}),
            false,
        );
        line(&mut f, 2, "tool_call", json!({"name": "Read"}), false);
        line(&mut f, 3, "future_thing", json!({}), true);
        let src = crate::footnote_harness::source::FootnoteSource {
            sessions_root: root.clone(),
            roots: None,
        };
        let files = src.sessions(0);
        assert_eq!(files.len(), 1);
        let raw = src.read(&files[0]);
        assert_eq!(src.turns(&raw).len(), 1);
        assert_eq!(src.tool_uses(&raw), 1);
        assert!(read_records(&path).is_ok());
        line(
            &mut f,
            4,
            "model_response",
            json!({"reported_model": "glm-x"}),
            false,
        );
        line(
            &mut f,
            5,
            "usage",
            json!({"input_tokens": 7, "output_tokens": 3}),
            false,
        );
        let payload = json!({"lane": {"name": "f", "harness": "footnote", "model": "glm-x"},
            "workdir": cwd, "started_epoch": 0.0, "footnote_sessions_root": root});
        let seen = crate::eval_attempt::observe(&payload);
        assert_eq!(
            (
                seen["lane_status"].as_str(),
                seen["usage"]["input"].as_u64()
            ),
            (Some("ok"), Some(7))
        );
        line(&mut f, 6, "future_thing", json!({}), false);
        assert!(read_records(&path).unwrap_err().contains("future_thing"));
    }
}
