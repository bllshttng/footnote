//! Classify the last tool call in a Claude transcript before a dead unit resumes.
//!
//! The classifier is deliberately observational. It never chooses whether the
//! loop retries a unit; it only describes what the resumed worker can safely
//! know about the interrupted turn.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One transcript record relevant to interrupted-call classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    /// An assistant message recorded a tool invocation.
    AssistantToolUse { id: String, name: String },
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
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return TranscriptEntry::Other,
    };

    let message = match value.get("message") {
        Some(message) => message,
        None => return TranscriptEntry::Other,
    };
    let content = match message.get("content") {
        Some(Value::Array(content)) => content,
        _ => return TranscriptEntry::Other,
    };

    let mut tool_use = None;
    let mut tool_result = None;
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
                    tool_use = Some((id.to_string(), name.to_string()));
                }
            }
            "tool_result" => {
                if let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) {
                    tool_result = Some(tool_use_id.to_string());
                }
            }
            _ => {}
        }
    }

    match (tool_use, tool_result) {
        (Some((id, name)), None) => TranscriptEntry::AssistantToolUse { id, name },
        (None, Some(tool_use_id)) => TranscriptEntry::ToolResult { tool_use_id },
        _ => TranscriptEntry::Other,
    }
}

/// Read and parse a JSONL transcript.
pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEntry>, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    Ok(content.lines().map(parse_transcript_entry).collect())
}

/// Classify tool calls by pairing each recorded `tool_use` with a later result.
///
/// More than one unanswered call is unresolved: the resume prompt must never
/// guess which side effect was in flight.
pub fn classify_interrupted(entries: &[TranscriptEntry]) -> InterruptedCallOutcome {
    let mut open: Vec<(String, String)> = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::AssistantToolUse { id, name } => {
                open.push((id.clone(), name.clone()));
            }
            TranscriptEntry::ToolResult { tool_use_id } => {
                if let Some(index) = open.iter().position(|(id, _)| id == tool_use_id) {
                    open.remove(index);
                }
            }
            TranscriptEntry::Other => {}
        }
    }

    match open.as_slice() {
        [] => InterruptedCallOutcome::NothingInFlight,
        [(.., name)] => InterruptedCallOutcome::Unknown { name: name.clone() },
        _ => InterruptedCallOutcome::Unresolved,
    }
}

/// Render the exact resume guidance for a classification.
pub fn resume_paragraph(outcome: &InterruptedCallOutcome) -> Option<String> {
    match outcome {
        InterruptedCallOutcome::Unknown { name } => Some(format!(
            "The tool call `{name}` was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly."
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
    let slug = cwd.to_string_lossy().replace('/', "-").replace('.', "-");
    let project_dir = crate::claude_drive::claude_projects_dir().join(slug);
    let Ok(entries) = std::fs::read_dir(project_dir) else {
        return TranscriptDiscovery::Unresolved;
    };

    let start_ns = dispatch_started
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos());
    let end_ns = dispatch_finished
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos());
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
        let outcome = classify_interrupted(&[tool_use("tool-1", "Bash"), tool_use("tool-2", "Edit")]);
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
}
