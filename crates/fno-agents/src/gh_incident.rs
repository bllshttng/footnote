//! The GitHub Actions incident probe: a cancelled run whose jobs never got
//! a runner is an infrastructure casualty, not a test failure. During a
//! githubstatus.com Actions incident a red verdict needs the explanation
//! printed beside it, so a lead waits instead of rerunning or filing a
//! main repair. Every live read is fail-open: a probe failure never
//! manufactures an incident and never softens the verdict.

use serde_json::{json, Value};

/// The statuspage API the probe reads. A plain HTTPS GET via curl: the host
/// is not the GitHub API, so `gh api` cannot reach it.
pub(crate) const COMPONENTS_URL: &str = "https://www.githubstatus.com/api/v2/components.json";

/// The job-level casualty shape: every job ended `cancelled` with a runner
/// never assigned (`runner_name` empty) and never started (`started_at`
/// empty). A started job - any `started_at` - is a human or timeout cancel;
/// any other conclusion is a test or parse failure. Both read as causes,
/// never as casualties.
pub(crate) fn cancelled_no_runner(jobs: &[Value]) -> bool {
    !jobs.is_empty()
        && jobs.iter().all(|job| {
            job.get("conclusion").and_then(Value::as_str) == Some("cancelled")
                && field_empty(job, "runner_name")
                && field_empty(job, "started_at")
        })
}

fn field_empty(job: &Value, key: &str) -> bool {
    job.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
}

/// The Actions component's live incident, when githubstatus reports the
/// component anything but operational: `{"status", "since"}` from its own
/// `status` and `updated_at`. The live payload names the state in `status`
/// ("operational" today) and often leaves `indicator` null, so `status`
/// decides and `indicator` only answers when `status` is missing; a
/// schema surprise reads operational, never a manufactured incident. No
/// Actions component or an operational one answers None.
pub(crate) fn actions_incident(components: &Value) -> Option<Value> {
    let actions = components
        .get("components")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("name").and_then(Value::as_str) == Some("Actions"))?;
    let status = actions.get("status").and_then(Value::as_str).unwrap_or("");
    let indicator = actions
        .get("indicator")
        .and_then(Value::as_str)
        .unwrap_or("");
    let operational = if status.is_empty() {
        indicator.is_empty() || indicator == "operational"
    } else {
        status.eq_ignore_ascii_case("operational")
    };
    if operational {
        return None;
    }
    let named = if status.is_empty() { indicator } else { status };
    Some(json!({
        "status": named,
        "since": actions.get("updated_at").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// The incident verdict for one red main-ci run: a `cancel` conclusion whose
/// jobs carry the never-got-a-runner shape, confirmed by an active
/// githubstatus incident. A `failure` conclusion (the jobs ran and failed)
/// is a test failure; a runner-assigned cancel is a human or timeout cause;
/// either, or no live incident, is None and the red verdict stands alone.
pub(crate) fn run_incident(run: &Value, jobs_page: &Value, components: &Value) -> Option<Value> {
    if run.get("conclusion").and_then(Value::as_str) != Some("cancel") {
        return None;
    }
    let jobs = jobs_page.get("jobs").and_then(Value::as_array)?;
    if !cancelled_no_runner(jobs) {
        return None;
    }
    actions_incident(components)
}

/// The live components read: one `curl` child, bounded. None on any
/// failure - an outage of the status site itself is not an Actions
/// incident either.
pub(crate) fn components_page() -> Option<Value> {
    let output = std::process::Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "10",
            COMPONENTS_URL,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&output.stdout).ok()
}

/// The line printed beside a verdict: `GitHub Actions incident: <status>
/// since <time>`.
pub(crate) fn incident_line(incident: &Value) -> String {
    let field = |k: &str| incident.get(k).and_then(Value::as_str).unwrap_or("unknown");
    format!(
        "GitHub Actions incident: {} since {}",
        field("status"),
        field("since")
    )
}

/// The stderr note under the same line: the wait instruction the whole
/// probe exists for.
pub(crate) fn incident_note(incident: &Value) -> String {
    format!(
        "note: {} - the cancelled check(s) never got a runner; wait out the incident instead of rerunning or filing a main repair",
        incident_line(incident)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The discrimination the reader exists for: a cancelled job with no
    /// runner and no start is a casualty; a started job (a manual or timeout
    /// cancel), a failure conclusion (tests ran and failed), a runner_name,
    /// and an empty slice all read as causes. The verdict needs the
    /// githubstatus confirmation too, and renders one line.
    #[test]
    fn a_cancelled_never_started_runnerless_job_is_a_casualty_nothing_else_is() {
        let casualty = serde_json::json!({
            "name": "cargo audit", "status": "completed", "conclusion": "cancelled",
            "runner_name": null, "started_at": null, "completed_at": "2026-10-05T19:48:00Z",
        });
        assert!(cancelled_no_runner(&[casualty.clone()]));
        let ran_then_cancelled = serde_json::json!({
            "name": "cli-ci", "status": "completed", "conclusion": "cancelled",
            "runner_name": null, "started_at": "2026-10-05T19:27:00Z",
        });
        assert!(!cancelled_no_runner(&[ran_then_cancelled]));
        let test_failure = serde_json::json!({
            "name": "cli-ci", "status": "completed", "conclusion": "failure",
            "runner_name": null, "started_at": "2026-10-05T19:27:00Z",
        });
        assert!(!cancelled_no_runner(&[test_failure]));
        assert!(!cancelled_no_runner(&[]));

        let run = serde_json::json!({"conclusion": "cancel"});
        let page = serde_json::json!({"total_count": 1, "jobs": [casualty]});
        // The live payload's own shape: `status` names the state,
        // `indicator` stays null.
        let degraded = serde_json::json!({"components": [
            {"name": "API", "indicator": null, "status": "operational"},
            {"name": "Actions", "indicator": null,
             "status": "Degraded Performance", "updated_at": "2026-10-05T19:11:58Z"},
        ]});
        let incident = run_incident(&run, &page, &degraded).expect("incident shape confirmed");
        assert_eq!(incident["status"], json!("Degraded Performance"));
        assert_eq!(incident["since"], json!("2026-10-05T19:11:58Z"));
        assert_eq!(
            incident_line(&incident),
            "GitHub Actions incident: Degraded Performance since 2026-10-05T19:11:58Z"
        );
        // The older Statuspage shape names the state in `indicator` and the
        // probe still reads it when `status` goes missing.
        let indicator_shaped = serde_json::json!({"components": [
            {"name": "Actions", "indicator": "degraded_performance", "status": "",
             "updated_at": "2026-10-05T19:11:58Z"},
        ]});
        let incident = actions_incident(&indicator_shaped).expect("indicator fallback");
        assert_eq!(incident["status"], json!("degraded_performance"));

        // A failure conclusion is a test failure even mid-incident.
        let failed_run = serde_json::json!({"conclusion": "failure"});
        assert_eq!(run_incident(&failed_run, &page, &degraded), None);
        // A casualty during an operational window explains nothing. The
        // live operational payload carries a lowercase word and a null
        // indicator; both read operational.
        let operational = serde_json::json!({"components": [
            {"name": "Actions", "indicator": null, "status": "operational"},
        ]});
        let casualty_run = serde_json::json!({"conclusion": "cancel"});
        assert_eq!(run_incident(&casualty_run, &page, &operational), None);
        assert_eq!(actions_incident(&operational), None);
        assert_eq!(actions_incident(&json!({"components": []})), None);
    }
}
