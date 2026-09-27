//! Per-session activity counters read straight off the transcript raw text:
//! tokens, edited lines, tool errors by class, aborted turns, touched file
//! extensions, and the assistant-timestamp stream. The fold stays the only
//! source of numbers; these parsers never read a transcript twice.

use crate::provenance::turn_ts_epoch;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Tokens {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Activity {
    pub(crate) tokens: Tokens,
    pub(crate) lines_added: u64,
    pub(crate) lines_removed: u64,
    pub(crate) tool_errors: BTreeMap<&'static str, u64>,
    pub(crate) aborted_turns: u64,
    pub(crate) extensions: BTreeMap<String, u64>,
    pub(crate) assistant_ts: Vec<f64>,
}

/// The error class of an `is_error` tool result, from its leading text.
/// Classes come from the 2026-09-21 sample of 3,313 real errors; `other` is
/// reported, so a new dominant shape surfaces as a large `other` bucket.
pub(crate) fn error_class(text: &str) -> &'static str {
    let t = text.trim_start();
    if t.starts_with("Exit code") {
        "command_failed"
    } else if t.contains("doesn't want to proceed") {
        "user_rejected"
    } else if t.contains("hook error")
        || t.starts_with("[fno ")
        || t.starts_with("king-delegation-guard:")
    {
        "hook_blocked"
    } else if t.starts_with("<tool_use_error>") {
        "tool_use_error"
    } else {
        "other"
    }
}

/// Lines added and removed between two edit strings: the shared leading and
/// trailing lines are trimmed, and what remains is the change. Exact for one
/// contiguous hunk, which is what an Edit call sends.
fn changed_lines(old: &str, new: &str) -> (u64, u64) {
    let mut o: Vec<&str> = old.lines().collect();
    let mut n: Vec<&str> = new.lines().collect();
    while !o.is_empty() && !n.is_empty() && o.last() == n.last() {
        o.pop();
        n.pop();
    }
    let mut shared = 0usize;
    while shared < o.len() && shared < n.len() && o[shared] == n[shared] {
        shared += 1;
    }
    ((n.len() - shared) as u64, (o.len() - shared) as u64)
}

fn extension_of(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn note_extension(extensions: &mut BTreeMap<String, u64>, path: &str) {
    if let Some(ext) = extension_of(path) {
        *extensions.entry(ext).or_insert(0) += 1;
    }
}

fn block_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn content_blocks(row: &Value) -> &[Value] {
    row.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// Activity counters for one claude transcript (`*.jsonl` row shape).
pub(crate) fn claude_activity(raw: &str) -> Activity {
    let mut act = Activity::default();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_assistant = row.get("type").and_then(|v| v.as_str()) == Some("assistant")
            || row
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(|v| v.as_str())
                == Some("assistant");
        if is_assistant {
            if let Some(ts) = turn_ts_epoch(&row) {
                act.assistant_ts.push(ts);
            }
            // Transcripts repeat one API message across several rows; a
            // token sum that skips the dedupe overcounts about 2x (measured
            // 2026-09-21). A row with no id counts once by itself.
            let id = row
                .get("message")
                .and_then(|m| m.get("id"))
                .and_then(|v| v.as_str());
            let fresh = match id {
                Some(id) => seen_ids.insert(id.to_string()),
                None => true,
            };
            if fresh {
                if let Some(usage) = row
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .filter(|u| u.is_object())
                {
                    let f = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    act.tokens.input += f("input_tokens");
                    act.tokens.output += f("output_tokens");
                    act.tokens.cache_read += f("cache_read_input_tokens");
                    act.tokens.cache_write += f("cache_creation_input_tokens");
                }
            }
        }
        for block in content_blocks(&row) {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let field = |k: &str| {
                        input
                            .get(k)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    match name {
                        "Edit" => {
                            let (added, removed) =
                                changed_lines(&field("old_string"), &field("new_string"));
                            act.lines_added += added;
                            act.lines_removed += removed;
                            note_extension(&mut act.extensions, &field("file_path"));
                        }
                        "MultiEdit" => {
                            if let Some(edits) = input.get("edits").and_then(|e| e.as_array()) {
                                for edit in edits {
                                    let s = |k: &str| {
                                        edit.get(k)
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string()
                                    };
                                    let (added, removed) =
                                        changed_lines(&s("old_string"), &s("new_string"));
                                    act.lines_added += added;
                                    act.lines_removed += removed;
                                }
                            }
                            note_extension(&mut act.extensions, &field("file_path"));
                        }
                        "Write" => {
                            act.lines_added += field("content").lines().count() as u64;
                            note_extension(&mut act.extensions, &field("file_path"));
                        }
                        "NotebookEdit" => {
                            let path = if field("file_path").is_empty() {
                                field("notebook_path")
                            } else {
                                field("file_path")
                            };
                            note_extension(&mut act.extensions, &path);
                        }
                        _ => {}
                    }
                }
                Some("tool_result") => {
                    if block.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                        let text =
                            block_text(&block.get("content").cloned().unwrap_or(Value::Null));
                        *act.tool_errors.entry(error_class(&text)).or_insert(0) += 1;
                    }
                }
                _ => {}
            }
        }
    }
    act.assistant_ts
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    act
}

