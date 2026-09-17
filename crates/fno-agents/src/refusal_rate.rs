//! Refusal rate over the trailing N tool calls: the cheapest available proxy
//! for context degradation, no model introspection needed. Feeds the
//! `refusal_rate` reading in `king_checkin.rs`.
//!
//! The bucket regex is the union of the 7 patterns proven against a live
//! reign transcript in `internal/fno/evals/kings/data/scan.py` (gate
//! refusals, usage errors, style-lint refusals, fno guard refusals,
//! timeouts, parse errors, operator rejections). A single combined check is
//! enough here: the reading reports one rate, not a per-bucket breakdown.

use serde_json::{json, Value};
use std::path::Path;

const REFUSAL_PATTERN: &str = concat!(
    r#"style-exception|rule \d+ \(|guard\]|king-delegation-guard|"#,
    r#""status": "refused"|spawn-gate:|provider_cap|gate_mutex|"#,
    r#"cpu_share_undecidable|registry_schema|refused:|No such option|"#,
    r#"Usage:|command not found|Error: No such|validation error|is required|"#,
    r#"timed out|did not complete within|Command timed out|"#,
    r#"Traceback|JSONDecodeError|SyntaxError|KeyError|zsh:|"#,
    r#"doesn't want to proceed"#,
);

/// The first N chars of a tool_result's content text that the regex reads.
/// Matches the 4000-char lead the reign-control retro measured against.
const CONTENT_LEAD_CHARS: usize = 4000;

fn refusal_regex() -> Result<regex::Regex, String> {
    regex::Regex::new(REFUSAL_PATTERN).map_err(|e| format!("bad refusal regex: {e}"))
}

/// One `tool_use` call paired with its `tool_result` text, in transcript
/// order. `result` is `None` when no result was ever recorded (an
/// interrupted call): that call still counts toward `total`, never toward
/// `refused` - an unanswered call is not evidence the machine declined.
struct ToolCall {
    result: Option<String>,
}

/// Parse a Claude JSONL transcript into ordered tool calls, pairing each
/// `tool_use` block with its later `tool_result` by `id`/`tool_use_id`.
fn parse_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut order: Vec<String> = Vec::new();
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = value.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        order.push(id.to_string());
                    }
                }
                Some("tool_result") => {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let text = match block.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(""),
                        _ => String::new(),
                    };
                    results.insert(id.to_string(), text);
                }
                _ => {}
            }
        }
    }
    order
        .into_iter()
        .map(|id| ToolCall {
            result: results.remove(&id),
        })
        .collect()
}

/// Refusal rate over the trailing `window` tool calls in `transcript`.
///
/// Never errors on a transcript that merely has fewer than `window` calls -
/// it reports against however many exist, and `"window"` in the return
/// value is that ACTUAL count, so the printed line never claims a
/// denominator it did not have. Errors only when the transcript itself
/// cannot be read.
pub fn refusal_rate(transcript: &Path, window: usize) -> Result<Value, String> {
    let text =
        std::fs::read_to_string(transcript).map_err(|e| format!("transcript unreadable: {e}"))?;
    let calls = parse_tool_calls(&text);
    let total = calls.len().min(window);
    let trailing = &calls[calls.len() - total..];
    let re = refusal_regex()?;
    let refused = trailing
        .iter()
        .filter(|c| match &c.result {
            Some(r) => re.is_match(&r[..r.len().min(CONTENT_LEAD_CHARS)]),
            None => false,
        })
        .count();
    let rate = if total == 0 {
        0.0
    } else {
        refused as f64 / total as f64
    };
    Ok(json!({
        "rate": rate,
        "refused": refused,
        "total": total,
        "window": total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn transcript_line(tool_use_id: &str, name: &str, result: Option<&str>) -> String {
        let mut lines = Vec::new();
        lines.push(
            json!({
                "message": {
                    "content": [{"type": "tool_use", "id": tool_use_id, "name": name}]
                }
            })
            .to_string(),
        );
        if let Some(result) = result {
            lines.push(
                json!({
                    "message": {
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": result,
                        }]
                    }
                })
                .to_string(),
            );
        }
        lines.join("\n")
    }

    fn write_transcript(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "{}", lines.join("\n")).unwrap();
        file
    }

    #[test]
    fn counts_refusals_by_combined_bucket_regex() {
        let lines = vec![
            transcript_line("t1", "Bash", Some("Usage: fno backlog get <id>")),
            transcript_line("t2", "Bash", Some("all good, done")),
            transcript_line("t3", "Bash", Some("[fno recursive-grep guard] refused")),
        ];
        let file = write_transcript(&lines);
        let result = refusal_rate(file.path(), 200).unwrap();
        assert_eq!(result["total"], 3);
        assert_eq!(result["refused"], 2);
        assert_eq!(result["window"], 3);
        assert!((result["rate"].as_f64().unwrap() - (2.0 / 3.0)).abs() < 1e-9);
    }

    #[test]
    fn window_never_exceeds_actual_call_count() {
        let lines = vec![transcript_line("t1", "Read", Some("ok"))];
        let file = write_transcript(&lines);
        let result = refusal_rate(file.path(), 200).unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["window"], 1);
    }

    #[test]
    fn trailing_window_drops_older_calls() {
        let mut lines = Vec::new();
        for i in 0..5 {
            lines.push(transcript_line(
                &format!("t{i}"),
                "Bash",
                Some("Usage: bad"),
            ));
        }
        lines.push(transcript_line("t5", "Bash", Some("clean result")));
        let file = write_transcript(&lines);
        let result = refusal_rate(file.path(), 1).unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["refused"], 0);
    }

    #[test]
    fn an_interrupted_call_with_no_result_counts_toward_total_never_refused() {
        let lines = vec![transcript_line("t1", "Bash", None)];
        let file = write_transcript(&lines);
        let result = refusal_rate(file.path(), 200).unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["refused"], 0);
    }

    #[test]
    fn unreadable_transcript_is_an_error() {
        let err = refusal_rate(Path::new("/nonexistent/path.jsonl"), 200).unwrap_err();
        assert!(err.contains("unreadable"));
    }
}
