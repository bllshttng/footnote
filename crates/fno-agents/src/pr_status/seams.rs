//! The status payload's live seams, ported from the Python legs the composer
//! retires: rerun recovery, merge authority, the dispatch-hold word, and the
//! durable-grant execution projection. A seam with no Rust owner yet spawns
//! the same verb its Python twin spawned, so behavior is identical and each
//! is its own future port leg.

use super::GhProbe;
use serde_json::{json, Value};
use std::path::Path;

/// Conclusions that count as a real failed attempt; CANCELLED stays out: a
/// taken-away run is not a concluded failure.
const RERUN_FAIL_CONCLUSIONS: [&str; 3] = ["failure", "timed_out", "startup_failure"];

/// One status read must not become a hundred gh calls.
const RERUN_MAX_RUNS: usize = 20;

fn no_recovery() -> Value {
    json!({"recovered": false, "failed": []})
}

/// Pure over pre-computed rows (no gh). Recovery = latest attempt passed, an
/// earlier attempt failed; `failed` names jobs, optional diagnostics.
fn recovery_from_run_rows(
    run_rows: &[Value],
    attempts_of: &dyn Fn(&str) -> Vec<Value>,
    failed_jobs_of: &dyn Fn(&str, i64) -> Vec<String>,
) -> Value {
    let mut recovered = false;
    let mut failed: Vec<String> = Vec::new();
    for row in run_rows {
        // Only a run that now passes can have recovered.
        if row.get("conclusion").and_then(Value::as_str).unwrap_or("") != "success" {
            continue;
        }
        let latest = row.get("run_attempt").and_then(Value::as_i64).unwrap_or(1);
        // A first-attempt pass never failed.
        if latest <= 1 {
            continue;
        }
        let Some(run_id) = row
            .get("id")
            .map(|v| v.to_string().trim_matches('"').to_string())
        else {
            continue;
        };
        if run_id.is_empty() || run_id == "null" {
            continue;
        }
        for attempt in attempts_of(&run_id) {
            let n = attempt
                .get("run_attempt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let conclusion = attempt
                .get("conclusion")
                .and_then(Value::as_str)
                .unwrap_or("");
            if n > 0 && n < latest && RERUN_FAIL_CONCLUSIONS.contains(&conclusion) {
                recovered = true;
                failed.extend(failed_jobs_of(&run_id, n));
            }
        }
    }
    json!({"recovered": recovered, "failed": failed})
}

fn gh_get_json<P: GhProbe>(probe: &P, cwd: &Path, path: &str) -> Option<Value> {
    let args = vec!["api".to_string(), path.to_string()];
    let (ok, stdout, _stderr) = probe.run_gh(cwd, &args).ok()?;
    if !ok {
        return None;
    }
    serde_json::from_str::<Value>(&stdout).ok()
}

/// Rerun-recovery fact for a PR head: `{recovered, failed}`. A re-run-
/// recovered failure reads green to the verdict; this names it. ANY read
/// error fails open: a fact beside the verdict, never a second red. `runs`
/// is the head's actions/runs listing when the caller (the reader) already
/// read it; None keeps a live read.
pub(crate) fn rerun_recovery<P: GhProbe>(
    probe: &P,
    cwd: &Path,
    slug: &str,
    sha: &str,
    runs: Option<&[Value]>,
) -> Value {
    if sha.is_empty() || slug.is_empty() {
        return no_recovery();
    }
    let owned_rows: Vec<Value>;
    let rows: &[Value] = match runs {
        Some(rows) => rows,
        None => {
            let listing = gh_get_json(
                probe,
                cwd,
                &format!("repos/{slug}/actions/runs?head_sha={sha}&per_page=100"),
            );
            owned_rows = listing
                .and_then(|v| v.get("workflow_runs").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            &owned_rows
        }
    };
    if rows.is_empty() {
        return no_recovery();
    }
    let attempts_of = |run_id: &str| -> Vec<Value> {
        gh_get_json(
            probe,
            cwd,
            &format!("repos/{slug}/actions/runs/{run_id}/attempts?per_page=100"),
        )
        .and_then(|v| v.get("workflow_runs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
    };
    let failed_jobs_of = |run_id: &str, attempt: i64| -> Vec<String> {
        gh_get_json(
            probe,
            cwd,
            &format!("repos/{slug}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"),
        )
        .map(|v| v.get("jobs").cloned().unwrap_or(v))
        .and_then(|jobs| {
            jobs.as_array().map(|rows| {
                rows.iter()
                    .filter(|j| {
                        !j.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .is_empty()
                            && RERUN_FAIL_CONCLUSIONS.contains(
                                &j.get("conclusion").and_then(Value::as_str).unwrap_or(""),
                            )
                    })
                    .filter_map(|j| j.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
    };
    let capped = &rows[..rows.len().min(RERUN_MAX_RUNS)];
    recovery_from_run_rows(capped, &attempts_of, &failed_jobs_of)
}

/// The GitHub platform-incident fact for a cancelled-only take-away: the
/// unresolved status-page incidents whose name or updates name Actions. A
/// reader that sees this holds reruns instead of burning attempts against a
/// degraded platform (2026-10-05: three sessions reran killed jobs for hours
/// during a critical Actions incident the surface could not name). Read-only
/// and fail-open: an unreachable page reads null, never a second red.
pub(crate) fn platform_incident() -> Value {
    let out = std::process::Command::new("curl")
        .args([
            "-sf",
            "--max-time",
            "5",
            "https://www.githubstatus.com/api/v2/incidents.json",
        ])
        .output();
    let Ok(out) = out else {
        return Value::Null;
    };
    if !out.status.success() {
        return Value::Null;
    }
    incidents_from_body(&String::from_utf8_lossy(&out.stdout))
}

/// Pure over a status-page incidents body: the unresolved incidents whose
/// name or any update body mentions Actions. The feed carries component ids,
/// not names, so the words are the portable signal. Resolved and postmortem
/// rows are history, not live. Empty or unreadable reads null.
pub(crate) fn incidents_from_body(body: &str) -> Value {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return Value::Null;
    };
    let Some(incidents) = parsed.get("incidents").and_then(Value::as_array) else {
        return Value::Null;
    };
    let mut live: Vec<Value> = Vec::new();
    for incident in incidents {
        let status = incident.get("status").and_then(Value::as_str).unwrap_or("");
        if matches!(status, "" | "resolved" | "postmortem" | "completed") {
            continue;
        }
        let name = incident.get("name").and_then(Value::as_str).unwrap_or("");
        let updates = incident.get("incident_updates").and_then(Value::as_array);
        let mentions_actions = name.to_lowercase().contains("action")
            || updates.is_some_and(|rows| {
                rows.iter().any(|u| {
                    u.get("body")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_lowercase()
                        .contains("action")
                })
            });
        if !mentions_actions {
            continue;
        }
        let updated_at = updates
            .and_then(|rows| rows.first())
            .and_then(|u| u.get("created_at"))
            .cloned()
            .unwrap_or(Value::Null);
        live.push(json!({
            "name": name,
            "status": status,
            "created_at": incident.get("created_at").cloned().unwrap_or(Value::Null),
            "updated_at": updated_at,
        }));
        if live.len() >= 3 {
            break;
        }
    }
    if live.is_empty() {
        Value::Null
    } else {
        Value::Array(live)
    }
}

/// The resolved merge-authority axes. Both keys fail-open to null on an
/// unreadable settings load: a receipt that cannot read config says so
/// rather than asserting "disabled" - a guessed NO is the direction a wedged
/// fleet reads as a disarm, and a guessed YES is the dangerous one.
pub(crate) fn merge_authority(cwd: &Path) -> Value {
    json!({
        "config_auto_merge_enabled": crate::agents_config::auto_merge_enabled(cwd),
        "grant": crate::agents_config::auto_merge_grant_dispatches(cwd),
    })
}

/// The dispatch-hold probe's full tri-state: the exit codes distinguish a
/// clean probe from a crashed one, and a preview that reuses the answer must
/// never read a crashed probe as clear.
pub(crate) enum HoldVerdict {
    Clear,
    Held(String),
    Unreadable,
}

/// The PR's dispatch-hold word through the same probe the merge path reads.
/// The binding facts ride the caller's own PR read when it has one, so the
/// common path runs in process; a read without the fields (a projection
/// older than the facts keys) falls back to the spawned verb.
pub(crate) fn hold_verdict(cwd: &Path, pr: u64, pr_json: Option<&Value>) -> HoldVerdict {
    if let Some(payload) = pr_json {
        if let Ok(facts) = crate::authorized_merge::facts_from_pulls(payload, Some(pr)) {
            return match crate::gate_probes::dispatch_hold(cwd, &facts) {
                crate::authorized_merge::ProbeOutcome::Clear => HoldVerdict::Clear,
                crate::authorized_merge::ProbeOutcome::Refused(r) => HoldVerdict::Held(r),
                crate::authorized_merge::ProbeOutcome::Inconclusive(_) => HoldVerdict::Unreadable,
            };
        }
    }
    let out = match std::process::Command::new(crate::scrape::fno_bin())
        .args(["do", "pr", "hold-check", &pr.to_string()])
        .current_dir(cwd)
        .output()
    {
        Ok(out) => out,
        Err(_) => return HoldVerdict::Unreadable,
    };
    match out.status.code() {
        Some(0) => HoldVerdict::Clear,
        Some(3) => HoldVerdict::Held(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        _ => HoldVerdict::Unreadable,
    }
}

/// The durable-grant execution state through the ONE resolver, in process:
/// whether a parked granted worker's PR would be merged by the watcher now.
/// A projection only - it never widens the merge verb's own gates. A receipt
/// never lies by crashing: every fault reads `unknown`.
pub(crate) fn merge_execution(cwd: &Path, pr: u64) -> Value {
    let payload = json!({
        "op": "grant-verdict",
        "cwd": cwd.to_string_lossy(),
        "pr": pr,
    });
    let out = crate::merge_grant::run_op("grant-verdict", &payload);
    serde_json::from_str(&out).unwrap_or_else(|_| {
        json!({
            "state": "unknown",
            "reason": "durable-grant resolve failed: unparseable receipt",
            "node_id": null,
            "claim_state": null,
        })
    })
}
