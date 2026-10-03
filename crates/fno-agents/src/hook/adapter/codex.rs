//! The codex hook payload: the claude-hook JSON convention codex's hooks
//! already deliver, plus a Stop-fire rollout read for the observed axes.
//!
//! The goal-leg shape (durable-mail pickup during goal mode): a goal
//! continuation is a user-role row in the transcript, so UserPromptSubmit
//! never fires and the only turn signal is PreToolUse on every goal-leg
//! tool call. That shape is pinned by the codex-pre-tool-use fixture.

use super::{effort_field, str_field, HookEvent};
use serde_json::Value;

/// How far back into a rollout the turn_context scan reads. A turn_context
/// row is written at each turn start, so a bounded tail answers "the latest"
/// without reading a multi-MB file.
const ROLLOUT_TAIL: u64 = 64 * 1024;

pub(crate) fn normalize(event: &str, payload: &Value) -> HookEvent {
    normalize_with_root(event, payload, None)
}
/// `rollout_root` overrides the codex sessions root for tests; `None` reads
/// the real store.
pub(crate) fn normalize_with_root(
    event: &str,
    payload: &Value,
    rollout_root: Option<&std::path::Path>,
) -> HookEvent {
    let session_id = str_field(payload, "session_id");
    let tool = str_field(payload, "tool_name");
    let message = str_field(payload, "message");
    let mut model = str_field(payload, "to_model");
    let mut effort = effort_field(payload);
    // The payload wins when it carries the axes; a Stop fire with none reads
    // the rollout's last turn_context instead (the observed truth the row
    // should carry).
    let posture = if event == "Stop" && !session_id.is_empty() {
        // The payload's transcript_path names the rollout directly when the
        // harness carries it; only a payload without one pays the sessions
        // store walk (unbounded, so it stays the fallback, not the path).
        let path = payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .filter(|p| std::path::Path::new(p).is_file())
            .map(std::path::PathBuf::from)
            .or_else(|| crate::codex_store::codex_rollout_path(rollout_root, &session_id));
        match path.and_then(|p| rollout_axes_at(&p)) {
            Some((m, e, p)) => {
                if model.is_empty() {
                    model = m;
                }
                if effort.is_empty() {
                    effort = e;
                }
                Some(p)
            }
            None => None,
        }
    } else {
        None
    };
    HookEvent {
        harness: "codex".into(),
        event: event.into(),
        session_id,
        tool,
        message,
        model,
        effort,
        posture,
    }
}
/// The rollout's last turn_context row, as (model, effort, posture). `None`
/// on any failure: a missing rollout is a report without observed axes,
/// never a failed hook.
fn rollout_axes_at(path: &std::path::Path) -> Option<(String, String, String)> {
    let blob = std::fs::read(path).ok()?;
    // The file can shrink between the read and any pre-computed offset, so
    // the tail is cut from the BLOB's length, never from a stat taken first
    // (a slice past the end would panic a fire-and-forget hook).
    let start = blob.len().saturating_sub(ROLLOUT_TAIL as usize);
    let text = String::from_utf8_lossy(&blob[start..]);
    let last = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|rec| rec.get("type").and_then(Value::as_str) == Some("turn_context"))
        .last()?;
    let ctx = last.get("payload")?;
    let model = ctx.get("model").and_then(Value::as_str).unwrap_or("");
    let effort = ctx
        .get("effort")
        .or_else(|| ctx.get("model_reasoning_effort"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let sandbox = string_or_mode(ctx.get("sandbox_policy"));
    let approval = ctx
        .get("approval_policy")
        .and_then(Value::as_str)
        .unwrap_or("");
    if sandbox.is_empty() || approval.is_empty() {
        return None;
    }
    Some((
        model.to_string(),
        effort.to_string(),
        format!("{sandbox}:{approval}"),
    ))
}

/// A sandbox_policy is a bare string on older codex builds and a
/// `{"mode": ...}` object on newer ones; both name the same word.
fn string_or_mode(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}
