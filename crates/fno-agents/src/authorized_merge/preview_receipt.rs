//! The preview receipt: one request in, the structured verdict out. Split
//! from authorized_merge.rs by the file-budget gate; the parent re-exports
//! the entry points, so every caller path is unchanged.

use std::path::PathBuf;

use serde_json::Value;

use super::{
    facts_from_pulls, preview_walk, BlockerClass, Effect, PreviewVerdict, Probes, RealProbes,
    Request,
};

/// The preview receipt for one request, in process. The status composer and
/// the verb share this one arm, so a status read pays no subprocess to its
/// own owner, and both surfaces can never render two different ready
/// verdicts.
pub(crate) fn preview_receipt(request: &Request) -> Value {
    let cwd = request.cwd.as_path();
    // Supplied facts win (preview-only): the status read's own projection
    // parses in process, and a payload too old to carry the fields falls back
    // to the guarded spawn so the walk never rides half a fact.
    let facts = request
        .supplied_facts
        .as_ref()
        .and_then(|p| facts_from_pulls(p, request.pr).ok());
    match match facts {
        Some(facts) => Ok(facts),
        None => RealProbes.pr_facts(cwd, request.pr),
    } {
        Err(reason) => serde_json::json!({ "outcome": "unknown", "reason": reason }),
        Ok(facts) => match preview_walk(&RealProbes, request, &facts) {
            PreviewVerdict::Go { waiver } => {
                let mut receipt = serde_json::json!({
                    "outcome": "authorized",
                    "head": facts.head_sha,
                    "blockers": [],
                });
                if let Some(note) = waiver {
                    receipt["coverage_waiver"] = Value::String(note);
                }
                receipt
            }
            PreviewVerdict::Blocked(rows) => serde_json::json!({
                "outcome": "held",
                "head": facts.head_sha,
                "blockers": rows
                    .iter()
                    .map(|b| serde_json::json!({
                        "code": b.code,
                        "class": match b.class {
                            BlockerClass::Held => "held",
                            BlockerClass::Refused => "refused",
                            BlockerClass::Unknown => "unknown",
                        },
                        "detail": b.detail,
                    }))
                    .collect::<Vec<_>>(),
            }),
        },
    }
}

/// The preview receipt for a raw payload: parse, then the same in-process
/// receipt the verb answers. An unusable payload reads `unknown` with the
/// parse error named, never a guessed verdict.
pub(crate) fn preview_receipt_payload(payload: &Value) -> Value {
    match parse_request(payload) {
        Ok(request) => preview_receipt(&request),
        Err(message) => serde_json::json!({ "outcome": "unknown", "detail": message }),
    }
}

pub(crate) fn read_payload(args: &[String]) -> Result<Value, String> {
    let text = match args.first() {
        // An inline JSON payload is the thin-forwarder form: a CLI verb with
        // no temp file. A path that names a payload file still works.
        Some(path) if path.trim_start().starts_with('{') => path.clone(),
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("authorized-merge: cannot read payload {path}: {e}\n"))?,
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| format!("authorized-merge: stdin read failed: {e}\n"))?;
            buf
        }
    };
    serde_json::from_str(&text).map_err(|e| format!("authorized-merge: bad payload: {e}\n"))
}

pub(crate) fn parse_request(payload: &Value) -> Result<Request, String> {
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "payload needs a cwd".to_string())?;
    let effect = payload
        .get("effect")
        .and_then(Value::as_str)
        .and_then(Effect::parse)
        .ok_or_else(|| "payload needs effect merge|arm|preview".to_string())?;
    Ok(Request {
        cwd,
        pr: payload.get("pr").and_then(Value::as_u64),
        effect,
        approved: payload.get("approved").and_then(Value::as_bool),
        auto_merge_source: payload
            .get("auto_merge_source")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // Preview defaults its CI gate ON: the question is "may this head
        // merge NOW", and a merge ask carries its own posture. A payload can
        // still pass require_checks: false to ask the gates without CI.
        require_checks: payload
            .get("require_checks")
            .and_then(Value::as_bool)
            .unwrap_or(effect == Effect::Preview),
        accept_flake: payload
            .get("accept_flake")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        covered_head: payload
            .get("covered_head")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        decide_only: payload
            .get("decide_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        authority: payload
            .get("authority")
            .and_then(Value::as_str)
            .map(str::to_owned),
        supplied_verdict: payload
            .get("verdict")
            .and_then(Value::as_str)
            .map(str::to_owned),
        supplied_counts: payload.get("counts").cloned(),
        supplied_rerun_recovered: payload.get("rerun_recovered").and_then(Value::as_bool),
        supplied_optional_unresolved: payload.get("optional_reviews_unresolved").map(|v| {
            if let Some(n) = v.as_i64() {
                Some(n)
            } else {
                None
            }
        }),
        supplied_github_blockers: payload
            .get("github_blockers")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        // Present-but-null is a probed CLEAR, so a held answer must stay a
        // string and an unprobed or malformed ask omits the key entirely.
        supplied_dispatch_hold: payload.get("dispatch_hold_reason").map(|v| match v {
            Value::String(reason) => Some(reason.to_owned()),
            Value::Null => None,
            other => Some(other.to_string()),
        }),
        // Same tri-state as the dispatch-hold answer above.
        supplied_review_hold: payload.get("review_hold_reason").map(|v| match v {
            Value::String(reason) => Some(reason.to_owned()),
            Value::Null => None,
            other => Some(other.to_string()),
        }),
        supplied_facts: payload.get("facts").filter(|v| v.is_object()).cloned(),
    })
}
