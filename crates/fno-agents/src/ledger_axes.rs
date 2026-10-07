//! The ledger axis fill and the reconcile backstop writer, in one keeper
//! method.
//!
//! Python's `upsert_ledger_pr` keeps its name and signature but becomes one
//! keeper request; this module owns the logic. The fill joins every
//! `type == "execution"` row that misses an axis key to ONE delivering
//! session and takes each absent key from three sources in order: the live
//! registry row, the reap receipt, the graph session row. Provider comes
//! only from the registry or a receipt, never inferred from a harness or
//! model value. The same `/tmp/fno-ledger.lock` the Python writers hold
//! serializes the read-modify-write; the write itself is atomic (temp file
//! in the ledger's directory, then rename).

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::graph_keeper::{cached_entries, StoreState};

const LOCK_PATH: &str = "/tmp/fno-ledger.lock";
const UNRESOLVED: &str = "unresolved:no-harness-session";
const AXES: [&str; 4] = ["harness", "provider", "model", "effort"];
/// Terminal reasons that mark a NON-delivery attempt (the ported
/// `_NON_DELIVERY_TERMINALS`): a merged PR never stamps onto one.
const NON_DELIVERY: [&str; 5] = ["Budget", "NoProgress", "Interrupted", "Aborted", "NoWork"];

pub(crate) fn ledger_backstop(state: &StoreState, params: &Value) -> Result<Value, String> {
    let graph = cached_entries(state, false, false).map_err(|e| e.to_string())?;
    ledger_backstop_core(&graph, params)
}

fn string_param(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn ledger_backstop_core(graph: &[Value], params: &Value) -> Result<Value, String> {
    let ledger_path = string_param(params, "ledger_path").ok_or_else(|| {
        "ledger_path is required (the caller passes paths.ledger_json())".to_string()
    })?;
    let registry_path = string_param(params, "registry_path");
    let dry_run = params
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let node_id = string_param(params, "node_id");
    let pr_number = params.get("pr_number").and_then(Value::as_i64);
    if node_id.is_some() && pr_number.is_none() {
        return Err("pr_number is required when node_id is present".to_string());
    }
    let pr_url = string_param(params, "pr_url");
    let project = string_param(params, "project");
    let merged_at = string_param(params, "merged_at");
    let plan_path = string_param(params, "plan_path");
    let node_sessions: Vec<String> = params
        .get("node_sessions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let ledger = Path::new(&ledger_path);
    if let Some(parent) = ledger.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOCK_PATH)
        .map_err(|e| format!("cannot open {LOCK_PATH}: {e}"))?;
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let result = run_locked(
        graph,
        ledger,
        registry_path.as_deref(),
        node_id.as_deref(),
        pr_number,
        pr_url.as_deref(),
        project.as_deref(),
        merged_at.as_deref(),
        plan_path.as_deref(),
        &node_sessions,
        dry_run,
    );
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

#[allow(clippy::too_many_arguments)]
fn run_locked(
    graph: &[Value],
    ledger: &Path,
    registry_path: Option<&str>,
    node_id: Option<&str>,
    pr_number: Option<i64>,
    pr_url: Option<&str>,
    project: Option<&str>,
    merged_at: Option<&str>,
    plan_path: Option<&str>,
    node_sessions: &[String],
    dry_run: bool,
) -> Result<Value, String> {
    let mut doc: Value = match std::fs::read_to_string(ledger) {
        // A missing ledger reads as empty - the ported `_load_ledger_data`
        // behavior; the write below creates it. Corrupt still refuses: the
        // Python recovery path stays the only writer that backs a ledger up.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"entries": []}),
        Err(e) => return Err(format!("ledger {} unreadable: {e}", ledger.display())),
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
            format!(
                "ledger {} is corrupt ({e}); refusing to write - the Python recovery path owns ledger backups",
                ledger.display()
            )
        })?,
    };
    if doc.get("entries").and_then(Value::as_array).is_none() {
        return Err(format!(
            "ledger {} carries no entries array; refusing to write",
            ledger.display()
        ));
    }
    let entries = doc["entries"].as_array_mut().expect("checked above");

    let mut outcome: Option<&'static str> = None;
    if let (Some(node_id), Some(pr_number)) = (node_id, pr_number) {
        outcome = Some(upsert(
            entries,
            node_id,
            pr_number,
            pr_url,
            project,
            merged_at,
            plan_path,
            node_sessions,
        ));
    }
    let fill = fill_axes(entries, graph, registry_path);
    // The write carries the UPSERT too: a created/stamped row whose axes the
    // fill cannot resolve (no sources) must still persist - the old gate
    // (`fill.changed > 0` only) silently dropped every such row.
    let mutated = !matches!(outcome.as_deref(), None | Some("already-present"));
    if (mutated || fill.changed > 0) && !dry_run {
        write_ledger(ledger, &doc)?;
    }
    Ok(json!({
        "outcome": outcome,
        "fill": {
            "scanned": fill.scanned,
            "changed": fill.changed,
            "harness": fill.harness,
            "provider": fill.provider,
            "model": fill.model,
            "effort": fill.effort,
            "unresolved": fill.unresolved,
        }
    }))
}

