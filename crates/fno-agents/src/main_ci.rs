//! main's CI verdict: the workflow-run reduction the king check-in renders,
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

/// One verdict token for the check-in line, from the workflow-run history on
/// main, not the tip's check runs: a tip that fires no run of a workflow must
/// not read green while that workflow's newest completed main run failed.
/// `at_sha` carries the push runs at main's current head; a workflow run
/// there is judged there - an in-flight head run reads pending rather than
/// falling back - and only a workflow the head never fired is judged from
/// the branch history page. Per workflow the newest COMPLETED run decides -
/// red on fail or cancel, naming the workflow and the sha it ran on; a
/// workflow whose newest run is still in flight reads pending; empty
/// history reads pending, never green.
fn main_ci_token_from_pages<'a>(at_sha: &'a [Value], on_main: &'a [Value]) -> Value {
    let mut workflows: Vec<(&'a str, &'a Value, Option<&'a Value>)> = Vec::new();
    fold_page(&mut workflows, at_sha.iter());
    let judged: Vec<&str> = workflows.iter().map(|(name, _, _)| *name).collect();
    fold_page(
        &mut workflows,
        on_main.iter().filter(|run| {
            let name = run.get("name").and_then(Value::as_str).unwrap_or("");
            !judged.contains(&name)
        }),
    );
    if let Some(run) = workflows
        .iter()
        .filter_map(|(_, _, completed)| *completed)
        .filter(|run| matches!(crate::pr_push::rest_bucket(run), "fail" | "cancel"))
        .max_by_key(|run| run.get("created_at").and_then(Value::as_str).unwrap_or(""))
    {
        let field = |k: &str| run.get(k).and_then(Value::as_str).unwrap_or("unknown");
        return json!({
            "verdict": "red",
            "workflow": field("name"),
            "sha": field("head_sha"),
        });
    }
    let inflight = workflows
        .iter()
        .any(|(_, newest, _)| newest.get("status").and_then(Value::as_str) != Some("completed"));
    Value::String(if workflows.is_empty() || inflight {
        "pending".into()
    } else {
        "green".into()
    })
}

/// The `main ci:` line body: a red verdict names the workflow and sha, the
/// string tokens pass through untouched.
pub(crate) fn main_ci_render(v: Option<&Value>) -> String {
    match v {
        Some(Value::Object(o)) => {
            let field = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("unknown");
            format!(
                "{} ({} at {})",
                field("verdict"),
                field("workflow"),
                field("sha")
            )
        }
        other => crate::king_checkin::dash(other),
    }
}

pub(crate) fn r_main_ci() -> Result<Value, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    main_ci_reading(&cwd)
}

