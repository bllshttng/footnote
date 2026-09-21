//! Classify the last tool call in a Claude transcript before a dead unit resumes.
//!
//! The classifier is deliberately observational. It never chooses whether the
//! loop retries a unit; it only describes what the resumed worker can safely
//! know about the interrupted turn.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TRANSCRIPT_MTIME_SKEW: Duration = Duration::from_secs(1);

/// One transcript record relevant to interrupted-call classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    /// An assistant message recorded a tool invocation. `at` is the record's
    /// own `timestamp`, when the line carries one.
    AssistantToolUse {
        id: String,
        name: String,
        at: Option<String>,
    },
    /// A later user/tool message recorded the invocation's result.
    ToolResult { tool_use_id: String },
    /// Any record that does not affect tool-call pairing.
    Other,
}

/// What the transcript proves about the dead turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptedCallOutcome {
    /// A tool invocation was recorded but no result was durably recorded.
    Unknown { name: String },
    /// Every recorded invocation has a matching result.
    NothingInFlight,
    /// The transcript could not be resolved or contained ambiguous evidence.
    Unresolved,
}

/// Parse one Claude JSONL transcript line into the small vocabulary needed by
/// the classifier. Malformed or unrelated records are intentionally `Other`.
pub fn parse_transcript_entry(line: &str) -> TranscriptEntry {
    parse_transcript_entries(line)
        .into_iter()
        .next()
        .unwrap_or(TranscriptEntry::Other)
}

/// Parse every relevant block in one Claude JSONL transcript line. Claude can
/// record more than one tool invocation in a single assistant message; dropping
/// all but the last one would turn ambiguous evidence into a false claim.
pub fn parse_transcript_entries(line: &str) -> Vec<TranscriptEntry> {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return vec![TranscriptEntry::Other],
    };

    let message = match value.get("message") {
        Some(message) => message,
        None => return vec![TranscriptEntry::Other],
    };
    let content = match message.get("content") {
        Some(Value::Array(content)) => content,
        _ => return vec![TranscriptEntry::Other],
    };

    let mut entries = Vec::new();
    let at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    for block in content {
        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        match block_type {
            "tool_use" => {
                if let (Some(id), Some(name)) = (
                    block.get("id").and_then(Value::as_str),
                    block.get("name").and_then(Value::as_str),
                ) {
                    entries.push(TranscriptEntry::AssistantToolUse {
                        id: id.to_string(),
                        name: name.to_string(),
                        at: at.clone(),
                    });
                }
            }
            "tool_result" => {
                if let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) {
                    entries.push(TranscriptEntry::ToolResult {
                        tool_use_id: tool_use_id.to_string(),
                    });
                }
            }
            _ => {}
        }
    }

    if entries.is_empty() {
        vec![TranscriptEntry::Other]
    } else {
        entries
    }
}

/// Read and parse a JSONL transcript.
pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEntry>, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty transcript",
        ));
    }
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<Value>(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed transcript line: {error}"),
            )
        })?;
    }
    Ok(content.lines().flat_map(parse_transcript_entries).collect())
}

/// Classify tool calls by pairing each recorded `tool_use` with a later result.
///
/// More than one unanswered call is unresolved: the resume prompt must never
/// guess which side effect was in flight.
pub fn classify_interrupted(entries: &[TranscriptEntry]) -> InterruptedCallOutcome {
    let open = open_calls(entries);
    match open.as_slice() {
        [] => InterruptedCallOutcome::NothingInFlight,
        [c] => InterruptedCallOutcome::Unknown {
            name: c.name.clone(),
        },
        _ => InterruptedCallOutcome::Unresolved,
    }
}

/// One unanswered tool call, with the time its record carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCall {
    pub id: String,
    pub name: String,
    pub at: Option<String>,
}

/// The calls in `entries` whose recorded `tool_use` has no later result,
/// in transcript order.
fn open_calls(entries: &[TranscriptEntry]) -> Vec<OpenCall> {
    let mut open: Vec<OpenCall> = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::AssistantToolUse { id, name, at } => {
                open.push(OpenCall {
                    id: id.clone(),
                    name: name.clone(),
                    at: at.clone(),
                });
            }
            TranscriptEntry::ToolResult { tool_use_id } => {
                if let Some(index) = open.iter().position(|c| &c.id == tool_use_id) {
                    open.remove(index);
                }
            }
            TranscriptEntry::Other => {}
        }
    }
    open
}