/// The ported `upsert_ledger_pr`: same outcomes, same row shape, same key
/// order (`preserve_order` keeps the created row byte-compatible with the
/// Python writer's dict).
#[allow(clippy::too_many_arguments)]
fn upsert(
    entries: &mut Vec<Value>,
    node_id: &str,
    pr_number: i64,
    pr_url: Option<&str>,
    project: Option<&str>,
    merged_at: Option<&str>,
    plan_path: Option<&str>,
    node_sessions: &[String],
) -> &'static str {
    let rows: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.get("type").and_then(Value::as_str) == Some("execution")
                && e.get("graph_node_id").and_then(Value::as_str) == Some(node_id)
        })
        .map(|(i, _)| i)
        .collect();
    // Already correctly attributed to THIS merge.
    if rows
        .iter()
        .any(|&i| entries[i].get("pr_number").and_then(Value::as_i64) == Some(pr_number))
    {
        return "already-present";
    }
    // Stamp a DELIVERY row that finalized without resolving its PR - never a
    // failed attempt (Budget/NoProgress/... stays unstamped; the delivery
    // gets a fresh backstop instead).
    let stamp = rows.into_iter().find(|&i| {
        entries[i]
            .get("pr_number")
            .and_then(Value::as_i64)
            .is_none()
            && entries[i]
                .get("termination_reason")
                .and_then(Value::as_str)
                .map_or(true, |tr| !NON_DELIVERY.contains(&tr))
    });
    if let Some(i) = stamp {
        entries[i]["pr_number"] = json!(pr_number);
        if let Some(url) = pr_url {
            entries[i]["pr_url"] = json!(url);
        }
        return "stamped";
    }
    let mut row = Map::new();
    row.insert("type".into(), json!("execution"));
    row.insert("status".into(), json!("done"));
    row.insert("graph_node_id".into(), json!(node_id));
    row.insert("pr_number".into(), json!(pr_number));
    row.insert("pr_url".into(), pr_url.map_or(Value::Null, |u| json!(u)));
    row.insert("project".into(), project.map_or(Value::Null, |p| json!(p)));
    row.insert(
        "plan_path".into(),
        plan_path.map_or(Value::Null, |s| json!(s)),
    );
    row.insert(
        "completed".into(),
        utc_iso(merged_at).map_or(Value::Null, |s| json!(s)),
    );
    row.insert("backstop".into(), json!(true));
    row.insert("termination_reason".into(), json!("reconcile-backstop"));
    row.insert("session_id".into(), Value::Null);
    row.insert("sessions".into(), sessions_or_unresolved(node_sessions));
    entries.push(Value::Object(row));
    "created"
}

/// The ported `_utc_iso`: aware UTC with a `+00:00` suffix at seconds
/// precision; a naive input reads as UTC; an unparseable string is stored
/// verbatim rather than losing the row.
fn utc_iso(value: Option<&str>) -> Option<String> {
    let raw = value?;
    if raw.trim().is_empty() {
        return Some(raw.to_string());
    }
    let normalized = raw.replace('Z', "+00:00");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        );
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(
            dt.and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        );
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        return Some(
            d.and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        );
    }
    Some(raw.to_string())
}

