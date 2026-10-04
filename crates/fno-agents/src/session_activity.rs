//! Per-session activity counters read straight off the transcript raw text:
//! tokens, edited lines, tool errors by class, aborted turns, touched file
//! extensions, and the assistant-timestamp stream. The fold stays the only
//! source of numbers; these parsers never read a transcript twice.

use crate::provenance::turn_ts_epoch;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Tokens {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
}

impl Tokens {
    /// The window delta between a cumulative total and the base read at the
    /// window's first row; saturating so a total reset reads as zero.
    pub(crate) fn since(self, base: Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(base.input),
            output: self.output.saturating_sub(base.output),
            cache_read: self.cache_read.saturating_sub(base.cache_read),
            cache_write: self.cache_write.saturating_sub(base.cache_write),
        }
    }
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
        || t.starts_with("lead-delegation-guard:")
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

/// A countable language key: a non-empty ASCII-alphanumeric extension with
/// at least one letter. `Path::extension` alone admits code fragments and
/// version digits from tool arguments that are not paths (`md\``, `html~`,
/// `0`, whole function bodies).
fn extension_of(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    let plausible = !ext.is_empty()
        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        && ext.bytes().any(|b| b.is_ascii_alphabetic());
    plausible.then_some(ext)
}

/// A `file_path` a model sent is not always a path: fragments of code and
/// prose carry dots too, and everything after the last dot became a
/// languages key (`fn facet_keys(view: &mut view, bytes: &[u8]) {` was one
/// such key in the 2026-09-28 fold). A key must look like an extension:
/// 1 to 12 characters of `[a-z0-9]`.
/// A `file_path` a model sent is not always a path: fragments of code and
/// prose carry dots too, and everything after the last dot became a
/// languages key (`fn facet_keys(view: &mut view, bytes: &[u8]) {` was one
/// such key in the 2026-09-28 fold). A key must look like an extension:
/// 1 to 12 characters of `[a-z0-9]`.
fn note_extension(extensions: &mut BTreeMap<String, u64>, path: &str) {
    if let Some(ext) = extension_of(path) {
        let shaped = !ext.is_empty()
            && ext.len() <= 12
            && ext
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if shaped {
            *extensions.entry(ext).or_insert(0) += 1;
        }
    }
}

/// The assistant API message id of one transcript row, when the row repeats
/// an assistant message: the token-dedupe key shared by the fold and the
/// rollup's cross-run window.
pub(crate) fn assistant_message_id(row: &Value) -> Option<&str> {
    let is_assistant = row.get("type").and_then(|v| v.as_str()) == Some("assistant")
        || row
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|v| v.as_str())
            == Some("assistant");
    if !is_assistant {
        return None;
    }
    row.get("message")
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
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
    let mut fold = ActivityFold::default();
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        fold.row(&row);
    }
    fold.finish()
}

#[derive(Default)]
pub(crate) struct ActivityFold {
    act: Activity,
    seen_ids: HashSet<String>,
    /// Whether this pass saw a codex token_count row: presence, not
    /// magnitude, decides whether a cumulative total replaces the stored
    /// one (a real row can legitimately carry zero).
    tokens_seen: bool,
}

impl ActivityFold {
    /// Pre-seed the dedupe set with ids seen on an earlier pass over the
    /// same transcript, so a message repeated across an offset boundary
    /// still counts once.
    pub(crate) fn seed_seen(&mut self, ids: impl IntoIterator<Item = String>) {
        self.seen_ids.extend(ids);
    }

    /// The most recent assistant timestamp this fold has seen, in row
    /// order: the running anchor a rollup's response gaps hang off.
    pub(crate) fn last_assistant_ts(&self) -> Option<f64> {
        self.act.assistant_ts.last().copied()
    }

    /// Whether a codex token_count row arrived in this pass.
    pub(crate) fn saw_tokens(&self) -> bool {
        self.tokens_seen
    }
}