/// The newest tool call in `text` (JSONL) when no later line answers it.
/// An older open call never counts: only the newest call can still be running.
pub fn trailing_open_call(text: &str) -> Option<OpenCall> {
    let entries: Vec<TranscriptEntry> = text.lines().flat_map(parse_transcript_entries).collect();
    let newest = entries.iter().rev().find_map(|e| match e {
        TranscriptEntry::AssistantToolUse { .. } => Some(e),
        _ => None,
    })?;
    let newest_id = match newest {
        TranscriptEntry::AssistantToolUse { id, .. } => id,
        _ => return None,
    };
    open_calls(&entries)
        .into_iter()
        .find(|c| &c.id == newest_id)
}

/// Render the exact resume guidance for a classification.
pub fn resume_paragraph(outcome: &InterruptedCallOutcome) -> Option<String> {
    match outcome {
        InterruptedCallOutcome::Unknown { name } => Some(format!(
            "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. The interrupted tool was `{name}`. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly."
        )),
        InterruptedCallOutcome::NothingInFlight => Some(
            "No tool call was left in flight; resuming this unit is retry-safe.".to_string(),
        ),
        InterruptedCallOutcome::Unresolved => None,
    }
}

/// Result of transcript discovery for a dispatch window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptDiscovery {
    Found(PathBuf),
    Unresolved,
}

/// Locate exactly one transcript for `cwd` whose mtime falls in the dispatch
/// window. Two candidates are unresolved rather than guessed.
pub fn discover_transcript(
    cwd: &Path,
    dispatch_started: SystemTime,
    dispatch_finished: SystemTime,
) -> TranscriptDiscovery {
    let projects_dir = crate::claude_drive::claude_projects_dir();
    discover_transcript_in(cwd, &projects_dir, dispatch_started, dispatch_finished)
}