/// The ported `sessions_or_unresolved`: distinct non-empty ids in order, or
/// the explicit unresolved marker.
fn sessions_or_unresolved(candidates: &[String]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for c in candidates {
        let s = c.trim();
        if !s.is_empty() && !out.iter().any(|v| v.as_str() == Some(s)) {
            out.push(json!(s));
        }
    }
    if out.is_empty() {
        out.push(json!(UNRESOLVED));
    }
    Value::Array(out)
}

/// Atomic replace, matching `_write_ledger_data`: temp file in the ledger's
/// directory, then rename. Pretty JSON, 2-space indent, trailing newline.
fn write_ledger(path: &Path, doc: &Value) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map_or_else(
        || "ledger.json".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let write = || -> Result<(), String> {
        let body = serde_json::to_string_pretty(doc)
            .map_err(|e| format!("ledger re-serialize failed: {e}"))?;
        std::fs::write(&tmp, body + "\n").map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("{} -> {}: {e}", tmp.display(), path.display()))
    };
    match write() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The ported `harvest_ledger_sessions`: fill an ABSENT `sessions` key on
/// every execution row from its graph node, never overwrite, under the same
/// ledger lock the append path takes. Returns (filled, marked). An
/// unreadable ledger reads as nothing to fill - a fill-only leg never
/// resets a corrupt file.
pub(crate) fn harvest_sessions(
    ledger: &Path,
    nodes_by_id: &serde_json::Map<String, Value>,
    dry_run: bool,
) -> Result<(usize, usize), String> {
    if !ledger.exists() {
        return Ok((0, 0));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOCK_PATH)
        .map_err(|e| format!("cannot open {LOCK_PATH}: {e}"))?;
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let result = harvest_locked(ledger, nodes_by_id, dry_run);
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

fn harvest_locked(
    ledger: &Path,
    nodes_by_id: &serde_json::Map<String, Value>,
    dry_run: bool,
) -> Result<(usize, usize), String> {
    let text = std::fs::read_to_string(ledger).map_err(|e| format!("{}: {e}", ledger.display()))?;
    let mut data: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"entries": []}));
    let Some(entries) = data.get_mut("entries").and_then(Value::as_array_mut) else {
        return Ok((0, 0));
    };
    let mut filled = 0;
    let mut marked = 0;
    for row in entries.iter_mut() {
        if row.get("type").and_then(Value::as_str) != Some("execution") {
            continue;
        }
        if row.get("sessions").is_some() {
            continue; // the Python leg skips the KEY's presence, null included
        }
        let node = row
            .get("graph_node_id")
            .and_then(Value::as_str)
            .and_then(|id| nodes_by_id.get(id));
        let ids: Vec<String> = node
            .and_then(|n| n.get("sessions"))
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|s| s.get("session_id"))
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        row["sessions"] = sessions_or_unresolved(&ids);
        if ids.is_empty() {
            marked += 1;
        } else {
            filled += 1;
        }
    }
    if !dry_run && (filled > 0 || marked > 0) {
        write_ledger(ledger, &data)?;
    }
    Ok((filled, marked))
}

#[derive(Default)]
struct FillCounts {
    scanned: usize,
    changed: usize,
    harness: usize,
    provider: usize,
    model: usize,
    effort: usize,
    unresolved: usize,
}

/// One graph session row (a node's `sessions[]` entry), reduced to the axes
/// the fill reads and the stamp the "latest" choice orders on.
struct GraphSession {
    session_id: String,
    phase: String,
    harness: Option<String>,
    effort: Option<String>,
    /// `observed_model.model` when its kind is `observed`; never a request.
    model: Option<String>,
    stamp: Option<chrono::NaiveDateTime>,
}

/// The axes one reap receipt answers, read through `model_provenance`.
struct ReceiptAxes {
    harness: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    effort: Option<String>,
}

