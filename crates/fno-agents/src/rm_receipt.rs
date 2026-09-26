//! The `rm` receipt: the one line a caller reads to learn what a removal
//! actually touched. It names every surface that survived, so a receipt
//! never reports a removal the removal did not perform, and the client's
//! exit code can key on the same facts the line prints.

use serde_json::Value;

/// The claims-release note a stop/rm receipt carries when the removal
/// released the row's dead claims (`; released N claim(s) ...`).
pub fn claims_release_suffix(result: &Value) -> String {
    let Some(claims) = result.get("claims") else {
        return String::new();
    };
    let (Some(released), Some(kept)) = (
        claims.get("released").and_then(Value::as_array),
        claims.get("kept").and_then(Value::as_array),
    ) else {
        return String::new();
    };
    if released.is_empty() && kept.is_empty() {
        return String::new();
    }
    let mut suffix = format!("; released {} claim(s)", released.len());
    for kept_claim in kept {
        suffix.push_str(&format!(
            "; kept {} ({})",
            kept_claim.get("key").and_then(Value::as_str).unwrap_or("?"),
            kept_claim
                .get("observed")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    suffix
}

/// The stop receipt's tail: the pid escalation the daemon performed, then the
/// claims-release note. `stopped_by == "pid"` means the stop ask alone did not
/// end the worker and the daemon ended the proved process set itself, so the
/// printed line names that second leg; every other stop prints the plain
/// claims suffix unchanged.
pub fn stop_receipt_suffix(result: &Value) -> String {
    let mut suffix = String::new();
    if result.get("stopped_by").and_then(Value::as_str) == Some("pid") {
        let pids = result
            .get("pids")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_u64)
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if !pids.is_empty() {
            suffix.push_str(&format!(" (ended pid {pids} after the stop ask)"));
        }
    }
    suffix.push_str(&claims_release_suffix(result));
    suffix
}

/// The receipt for one `agent.rm` result: `removed: <name>` when every
/// surface confirmed, and the surfaces plus refusals spelled out when they
/// did not. `None` when the result carries no receipt.
pub fn receipt(name: &str, result: &Value) -> Option<String> {
    let harness = result.get("harness").and_then(Value::as_str).unwrap_or("");
    let mut removed = vec!["fno"];
    let mut notes = Vec::new();
    // The harness row we just tore down IS the resume handle. The seam
    // warns BEFORE the reap; this names the reversal AFTER it, so a
    // direct `fno-agents rm` (which never passes the Python seam) is
    // not silent about the loss either.
    let mut adopt_hint: Option<String> = None;
    if !harness.is_empty() {
        let reason = result
            .get("harness_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        let row_id = result
            .get("harness_row_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        // Prefer the FULL session id over `harness_row_id`: that field
        // falls back to this id's first eight chars, which is not a
        // unique adopt key for a codex row (time-prefixed ids collide
        // across same-window sessions) and is not even hex for a
        // non-uuid id. Only name a handle that resolves back.
        let adopt_key = result
            .get("harness_session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or(row_id);
        match result.get("harness_removed").and_then(Value::as_bool) {
            Some(true) => {
                removed.push(harness);
                if adopt_key != "unknown" {
                    adopt_hint = Some(format!(
                        "\nthe {harness} session record was the resume handle; \
                         the transcript stays on disk.\nreverse it with: \
                         fno agents adopt {adopt_key} --cross-project"
                    ));
                }
            }
            Some(false) if reason.contains("already absent") => {
                notes.push(format!("{harness} row already absent"))
            }
            Some(false) => notes.push(format!("{harness} row {row_id} survives: {reason}")),
            None if harness == "claude" => {
                notes.push("claude list unreadable, harness side unverified".to_string())
            }
            None if !reason.is_empty() => {
                notes.push(format!("{harness} side unverified: {reason}"))
            }
            None => {}
        }
    }
    let pane_reason = result
        .get("pane_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    match result.get("pane_removed").and_then(Value::as_bool) {
        Some(true) => {
            removed.push("mux");
            // the confirmed stop's measurement (pane killed,
            // pid gone) is the printed proof - a bare "mux" would
            // name the surface but not the death it claims.
            if !pane_reason.is_empty() {
                notes.push(pane_reason.to_string());
            }
        }
        Some(false) if pane_reason.contains("already absent") => {
            notes.push("mux pane already absent".to_string())
        }
        Some(false) => {
            let session = result
                .get("pane_session")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let pane_id = result
                .get("pane_id")
                .and_then(Value::as_u64)
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            notes.push(format!(
                "mux pane {session}:{pane_id} survives: {pane_reason}"
            ));
        }
        None => {}
    }
    if result.get("event_written").and_then(Value::as_bool) == Some(false) {
        let reason = result
            .get("event_reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        notes.push(format!("event record not written: {reason}"));
    }
    if result.get("worktree_outcome").and_then(Value::as_str) == Some("removed") {
        // `null` means the size walk hit its budget on a large or
        // slow-storage tree - print `unmeasured`, never a `0` that
        // reads identically to "measured, nothing to reclaim".
        let bytes = match result.get("reclaimed_bytes").and_then(Value::as_u64) {
            Some(n) => n.to_string(),
            None => "unmeasured".to_string(),
        };
        notes.push(format!(
            "WARNING: worktree removed by guarded cleanup (reclaimed_bytes={bytes})"
        ));
    }
    if removed.len() == 1
        && notes.is_empty()
        && result.get("pane_removed").is_none_or(Value::is_null)
    {
        return Some(format!("removed: {name}{}", claims_release_suffix(result)));
    }
    let has_survivor = notes
        .iter()
        .any(|note| note.contains("survives") || note.contains("unverified"));
    let surfaces = if removed.len() == 1 && has_survivor {
        "fno only".to_string()
    } else {
        removed.join(" + ")
    };
    let detail = if notes.is_empty() {
        surfaces
    } else {
        format!("{surfaces}; {}", notes.join("; "))
    };
    Some(format!(
        "removed: {name} ({detail}){}{}",
        adopt_hint.unwrap_or_default(),
        claims_release_suffix(result)
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::stop_receipt_suffix;

    #[test]
    fn stop_receipt_suffix_names_the_ended_pids_after_an_escalation() {
        let result = json!({"stopped": true, "stopped_by": "pid", "pids": [4242]});
        assert_eq!(
            stop_receipt_suffix(&result),
            " (ended pid 4242 after the stop ask)"
        );
    }

    #[test]
    fn stop_receipt_suffix_is_claims_only_without_an_escalation() {
        // A shellout stop (the ask alone ended the worker) prints exactly
        // today's line; the escalation suffix never appears.
        assert_eq!(
            stop_receipt_suffix(&json!({"stopped_by": "shellout", "pids": [4242]})),
            ""
        );
        assert_eq!(stop_receipt_suffix(&json!({})), "");
    }
}
