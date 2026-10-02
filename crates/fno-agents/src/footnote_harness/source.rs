//! Intel's view of footnote sessions: each transcript rendered as
//! claude-shaped rows, so the classifier reads it like every other harness.

use crate::provenance::{
    claude_shaped_tool_uses, claude_shaped_turns, in_roots, within_window, SessionFile,
    TranscriptSource, Turn,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub(crate) struct FootnoteSource {
    pub(crate) sessions_root: PathBuf,
    pub(crate) roots: Option<Vec<PathBuf>>,
}

fn header_cwd(transcript: &Path) -> Option<PathBuf> {
    let first = std::fs::read_to_string(transcript)
        .ok()?
        .lines()
        .next()?
        .to_string();
    let rec: Value = serde_json::from_str(&first).ok()?;
    rec["data"]["cwd"].as_str().map(PathBuf::from)
}

impl TranscriptSource for FootnoteSource {
    fn harness(&self) -> &'static str {
        "footnote"
    }

    fn sessions(&self, days: u64) -> Vec<SessionFile> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut out = Vec::new();
        let projects = std::fs::read_dir(&self.sessions_root)
            .into_iter()
            .flatten()
            .flatten();
        for entry in
            projects.flat_map(|p| std::fs::read_dir(p.path()).into_iter().flatten().flatten())
        {
            // The record is the top-level `<fno_id>.jsonl` file; the
            // sidecar dir carries no extension and child transcripts live
            // inside it, one level down.
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if !within_window(mtime, days, now)
                || !in_roots(header_cwd(&path).as_deref(), self.roots.as_deref())
            {
                continue;
            }
            out.push(SessionFile {
                session_id: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                path,
                mtime,
                size: meta.len(),
            });
        }
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        out
    }

    /// Typed input is a user row; hook, skill and agent input is a meta
    /// row; each tool call is an assistant row with one tool_use block.
    fn read(&self, file: &SessionFile) -> String {
        let raw = std::fs::read_to_string(&file.path).unwrap_or_default();
        let mut out = String::new();
        for rec in raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let row = match rec["type"].as_str() {
                Some("user_input") => {
                    let mut row = json!({"type": "user", "timestamp": rec["ts"],
                        "message": {"role": "user", "content": rec["data"]["text"]}});
                    if rec["data"]["origin"] != "operator" {
                        row["isMeta"] = json!(true);
                    }
                    row
                }
                Some("tool_call") => json!({"type": "assistant", "timestamp": rec["ts"],
                    "message": {"content": [{"type": "tool_use", "name": rec["data"]["name"]}]}}),
                _ => continue,
            };
            out.push_str(&row.to_string());
            out.push('\n');
        }
        out
    }

    fn turns(&self, raw: &str) -> Vec<Turn> {
        claude_shaped_turns(raw)
    }

    fn tool_uses(&self, raw: &str) -> usize {
        claude_shaped_tool_uses(raw)
    }
}