fn fill_axes(entries: &mut [Value], graph: &[Value], registry_path: Option<&str>) -> FillCounts {
    let registry = registry_path.and_then(|p| crate::state::load_registry(Path::new(p)).ok());
    let sessions = collect_graph_sessions(graph);
    let mut receipts: Option<HashMap<String, ReceiptAxes>> = None;
    let mut counts = FillCounts::default();
    for row in entries.iter_mut() {
        if row.get("type").and_then(Value::as_str) != Some("execution") {
            continue;
        }
        let missing_before: Vec<String> = AXES
            .iter()
            .filter(|a| axis_value(row, a).is_none())
            .map(|a| a.to_string())
            .collect();
        if missing_before.is_empty() {
            continue;
        }
        counts.scanned += 1;
        let ids = row_ids(row);
        let chosen = choose_graph_session(&sessions, &ids);
        let mut sid = chosen.as_ref().map(|g| g.session_id.clone()).or_else(|| {
            ids.iter()
                .find(|id| {
                    registry
                        .as_ref()
                        .is_some_and(|r| r.find_name_or_full_session_id(id).is_some())
                })
                .cloned()
        });
        if let Some(sid) = &sid {
            // Source 1: the live registry row.
            if let Some(reg) = &registry {
                if let Some(e) = reg.find_name_or_full_session_id(sid) {
                    fill_axis(row, "harness", e.harness.clone(), &mut counts);
                    fill_axis(row, "provider", e.provider.clone(), &mut counts);
                    fill_axis(
                        row,
                        "model",
                        e.model.clone().or_else(|| e.requested_model.clone()),
                        &mut counts,
                    );
                    fill_axis(
                        row,
                        "effort",
                        e.effort.clone().or_else(|| e.requested_effort.clone()),
                        &mut counts,
                    );
                }
            }
            // Source 2: the reap receipts, indexed once - a session's own
            // final provenance outranks the graph's observed sample.
            if AXES.iter().any(|a| axis_value(row, a).is_none()) {
                if receipts.is_none() {
                    receipts = Some(index_receipts(registry_path));
                }
                if let Some(r) = receipts.as_ref().expect("just indexed").get(sid) {
                    fill_axis(row, "harness", r.harness.clone(), &mut counts);
                    fill_axis(row, "provider", r.provider.clone(), &mut counts);
                    fill_axis(row, "model", r.model.clone(), &mut counts);
                    fill_axis(row, "effort", r.effort.clone(), &mut counts);
                }
            }
            // Source 3: the chosen graph session row. Provider is never a
            // graph answer - the graph never learns the vendor axis.
            if let Some(g) = chosen {
                fill_axis(row, "harness", g.harness.clone(), &mut counts);
                fill_axis(row, "model", g.model.clone(), &mut counts);
                fill_axis(row, "effort", g.effort.clone(), &mut counts);
            }
        }
        // Receipt fallback for a row whose sid no earlier source resolved: the
        // receipt key itself names the session.
        if AXES.iter().any(|a| axis_value(row, a).is_none()) {
            if receipts.is_none() {
                receipts = Some(index_receipts(registry_path));
            }
            let map = receipts.as_ref().expect("just indexed");
            if sid.is_none() {
                sid = ids.iter().find(|id| map.contains_key(*id)).cloned();
            }
            if let Some(sid) = &sid {
                if let Some(r) = map.get(sid) {
                    fill_axis(row, "harness", r.harness.clone(), &mut counts);
                    fill_axis(row, "provider", r.provider.clone(), &mut counts);
                    fill_axis(row, "model", r.model.clone(), &mut counts);
                    fill_axis(row, "effort", r.effort.clone(), &mut counts);
                }
            }
        }
        if AXES.iter().any(|a| axis_value(row, a).is_none()) {
            counts.unresolved += 1;
        }
        if AXES
            .iter()
            .any(|a| axis_value(row, a).is_some() && missing_before.contains(&a.to_string()))
        {
            counts.changed += 1;
        }
    }
    counts
}

/// A key the fill may write: present unless absent or null.
fn axis_value<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    row.get(key).filter(|v| !v.is_null())
}

fn fill_axis(row: &mut Value, key: &str, value: Option<String>, counts: &mut FillCounts) {
    let Some(v) = value else { return };
    if axis_value(row, key).is_none() {
        row[key] = json!(v);
        match key {
            "harness" => counts.harness += 1,
            "provider" => counts.provider += 1,
            "model" => counts.model += 1,
            "effort" => counts.effort += 1,
            _ => {}
        }
    }
}

