//! PreToolUse guard: refuse EnterWorktree from a subagent.
//!
//! A subagent's EnterWorktree call moves the whole session into the target
//! worktree, the parent included: the parent transcript then carries
//! `relocated` and `worktree-state` rows keyed by the parent session id, and
//! the parent's own commands run as if isolated there. Claude Code owns that
//! session-scoped move; the refusal keeps it from firing. A parent already
//! moved recovers with the ExitWorktree tool, `action: keep`, which keeps the
//! subagent's files and returns the session to its own directory.
//!
//! A subagent payload carries `agent_id`; an in-process teammate's carries a
//! `transcript_path` under a `subagents/` directory. Either marker refuses.
//! A payload with neither marker is a top-level session and allows. Any
//! parse failure allows too (fail open): an unreadable payload is not a
//! refusal. The taught alternative is the absolute path: Read, Edit and
//! Write take absolute paths, and git runs as `git -C <path> ...`.

use serde_json::Value;

/// Entry: read one PreToolUse payload, decide, print, always exit 0.
pub fn run(_args: &[String]) -> i32 {
    let payload: Value = serde_json::from_str(super::read_stdin().trim()).unwrap_or(Value::Null);
    match reason_for(&payload) {
        Some(reason) => super::emit_block(&reason),
        None => super::emit_allow(),
    }
}

/// The refusal text for a payload the guard must block, or None to allow.
/// Separated from `run` so the tests exercise the same predicate the hook
/// does, with no stdin in the loop.
fn reason_for(payload: &Value) -> Option<String> {
    if payload.get("tool_name").and_then(Value::as_str) != Some("EnterWorktree") {
        return None;
    }
    let agent_id = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let transcript = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    let sid = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if agent_id.is_empty() && !super::lead_guard::is_subagent_transcript(transcript, sid) {
        return None;
    }
    let target = payload
        .get("tool_input")
        .and_then(|ti| {
            ti.get("path")
                .and_then(Value::as_str)
                .or_else(|| ti.get("name").and_then(Value::as_str))
        })
        .unwrap_or("the target worktree");
    let who = if agent_id.is_empty() {
        "a subagent".to_string()
    } else {
        format!("a subagent (agent_id {agent_id})")
    };
    Some(format!(
        "EnterWorktree refused: this call comes from {who}. Claude Code moves the whole session into {target}, the parent included, so the parent's own commands then run as if isolated there. Do not enter the worktree. Pass absolute paths to Read, Edit and Write, and run git as `git -C {target} ...`. A top-level session can still enter a worktree."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SID: &str = "7a3f9c1e-5b2d-4e8f-9a0c-1d2e3f4a5b6c";
    const TOP_TRANSCRIPT: &str = "/fixtures/proj/7a3f9c1e-5b2d-4e8f-9a0c-1d2e3f4a5b6c.jsonl";

    #[test]
    fn top_level_payload_allows() {
        let payload = json!({
            "session_id": SID,
            "transcript_path": TOP_TRANSCRIPT,
            "tool_name": "EnterWorktree",
            "tool_input": {"path": "/tmp/w"}
        });
        assert!(reason_for(&payload).is_none());
    }

    #[test]
    fn agent_id_payload_blocks_and_names_the_path() {
        let payload = json!({
            "session_id": SID,
            "transcript_path": TOP_TRANSCRIPT,
            "agent_id": "abc",
            "tool_name": "EnterWorktree",
            "tool_input": {"path": "/tmp/w"}
        });
        let reason = reason_for(&payload).expect("blocks");
        assert!(reason.contains("/tmp/w"));
        assert!(reason.contains("git -C"));
        assert!(reason.contains("agent_id abc"));
    }

    #[test]
    fn subagents_transcript_payload_blocks() {
        let payload = json!({
            "session_id": SID,
            "transcript_path": format!("/fixtures/proj/{SID}/subagents/agent-x.jsonl"),
            "tool_name": "EnterWorktree",
            "tool_input": {"name": "wt-name"}
        });
        let reason = reason_for(&payload).expect("blocks");
        assert!(reason.contains("wt-name"));
    }

    #[test]
    fn malformed_json_allows() {
        // `run` maps unparsable stdin to Value::Null; the same shape allows.
        assert!(reason_for(&Value::Null).is_none());
        assert!(reason_for(&json!("not an object")).is_none());
    }

    #[test]
    fn other_tool_allows() {
        let payload = json!({
            "session_id": SID,
            "transcript_path": TOP_TRANSCRIPT,
            "agent_id": "abc",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"}
        });
        assert!(reason_for(&payload).is_none());
    }
}
