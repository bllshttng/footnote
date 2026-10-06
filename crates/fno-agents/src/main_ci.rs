//! main's CI verdict: the workflow-run reduction the lead check-in renders,
//! the same token the merge gate (`authorized_merge`) reads behind a short
//! TTL row cache, and the red-run shape accessor both share.

use serde_json::{json, Value};
use std::path::Path;

/// Per-workflow verdict inputs: (name, newest run, newest completed run).
/// Row position is never trusted - the branch listing has served stale rows
/// first - `created_at` alone picks the newest of each.
fn fold_page<'a>(
    acc: &mut Vec<(&'a str, &'a Value, Option<&'a Value>)>,
    runs: impl Iterator<Item = &'a Value>,
) {
    for run in runs {
        let name = run.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let created = run.get("created_at").and_then(Value::as_str).unwrap_or("");
        let completed = run.get("status").and_then(Value::as_str) == Some("completed");
        match acc.iter_mut().find(|(seen, _, _)| *seen == name) {
            None => acc.push((name, run, completed.then_some(run))),
            Some((_, newest, newest_completed)) => {
                if created
                    > newest
                        .get("created_at")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                {
                    *newest = run;
                }
                if completed {
                    let is_newest = match *newest_completed {
                        Some(c) => {
                            created > c.get("created_at").and_then(Value::as_str).unwrap_or("")
                        }
                        None => true,
                    };
                    if is_newest {
                        *newest_completed = Some(run);
                    }
                }
            }
        }
    }
}

/// How far back the check-in reports an old-commit failure: past this the
/// failure is archaeology, not a live signal, and the un-paginated branch
/// page would bury it anyway.
const STALE_REPORT_WINDOW_DAYS: i64 = 14;