/// The ids one ledger row names: its `sessions` minus `unresolved:` markers,
/// plus the scalar `session_id` and `fno_id`.
fn row_ids(row: &Value) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let push = |s: &str, ids: &mut Vec<String>| {
        if !s.is_empty() && !ids.contains(&s.to_string()) {
            ids.push(s.to_string());
        }
    };
    if let Some(list) = row.get("sessions").and_then(Value::as_array) {
        for v in list {
            if let Some(s) = v.as_str() {
                if !s.starts_with("unresolved:") {
                    push(s, &mut ids);
                }
            }
        }
    }
    for key in ["session_id", "fno_id"] {
        if let Some(s) = row.get(key).and_then(Value::as_str) {
            push(s, &mut ids);
        }
    }
    ids
}

/// The ONE delivering session: the latest `ship`-phase graph row whose id the
/// row names, else the latest `execute`-phase row.
fn choose_graph_session<'a>(
    sessions: &'a [GraphSession],
    ids: &[String],
) -> Option<&'a GraphSession> {
    let in_ids = |g: &GraphSession| ids.iter().any(|i| i == &g.session_id);
    let latest = |phase: &str| {
        sessions
            .iter()
            .filter(|g| g.phase == phase && in_ids(g))
            .max_by(|a, b| a.stamp.cmp(&b.stamp))
    };
    latest("ship").or_else(|| latest("execute"))
}

fn collect_graph_sessions(graph: &[Value]) -> Vec<GraphSession> {
    let mut out = Vec::new();
    for node in graph {
        let Some(list) = node.get("sessions").and_then(Value::as_array) else {
            continue;
        };
        for s in list {
            let Some(sid) = s
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let model = s
                .get("observed_model")
                .filter(|o| o.get("kind").and_then(Value::as_str) == Some("observed"))
                .and_then(|o| o.get("model").and_then(Value::as_str))
                .map(str::to_string);
            let stamp = [s.get("ended_at"), s.get("started_at")]
                .into_iter()
                .find_map(|v| v.and_then(Value::as_str))
                .and_then(parse_local);
            out.push(GraphSession {
                session_id: sid.to_string(),
                phase: s
                    .get("phase")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                harness: s.get("harness").and_then(Value::as_str).map(str::to_string),
                effort: s.get("effort").and_then(Value::as_str).map(str::to_string),
                model,
                stamp,
            });
        }
    }
    out
}

