//! Refusal rate over the trailing N tool calls: the cheapest available proxy
//! for context degradation, no model introspection needed. Feeds the
//! `refusal_rate` reading in `king_checkin.rs`.
//!
//! The bucket regex is the union of the 7 patterns proven against a live
//! reign transcript (gate
//! refusals, usage errors, style-lint refusals, fno guard refusals,
//! timeouts, parse errors, operator rejections). A single combined check is
//! enough here: the reading reports one rate, not a per-bucket breakdown.

use serde_json::{json, Value};
use std::path::Path;
use std::sync::OnceLock;

pub(crate) const REFUSAL_BUCKETS: [(&str, &str); 7] = [
    ("style_lint", r"style-exception|rule \d+ \("),
    ("fno_guard", r"guard\]|king-delegation-guard"),
    (
        "gate_refusal",
        r#""status": "refused"|spawn-gate:|provider_cap|gate_mutex|cpu_share_undecidable|registry_schema|refused:"#,
    ),
    (
        "usage_error",
        r"No such option|Usage:|command not found|Error: No such|validation error|is required",
    ),
    (
        "timeout",
        r"timed out|did not complete within|Command timed out",
    ),
    (
        "parse_error",
        r"Traceback|JSONDecodeError|SyntaxError|KeyError|zsh:",
    ),
    ("user_reject", r"doesn't want to proceed"),
];

/// The first N chars of a tool_result's content text that the regex reads.
/// Matches the 4000-char lead the reign-control retro measured against.
const CONTENT_LEAD_CHARS: usize = 4000;

static REFUSAL_RE: OnceLock<Result<regex::Regex, String>> = OnceLock::new();
static BUCKET_SET: OnceLock<regex::RegexSet> = OnceLock::new();

fn refusal_regex() -> Result<regex::Regex, String> {
    let pattern = REFUSAL_BUCKETS
        .iter()
        .map(|(_, pattern)| *pattern)
        .collect::<Vec<_>>()
        .join("|");
    match REFUSAL_RE
        .get_or_init(|| regex::Regex::new(&pattern).map_err(|e| format!("bad refusal regex: {e}")))
    {
        Ok(regex) => Ok(regex.clone()),
        Err(error) => Err(error.clone()),
    }
}

pub(crate) fn buckets_of(text: &str) -> impl Iterator<Item = &'static str> + '_ {
    let set = BUCKET_SET.get_or_init(|| {
        regex::RegexSet::new(REFUSAL_BUCKETS.map(|(_, pattern)| pattern))
            .expect("refusal bucket patterns are valid")
    });
    let matches = set.matches(lead(text, CONTENT_LEAD_CHARS));
    REFUSAL_BUCKETS
        .iter()
        .enumerate()
        .filter_map(move |(idx, (bucket, _))| matches.matched(idx).then_some(*bucket))
}

/// The first `max_chars` characters of `text`, split on a char boundary.
/// A byte-offset slice (`&text[..N]`) panics when `N` lands inside a
/// multi-byte UTF-8 character - real tool output routinely carries one
/// (an em dash, a checkmark, a box-drawing glyph), so this reader must
/// never assume ASCII.
fn lead(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

fn trailing_calls(text: &str, window: usize) -> Vec<Option<String>> {
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
        let mut entries = Vec::new();
        crate::reign_hygiene::claude_row_entries(&value, &mut entries);
        order.extend(
            entries
                .iter()
                .filter(|entry| entry.kind == "tool_use")
                .filter_map(|entry| entry.tool_use_id.clone()),
        );
        if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
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
        }
    }
    let start = order.len().saturating_sub(window);
    order
        .into_iter()
        .skip(start)
        .map(|id| results.remove(&id))
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
    let trailing = trailing_calls(&text, window);
    let total = trailing.len();
    let re = refusal_regex()?;
    let refused = trailing
        .iter()
        .filter(|c| {
            c.as_ref()
                .is_some_and(|r| re.is_match(lead(r, CONTENT_LEAD_CHARS)))
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

    // A byte-offset slice at exactly CONTENT_LEAD_CHARS bytes would panic
    // here (multi-byte char straddling the cut); `lead` must split on a
    // char boundary instead.
    #[test]
    fn a_multibyte_character_at_the_lead_boundary_does_not_panic() {
        let mut content = "x".repeat(CONTENT_LEAD_CHARS - 1);
        content.push('—'); // em dash: 3 UTF-8 bytes, straddles the cut
        content.push_str("Usage: this text is past the lead and unread");
        let lines = vec![transcript_line("t1", "Bash", Some(&content))];
        let file = write_transcript(&lines);
        let result = refusal_rate(file.path(), 200).unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["refused"], 0, "the matching text sits past the lead");
    }
}