/// One verdict token for the check-in line, from the workflow-run history on
/// main, judged at the branch head only: the verdict reduces rows whose
/// head sha equals `head_sha` - `at_sha` carries those push runs, and the
/// branch page is filtered to the head and to push-event rows, so only push
/// CI at the head sets the verdict: a schedule-event nightly release failing
/// at the head never reads main red while the head's push CI is green. A run
/// on an OLDER commit never sets the verdict, however fresh the workflow: a
/// release run that failed or was
/// cancelled days ago on an old sha must not read main red while the head
/// is clean. Such failures ride the token's `stale` list instead and the
/// check-in names each on its own line with its age. Per workflow with head
/// rows the newest COMPLETED run decides - red on fail or cancel, naming
/// the workflow and the sha it ran on; a workflow whose newest head run is
/// still in flight reads pending; no head rows at all reads pending, never
/// green. The stale list reports within a bounded window (`now` minus
/// [`STALE_REPORT_WINDOW_DAYS`]): the branch page is one un-paginated read,
/// so an unbounded stale list would silently expire at the 100-row page
/// bound instead of at a named age.
fn main_ci_token_from_pages<'a>(
    head_sha: &str,
    at_sha: &'a [Value],
    on_main: &'a [Value],
    now: chrono::DateTime<chrono::Utc>,
) -> (Value, Option<&'a Value>) {
    let at_head = |run: &Value| run.get("head_sha").and_then(Value::as_str) == Some(head_sha);
    // The verdict is main's push CI at the head. A schedule-event nightly
    // release failing at the head is a release-engineering concern, never
    // main red while the head's push CI is green; only rows the API lists as
    // push events fold into the verdict.
    let is_push = |run: &Value| run.get("event").and_then(Value::as_str) == Some("push");
    let mut head: Vec<(&'a str, &'a Value, Option<&'a Value>)> = Vec::new();
    fold_page(&mut head, at_sha.iter());
    fold_page(
        &mut head,
        on_main.iter().filter(|run| at_head(run) && is_push(run)),
    );
    let mut older: Vec<(&'a str, &'a Value, Option<&'a Value>)> = Vec::new();
    fold_page(&mut older, on_main.iter().filter(|run| !at_head(run)));
    let window_start = now - chrono::Duration::days(STALE_REPORT_WINDOW_DAYS);
    let stale: Vec<Value> = older
        .iter()
        .filter_map(|(_, _, completed)| *completed)
        .filter(|run| matches!(crate::pr_push::rest_bucket(run), "fail" | "cancel"))
        .filter(|run| {
            run.get("created_at")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|ts| ts.with_timezone(&chrono::Utc) >= window_start)
                .unwrap_or(true)
        })
        .map(|run| {
            json!({
                "workflow": run.get("name").and_then(Value::as_str).unwrap_or("unknown"),
                "sha": run.get("head_sha").and_then(Value::as_str).unwrap_or("unknown"),
                "created_at": run.get("created_at").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();
    let red = head
        .iter()
        .filter_map(|(_, _, completed)| *completed)
        .filter(|run| matches!(crate::pr_push::rest_bucket(run), "fail" | "cancel"))
        .max_by_key(|run| run.get("created_at").and_then(Value::as_str).unwrap_or(""));
    let mut token = match red {
        Some(run) => json!({
            "verdict": "red",
            "workflow": run.get("name").and_then(Value::as_str).unwrap_or("unknown"),
            "sha": run.get("head_sha").and_then(Value::as_str).unwrap_or("unknown"),
        }),
        None => {
            let inflight = head.iter().any(|(_, newest, _)| {
                newest.get("status").and_then(Value::as_str) != Some("completed")
            });
            json!({ "verdict": if head.is_empty() || inflight { "pending" } else { "green" } })
        }
    };
    if !stale.is_empty() {
        token
            .as_object_mut()
            .expect("token is built as an object")
            .insert("stale".into(), Value::Array(stale));
    }
    (token, red)
}

/// The incident attachment: a red verdict whose run ended `cancel` reads the
/// run's jobs for the never-got-a-runner shape, then confirms against the
/// shared status-page seam before the explanation rides the token. Every
/// read is fail-open - a jobs or status-site failure leaves the token
/// exactly as the fold built it.
fn attach_incident(cwd: &Path, token: &mut Value, red: Option<&Value>) {
    let Some(run) = red else {
        return;
    };
    if run.get("conclusion").and_then(Value::as_str) != Some("cancel") {
        return;
    }
    let Some(id) = run.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Ok(raw) = crate::pr_push::gh_api(
        "gh",
        cwd,
        &format!("repos/{{owner}}/{{repo}}/actions/runs/{id}/jobs?per_page=100"),
        &[],
    ) else {
        return;
    };
    let Ok(jobs_page) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    let jobs = jobs_page.get("jobs").and_then(Value::as_array);
    let Some(true) = jobs.map(|jobs| crate::gh_incident::cancelled_no_runner(jobs)) else {
        return;
    };
    let incidents = crate::pr_status::seams::platform_incident();
    if incidents.is_null() {
        return;
    }
    token
        .as_object_mut()
        .expect("token is built as an object")
        .insert("platform_incident".into(), incidents);
}

/// The `main ci:` line body: a red verdict names the workflow and sha, a
/// plain verdict is the word alone, and legacy cached string tokens pass
/// through untouched.
pub(crate) fn main_ci_render(v: Option<&Value>) -> String {
    match v {
        Some(Value::Object(o)) => {
            let field = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("unknown");
            let base = match o.get("workflow") {
                Some(_) => format!(
                    "{} ({} at {})",
                    field("verdict"),
                    field("workflow"),
                    field("sha")
                ),
                None => field("verdict").to_string(),
            };
            match o.get("platform_incident") {
                Some(incidents) => {
                    format!("{base} {}", crate::gh_incident::incident_line(incidents))
                }
                None => base,
            }
        }
        other => crate::lead_checkin::dash(other),
    }
}

/// The check-in's stale-failure lines: one per old-commit failure the token
/// carries, each naming the workflow, the sha and the run's age. Legacy
/// string tokens and tokens without a stale list emit nothing.
pub(crate) fn main_ci_stale_lines(
    v: Option<&Value>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<String> {
    let Some(stale) = v.and_then(|t| t.get("stale")).and_then(Value::as_array) else {
        return Vec::new();
    };
    stale
        .iter()
        .map(|run| {
            let field = |k: &str| run.get(k).and_then(Value::as_str).unwrap_or("unknown");
            let age = run
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|ts| {
                    let hours = (now - ts.with_timezone(&chrono::Utc)).num_hours();
                    if hours >= 48 {
                        format!("{}d old", hours / 24)
                    } else if hours >= 1 {
                        format!("{}h old", hours)
                    } else {
                        "<1h old".to_string()
                    }
                })
                .unwrap_or_else(|| "age unknown".to_string());
            format!(
                "main ci stale: {} at {} ({})",
                field("workflow"),
                field("sha"),
                age
            )
        })
        .collect()
}

pub(crate) fn r_main_ci() -> Result<Value, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    main_ci_reading(&cwd)
}

/// The live reading for any cwd: the lead check-in renders it per beat, and
/// the merge gate reads it behind the TTL cache below.
pub(crate) fn main_ci_reading(cwd: &Path) -> Result<Value, String> {
    // Judge at main's current head: the verdict reduces only rows whose head
    // sha equals the branch head, so the head's own push runs are read
    // directly, the branch page contributes its head-sha push-event rows
    // only, and its older rows only ever feed the stale
    // list, never the verdict.
    let head_raw = crate::pr_push::gh_api("gh", &cwd, "repos/{owner}/{repo}/branches/main", &[])
        .map_err(|error| {
            format!(
                "gh api failed: {}",
                crate::lead_checkin::gh_error_cause(&error)
            )
        })?;
    let head_sha = serde_json::from_str::<Value>(&head_raw)
        .ok()
        .and_then(|page| {
            page.pointer("/commit/sha")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| "gh api branches/main returned no head sha".to_string())?;
    // One un-paginated page per read: the listings grow with the repo's age,
    // and a paginated read would walk the whole history on every beat. The
    // branch page's head-sha rows join the verdict fold; its older rows feed
    // the stale list only.
    let read_runs = |query: &str| -> Result<Vec<Value>, String> {
        let raw = crate::pr_push::gh_api("gh", &cwd, query, &[]).map_err(|error| {
            format!(
                "gh api failed: {}",
                crate::lead_checkin::gh_error_cause(&error)
            )
        })?;
        let runs = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|page| page.get("workflow_runs").and_then(Value::as_array).cloned())
            .unwrap_or_default();
        Ok(runs)
    };
    let at_sha = read_runs(&format!(
        "repos/{{owner}}/{{repo}}/actions/runs?head_sha={head_sha}&event=push&per_page=100"
    ))?;
    let mut on_main = read_runs("repos/{owner}/{repo}/actions/runs?branch=main&per_page=100")?;
    // The verdict only needs the head rows, but the stale report owes the
    // whole window: when the newest 100 rows end inside it, a burst has
    // pushed in-window failures onto page 2. One extra page, and no more -
    // a burst past two pages outruns the stale report, never the verdict.
    let window_start = chrono::Utc::now() - chrono::Duration::days(STALE_REPORT_WINDOW_DAYS);
    let covers_window = on_main
        .iter()
        .filter_map(|run| {
            run.get("created_at")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        })
        .min()
        .map(|oldest| oldest.with_timezone(&chrono::Utc) <= window_start)
        .unwrap_or(true);
    if !covers_window {
        on_main.extend(read_runs(
            "repos/{owner}/{repo}/actions/runs?branch=main&per_page=100&page=2",
        )?);
    }
    Ok({
        let (mut token, red) =
            main_ci_token_from_pages(&head_sha, &at_sha, &on_main, chrono::Utc::now());
        attach_incident(cwd, &mut token, red);
        token
    })
}

/// The one red run a main-ci token names, as (workflow, head sha). The word
/// tokens (`green`, `pending`) and any other shape are not red: a pending
/// main is not red. Lives beside the token's producer so the shape is read
/// in one module.
pub(crate) fn main_ci_red_run(token: &Value) -> Option<(String, String)> {
    if token.get("verdict").and_then(Value::as_str) != Some("red") {
        return None;
    }
    let field = |k: &str| token.get(k).and_then(Value::as_str).unwrap_or("unknown");
    Some((field("workflow").to_string(), field("sha").to_string()))
}

/// How long a cached main verdict answers before the live read runs again.
const MAIN_CI_CACHE_TTL_S: u64 = 60;

/// [`main_ci_reading`] behind the gh-facts row store, TTL'd: the merge gate
/// and the preview walk read main's verdict on every pass (every status
/// receipt included), far more often than the verdict changes. A stale,
/// missing or corrupt row falls through to the live read, which stays the
/// truth; a failed live read is an `Err`, never a manufactured red.
pub(crate) fn main_ci_reading_cached(cwd: &Path) -> Result<Value, String> {
    let root = crate::gh_cache::rows_root(cwd);
    let slug = crate::finalize::slug_from_git_remote(cwd);
    let (Some(root), Some(slug)) = (root, slug) else {
        return main_ci_reading(cwd);
    };
    let fresh = crate::gh_cache::read_row(
        &root,
        "main-ci",
        &slug,
        None,
        Some(MAIN_CI_CACHE_TTL_S),
        crate::gh_cache::now_secs(),
    );
    if let Some(token) = fresh.get("row").and_then(|r| r.get("token")) {
        return Ok(token.clone());
    }
    let token = main_ci_reading(cwd)?;
    // The token rides an object row because the row store only holds
    // objects; the bare `green`/`pending` words must cache too, or the
    // common case pays the live read on every pass.
    crate::gh_cache::write_row(
        &root,
        "main-ci",
        &slug,
        None,
        &json!({ "token": token.clone() }),
    );
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workflow-run rows an `/actions/runs` page carries, in the fields
    /// the reducer reads; row order is free.
    fn wf_run(name: &str, sha: &str, status: &str, conclusion: &str, created: &str) -> Value {
        wf_run_event(name, sha, status, conclusion, created, "push")
    }

    /// The same row with an explicit trigger event: push rows set the
    /// verdict, anything else (the nightly's `schedule`) never does.
    fn wf_run_event(
        name: &str,
        sha: &str,
        status: &str,
        conclusion: &str,
        created: &str,
        event: &str,
    ) -> Value {
        serde_json::json!({
            "name": name, "head_sha": sha, "status": status,
            "conclusion": conclusion, "created_at": created, "event": event,
        })
    }

    /// The reducer at the tests' fixed clock, 2026-09-30T12:00:00Z, so the
    /// stale window's boundary never drifts under a hardcoded date.
    fn token_at(head_sha: &str, at_sha: &[Value], on_main: &[Value]) -> Value {
        main_ci_token_from_pages(
            head_sha,
            at_sha,
            on_main,
            chrono::DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        )
        .0
    }

    /// An old-commit failure - push or a dispatch-only nightly the head
    /// never fires - never sets the verdict: a clean head reads the plain
    /// green word while the failure rides the stale list with its age, and
    /// past the report window the failure is archaeology and rides no list.
    #[test]
    fn an_old_commit_failure_rides_the_stale_list_and_never_sets_the_verdict() {
        let at_sha = vec![
            wf_run(
                "guards",
                "b2",
                "completed",
                "success",
                "2026-09-26T04:00:00Z",
            ),
            wf_run(
                "rust-ci",
                "b2",
                "completed",
                "success",
                "2026-09-26T04:00:00Z",
            ),
        ];
        let on_main = vec![wf_run(
            "cli-ci",
            "a1",
            "completed",
            "failure",
            "2026-09-26T03:35:00Z",
        )];
        // A clean head with no older failure is the plain word: every head
        // workflow's newest completed run passed.
        assert_eq!(
            token_at("b2", &at_sha, &[]),
            serde_json::json!({"verdict": "green"})
        );
        assert_eq!(
            token_at("b2", &at_sha, &on_main),
            serde_json::json!({
                "verdict": "green",
                "stale": [
                    {"workflow": "cli-ci", "sha": "a1", "created_at": "2026-09-26T03:35:00Z"}
                ]
            })
        );
        // A cancelled dispatch-only release on an older commit rides the
        // stale list the same way.
        let head_rows = vec![wf_run(
            "guards",
            "head",
            "completed",
            "success",
            "2026-09-28T08:00:00Z",
        )];
        let cancelled = vec![
            wf_run(
                "guards",
                "head",
                "completed",
                "success",
                "2026-09-28T08:00:00Z",
            ),
            wf_run_event(
                "release",
                "oldsha",
                "completed",
                "cancelled",
                "2026-09-26T03:35:00Z",
                "workflow_dispatch",
            ),
        ];
        assert_eq!(
            token_at("head", &head_rows, &cancelled),
            serde_json::json!({
                "verdict": "green",
                "stale": [
                    {"workflow": "release", "sha": "oldsha", "created_at": "2026-09-26T03:35:00Z"}
                ]
            })
        );
        // Past the report window the failure is archaeology: it rides no
        // list and the token is the plain word.
        let ancient = vec![
            wf_run(
                "guards",
                "head",
                "completed",
                "success",
                "2026-09-28T08:00:00Z",
            ),
            wf_run(
                "release",
                "oldsha",
                "completed",
                "cancelled",
                "2026-08-01T03:35:00Z",
            ),
        ];
        assert_eq!(
            token_at("head", &head_rows, &ancient),
            serde_json::json!({"verdict": "green"})
        );
    }

    /// A workflow whose newest head run is still in flight reads pending -
    /// never green, and with no head rows at all too - while an old-commit
    /// failure only rides the stale list, never the verdict.
    #[test]
    fn main_ci_reads_pending_when_a_workflows_newest_run_is_in_flight() {
        let runs = vec![
            wf_run(
                "guards",
                "b2",
                "completed",
                "success",
                "2026-09-26T04:00:00Z",
            ),
            wf_run("cli-ci", "b2", "in_progress", "", "2026-09-12T04:00:00Z"),
        ];
        assert_eq!(
            token_at("b2", &runs, &[]),
            serde_json::json!({"verdict": "pending"})
        );
        assert_eq!(
            token_at("b2", &[], &[]),
            serde_json::json!({"verdict": "pending"})
        );
        let at_sha = vec![wf_run(
            "cli-ci",
            "b3",
            "in_progress",
            "",
            "2026-09-26T04:10:00Z",
        )];
        let on_main = vec![wf_run(
            "cli-ci",
            "a1",
            "completed",
            "failure",
            "2026-09-26T03:35:00Z",
        )];
        assert_eq!(
            token_at("b3", &at_sha, &on_main),
            serde_json::json!({
                "verdict": "pending",
                "stale": [
                    {"workflow": "cli-ci", "sha": "a1", "created_at": "2026-09-26T03:35:00Z"}
                ]
            })
        );
    }

    /// The filed bug's shape: the branch page led with a stale green from
    /// five days before the head while the newer cli-ci run had failed.
    /// Row position is never trusted, so the stale-first page still reads
    /// red; the position-trusting reader read green here.
    #[test]
    fn main_ci_reads_red_when_the_branch_page_leads_with_a_stale_green() {
        let on_main = vec![
            wf_run(
                "cli-ci",
                "stale",
                "completed",
                "success",
                "2026-09-23T05:52:00Z",
            ),
            wf_run(
                "guards",
                "stale",
                "completed",
                "success",
                "2026-09-23T05:52:00Z",
            ),
            wf_run(
                "cli-ci",
                "fresh",
                "completed",
                "failure",
                "2026-09-28T07:30:00Z",
            ),
        ];
        assert_eq!(
            token_at("fresh", &[], &on_main),
            serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "fresh"})
        );
    }

    /// A head run is judged at the head even mid-flight, and no older row of
    /// any workflow sets the verdict while it runs.
    #[test]
    fn main_ci_judges_head_sha_runs_and_lets_no_older_row_set_the_verdict() {
        let at_sha = vec![wf_run(
            "cli-ci",
            "head",
            "in_progress",
            "",
            "2026-09-28T08:00:00Z",
        )];
        let on_main = vec![
            wf_run(
                "cli-ci",
                "stale",
                "completed",
                "success",
                "2026-09-23T05:52:00Z",
            ),
            wf_run(
                "guards",
                "head",
                "completed",
                "success",
                "2026-09-28T08:00:00Z",
            ),
        ];
        assert_eq!(
            token_at("head", &at_sha, &on_main),
            serde_json::json!({"verdict": "pending"})
        );
    }

    /// The filed bug's shape: the nightly release fired on `schedule` and
    /// failed at the head sha while the head's push cli-ci was green. The
    /// verdict is the push CI only: green, never red off the scheduled run,
    /// and a schedule run still in flight at the head does not hold pending.
    #[test]
    fn a_schedule_release_failure_at_the_head_never_reads_main_red() {
        let at_sha = vec![wf_run(
            "cli-ci",
            "head",
            "completed",
            "success",
            "2026-09-30T05:00:00Z",
        )];
        let push_and_nightly = vec![
            wf_run(
                "cli-ci",
                "head",
                "completed",
                "success",
                "2026-09-30T05:00:00Z",
            ),
            wf_run_event(
                "release",
                "head",
                "completed",
                "failure",
                "2026-09-30T03:00:00Z",
                "schedule",
            ),
        ];
        assert_eq!(
            token_at("head", &at_sha, &push_and_nightly),
            serde_json::json!({"verdict": "green"})
        );
        let nightly_in_flight = vec![
            wf_run(
                "cli-ci",
                "head",
                "completed",
                "success",
                "2026-09-30T05:00:00Z",
            ),
            wf_run_event(
                "release",
                "head",
                "in_progress",
                "",
                "2026-09-30T06:00:00Z",
                "schedule",
            ),
        ];
        assert_eq!(
            token_at("head", &at_sha, &nightly_in_flight),
            serde_json::json!({"verdict": "green"})
        );
    }

    /// The rendered line names the workflow and sha; a plain verdict is the
    /// word alone, legacy string tokens pass through untouched, and a live
    /// platform incident rides the explanation beside the verdict. The
    /// casualty discrimination feeding the attachment lives here too: only a
    /// cancelled, never-started, runnerless job set is a casualty - a
    /// started job (manual or timeout cancel), a failure conclusion, and an
    /// empty job set are causes.
    #[test]
    fn main_ci_render_names_the_failed_workflow_and_sha() {
        let red = serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "a1"});
        assert_eq!(main_ci_render(Some(&red)), "red (cli-ci at a1)".to_string());
        assert_eq!(
            main_ci_render(Some(&serde_json::json!({"verdict": "green"}))),
            "green".to_string()
        );
        assert_eq!(
            main_ci_render(Some(&Value::String("green".into()))),
            "green".to_string()
        );
        assert_eq!(main_ci_render(None), "-".to_string());
        let casualty = serde_json::json!({
            "name": "cargo audit", "status": "completed", "conclusion": "cancelled",
            "runner_name": null, "started_at": null, "completed_at": "2026-10-05T19:48:00Z",
        });
        assert!(crate::gh_incident::cancelled_no_runner(&[casualty.clone()]));
        let ran_then_cancelled = serde_json::json!({
            "name": "cli-ci", "status": "completed", "conclusion": "cancelled",
            "runner_name": null, "started_at": "2026-10-05T19:27:00Z",
        });
        assert!(!crate::gh_incident::cancelled_no_runner(&[
            ran_then_cancelled
        ]));
        let test_failure = serde_json::json!({
            "name": "rollup", "status": "completed", "conclusion": "failure",
            "runner_name": null, "started_at": "2026-10-05T19:27:00Z",
        });
        assert!(!crate::gh_incident::cancelled_no_runner(&[test_failure]));
        assert!(!crate::gh_incident::cancelled_no_runner(&[]));
        let incidents = serde_json::json!([
            {"name": "Incident with Actions", "status": "investigating",
             "created_at": "2026-10-05T19:11:58Z", "updated_at": "2026-10-05T21:32:31Z"},
        ]);
        let mut with_incident = red.as_object().unwrap().clone();
        with_incident.insert("platform_incident".into(), incidents.clone());
        assert_eq!(
            main_ci_render(Some(&Value::Object(with_incident))),
            "red (cli-ci at a1) GitHub Actions incident: investigating since 2026-10-05T21:32:31Z"
                .to_string()
        );
        assert_eq!(
            crate::gh_incident::incident_line(&Value::Array(vec![])),
            "GitHub Actions incident: unknown since unknown"
        );
    }

    /// Each stale row renders its own line naming the workflow, sha and age;
    /// tokens without a stale list and legacy string tokens render none.
    #[test]
    fn main_ci_stale_lines_name_the_workflow_sha_and_age() {
        let token = serde_json::json!({
            "verdict": "green",
            "stale": [
                {"workflow": "release", "sha": "oldsha", "created_at": "2026-09-26T03:35:00Z"}
            ]
        });
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-28T03:35:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            main_ci_stale_lines(Some(&token), now),
            vec!["main ci stale: release at oldsha (2d old)".to_string()]
        );
        let fresh = serde_json::json!({
            "verdict": "green",
            "stale": [
                {"workflow": "release", "sha": "oldsha", "created_at": "2026-09-28T03:00:00Z"}
            ]
        });
        assert_eq!(
            main_ci_stale_lines(Some(&fresh), now),
            vec!["main ci stale: release at oldsha (<1h old)".to_string()]
        );
        assert!(
            main_ci_stale_lines(Some(&serde_json::json!({"verdict": "green"})), now).is_empty()
        );
        assert!(main_ci_stale_lines(Some(&Value::String("green".into())), now).is_empty());
        assert!(main_ci_stale_lines(None, now).is_empty());
    }

    /// The merge gate's main-verdict read is the TTL-cached one: a fresh row
    /// answers (with the freshness stamp) and no live gh read runs behind it,
    /// the red-run shape is read in one accessor, and an expired row is a
    /// miss for the row op. The row store rides `FNO_GH_FACTS_DIR` so no
    /// test touches a real state root; the variable is process-global but
    /// nothing else in this binary reads it.
    #[test]
    fn the_merge_gates_main_verdict_read_is_ttl_cached() {
        let root = std::env::temp_dir().join(format!("fno-main-ci-cache-{}", std::process::id()));
        std::env::set_var("FNO_GH_FACTS_DIR", &root);
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let slug = crate::finalize::slug_from_git_remote(repo).expect("crate lives in a repo");
        let token = serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "a1"});
        assert!(crate::gh_cache::write_row(
            &root,
            "main-ci",
            &slug,
            None,
            &json!({ "token": token })
        ));
        let got = main_ci_reading_cached(repo).unwrap();
        assert_eq!(got["verdict"], json!("red"));
        assert_eq!(got["workflow"], json!("cli-ci"));
        assert_eq!(got["sha"], json!("a1"));
        assert!(crate::gh_cache::write_row(
            &root,
            "main-ci",
            &slug,
            None,
            &json!({ "token": json!("green") })
        ));
        assert_eq!(main_ci_reading_cached(repo).unwrap(), json!("green"));
        assert_eq!(
            main_ci_red_run(&got),
            Some(("cli-ci".to_string(), "a1".to_string()))
        );
        assert_eq!(main_ci_red_run(&Value::String("pending".into())), None);
        let kind_dir = root.join("main-ci");
        let path = std::fs::read_dir(&kind_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut row =
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();
        row["ts"] = json!(0.0);
        std::fs::write(&path, row.to_string()).unwrap();
        assert_eq!(
            crate::gh_cache::read_row(
                &root,
                "main-ci",
                &slug,
                None,
                Some(60),
                crate::gh_cache::now_secs()
            )["row"],
            Value::Null
        );
        std::env::remove_var("FNO_GH_FACTS_DIR");
        std::fs::remove_dir_all(&root).ok();
    }
}
