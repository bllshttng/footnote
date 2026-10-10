//! The one function from records to a request, and the rules that read a
//! transcript back: where resume starts, which calls re-run, and what a
//! compaction keeps. The live loop builds every request through
//! `build_request`, so what the model saw is what a reader rebuilds.

use serde_json::{json, Value};

pub const MAX_TOKENS: u64 = 8192;
const TAIL: usize = 6;

fn data(r: &Value) -> &Value {
    &r["data"]
}

fn ty(r: &Value) -> &str {
    r["type"].as_str().unwrap_or("")
}

/// Index of the first record after the last compaction (0 when none).
fn start(records: &[Value]) -> (usize, Vec<Value>) {
    match records.iter().rposition(|r| ty(r) == "compaction") {
        Some(i) => (
            i + 1,
            data(&records[i])["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        ),
        None => (0, Vec::new()),
    }
}

fn push_block(messages: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
        let blocks = last["content"].as_array_mut().expect("content is an array");
        // tool_result blocks lead a user message, ahead of any text.
        if block["type"] == "tool_result" {
            let at = blocks
                .iter()
                .position(|b| b["type"] != "tool_result")
                .unwrap_or(blocks.len());
            blocks.insert(at, block);
        } else {
            blocks.push(block);
        }
        return;
    }
    messages.push(json!({"role": role, "content": [block]}));
}

/// The message list a request carries, from the records alone.
pub fn messages(records: &[Value]) -> Vec<Value> {
    let (from, mut out) = start(records);
    let provider_ids: std::collections::HashMap<String, Value> = records
        .iter()
        .filter(|r| ty(r) == "tool_call")
        .map(|r| {
            let d = data(r);
            (
                d["tool_call_uid"].as_str().unwrap_or("").to_string(),
                d["provider_tool_call_id"].clone(),
            )
        })
        .collect();
    for r in &records[from..] {
        let d = data(r);
        match ty(r) {
            "user_input" => {
                push_block(&mut out, "user", json!({"type": "text", "text": d["text"]}))
            }
            "model_response" => {
                let blocks: Vec<Value> = d["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|b| {
                        let mut b = b.clone();
                        if let Some(o) = b.as_object_mut() {
                            o.remove("raw_input");
                        }
                        b
                    })
                    .collect();
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool_result" => {
                let uid = d["tool_call_uid"].as_str().unwrap_or("");
                let id = provider_ids.get(uid).cloned().unwrap_or(Value::Null);
                push_block(
                    &mut out,
                    "user",
                    json!({"type": "tool_result", "tool_use_id": id, "content": d["model_text"],
                        "is_error": d["is_error"].as_bool().unwrap_or(false)}),
                );
            }
            _ => {}
        }
    }
    out
}

/// The system prompt: the header's base text plus every SessionStart
/// context the hooks handed back.
fn system(records: &[Value]) -> String {
    let mut s = records
        .iter()
        .find(|r| ty(r) == "header")
        .and_then(|h| data(h)["system_prompt"].as_str())
        .unwrap_or("")
        .to_string();
    for r in records.iter().filter(|r| ty(r) == "hook_decision") {
        let d = data(r);
        if d["event"] == "SessionStart" {
            if let Some(c) = d["context"].as_str().filter(|c| !c.is_empty()) {
                s.push_str("\n\n");
                s.push_str(c);
            }
        }
    }
    s
}

/// The exact wire body for the next request, from the records alone.
pub fn build_request(records: &[Value]) -> String {
    let header = records
        .iter()
        .find(|r| ty(r) == "header")
        .map(data)
        .cloned()
        .unwrap_or(Value::Null);
    let internal = json!({
        "model": header["model"],
        "max_tokens": MAX_TOKENS,
        "system": system(records),
        "messages": messages(records),
        "tools": super::tools::schemas(),
    });
    let wire = if header["wire"] == "openai" {
        super::model::Wire::OpenAi
    } else {
        super::model::Wire::Anthropic
    };
    super::model::wire_body(wire, &internal).to_string()
}

pub enum Pending {
    /// Never started, or started read-only: run it (through PreToolUse again).
    Run(Value),
    /// Started and effect-capable: the outcome is unknown; never re-run.
    Unknown(Value),
}