impl ActivityFold {
    pub(crate) fn row(&mut self, row: &Value) {
        if row.get("payload").is_some_and(|p| p.is_object()) {
            self.codex_row(row);
            return;
        }
        let is_assistant = row.get("type").and_then(|v| v.as_str()) == Some("assistant")
            || row
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(|v| v.as_str())
                == Some("assistant");
        if is_assistant {
            if let Some(ts) = turn_ts_epoch(&row) {
                self.act.assistant_ts.push(ts);
            }
            // Transcripts repeat one API message across several rows; a
            // token sum that skips the dedupe overcounts about 2x (measured
            // 2026-09-21). A row with no id counts once by itself.
            let id = assistant_message_id(row);
            let fresh = match id {
                Some(id) => self.seen_ids.insert(id.to_string()),
                None => true,
            };
            if fresh {
                if let Some(usage) = row
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .filter(|u| u.is_object())
                {
                    let f = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    self.act.tokens.input += f("input_tokens");
                    self.act.tokens.output += f("output_tokens");
                    self.act.tokens.cache_read += f("cache_read_input_tokens");
                    self.act.tokens.cache_write += f("cache_creation_input_tokens");
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
                            self.act.lines_added += added;
                            self.act.lines_removed += removed;
                            note_extension(&mut self.act.extensions, &field("file_path"));
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
                                    self.act.lines_added += added;
                                    self.act.lines_removed += removed;
                                }
                            }
                            note_extension(&mut self.act.extensions, &field("file_path"));
                        }
                        "Write" => {
                            self.act.lines_added += field("content").lines().count() as u64;
                            note_extension(&mut self.act.extensions, &field("file_path"));
                        }
                        "NotebookEdit" => {
                            let path = if field("file_path").is_empty() {
                                field("notebook_path")
                            } else {
                                field("file_path")
                            };
                            note_extension(&mut self.act.extensions, &path);
                        }
                        _ => {}
                    }
                }
                Some("tool_result") => {
                    if block.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                        let text =
                            block_text(&block.get("content").cloned().unwrap_or(Value::Null));
                        *self.act.tool_errors.entry(error_class(&text)).or_insert(0) += 1;
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn finish(mut self) -> Activity {
        self.act
            .assistant_ts
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        self.act
    }

    /// The codex arm: a rollout row (`{type, payload}`). The body is
    /// `codex_activity`'s loop moved beside the claude body, with the output
    /// text and error shape it was missing.
    fn codex_row(&mut self, row: &Value) {
        let act = &mut self.act;
        let row_type = row.get("type").and_then(|v| v.as_str());
        let Some(payload) = row.get("payload").filter(|p| p.is_object()) else {
            return;
        };
        let ptype = payload.get("type").and_then(|v| v.as_str());
        match (row_type, ptype) {
            (Some("event_msg"), Some("token_count")) => {
                if let Some(total) = codex_token_total(row) {
                    act.tokens = total;
                    self.tokens_seen = true;
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => act.aborted_turns += 1,
            (Some("response_item"), Some("message"))
                if payload.get("role").and_then(|v| v.as_str()) == Some("assistant") =>
            {
                if let Some(ts) = turn_ts_epoch(row) {
                    act.assistant_ts.push(ts);
                }
            }
            (Some("response_item"), Some("function_call"))
            | (Some("response_item"), Some("custom_tool_call")) => {
                let input = codex_call_input(payload);
                if input.contains("*** Begin Patch") {
                    patch_activity(&input, act);
                }
            }
            (Some("response_item"), Some("custom_tool_call_output"))
            | (Some("response_item"), Some("function_call_output")) => {
                if codex_output_is_error(&codex_output_text(payload)) {
                    *act.tool_errors.entry("command_failed").or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }
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

/// The tool output text of a codex output row: the string form, or the list's
/// `text` parts joined with "" (1,465 of 1,471 Kestrel II outputs are the
/// list shape; measured 2026-09-30).
pub(crate) fn codex_output_text(payload: &Value) -> Cow<'_, str> {
    match payload.get("output") {
        Some(Value::String(s)) => Cow::Borrowed(s),
        Some(Value::Array(parts)) => Cow::Owned(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => Cow::Borrowed(""),
    }
}

/// One failed command: the leading text codex writes for a failed or
/// terminated script, or an inner result with a non-zero exit code. The
/// escaped-quote form matches output text embedded in a JSON string.
fn codex_output_is_error(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with("Script failed") || t.starts_with("Script terminated") {
        return true;
    }
    static EXIT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = EXIT.get_or_init(|| {
        regex::Regex::new(r#"\\?"exit_code\\?":\s*(-?\d+)"#).expect("valid pattern")
    });
    re.captures_iter(text).any(|c| c[1].parse::<i64>() != Ok(0))
}

/// The cumulative token total of a codex `token_count` row, `None` for any
/// other row and when `info` is null (a null never wins).
pub(crate) fn codex_token_total(row: &Value) -> Option<Tokens> {
    if row.get("type").and_then(|v| v.as_str()) != Some("event_msg") {
        return None;
    }
    let payload = row.get("payload")?;
    if payload.get("type").and_then(|v| v.as_str()) != Some("token_count") {
        return None;
    }
    let usage = payload
        .get("info")
        .filter(|i| !i.is_null())
        .and_then(|i| i.get("total_token_usage"))
        .filter(|u| u.is_object())?;
    let f = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    Some(Tokens {
        input: f("input_tokens"),
        output: f("output_tokens"),
        cache_read: f("cached_input_tokens"),
        cache_write: f("cache_write_input_tokens"),
    })
}

/// Activity counters for one codex rollout (`payload` row shape).
pub(crate) fn codex_activity(raw: &str) -> Activity {
    let mut fold = ActivityFold::default();
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        fold.row(&row);
    }
    fold.finish()
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
        let fragment = json!({"type": "tool_use", "name": "Write",
                              "input": {"file_path": "y.rs (x)", "content": "body"}});
        let raw = claude_lines(
            &[json!({"type": "assistant", "message": {"role": "assistant",
                    "content": [edit("x.rs"), multiedit, write, fragment]}})],
        );
        let act = claude_activity(&raw);
        // The fragment's body line still counts as a line; it never
        // counts as a language.
        assert_eq!(act.lines_added, 7);
        assert_eq!(act.lines_removed, 3);
        assert_eq!(act.extensions.get("rs"), Some(&3));
        assert!(act.extensions.get("rs (x)").is_none());
        assert_eq!(act.extensions.len(), 1);
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
        // No extension, and every non-extension tail: code fragments, backup
        // and version tails, and empty tails read no language key.
        for path in [
            "Makefile",
            "backup.html~",
            "release.2",
            "trailing.md`",
            "file.",
            "fn facet_keys(view: &mut view, bytes: &[u8]) {",
            "cre \\\nn  function stoppoll() {",
        ] {
            let raw = claude_lines(
                &[json!({"type": "assistant", "message": {"role": "assistant",
                "content": [json!({"type": "tool_use", "name": "Write",
                    "input": {"file_path": path, "content": "x"}})]}})],
            );
            let act = claude_activity(&raw);
            assert!(
                act.extensions.is_empty(),
                "path {path:?} counted {:?}",
                act.extensions
            );
        }
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
                "lead-delegation-guard: leads do not implement",
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
