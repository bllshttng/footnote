//! Contract-dependent reconciliation and its cross-process pending sentinel.
use super::advance::{self, AdvanceResult};
use serde_json::{json, Value};
use std::path::Path;

fn result(
    node: &str,
    reason: &str,
    detail: Option<String>,
    events: Option<&Path>,
) -> AdvanceResult {
    advance::advance_emit(
        advance::EVENT_SKIPPED,
        json!({"node_id":node,"kind":"reconcile","reason":reason,"detail":detail}),
        events,
    );
    let mut r = AdvanceResult::new(
        "skipped",
        advance::EVENT_SKIPPED,
        Some(reason.into()),
        Some(node.into()),
    );
    r.detail = detail;
    r
}
fn dispatch(dep: &Value, root: &Path, manifest: &Path, events: Option<&Path>) -> AdvanceResult {
    let id = dep["id"].as_str().unwrap_or("");
    let root_text = root.to_string_lossy();
    if let Some(reason) =
        advance::node_dispatch_block_reason(id, Some(&root_text), None, None, events)
    {
        return result(id, &reason, None, events);
    }
    let key = format!("dispatch:{id}");
    let holder = format!("reconcile:{}", std::process::id());
    match crate::claims::acquire(
        &key,
        &holder,
        crate::claims::AcquireOpts {
            ttl_ms: Some(180_000),
            reason: Some(format!("reconcile dispatch for {id}")),
            ..Default::default()
        },
    ) {
        crate::claims::AcquireOutcome::Acquired(_) => {}
        crate::claims::AcquireOutcome::HeldByOther { .. } => {
            return result(id, "already-claimed", None, events)
        }
        crate::claims::AcquireOutcome::Error(e) => {
            return result(id, "claim-error", Some(e), events)
        }
    }
    let spawned = super::advance_dispatch::resolve_reconcile_spawn(dep, &root_text, manifest)
        .and_then(|args| {
            super::advance_dispatch::launch_resolved(
                id,
                args,
                Some((&key, &holder)),
                "reconcile_dispatch",
                Some("rd"),
                events,
            )
        });
    match spawned {
        Ok((identity, row)) => {
            advance::advance_emit(
                advance::EVENT_DISPATCHED,
                json!({"node_id":id,"short_id":identity,"kind":"reconcile","agent_name":row["agent_name"],"notes":row["notes"]}),
                events,
            );
            let mut r = AdvanceResult::new(
                "dispatched",
                advance::EVENT_DISPATCHED,
                None,
                Some(id.into()),
            );
            r.short_id = Some(identity);
            r.notes = row["notes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            r
        }
        Err(super::advance_dispatch::SpawnOutcome::AlreadyRunning(_)) => {
            advance::safe_release(&key, &holder);
            result(id, "already-claimed", None, events)
        }
        Err(super::advance_dispatch::SpawnOutcome::Failed(e)) => {
            advance::safe_release(&key, &holder);
            if e.kind == super::advance::SpawnFailureKind::AlreadyRunning {
                return result(id, "already-claimed", None, events);
            }
            if let Some(refusal) = advance::gate_refusal(&e) {
                let mut r = result(id, &refusal.reason, Some(refusal.detail), events);
                r.exit_code = Some(refusal.exit_code);
                r
            } else {
                advance::advance_emit(
                    advance::EVENT_FAILED,
                    json!({"node_id":id,"kind":"reconcile","error":e.detail}),
                    events,
                );
                let mut r = AdvanceResult::new(
                    "failed",
                    advance::EVENT_FAILED,
                    Some("spawn-failed".into()),
                    Some(id.into()),
                );
                r.detail = Some(if e.detail.is_empty() {
                    e.message
                } else {
                    e.detail
                });
                r
            }
        }
    }
}
pub fn for_blocker(closed: &str) -> Vec<AdvanceResult> {
    if !advance::auto_continue_enabled(None)
        || advance::walker_key().is_some_and(|key| advance::claim_is_live(&key))
    {
        return Vec::new();
    }
    let events = advance::advance_events_path(None);
    let rows = match crate::graph_store::read_rows_strict(&super::settings::graph_path()) {
        Ok(r) => r,
        Err(e) => {
            return vec![result(
                closed,
                "dependents-error",
                Some(e.to_string()),
                events.as_deref(),
            )]
        }
    };
    rows.iter()
        .filter(|r| {
            r["dep"] == "contract"
                && r["blocked_by"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(closed)))
        })
        .map(|dep| {
            let id = dep["id"].as_str().unwrap_or("");
            let root =
                super::triage::project_root_from_settings(dep["project"].as_str()).or_else(|| {
                    dep["cwd"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
            let Some(root) = root else {
                return result(id, "unmapped-project", None, events.as_deref());
            };
            let root = Path::new(&root);
            let manifest = root.join(".fno").join(format!("stub-manifest-{id}.json"));
            if !manifest.exists() {
                let key = format!("reconcile:{id}");
                match crate::claims::acquire(
                    &key,
                    &format!("reconcile-pending:{id}"),
                    crate::claims::AcquireOpts {
                        ttl_ms: Some(6 * 60 * 60 * 1000),
                        reason: Some(format!("reconcile pending: {id} manifest not yet written")),
                        ..Default::default()
                    },
                ) {
                    crate::claims::AcquireOutcome::Error(e) => {
                        result(id, "sentinel-error", Some(e), events.as_deref())
                    }
                    _ => result(
                        id,
                        "reconcile-pending",
                        Some("manifest not yet written".into()),
                        events.as_deref(),
                    ),
                }
            } else if std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .is_some_and(|v| v["reconciled"] == true)
            {
                result(id, "already-reconciled", None, events.as_deref())
            } else {
                dispatch(dep, root, &manifest, events.as_deref())
            }
        })
        .collect()
}
pub fn fire_pending(id: &str, root: &Path) -> Option<Value> {
    let key = format!("reconcile:{id}");
    if !advance::claim_is_live(&key) {
        return None;
    }
    let rows =
        crate::graph_store::read_rows_strict(&super::settings::graph_path()).unwrap_or_default();
    let fallback = json!({"id":id,"cwd":root,"dispatch_verb":"/target"});
    let dep = rows.iter().find(|r| r["id"] == id).unwrap_or(&fallback);
    let events = advance::advance_events_path(Some(root));
    let manifest = root.join(".fno").join(format!("stub-manifest-{id}.json"));
    let r = dispatch(dep, root, &manifest, events.as_deref());
    if r.decision == "dispatched" || r.reason.as_deref() == Some("already-claimed") {
        advance::safe_release(&key, &format!("reconcile-pending:{id}"));
    }
    Some(
        json!({"decision":r.decision,"reason":r.reason,"node_id":r.node_id,"short_id":r.short_id,"detail":r.detail,"notes":r.notes}),
    )
}

pub fn queue_pending(id: &str, root: &Path) -> Result<Option<Value>, String> {
    if !advance::claim_is_live(&format!("reconcile:{id}")) {
        return Ok(None);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(exe)
        .args(["backlog", "advance", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| e.to_string())?;
    let request = json!({"op":"fire-pending-now","node_id":id,"root":root});
    use std::io::Write;
    let written = child
        .stdin
        .take()
        .ok_or("pending reconcile child has no input pipe")?
        .write_all(request.to_string().as_bytes());
    if let Err(e) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.to_string());
    }
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(Some(json!({"decision":"scheduled","node_id":id,"pid":pid})))
}