/// Tool calls since the last compaction that have no result.
pub fn pending(records: &[Value]) -> Vec<Pending> {
    let (from, _) = start(records);
    let tail = &records[from..];
    let has = |kind: &str, uid: &str| {
        tail.iter()
            .any(|r| ty(r) == kind && data(r)["tool_call_uid"] == uid)
    };
    tail.iter()
        .filter(|r| ty(r) == "tool_call")
        .filter_map(|r| {
            let d = data(r).clone();
            let uid = d["tool_call_uid"].as_str().unwrap_or("").to_string();
            if has("tool_result", &uid) {
                return None;
            }
            let effect = super::tools::effect_capable(d["name"].as_str().unwrap_or(""));
            Some(if has("tool_start", &uid) && effect {
                Pending::Unknown(d)
            } else {
                Pending::Run(d)
            })
        })
        .collect()
}

/// The `## Acceptance Criteria` and `## Open decisions` sections, verbatim.
pub fn plan_anchor(plan: &str) -> Option<String> {
    let mut out = String::new();
    let mut keep = false;
    for line in plan.lines() {
        if line.starts_with("## ") {
            keep = line == "## Acceptance Criteria" || line == "## Open decisions";
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The compacted message list and the compaction record's data. The first
/// user message keeps its text plus the marker, the plan anchor and the
/// node; the tail starts at an assistant message so every kept tool_result
/// still has its tool_use.
pub fn compact(
    records: &[Value],
    plan_path: Option<&str>,
    node: Option<&str>,
    tokens_before: u64,
) -> Value {
    let msgs = messages(records);
    let first_text = msgs
        .first()
        .and_then(|m| m["content"].as_array())
        .map(|b| {
            b.iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let mut tail_at = msgs.len().saturating_sub(TAIL).max(1);
    while tail_at < msgs.len() && msgs[tail_at]["role"] != "assistant" {
        tail_at += 1;
    }
    let (anchor, missing) = match plan_path {
        None => (None, Some("no plan_path in the manifest".to_string())),
        Some(p) => match std::fs::read_to_string(p) {
            Err(e) => (None, Some(format!("{p}: {e}"))),
            Ok(t) => match plan_anchor(&t) {
                Some(a) => (Some(a), None),
                None => (
                    None,
                    Some(format!(
                        "{p}: no Acceptance Criteria or Open decisions section"
                    )),
                ),
            },
        },
    };
    let (from, _) = start(records);
    let first_seq = records
        .get(from)
        .and_then(|r| r["seq"].as_u64())
        .unwrap_or(0);
    let last_seq = records.last().and_then(|r| r["seq"].as_u64()).unwrap_or(0);
    let summary = format!(
        "[compacted: {} earlier messages and their tool results were dropped (transcript seq {first_seq}..{last_seq})]",
        tail_at.saturating_sub(1)
    );
    let mut text = format!("{first_text}\n\n{summary}");
    if let Some(n) = node {
        text.push_str(&format!("\n\nClaimed node: {n}"));
    }
    if let Some(a) = &anchor {
        text.push_str(&format!(
            "\n\nPlan anchor ({}):\n{a}",
            plan_path.unwrap_or("")
        ));
    }
    let mut kept = vec![json!({"role": "user", "content": [{"type": "text", "text": text}]})];
    kept.extend_from_slice(&msgs[tail_at.min(msgs.len())..]);
    let tokens_after = (serde_json::to_string(&kept).map(|s| s.len()).unwrap_or(0) / 4) as u64;
    json!({
        "summary": summary,
        "tokens_before": tokens_before,
        "tokens_after": tokens_after,
        "trigger": "auto_80pct",
        "retained_tail": msgs.len() - tail_at.min(msgs.len()),
        "messages": kept,
        "plan_path": plan_path,
        "plan_anchor": anchor,
        "plan_anchor_sha256": anchor.as_deref().map(|a| super::transcript::sha256_hex(a.as_bytes())),
        "plan_anchor_missing": missing,
        "replaced_seq_range": [first_seq, last_seq],
    })
}