/// Lines added and removed plus one extension count per file, out of a codex
/// `*** Begin Patch` block. `***` header lines and `@@` hunks count nothing.
fn patch_activity(input: &str, act: &mut Activity) {
    let mut in_patch = false;
    for line in input.lines() {
        if line.trim_start().starts_with("*** Begin Patch") {
            in_patch = true;
            continue;
        }
        if !in_patch {
            continue;
        }
        if line.trim_start().starts_with("*** End Patch") {
            break;
        }
        let trimmed = line.trim_start();
        if let Some(path) = trimmed
            .strip_prefix("*** Add File:")
            .or_else(|| trimmed.strip_prefix("*** Update File:"))
        {
            note_extension(&mut act.extensions, path.trim());
            continue;
        }
        if trimmed.starts_with("***") || trimmed.starts_with("@@") {
            continue;
        }
        if line.starts_with('+') {
            act.lines_added += 1;
        } else if line.starts_with('-') {
            act.lines_removed += 1;
        }
    }
}

fn codex_call_input(payload: &Value) -> String {
    payload
        .get("input")
        .or_else(|| payload.get("arguments"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Activity counters for one codex rollout (`payload` row shape).
pub(crate) fn codex_activity(raw: &str) -> Activity {
    let mut act = Activity::default();
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let row_type = row.get("type").and_then(|v| v.as_str());
        let Some(payload) = row.get("payload").filter(|p| p.is_object()) else {
            continue;
        };
        let ptype = payload.get("type").and_then(|v| v.as_str());
        match (row_type, ptype) {
            // token_count totals are cumulative, so the last readable event
            // is the session total.
            (Some("event_msg"), Some("token_count")) => {
                if let Some(usage) = payload
                    .get("info")
                    .filter(|i| !i.is_null())
                    .and_then(|i| i.get("total_token_usage"))
                    .filter(|u| u.is_object())
                {
                    let f = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    act.tokens = Tokens {
                        input: f("input_tokens"),
                        output: f("output_tokens"),
                        cache_read: f("cached_input_tokens"),
                        cache_write: f("cache_write_input_tokens"),
                    };
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => act.aborted_turns += 1,
            (Some("response_item"), Some("message"))
                if payload.get("role").and_then(|v| v.as_str()) == Some("assistant") =>
            {
                if let Some(ts) = turn_ts_epoch(&row) {
                    act.assistant_ts.push(ts);
                }
            }
            (Some("response_item"), Some("function_call"))
            | (Some("response_item"), Some("custom_tool_call")) => {
                let input = codex_call_input(payload);
                if input.contains("*** Begin Patch") {
                    patch_activity(&input, &mut act);
                }
            }
            (Some("response_item"), Some("custom_tool_call_output")) => {
                let text = payload.get("output").and_then(|v| v.as_str()).unwrap_or("");
                if text.trim_start().starts_with("Script failed") {
                    *act.tool_errors.entry("command_failed").or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }
    act.assistant_ts
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    act
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude_lines(rows: &[Value]) -> String {
        rows.iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn one_message_id_over_three_rows_counts_once() {
        // AC1-HP: 3 rows share one message.id, each carrying 10 in / 5 out.
        let msg = json!({"role": "assistant", "id": "msg_1",
                         "usage": {"input_tokens": 10, "output_tokens": 5}});
        let raw = claude_lines(&[
            json!({"type": "assistant", "timestamp": "2026-09-16T12:00:00.000Z", "message": msg}),
            json!({"type": "assistant", "timestamp": "2026-09-16T12:00:01.000Z", "message": msg}),
            json!({"type": "assistant", "timestamp": "2026-09-16T12:00:02.000Z", "message": msg}),
        ]);
        let act = claude_activity(&raw);
        assert_eq!(act.tokens.input, 10);
        assert_eq!(act.tokens.output, 5);
    }

    #[test]
    fn rows_without_an_id_each_count_once() {
        let msg = json!({"role": "assistant",
                         "usage": {"input_tokens": 7, "output_tokens": 3}});
        let raw = claude_lines(&[
            json!({"type": "assistant", "message": msg}),
            json!({"type": "assistant", "message": msg}),
        ]);
        let act = claude_activity(&raw);
        assert_eq!(act.tokens.input, 14);
        assert_eq!(act.tokens.output, 6);
    }

    #[test]
    fn edit_multiedit_write_lines_and_extensions() {
        // AC2-EDGE: shared first+last lines, one differing middle line.
        let edit = |path: &str| {
            json!({"type": "tool_use", "name": "Edit",
                   "input": {"file_path": path,
                             "old_string": "fn a() {\n    old\n}", "new_string": "fn a() {\n    new\n}"}})
        };
        let multiedit = json!({"type": "tool_use", "name": "MultiEdit",
            "input": {"file_path": "y.rs", "edits": [
                {"old_string": "a\nb\nc", "new_string": "a\nd\nc"},
                {"old_string": "x\ny", "new_string": "x\nz"}]}});
        let write = json!({"type": "tool_use", "name": "Write",
                           "input": {"file_path": "a/b.RS", "content": "one\ntwo\nthree"}});
        let raw = claude_lines(
            &[json!({"type": "assistant", "message": {"role": "assistant",
                    "content": [edit("x.rs"), multiedit, write]}})],
        );
        let act = claude_activity(&raw);
        assert_eq!(act.lines_added, 6);
        assert_eq!(act.lines_removed, 3);
        assert_eq!(act.extensions.get("rs"), Some(&3));
    }

    #[test]
    fn identical_edit_strings_count_nothing() {
        let raw = claude_lines(
            &[json!({"type": "assistant", "message": {"role": "assistant",
            "content": [json!({"type": "tool_use", "name": "Edit",
                "input": {"file_path": "a.rs", "old_string": "same", "new_string": "same"}})]}})],
        );
        let act = claude_activity(&raw);
        assert_eq!(act.lines_added, 0);
        assert_eq!(act.lines_removed, 0);
        assert_eq!(act.extensions.get("rs"), Some(&1));
    }

    #[test]
    fn a_path_with_no_extension_counts_nothing() {
        let raw = claude_lines(
            &[json!({"type": "assistant", "message": {"role": "assistant",
            "content": [json!({"type": "tool_use", "name": "Write",
                "input": {"file_path": "Makefile", "content": "all:\n\ttrue"}})]}})],
        );
        let act = claude_activity(&raw);
        assert!(act.extensions.is_empty());
        assert_eq!(act.lines_added, 2);
    }

    #[test]
    fn the_five_error_classes_sort_one_each() {
        // AC3-HP: one text per measured class.
        let texts = [
            ("Exit code 1", "command_failed"),
            ("The user doesn't want to proceed", "user_rejected"),
            ("PreToolUse:Bash hook error: refused", "hook_blocked"),
            ("[fno backlog] node locked", "hook_blocked"),
            (
                "king-delegation-guard: kings do not implement",
                "hook_blocked",
            ),
            (
                "<tool_use_error>File has not been read yet",
                "tool_use_error",
            ),
            ("boom", "other"),
        ];
        for (text, want) in texts {
            assert_eq!(error_class(text), want, "text: {text}");
        }
    }

    #[test]
    fn is_error_tool_results_are_counted_by_class() {
        let result = |text: &str| json!({"type": "tool_result", "is_error": true, "content": text});
        let raw = claude_lines(&[json!({"type": "user", "message": {"role": "user",
                    "content": [result("Exit code 101"), result("boom")]}})]);
        let act = claude_activity(&raw);
        assert_eq!(act.tool_errors.get("command_failed"), Some(&1));
        assert_eq!(act.tool_errors.get("other"), Some(&1));
    }

    #[test]
    fn assistant_rows_leave_timestamps_sorted() {
        let raw = claude_lines(&[
            json!({"type": "assistant", "timestamp": "2026-09-16T12:00:02.000Z",
                   "message": {"role": "assistant", "content": []}}),
            json!({"type": "assistant", "timestamp": "2026-09-16T12:00:00.000Z",
                   "message": {"role": "assistant", "content": []}}),
        ]);
        let act = claude_activity(&raw);
        assert_eq!(act.assistant_ts.len(), 2);
        assert!(act.assistant_ts[0] <= act.assistant_ts[1]);
    }

    #[test]
    fn the_codex_parsers_read_tokens_patch_and_errors() {
        // AC4-HP: two cumulative token events, a patch, an exec failure,
        // one aborted turn.
        let patch = "shell: cat <<'EOF'\n*** Begin Patch\n*** Update File: x.py\n@@\n+a\n+b\n-c\n*** End Patch\nEOF";
        let raw = claude_lines(&[
            json!({"type": "event_msg", "timestamp": "2026-09-16T13:00:00.000Z",
                   "payload": {"type": "token_count",
                               "info": {"total_token_usage": {"input_tokens": 100,
                                        "output_tokens": 50, "cached_input_tokens": 10}}}}),
            json!({"type": "event_msg", "timestamp": "2026-09-16T13:01:00.000Z",
                   "payload": {"type": "token_count",
                               "info": {"total_token_usage": {"input_tokens": 200,
                                        "output_tokens": 90, "cached_input_tokens": 20,
                                        "cache_write_input_tokens": 5}}}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call",
                    "input": patch}}),
            json!({"type": "response_item", "payload": {"type": "function_call",
                    "arguments": "{\"command\":\"ls\"}"}}),
            json!({"type": "response_item", "payload": {"type": "custom_tool_call_output",
                    "output": "Script failed: exit 1"}}),
            json!({"type": "event_msg", "payload": {"type": "turn_aborted"}}),
            json!({"type": "response_item", "timestamp": "2026-09-16T13:02:00.000Z",
                   "payload": {"type": "message", "role": "assistant", "content": [{"type": "text", "text": "done"}]}}),
        ]);
        let act = codex_activity(&raw);
        assert_eq!(
            act.tokens,
            Tokens {
                input: 200,
                output: 90,
                cache_read: 20,
                cache_write: 5
            }
        );
        assert_eq!(act.lines_added, 2);
        assert_eq!(act.lines_removed, 1);
        assert_eq!(act.extensions.get("py"), Some(&1));
        assert_eq!(act.tool_errors.get("command_failed"), Some(&1));
        assert_eq!(act.aborted_turns, 1);
        assert_eq!(act.assistant_ts.len(), 1);
    }

    #[test]
    fn a_token_count_with_null_info_never_wins() {
        let raw = claude_lines(&[
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}}),
            json!({"type": "event_msg", "payload": {"type": "token_count",
                    "info": {"total_token_usage": {"input_tokens": 9, "output_tokens": 4}}}}),
        ]);
        let act = codex_activity(&raw);
        assert_eq!(act.tokens.output, 4);
    }

    #[test]
    fn codex_patch_headers_and_hunks_count_nothing() {
        let patch = "*** Begin Patch\n*** Add File: new.py\n+line one\n@@ context\n*** End Patch";
        let raw = json!({"type": "response_item", "payload": {"type": "custom_tool_call",
                         "input": patch}})
        .to_string();
        let act = codex_activity(&raw);
        assert_eq!(act.lines_added, 1);
        assert_eq!(act.lines_removed, 0);
        assert_eq!(act.extensions.get("py"), Some(&1));
    }

    #[test]
    fn changed_lines_handles_write_and_noop_shapes() {
        assert_eq!(changed_lines("", "a\nb"), (2, 0));
        assert_eq!(changed_lines("a\nb", ""), (0, 2));
        assert_eq!(changed_lines("same", "same"), (0, 0));
    }
}