/// The live reading for any cwd: the king check-in renders it per beat, and
/// the merge gate reads it behind the TTL cache below.
pub(crate) fn main_ci_reading(cwd: &Path) -> Result<Value, String> {
    // Judge at main's current head: the branch listing has served rows from
    // five days before the head while newer runs existed, so the head's own
    // push runs are read directly and only a workflow the head never fired
    // falls back to the branch history page.
    let head_raw = crate::pr_push::gh_api("gh", &cwd, "repos/{owner}/{repo}/branches/main", &[])
        .map_err(|error| {
            format!(
                "gh api failed: {}",
                crate::king_checkin::gh_error_cause(&error)
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
    // branch page only backfills workflows the head sha page never carried.
    let read_runs = |query: &str| -> Result<Vec<Value>, String> {
        let raw = crate::pr_push::gh_api("gh", &cwd, query, &[]).map_err(|error| {
            format!(
                "gh api failed: {}",
                crate::king_checkin::gh_error_cause(&error)
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
    let on_main = read_runs("repos/{owner}/{repo}/actions/runs?branch=main&per_page=100")?;
    Ok(main_ci_token_from_pages(&at_sha, &on_main))
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

/// The workflow-run rows an `/actions/runs` page carries, in the fields
/// the reducer reads; row order is free.
fn wf_run(name: &str, sha: &str, status: &str, conclusion: &str, created: &str) -> Value {
    serde_json::json!({
        "name": name, "head_sha": sha, "status": status,
        "conclusion": conclusion, "created_at": created,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_ci_reads_green_when_every_workflows_newest_completed_run_passed() {
        let runs = vec![
            wf_run(
                "guards",
                "b1",
                "completed",
                "success",
                "2026-09-26T03:00:00Z",
            ),
            wf_run(
                "cli-ci",
                "b1",
                "completed",
                "success",
                "2025-09-26T02:50:00Z",
            ),
        ];
        assert_eq!(
            main_ci_token_from_pages(&runs, &[]),
            Value::String("green".into())
        );
    }

    /// The filed bug's shape: the newest completed cli-ci main run failed,
    /// and the later tip fires no cli-ci at all. The tip-only reader read
    /// green here; the history reader names the failed workflow and sha.
    #[test]
    fn main_ci_reads_red_from_a_newer_failed_cli_ci_when_the_tip_fires_none() {
        let runs = vec![
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
            wf_run(
                "cli-ci",
                "a1",
                "completed",
                "failure",
                "2026-09-26T03:35:00Z",
            ),
        ];
        assert_eq!(
            main_ci_token_from_pages(&runs, &[]),
            serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "a1"})
        );
    }

    /// A workflow whose newest run is still in flight reads pending even when
    /// its newest completed run passed, and empty history reads pending too -
    /// never green.
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
            main_ci_token_from_pages(&runs, &[]),
            Value::String("pending".into())
        );
        assert_eq!(
            main_ci_token_from_pages(&[], &[]),
            Value::String("pending".into())
        );
    }

    /// Red outranks an in-flight run of the same workflow: the newest
    /// completed result is the failure until a newer run completes green.
    #[test]
    fn main_ci_reads_red_even_while_a_newer_run_of_the_same_workflow_is_in_flight() {
        let runs = vec![
            wf_run("cli-ci", "b3", "in_progress", "", "2026-09-26T04:10:00Z"),
            wf_run(
                "cli-ci",
                "a1",
                "completed",
                "failure",
                "2026-09-26T03:35:00Z",
            ),
        ];
        assert_eq!(
            main_ci_token_from_pages(&runs, &[]),
            serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "a1"})
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
            main_ci_token_from_pages(&[], &on_main),
            serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "fresh"})
        );
    }

    /// A workflow run at the head sha is judged there even mid-flight - a
    /// pending head run beats the branch page's older green - and only a
    /// workflow the head never fired falls back to the branch page.
    #[test]
    fn main_ci_judges_head_sha_runs_first_and_falls_back_only_for_absent_workflows() {
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
            main_ci_token_from_pages(&at_sha, &on_main),
            Value::String("pending".into())
        );
    }

    /// A workflow the head never fired is judged from the branch page's
    /// newest completed run.
    #[test]
    fn main_ci_falls_back_to_the_branch_pages_newest_completed_run() {
        let at_sha = vec![wf_run(
            "guards",
            "head",
            "completed",
            "success",
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
                "cli-ci",
                "gone",
                "completed",
                "failure",
                "2026-09-26T03:35:00Z",
            ),
        ];
        assert_eq!(
            main_ci_token_from_pages(&at_sha, &on_main),
            serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "gone"})
        );
    }

    /// The rendered line names the workflow and sha; the string tokens pass
    /// through untouched.
    #[test]
    fn main_ci_render_names_the_failed_workflow_and_sha() {
        let red = serde_json::json!({"verdict": "red", "workflow": "cli-ci", "sha": "a1"});
        assert_eq!(main_ci_render(Some(&red)), "red (cli-ci at a1)".to_string());
        assert_eq!(
            main_ci_render(Some(&Value::String("green".into()))),
            "green".to_string()
        );
        assert_eq!(main_ci_render(None), "-".to_string());
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