/// Locate a transcript under an explicit Claude config's `projects/` tree.
/// Account rotation changes this root per dispatch, so callers must pass the
/// effective directory selected for the child process.
pub fn discover_transcript_in(
    cwd: &Path,
    projects_dir: &Path,
    dispatch_started: SystemTime,
    dispatch_finished: SystemTime,
) -> TranscriptDiscovery {
    let slug = cwd.to_string_lossy().replace('/', "-").replace('.', "-");
    let project_dir = projects_dir.join(slug);
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return TranscriptDiscovery::Unresolved;
    };

    let start_ns = dispatch_started.duration_since(UNIX_EPOCH).ok().map(|d| {
        d.as_nanos()
            .saturating_sub(TRANSCRIPT_MTIME_SKEW.as_nanos())
    });
    let end_ns = dispatch_finished.duration_since(UNIX_EPOCH).ok().map(|d| {
        d.as_nanos()
            .saturating_add(TRANSCRIPT_MTIME_SKEW.as_nanos())
    });
    let (Some(start_ns), Some(end_ns)) = (start_ns, end_ns) else {
        return TranscriptDiscovery::Unresolved;
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(modified_ns) = modified.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()) else {
            continue;
        };
        if modified_ns >= start_ns && modified_ns <= end_ns {
            candidates.push((modified_ns, path));
        }
    }

    if candidates.len() != 1 {
        return TranscriptDiscovery::Unresolved;
    }
    candidates.sort_by_key(|(modified_ns, _)| *modified_ns);
    TranscriptDiscovery::Found(candidates.remove(0).1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str, name: &str) -> TranscriptEntry {
        TranscriptEntry::AssistantToolUse {
            id: id.to_string(),
            name: name.to_string(),
            at: None,
        }
    }

    fn result(id: &str) -> TranscriptEntry {
        TranscriptEntry::ToolResult {
            tool_use_id: id.to_string(),
        }
    }

    #[test]
    fn unanswered_tool_is_unknown_and_names_tool() {
        let outcome = classify_interrupted(&[tool_use("tool-1", "Bash")]);
        assert_eq!(
            outcome,
            InterruptedCallOutcome::Unknown {
                name: "Bash".to_string()
            }
        );
        let paragraph = resume_paragraph(&outcome).expect("unknown has guidance");
        assert!(paragraph.contains("`Bash`"));
        assert!(paragraph.contains("first verify external state or ask the user"));
        assert!(paragraph.contains("Do not retry blindly."));
    }

    #[test]
    fn answered_tools_are_retry_safe() {
        let outcome = classify_interrupted(&[tool_use("tool-1", "Bash"), result("tool-1")]);
        assert_eq!(outcome, InterruptedCallOutcome::NothingInFlight);
        assert_eq!(
            resume_paragraph(&outcome).as_deref(),
            Some("No tool call was left in flight; resuming this unit is retry-safe.")
        );
    }

    #[test]
    fn multiple_unanswered_tools_are_unresolved_without_guidance() {
        let outcome =
            classify_interrupted(&[tool_use("tool-1", "Bash"), tool_use("tool-2", "Edit")]);
        assert_eq!(outcome, InterruptedCallOutcome::Unresolved);
        assert_eq!(resume_paragraph(&outcome), None);
    }

    #[test]
    fn parses_claude_tool_blocks() {
        let entry = parse_transcript_entry(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{}}]}}"#,
        );
        assert_eq!(entry, tool_use("tool-1", "Bash"));
        let entry = parse_transcript_entry(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"ok"}]}}"#,
        );
        assert_eq!(entry, result("tool-1"));
    }

    #[test]
    fn preserves_multiple_tool_blocks_in_one_message() {
        let entries = parse_transcript_entries(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool-1","name":"Bash","input":{}},{"type":"tool_use","id":"tool-2","name":"Edit","input":{}}]}}"#,
        );
        assert_eq!(
            entries,
            vec![tool_use("tool-1", "Bash"), tool_use("tool-2", "Edit")]
        );
        assert_eq!(
            classify_interrupted(&entries),
            InterruptedCallOutcome::Unresolved
        );
    }

    #[test]
    fn malformed_transcript_is_not_treated_as_retry_safe() {
        let path = std::env::temp_dir().join(format!(
            "fno-interrupt-malformed-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "not-json\n").unwrap();
        assert!(read_transcript(&path).is_err());
        std::fs::remove_file(path).ok();
    }

    // AC1-HP: the trailing reader answers with the newest call's name, id and time.
    #[test]
    fn trailing_open_call_reads_the_newest_call_with_its_time() {
        let tail = concat!(
            r#"{"type":"assistant","timestamp":"2026-09-21T08:21:13.913Z","message":{"content":[{"type":"tool_use","id":"toolu_01DRy8JKGCBLeubeGxap9SwP","name":"Bash","input":{}}]},"uuid":"u1","parentUuid":null}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-09-21T08:21:40.000Z","message":{"content":[{"type":"queue-operation","content":"x"}]}}"#,
            "\n",
        );
        let call = trailing_open_call(tail).expect("the newest call is open");
        assert_eq!(call.id, "toolu_01DRy8JKGCBLeubeGxap9SwP");
        assert_eq!(call.name, "Bash");
        assert_eq!(call.at.as_deref(), Some("2026-09-21T08:21:13.913Z"));
    }

    // AC1-EDGE: an older open call never counts once a newer call is answered.
    #[test]
    fn trailing_open_call_ignores_older_open_calls() {
        let tail = concat!(
            r#"{"type":"assistant","timestamp":"2026-09-21T08:00:00.000Z","message":{"content":[{"type":"tool_use","id":"old","name":"Bash","input":{}}]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-09-21T08:05:00.000Z","message":{"content":[{"type":"tool_use","id":"new","name":"Read","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"new","content":"ok"}]}}"#,
            "\n",
        );
        assert!(trailing_open_call(tail).is_none());
    }

    // A partial (unterminated) tail line must not break the newest-call read;
    // the walk skips it like any Other record.
    #[test]
    fn trailing_open_call_skips_a_partial_last_line() {
        let tail = concat!(
            r#"{"type":"assistant","timestamp":"2026-09-21T08:21:13.913Z","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_re"#,
        );
        assert!(trailing_open_call(tail).is_none());
    }
}
