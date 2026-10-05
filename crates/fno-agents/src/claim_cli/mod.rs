//! The operator leaf surface for `fno agents claim`, ported wave by wave
//! from `cli/src/fno/claims/cli.py`.
//!
//! `claim_verbs.rs::run_claim` dispatches one op per wave here. The Python
//! leaf forwards binary-direct (the `_forward_to_binary` shape), so this
//! module owns the operator contract: flag spellings, validation messages,
//! exit codes (0 acquired; 1 held-by-other/contention; 2 validation; 3
//! transient) and the human/JSON output shapes. The goldens frozen from the
//! Python leaf (`tests/claim_acquire_parity.rs`) are the contract.

pub mod acquire;
pub mod refresh;
pub mod release;

use serde_json::Value;
use std::path::PathBuf;

/// typer 0.27's UsageError layout, shared by the leaves whose goldens pin
/// the exact lines.
pub(crate) fn usage_refusal(usage: &str, help_hint: &str, detail: &str) -> i32 {
    eprintln!("Usage: {usage}");
    eprintln!("Try '{help_hint}' for help.");
    eprintln!();
    eprintln!("{detail}");
    2
}

/// `--ttl` expression ("30m" / "1h" / "3600s" / "5000") into milliseconds.
/// `Ok(None)` for the empty string (the caller decides the default); plain
/// digits are seconds. The error text is the frozen Python `typer
/// .BadParameter` message body. i128 carries the unbounded-int cases
/// Python's parse survives so the range refusal can name the number.
pub fn parse_ttl_expression(value: &str) -> Result<Option<i128>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    let t = value.trim();
    let (num_part, unit) = match t.chars().last() {
        Some(c @ ('s' | 'm' | 'h' | 'S' | 'M' | 'H')) => {
            let head = &t[..t.len() - c.len_utf8()];
            (head.trim_end(), c.to_ascii_lowercase())
        }
        _ => (t, '\0'),
    };
    if num_part.is_empty() || !num_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "invalid TTL format: '{value}' (use '30m', '1h', '3600s')"
        ));
    }
    let n: i128 = num_part.parse().unwrap_or(i128::MAX);
    let mult = match unit {
        'm' => 60_000i128,
        'h' => 3_600_000,
        _ => 1_000,
    };
    let ms = n.checked_mul(mult).ok_or_else(|| {
        format!(
            "ttl_ms={num_part} out of range [{}{}]",
            crate::claims::MIN_TTL_MS,
            crate::claims::MAX_TTL_MS
        )
    })?;
    Ok(Some(ms))
}

/// The parsed TTL narrowed to the engine's i64. Out-of-range names the
/// number exactly as `core._validate_inputs` does (the engine's own range
/// check cannot see an i128 that overflowed it).
pub fn ttl_ms_checked(ttl: i128) -> Result<i64, String> {
    const MIN: i128 = crate::claims::MIN_TTL_MS as i128;
    const MAX: i128 = crate::claims::MAX_TTL_MS as i128;
    if ttl < MIN || ttl > MAX {
        return Err(format!("ttl_ms={ttl} out of range [{MIN}, {MAX}]"));
    }
    Ok(ttl as i64)
}

/// `--metadata`: empty means `{}`; else a JSON object or the frozen Python
/// refusal.
pub fn parse_metadata_arg(value: &str) -> Result<serde_json::Map<String, Value>, String> {
    if value.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_str::<Value>(value) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("--metadata must be a JSON object".into()),
        Err(e) => Err(format!("--metadata is not valid JSON: {e}")),
    }
}

/// The claims root a key routes to: `Some` for the global-id prefixes, None
/// for repo-local keys. The one routing read (`claims_root.rs` owns the
/// prefix list), matching the Python leaf's `_node_aware_root`.
pub fn node_aware_root(key: &str) -> Option<PathBuf> {
    crate::claims_root::claims_root_for(key)
}

