//! What did the agent say it intends? Promise/watching/aborted intent detected from the hook payload and the transcript.

use super::*;

#[derive(Debug, PartialEq)]
pub(super) enum Intent {
    Promise,
    Aborted {
        reason: String,
    },
    /// Agent-declared async watch: it has armed a harness-tracked
    /// watcher and wants the session to idle until that watcher fires rather
    /// than re-blocking every stop tick. All attributes are advisory (used for
    /// the event and the lease math), never load-bearing: external truth
    /// decides whether idling is actually allowed.
    Watching {
        reason: String,
        pr: Option<String>,
        timeout: Option<String>,
        task_id: Option<String>,
    },
    None,
}

pub(crate) fn extract_assistant_text(val: &Value) -> String {
    // Try /message/content as string
    if let Some(s) = val.pointer("/message/content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    // Try /message/content as array of blocks
    if let Some(arr) = val.pointer("/message/content").and_then(|v| v.as_array()) {
        let mut parts = Vec::new();
        for block in arr {
            // Only include text blocks (not tool_use, tool_result)
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
        }
        return parts.join(" ");
    }
    // Fallback: top-level content
    if let Some(s) = val.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

/// Detect intent with proper attribute extraction. Precedence within one
/// message: aborted > watching > promise. aborted is the hardest stop;
/// watching outranks promise so a session that both promises and asks to idle
/// idles (its promise is re-evaluated on the next wake).
pub(super) fn detect_intent_from_text(text: &str) -> Intent {
    // Look for <aborted ...> tag
    if let Some(aborted_start) = text.find("<aborted") {
        // Find the closing >
        if let Some(gt) = text[aborted_start..].find('>') {
            let tag_text = &text[aborted_start..aborted_start + gt + 1];
            let reason = parse_xml_attr(tag_text, "reason").unwrap_or_default();
            return Intent::Aborted { reason };
        }
    }
    if let Some(w_start) = text.find("<watching") {
        if let Some(gt) = text[w_start..].find('>') {
            let tag_text = &text[w_start..w_start + gt + 1];
            let task_id = parse_xml_attr(tag_text, "task_id");
            if task_id
                .as_deref()
                .is_some_and(|id| !valid_watch_task_id(id))
            {
                return Intent::None;
            }
            return Intent::Watching {
                reason: parse_xml_attr(tag_text, "reason").unwrap_or_default(),
                pr: parse_xml_attr(tag_text, "pr"),
                timeout: parse_xml_attr(tag_text, "timeout"),
                task_id,
            };
        }
    }
    if text.contains("<promise>") {
        return Intent::Promise;
    }
    Intent::None
}

fn valid_watch_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 128
        && task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

pub(crate) fn declared_recovery_hold(text: &str) -> Option<String> {
    match detect_intent_from_text(text) {
        Intent::Promise => Some("mission_complete".into()),
        Intent::Aborted { .. } => Some("mission_aborted".into()),
        Intent::Watching { reason, .. } => Some(format!("{reason}_watch")),
        Intent::None if text.contains("<help") => Some("declared_help".into()),
        Intent::None => None,
    }
}

pub(crate) fn parse_xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"{attr}=""#);
    let start = tag_text.find(&pattern)? + pattern.len();
    let end = tag_text[start..].find('"')?;
    Some(tag_text[start..start + end].to_string())
}

/// Extract `last_assistant_message` from the Stop-hook stdin JSON
///. The harness emits it as a plain string (the stopping
/// turn's final assistant text, blocks joined by newline and trimmed),
/// omitted when empty. Any parse failure -> None so the caller falls back
/// to the transcript scan.
pub(super) fn extract_last_assistant_message(hook_input: &str) -> Option<String> {
    let val: Value = serde_json::from_str(hook_input).ok()?;
    let s = val.get("last_assistant_message")?.as_str()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// A-primary, B-fallback intent read. A present payload is the
/// stopping turn's final text - recomputed per fire, race-free, overwrite-
/// proof - and is authoritative, INCLUDING its "no tag" answer. Falling
/// through to the transcript behind a tag-less payload would resurrect the
/// stale-promise edge the bounded scan exists to contain. Returns the intent
/// plus its source for the loop_check event (`payload` | `transcript`).
pub(super) fn detect_intent(
    last_assistant_message: Option<&str>,
    transcript_path: &Path,
) -> (Intent, &'static str) {
    match last_assistant_message {
        Some(text) => (detect_intent_from_text(text), "payload"),
        None => (detect_intent_full(transcript_path), "transcript"),
    }
}

/// Fallback transcript scan: bounded lookback over the
/// newest INTENT_LOOKBACK_ENTRIES assistant text entries instead of
/// last-line-only. Newest tag wins; a tag-less entry no longer ends the
/// scan, which covers the promise-overwritten-by-block-feedback shape when
/// no payload exists. The bound is load-bearing: a stale promise from
/// pivoted work must fall out of the window (done()'s head_shipped read is
/// the real gate against the remainder).
pub(super) const INTENT_LOOKBACK_ENTRIES: usize = 5;

pub(super) fn detect_intent_full(transcript_path: &Path) -> Intent {
    // The lookback needs at most INTENT_LOOKBACK_ENTRIES assistant entries
    // from the tail, so read tail windows, not the file: a session
    // transcript here reaches 135MB, and a whole-file read plus a Vec of
    // every line cost a hook fire 1.4GB of churn. Grow the window only
    // while the entries it holds have not filled the lookback.
    let mut window_bytes: u64 = 1 << 20;
    let window = loop {
        let candidate = read_tail_lines(transcript_path, window_bytes);
        if (candidate.len() >= 2 && enough_entries(&candidate)) || window_bytes >= (64u64 << 20) {
            break candidate;
        }
        window_bytes *= 4;
    };

    let mut scanned: usize = 0;
    // `watching` is honored ONLY from the single newest assistant entry
    //: a stale watch-request from earlier work must not idle a session
    // that has since moved on. `promise`/`aborted` keep their bounded lookback.
    let mut newest_entry = true;
    for line in window.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = val
            .pointer("/message/role")
            .or_else(|| val.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role != "assistant" {
            continue;
        }
        let text = extract_assistant_text(&val);
        if text.is_empty() {
            continue;
        }
        match detect_intent_from_text(&text) {
            Intent::None => {
                scanned += 1;
                if scanned >= INTENT_LOOKBACK_ENTRIES {
                    return Intent::None;
                }
            }
            // A watching tag below the newest entry is stale: skip it (counts
            // as a scanned entry) and keep scanning for a promise/aborted.
            Intent::Watching { .. } if !newest_entry => {
                scanned += 1;
                if scanned >= INTENT_LOOKBACK_ENTRIES {
                    return Intent::None;
                }
            }
            tagged => return tagged,
        }
        newest_entry = false;
    }
    Intent::None
}

/// Assistant entries carrying text, the same predicate the reverse scan
/// applies, so the window is grown only while the scan could run past it.
fn enough_entries(lines: &[String]) -> bool {
    let mut count = 0usize;
    for line in lines {
        let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let role = val
            .pointer("/message/role")
            .or_else(|| val.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if role == "assistant" && !extract_assistant_text(&val).is_empty() {
            count += 1;
            if count >= INTENT_LOOKBACK_ENTRIES {
                return true;
            }
        }
    }
    false
}

/// The last `cap` bytes of the file as complete lines, oldest first. A window
/// that starts mid-line drops that torn line; a window covering the whole
/// file keeps its first line.
pub(crate) fn read_tail_lines(path: &Path, cap: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.seek(SeekFrom::End(0)) else {
        return Vec::new();
    };
    if len == 0 {
        return Vec::new();
    }
    let start = len.saturating_sub(cap);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // Torn line at the window edge: the byte before it in the file is
        // not a newline, so this partial line is not the file's first.
        if let Some(pos) = text.find('\n') {
            text.drain(..=pos);
        } else {
            return Vec::new();
        }
    }
    text.lines().map(str::to_string).collect()
}