/// The fold's `_parse_ts` shape: an aware stamp lands on the local timeline
/// before its tzinfo is stripped, so offsets do not read as shifts.
fn parse_local(raw: &str) -> Option<chrono::NaiveDateTime> {
    let normalized = raw.replace('Z', "+00:00");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.with_timezone(&chrono::Local).naive_local());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt);
    }
    chrono::NaiveDate::parse_from_str(&normalized, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// Index the reap receipts beside the registry once per call. A receipt that
/// cannot be read is skipped; absence answers absence.
fn index_receipts(registry_path: Option<&str>) -> HashMap<String, ReceiptAxes> {
    let mut map = HashMap::new();
    let Some(registry_path) = registry_path else {
        return map;
    };
    let Some(dir) = Path::new(registry_path)
        .parent()
        .map(|p| p.join("reap-receipts"))
    else {
        return map;
    };
    let Ok(list) = std::fs::read_dir(&dir) else {
        return map;
    };
    for entry in list.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(receipt) = crate::receipt::read_reap_receipt(&path) else {
            continue;
        };
        let provenance = receipt.model_provenance.unwrap_or(Value::Null);
        let axis = |key: &str| {
            provenance
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        map.insert(
            receipt.harness_session_id.clone(),
            ReceiptAxes {
                harness: Some(receipt.harness.clone()).filter(|h| !h.is_empty()),
                model: axis("model"),
                provider: axis("provider"),
                effort: axis("effort"),
            },
        );
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const REGISTRY_WITH_U1: &str = r#"{"schema_version":20,"agents":[{"name":"w","harness":"claude","harness_session_id":"u1","cwd":"/tmp/x","created_at":"2026-09-01T00:00:00Z","status":"live","provider":"anthropic","model":"glm-5.3-flash","effort":"high"}]}"#;
    const RECEIPT_FOR_U1: &str = r#"{"row_name":"w","short_id":"w-id","harness":"claude","harness_session_id":"u1","cwd":"/tmp/x","created_at":"2026-09-01T00:00:00Z","reaped_at":"2026-09-02T00:00:00Z","removed_by":"gc-sweep","removal_trigger":"unattended","resume":"claude --resume u1","model_provenance":{"model":"glm-5.3-flash","provider":"zai","effort":"flash"}}"#;

    struct World {
        dir: TempDir,
    }

    impl World {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            World { dir }
        }
        fn ledger_path(&self) -> std::path::PathBuf {
            self.dir.path().join("ledger.json")
        }
        fn registry_path(&self) -> std::path::PathBuf {
            self.dir.path().join("agents").join("registry.json")
        }
        fn write_registry(&self, body: &str) {
            std::fs::create_dir_all(self.registry_path().parent().unwrap()).unwrap();
            crate::registry_store::seed_raw(&self.registry_path(), body);
        }
        fn write_receipt(&self) {
            // Production layout: the receipts live beside the registry, under
            // the agents home (`<registry parent>/reap-receipts/`), and the
            // reader derives the dir from the registry path.
            let dir = self.registry_path().parent().unwrap().join("reap-receipts");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("claude-u1.json"), RECEIPT_FOR_U1).unwrap();
        }
        fn write_ledger(&self, entries: Value) {
            std::fs::write(
                self.ledger_path(),
                serde_json::to_string_pretty(&json!({"entries": entries})).unwrap() + "\n",
            )
            .unwrap();
        }
        fn read_entries(&self) -> Vec<Value> {
            serde_json::from_str::<Value>(&std::fs::read_to_string(self.ledger_path()).unwrap())
                .unwrap()["entries"]
                .as_array()
                .unwrap()
                .clone()
        }
        fn graph_u1(&self, phase: &str, harness: &str, model: Option<&str>) -> Value {
            let mut s = json!({"phase": phase, "harness": harness, "session_id": "u1",
                "started_at": "2026-09-01T10:00:00Z", "ended_at": "2026-09-01T11:00:00Z"});
            if let Some(m) = model {
                s["observed_model"] = json!({"kind": "observed", "model": m, "samples": 3});
            }
            json!({"id": "x-1", "sessions": [s]})
        }
        fn run(&self, graph: &[Value], params: Value) -> Value {
            let mut p = json!({
                "ledger_path": self.ledger_path().display().to_string(),
                "registry_path": self.registry_path().display().to_string(),
            });
            let obj = p.as_object_mut().unwrap();
            for (k, v) in params.as_object().unwrap() {
                obj.insert(k.clone(), v.clone());
            }
            ledger_backstop_core(graph, &p).expect("core run")
        }
    }

    #[test]
    fn upsert_created_row_carries_the_axes_the_fill_resolves() {
        let w = World::new();
        w.write_registry(REGISTRY_WITH_U1);
        w.write_ledger(json!([]));
        let graph = [w.graph_u1("ship", "claude", Some("ignored-graph-model"))];
        let reply = w.run(
            &graph,
            json!({"node_id": "x-1", "pr_number": 101, "pr_url": "http://pr/101",
                "project": "fno", "merged_at": "2026-07-18T00:00:00Z",
                "plan_path": "plans/x-1.md", "node_sessions": ["u1"]}),
        );
        assert_eq!(reply["outcome"], "created", "{reply}");
        let rows = w.read_entries();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        // The created row opens with the Python writer's dict order; the
        // fill appends the four axes after it in the same write.
        let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            &keys[..12],
            [
                "type",
                "status",
                "graph_node_id",
                "pr_number",
                "pr_url",
                "project",
                "plan_path",
                "completed",
                "backstop",
                "termination_reason",
                "session_id",
                "sessions"
            ]
        );
        assert_eq!(&keys[12..], ["harness", "provider", "model", "effort"]);
        assert_eq!(r["completed"], "2026-07-18T00:00:00+00:00");
        assert_eq!(r["backstop"], true);
        assert_eq!(r["sessions"], json!(["u1"]));
        // The fill resolved the axes from the registry row (u1): the graph
        // answer loses the model race, and the graph never answers provider.
        assert_eq!(r["harness"], "claude");
        assert_eq!(r["provider"], "anthropic");
        assert_eq!(r["model"], "glm-5.3-flash");
        assert_eq!(r["effort"], "high");
        assert!(r.get("provider_id").is_none());
        assert_eq!(reply["fill"]["changed"], 1, "{reply}");
    }

    #[test]
    fn upsert_stamps_a_null_pr_row_and_never_a_failed_attempt() {
        let w = World::new();
        w.write_ledger(json!([
            {"type": "execution", "status": "done", "graph_node_id": "x-2",
             "pr_number": null, "cost_usd": 1.23, "phases_completed": ["do", "ship"],
             "completed": "2026-07-18T01:00:00Z", "fno_id": "sess-b"},
            {"type": "execution", "graph_node_id": "x-3", "pr_number": null,
             "termination_reason": "Budget", "cost_usd": 5.0}
        ]));
        let reply = w.run(
            &[],
            json!({"node_id": "x-2", "pr_number": 202, "pr_url": "http://pr/202",
                "project": "fno", "merged_at": "2026-07-18T09:00:00Z"}),
        );
        assert_eq!(reply["outcome"], "stamped", "{reply}");
        let rows = w.read_entries();
        assert_eq!(rows.len(), 2, "stamped in place, no new row");
        assert_eq!(rows[0]["pr_number"], 202);
        assert_eq!(rows[0]["cost_usd"], 1.23, "full-fidelity fields preserved");
        assert_eq!(
            rows[0]["completed"], "2026-07-18T01:00:00Z",
            "stamp never clobbers"
        );

        let reply = w.run(
            &[],
            json!({"node_id": "x-3", "pr_number": 707, "pr_url": "http://pr/707",
                "project": "fno", "merged_at": "2026-07-18T00:00:00Z"}),
        );
        assert_eq!(reply["outcome"], "created", "{reply}");
        let rows = w.read_entries();
        assert_eq!(
            rows.len(),
            3,
            "failed attempt preserved, delivery backstop added"
        );
        assert_eq!(rows[1]["pr_number"], Value::Null, "NOT stamped");
        assert_eq!(rows[2]["pr_number"], 707);
    }

    #[test]
    fn upsert_already_present_noops() {
        let w = World::new();
        w.write_ledger(json!([
            {"type": "execution", "graph_node_id": "x-4", "pr_number": 303,
             "pr_url": "http://pr/303"}
        ]));
        let reply = w.run(
            &[],
            json!({"node_id": "x-4", "pr_number": 303, "pr_url": "http://pr/303",
                "project": "fno", "merged_at": "2026-07-18T00:00:00Z"}),
        );
        assert_eq!(reply["outcome"], "already-present", "{reply}");
        assert_eq!(w.read_entries().len(), 1);
    }

    #[test]
    fn fill_sources_run_registry_then_receipt_then_graph() {
        let w = World::new();
        // Registry answers harness + provider only.
        w.write_registry(
            r#"{"schema_version":20,"agents":[{"name":"w","harness":"claude","harness_session_id":"u1","cwd":"/tmp/x","created_at":"2026-09-01T00:00:00Z","status":"live","provider":"anthropic"}]}"#,
        );
        // The receipt answers model + effort.
        w.write_receipt();
        w.write_ledger(json!([
            {"type": "execution", "completed": "2026-09-01T12:00:00Z",
             "sessions": ["u1"]}
        ]));
        // The graph row would answer harness codex and model a-graph-model;
        // both lose to their earlier sources.
        let graph = [w.graph_u1("ship", "codex", Some("a-graph-model"))];
        let reply = w.run(&graph, json!({}));
        let fill = &reply["fill"];
        assert_eq!(fill["scanned"], 1, "{reply}");
        assert_eq!(fill["harness"], 1);
        assert_eq!(fill["provider"], 1);
        assert_eq!(fill["model"], 1);
        assert_eq!(fill["effort"], 1);
        assert_eq!(fill["unresolved"], 0);
        let r = &w.read_entries()[0];
        assert_eq!(r["harness"], "claude");
        assert_eq!(r["provider"], "anthropic");
        assert_eq!(r["model"], "glm-5.3-flash");
        assert_eq!(r["effort"], "flash");
        assert!(r.get("provider_id").is_none(), "never write provider_id");
    }

    #[test]
    fn fill_never_overwrites_present_keys() {
        let w = World::new();
        w.write_registry(REGISTRY_WITH_U1);
        w.write_ledger(json!([
            {"type": "execution", "completed": "2026-09-01T12:00:00Z",
             "harness": "codex", "model": "kept-model", "sessions": ["u1"]}
        ]));
        let graph = [w.graph_u1("ship", "claude", Some("other-model"))];
        let reply = w.run(&graph, json!({}));
        let r = &w.read_entries()[0];
        assert_eq!(r["harness"], "codex", "present key untouched");
        assert_eq!(r["model"], "kept-model");
        assert_eq!(reply["fill"]["harness"], 0, "{reply}");
        assert_eq!(reply["fill"]["model"], 0);
        // provider and effort were absent and the registry answers them.
        assert_eq!(r["provider"], "anthropic");
        assert_eq!(r["effort"], "high");
    }

    #[test]
    fn dry_run_counts_but_writes_nothing() {
        let w = World::new();
        w.write_registry(REGISTRY_WITH_U1);
        w.write_ledger(json!([
            {"type": "execution", "completed": "2026-09-01T12:00:00Z", "sessions": ["u1"]}
        ]));
        let before = std::fs::read_to_string(w.ledger_path()).unwrap();
        let reply = w.run(&[], json!({"dry_run": true}));
        assert_eq!(reply["fill"]["harness"], 1, "{reply}");
        assert_eq!(reply["outcome"], Value::Null, "no node_id, no upsert");
        let after = std::fs::read_to_string(w.ledger_path()).unwrap();
        assert_eq!(before, after, "dry run never writes");
        // A plain fill run persists the same answer.
        w.run(&[], json!({}));
        let r = &w.read_entries()[0];
        assert_eq!(r["harness"], "claude");
    }

    #[test]
    fn marker_only_row_counts_as_unresolved() {
        let w = World::new();
        w.write_registry(REGISTRY_WITH_U1);
        w.write_ledger(json!([
            {"type": "execution", "completed": "2026-09-01T12:00:00Z",
             "sessions": ["unresolved:no-harness-session"]}
        ]));
        let reply = w.run(&[], json!({}));
        let fill = &reply["fill"];
        assert_eq!(fill["scanned"], 1, "{reply}");
        assert_eq!(fill["unresolved"], 1);
        assert_eq!(fill["harness"], 0);
        let r = &w.read_entries()[0];
        assert!(r.get("harness").is_none(), "no axis guessed: {r}");
    }

    #[test]
    fn utc_iso_and_session_list_shapes_match_the_python_writer() {
        let w = World::new();
        w.write_ledger(json!([]));
        w.run(
            &[],
            json!({"node_id": "x-9", "pr_number": 1, "project": "p",
                "merged_at": "junk-stamp", "node_sessions": ["u2", "u2", "", "u3"]}),
        );
        let r = &w.read_entries()[0];
        assert_eq!(r["completed"], "junk-stamp", "junk in, junk out");
        assert_eq!(
            r["sessions"],
            json!(["u2", "u3"]),
            "distinct, ordered, no blanks"
        );

        w.run(
            &[],
            json!({"node_id": "x-10", "pr_number": 2, "project": "p",
                "merged_at": "2026-07-18"}),
        );
        let r = &w.read_entries()[1];
        assert_eq!(
            r["completed"], "2026-07-18T00:00:00+00:00",
            "a date reads as UTC midnight"
        );

        w.run(
            &[],
            json!({"node_id": "x-11", "pr_number": 3, "project": "p"}),
        );
        let r = &w.read_entries()[2];
        assert_eq!(r["sessions"], json!(["unresolved:no-harness-session"]));
        assert_eq!(r["completed"], Value::Null);

        // A MISSING ledger reads as empty and the write creates it (the
        // ported `_load_ledger_data` behavior).
        std::fs::remove_file(w.ledger_path()).unwrap();
        w.run(
            &[],
            json!({"node_id": "x-12", "pr_number": 4, "project": "p"}),
        );
        let rows = w.read_entries();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["graph_node_id"], "x-12");
    }
}