/// The registry effort for the owned identity, read through the shared
/// registry path (mirrors `_owned_registry_effort`: unreadable or malformed
/// state leaves effort unknown).
fn owned_registry_effort(harness: &str, session_id: &str) -> Option<String> {
    if harness.is_empty() || session_id.is_empty() {
        return None;
    }
    let path = crate::paths::AgentsHome::shared_registry_json();
    let raw: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let rows = raw.get("agents")?.as_array()?;
    let legacy_key = match harness {
        "claude" => Some("claude_session_uuid"),
        "codex" => Some("codex_session_id"),
        "gemini" => Some("gemini_session_id"),
        _ => None,
    };
    for row in rows {
        let Some(obj) = row.as_object() else {
            continue;
        };
        let Some(row_harness) = obj
            .get("harness")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("provider")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
        else {
            continue;
        };
        if row_harness != harness {
            continue;
        }
        let mut row_session = obj
            .get("harness_session_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if row_session.is_none() {
            row_session = legacy_key
                .and_then(|k| obj.get(k))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if row_session.as_deref() != Some(session_id) {
            continue;
        }
        return obj
            .get("effort")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    None
}

/// The (harness, session_id) an execute provenance row is written under:
/// the OWNED identity, ambient only as the fallback when the owned values
/// are absent (mirrors `_owned_do_identity`; the acquire and release stamps
/// must agree on the row key so release fills the row acquire opened).
fn owned_do_identity(claim: &crate::claims::ClaimRecord, holder: &str) -> (String, String) {
    let ambient = crate::claims::resolve_identity();
    let harness = claim
        .harness
        .as_deref()
        .filter(|h| !h.trim().is_empty())
        .map(str::to_string)
        .or_else(|| ambient.1.filter(|h| !h.trim().is_empty()))
        .unwrap_or_default();
    let session_id = holder
        .strip_prefix("target-session:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| ambient.0.filter(|s| !s.trim().is_empty()))
        .unwrap_or_default();
    (harness, session_id)
}

/// The `(node_id, harness, session_id, started_at, effort)` naming the do row
/// for this claim, or None after printing the named skip (mirrors
/// `_do_row_coordinates`): every do-row writer addresses the SAME row, and
/// `started_at` is the claim's own acquire time. `action` names the caller
/// in the skip line.
fn do_row_coordinates(
    key: &str,
    claim: &crate::claims::ClaimRecord,
    holder: &str,
    action: &str,
) -> Option<(String, String, String, String, Option<String>)> {
    let node_id = key.split_once(':').map(|(_, id)| id).unwrap_or("");
    if node_id.is_empty() {
        return None;
    }
    let (harness, session_id) = owned_do_identity(claim, holder);
    if harness.is_empty() || session_id.is_empty() {
        eprintln!(
            "claim {action}: no owned identity for the execute provenance row of \
             {node_id}; the row is skipped. Skipped."
        );
        return None;
    }
    let started = chrono::DateTime::from_timestamp_millis(claim.acquired_at)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();
    let effort = owned_registry_effort(&harness, &session_id);
    Some((node_id.to_string(), harness, session_id, started, effort))
}

/// Open the do lifecycle row at claim acquire (mirrors
/// `_stamp_do_on_acquire`): best-effort, node-keyed, named skips on stderr,
/// never fails the acquire.
pub fn stamp_do_on_acquire(key: &str, claim: &crate::claims::ClaimRecord, holder: &str) {
    let Some((node_id, harness, session_id, started, effort)) =
        do_row_coordinates(key, claim, holder, "acquire")
    else {
        return;
    };
    let row = match crate::graph_keeper::session_row(
        "execute",
        &harness,
        &session_id,
        effort.as_deref(),
        Some(&started),
        None,
        None,
        None,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("claim acquire: execute provenance open skipped for {node_id}: {e}");
            return;
        }
    };
    let found = std::cell::Cell::new(false);
    let graph = crate::backlog::settings::graph_path();
    let ok = crate::backlog::mutate_single_row(&graph, "session_append", |rows| {
        let (f, _added) = crate::graph_keeper::session_append(rows, &node_id, row.clone())
            .map_err(|e| e.to_string())?;
        found.set(f);
        Ok(f)
    });
    if let Err(e) = ok {
        eprintln!("claim acquire: execute provenance open skipped for {node_id}: {e}");
        return;
    }
    if !found.get() {
        eprintln!(
            "claim acquire: execute provenance open skipped for {node_id} \
             (node not in graph); the row was not written. Skipped."
        );
    }
}

/// Close the do lifecycle row the acquire stamp opened: fills `ended_at`
/// (mirrors `_stamp_do_on_release`). Best-effort: a graph failure or missing
/// identity is a named stderr skip and never fails the release.
pub fn stamp_do_on_release(key: &str, claim: &crate::claims::ClaimRecord, holder: &str) {
    let Some((node_id, harness, session_id, started, effort)) =
        do_row_coordinates(key, claim, holder, "release")
    else {
        return;
    };
    let ended = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let row = match crate::graph_keeper::session_row(
        "execute",
        &harness,
        &session_id,
        effort.as_deref(),
        Some(&started),
        Some(&ended),
        None,
        None,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("claim release: execute provenance stamp skipped for {node_id}: {e}");
            return;
        }
    };
    let found = std::cell::Cell::new(false);
    let graph = crate::backlog::settings::graph_path();
    let ok = crate::backlog::mutate_single_row(&graph, "session_append", |rows| {
        let (f, _added) = crate::graph_keeper::session_append(rows, &node_id, row.clone())
            .map_err(|e| e.to_string())?;
        found.set(f);
        Ok(f)
    });
    if let Err(e) = ok {
        eprintln!("claim release: execute provenance stamp skipped for {node_id}: {e}");
        return;
    }
    if !found.get() {
        eprintln!(
            "claim release: execute provenance stamp skipped for {node_id} \
             (node not in graph); the row was not written. Skipped."
        );
    }
}

/// Drop the open do row this claim's acquire opened, for a releaser whose
/// post-acquire validation refused it (mirrors `_rollback_do_on_release`).
/// The graph primitive only removes an OPEN row whose started_at equals this
/// claim's acquire time; best-effort and named on skip.
pub fn rollback_do_on_release(key: &str, claim: &crate::claims::ClaimRecord, holder: &str) {
    let Some((node_id, harness, session_id, started, _effort)) =
        do_row_coordinates(key, claim, holder, "release --rollback-do")
    else {
        return;
    };
    let outcome = std::cell::Cell::new((false, false));
    let graph = crate::backlog::settings::graph_path();
    let ok = crate::backlog::mutate_single_row(&graph, "session_remove_open", |rows| {
        let (f, removed) = crate::graph_keeper::session_remove_open(
            rows,
            &node_id,
            "execute",
            &harness,
            &session_id,
            &started,
        )
        .map_err(|e| e.to_string())?;
        outcome.set((f, removed));
        Ok(f || removed)
    });
    match ok {
        Err(e) => {
            eprintln!("claim release: execute provenance rollback skipped for {node_id}: {e}");
        }
        _ if !outcome.get().0 => {
            eprintln!(
                "claim release: execute provenance rollback skipped for {node_id} \
                 (node not in graph); nothing was removed. Skipped."
            );
        }
        _ if !outcome.get().1 => {
            eprintln!(
                "claim release: no open execute row to roll back for {node_id} \
                 (none was opened, or the row is already closed)."
            );
        }
        _ => {}
    }
}

/// Serialize the claim the way the Python leaf printed it:
/// `Claim.to_yaml_dict` field order under `json.dumps` default separators
/// (", " / ": "). `pid` stays `null` for a pid-unavailable claim exactly as
/// Python printed `None`. Nested metadata keys arrive sorted (serde_json's
/// map), where Python preserved source order; the goldens use one-key
/// metadata so the frozen bytes do not depend on it.
pub fn claim_json_string(claim: &crate::claims::ClaimRecord) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |k: &str, v: Value| {
        parts.push(format!(
            "{}: {}",
            serde_json::to_string(k).unwrap(),
            spaced(&v)
        ));
    };
    push("schema_version", claim.schema_version.into());
    push("key", Value::String(claim.key.clone()));
    push("holder", Value::String(claim.holder.clone()));
    push("acquired_at", claim.acquired_at.into());
    push("pid", claim.pid.map(Value::from).unwrap_or(Value::Null));
    push("host", Value::String(claim.host.clone()));
    if claim.pid_unavailable {
        push("pid_unavailable", Value::Bool(true));
    }
    if let Some(m) = &claim.machine_id {
        push("machine_id", Value::String(m.clone()));
    }
    if let Some(e) = claim.expires_at {
        push("expires_at", e.into());
    }
    if let Some(r) = &claim.reason {
        push("reason", Value::String(r.clone()));
    }
    if let Some(h) = &claim.harness {
        push("harness", Value::String(h.clone()));
    }
    if let Some(s) = &claim.session_id {
        push("session_id", Value::String(s.clone()));
    }
    if let Some(p) = &claim.pid_provenance {
        push("pid_provenance", Value::String(p.clone()));
    }
    if !claim.metadata.is_empty() {
        push("metadata", Value::Object(claim.metadata.clone()));
    }
    format!("{{{}}}", parts.join(", "))
}

/// `json.dumps` default separators over a serde value. Object keys iterate in
/// serde_json's map order.
fn spaced(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, val)| format!("{}: {}", serde_json::to_string(k).unwrap(), spaced(val)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().map(spaced).collect();
            format!("[{}]", parts.join(", "))
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".into()),
    }
}
