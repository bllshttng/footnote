//! The Actions casualty shape and its render line: a cancelled run whose
//! jobs never got a runner is an infrastructure casualty, not a test
//! failure. The status-page confirmation itself is
//! [`crate::pr_status::seams::platform_incident`], the one landed parser
//! both surfaces share.

use serde_json::Value;

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

/// The line printed beside a verdict, from the first live incident row the
/// shared status-page seam returned: `GitHub Actions incident: <status>
/// since <time>`.
pub(crate) fn incident_line(incidents: &Value) -> String {
    let first = incidents.get(0);
    let field = |k: &str| {
        first
            .and_then(|row| row.get(k))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    };
    format!(
        "GitHub Actions incident: {} since {}",
        field("status"),
        field("updated_at")
    )
}

/// The stderr note under the same line: the wait instruction the whole
/// probe exists for.
pub(crate) fn incident_note(incidents: &Value) -> String {
    format!(
        "note: {} - the cancelled check(s) coincide with the outage; wait it out instead of rerunning or filing a main repair",
        incident_line(incidents)
    )
}
