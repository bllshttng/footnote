//! The logs verb (`fno do pr logs`): why CI failed, in a bounded tail.
//! Ported from `fno.pr._logs` on top of `read_pr` and the shared job-log
//! cache, so logs, heal and status spend one read per job.

use super::cache::CountingProbe;
use crate::king_board::prs::classify_check;
use crate::pr_status_facts::{GhProbe, RealGhProbe};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;

const TAIL_LINES: usize = 40;
const LOG_NAME: &str = "last-ci.log";

/// Why gh failed, named as the operator action it asks for (`_logs.
/// _gh_failure_reason`); a bare non-zero exit never reaches the caller.
fn gh_failure_reason(stderr: &str) -> String {
    let blob = stderr.to_lowercase();
    if blob.contains("gh auth login") || blob.contains("401") || blob.contains("authentication") {
        "authentication (run `gh auth login`)".into()
    } else if blob.contains("rate limit") || blob.contains("429") || blob.contains("403") {
        "rate limit or forbidden".into()
    } else if blob.contains("410") || blob.contains("gone") || blob.contains("expired") {
        "log expired (past GitHub's retention window)".into()
    } else if blob.contains("no pull requests found")
        || blob.contains("404")
        || blob.contains("not found")
    {
        "no such PR, or no PR for the current branch".into()
    } else {
        "gh error".into()
    }
}

fn repo_root(cwd: &Path) -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Write `text` to <root>/.fno/last-ci.log via temp+rename: two agents can
/// spool concurrently without a reader seeing a half-written log.
fn spool(root: &Path, text: &str) -> Result<PathBuf, String> {
    let d = root.join(".fno");
    let dest = d.join(LOG_NAME);
    let tmp = d.join(format!(".{LOG_NAME}.{}.tmp", std::process::id()));
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(dest)
}

/// The verb body: (exit, stdout, stderr). The rollup rides `read_pr`, the
/// failing job's log rides the shared job cache (`job_log`), so a status
/// read that already fetched it costs this verb zero log reads.
pub(crate) fn run_logs_with<P: GhProbe>(
    probe: &P,
    cwd: &Path,
    slug: &str,
    pr: u64,
    job: Option<&str>,
    lines: usize,
    full: bool,
    root: &Path,
) -> (i32, String, String) {
    let pr_json = match super::read_pr(probe, cwd, slug, pr) {
        Ok(v) => v,
        Err(reason) => {
            return (
                4,
                String::new(),
                format!("fno do pr logs: cannot read CI state: {}\n", reason.text),
            );
        }
    };
    let rollup = pr_json
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let deduped = crate::check_supersession::latest_per_name(&Value::Array(rollup.clone()));
    let deduped = deduped.as_array().cloned().unwrap_or_default();
    if deduped.is_empty() {
        return (3, "no checks on this PR\n".into(), String::new());
    }
    let failing: Vec<Value> = deduped
        .iter()
        .filter(|c| classify_check(c) == "fail")
        .cloned()
        .collect();
    let pending_count = deduped
        .iter()
        .filter(|c| classify_check(c) == "pending")
        .count();
    let mut stdout = String::new();
    if failing.is_empty() {
        if pending_count > 0 {
            let mut out = format!("{pending_count} check(s) still running, none failed yet:\n");
            for c in deduped.iter().filter(|c| classify_check(c) == "pending") {
                out.push_str(&format!("  pending: {}\n", super::check_name(c)));
            }
            return (2, out, String::new());
        }
        return (
            0,
            format!("all {} checks green\n", deduped.len()),
            String::new(),
        );
    }
    let names: Vec<String> = failing.iter().map(|c| super::check_name(c)).collect();
    stdout.push_str(&format!(
        "{} failing check(s): {}\n",
        failing.len(),
        names.join(", ")
    ));
    let mut target: &Value = &failing[0];
    if let Some(job) = job {
        let matched = failing
            .iter()
            .find(|c| super::check_name(c) == job)
            .or_else(|| {
                failing.iter().find(|c| {
                    super::check_name(c)
                        .to_lowercase()
                        .contains(&job.to_lowercase())
                })
            });
        match matched {
            Some(c) => target = c,
            None => {
                return (
                    1,
                    stdout,
                    format!(
                        "fno do pr logs: no failing check matches --job '{job}'; \
                         failing: {}\n",
                        names.join(", ")
                    ),
                );
            }
        }
    }
    let Some((owner, repo, job_id)) = super::job_ref(target) else {
        let url = {
            let d = target
                .get("detailsUrl")
                .and_then(Value::as_str)
                .unwrap_or("");
            if d.is_empty() {
                target
                    .get("targetUrl")
                    .and_then(Value::as_str)
                    .unwrap_or("(no url)")
            } else {
                d
            }
        };
        stdout.push_str(&format!(
            "{} is not a GitHub Actions job; see {url}\n",
            super::check_name(target)
        ));
        return (1, stdout, String::new());
    };
    stdout.push_str(&format!(
        "fetching: {} (job {job_id})\n",
        super::check_name(target)
    ));
    let slug_key = slug.replace('/', "--");
    match super::job_log(probe, cwd, &slug_key, &owner, &repo, &job_id) {
        Err(why) => (
            1,
            stdout,
            format!(
                "fno do pr logs: could not fetch the log for {}: {}\n",
                super::check_name(target),
                gh_failure_reason(&why)
            ),
        ),
        Ok(log) => {
            let dest = match spool(root, &log) {
                Ok(d) => d,
                Err(e) => {
                    return (
                        1,
                        stdout,
                        format!(
                            "fno do pr logs: fetched the log but could not \
                             write {}: {e}\n",
                            root.join(".fno").join(LOG_NAME).display()
                        ),
                    );
                }
            };
            if full {
                stdout.push_str(&log);
                stdout.push_str(&format!("\nfull log: {}\n", dest.display()));
                return (1, stdout, String::new());
            }
            let log_lines: Vec<&str> = log.split_inclusive('\n').collect();
            let start = log_lines.len().saturating_sub(lines);
            let shown: Vec<&str> = if lines > 0 {
                log_lines[start..].to_vec()
            } else {
                Vec::new()
            };
            stdout.push_str(&format!(
                "last {} lines of {} - read from the end, expand upward \
                 if needed: tail -200 {}\n",
                shown.len(),
                super::check_name(target),
                dest.display()
            ));
            for line in &shown {
                stdout.push_str(line);
            }
            if shown.last().is_some_and(|l| !l.ends_with('\n')) {
                stdout.push('\n');
            }
            (1, stdout, String::new())
        }
    }
}

/// The current branch's open PR, the `_rest.resolve_current_pr_number_rest`
/// read: one REST query, no `gh pr view` GraphQL spend.
fn resolve_current_pr<P: GhProbe>(probe: &P, cwd: &Path, slug: &str) -> Result<u64, String> {
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty() && b != "HEAD")
        .ok_or_else(|| "no current branch".to_string())?;
    let args = vec![
        "api".to_string(),
        format!("repos/{slug}/pulls?head={slug}:{branch}&state=open"),
    ];
    let (ok, stdout, _) = probe.run_gh(cwd, &args)?;
    if !ok {
        return Err("pulls lookup failed".to_string());
    }
    let rows: Value =
        serde_json::from_str(&stdout).map_err(|e| format!("unparseable pulls list: {e}"))?;
    Ok(rows
        .as_array()
        .and_then(|r| r.first())
        .and_then(|r| r.get("number"))
        .and_then(Value::as_u64)
        .ok_or_else(|| "no pull requests found for branch".to_string())?)
}

/// The `status-logs` door op: `{cwd, pr, job, lines, full}`. A zero pr asks
/// for the current branch's PR.
pub(crate) fn run_logs_door(payload: &Value) -> (i32, String, String) {
    let cwd_str = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
    let pr = payload.get("pr").and_then(Value::as_u64).unwrap_or(0);
    let job = payload
        .get("job")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let lines = payload
        .get("lines")
        .and_then(Value::as_u64)
        .unwrap_or(TAIL_LINES as u64) as usize;
    let full = payload
        .get("full")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cwd = Path::new(cwd_str);
    let Some(slug) = super::cache::git_slug(cwd) else {
        return (
            4,
            String::new(),
            "fno do pr logs: cannot read CI state: no repo\n".into(),
        );
    };
    let probe = CountingProbe {
        inner: RealGhProbe,
        calls: AtomicUsize::new(0),
    };
    let pr = if pr == 0 {
        match resolve_current_pr(&probe, cwd, &slug) {
            Ok(n) => n,
            Err(why) => {
                return (
                    4,
                    String::new(),
                    format!("fno do pr logs: cannot read CI state: {why}\n"),
                )
            }
        }
    } else {
        pr
    };
    run_logs_with(&probe, cwd, &slug, pr, job, lines, full, &repo_root(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_raw(name: &str) -> Value {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pr_status")
            .join(format!("{name}.json"));
        let text = std::fs::read_to_string(base).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["inputs"]["raw"].clone()
    }

    /// A fake that answers read_pr's calls off a fixture's raw responses
    /// (the rollup source of the logs verb).
    struct RollupFake(Value);

    impl GhProbe for RollupFake {
        fn run_gh(&self, _cwd: &Path, args: &[String]) -> Result<(bool, String, String), String> {
            let cmd = args.join(" ");
            let serve = |v: &Value| Ok((true, v.to_string(), String::new()));
            if cmd.contains("/pulls/") {
                return serve(&self.0["pulls"]);
            }
            if cmd.contains("/check-runs") && cmd.contains("page=") {
                return serve(&self.0["check_runs_pages"][0]);
            }
            if cmd.contains("/actions/runs?head_sha=") {
                return serve(&self.0["runs_listing"]);
            }
            if cmd.contains("/actions/runs/") && cmd.contains("/jobs?per_page=") {
                return serve(&json!({"total_count": 2, "jobs": []}));
            }
            if cmd.ends_with("/status") {
                return serve(&self.0["statuses"]);
            }
            if cmd.contains("rate_limit") {
                return serve(&json!({"resources": {"core": {"remaining": 10}}}));
            }
            Err(format!("RollupFake has no answer for: {cmd}"))
        }
    }

    #[test]
    fn a_green_pr_fetches_nothing_and_reads_green() {
        let fake = RollupFake(fixture_raw("green_settled"));
        let root = tempfile::tempdir().unwrap();
        let (code, stdout, _stderr) = run_logs_with(
            &fake,
            Path::new("/tmp"),
            "Owner/Repo",
            42,
            None,
            40,
            false,
            root.path(),
        );
        assert_eq!(code, 0, "green");
        assert_eq!(stdout, "all 2 checks green\n", "{stdout}");
    }

    #[test]
    fn a_cached_log_prints_the_tail_with_zero_log_reads() {
        let _guard = super::super::cache_env_lock();
        let fake = RollupFake(fixture_raw("red_detailed_capped"));
        let cache = tempfile::tempdir().unwrap();
        let spool_root = tempfile::tempdir().unwrap();
        // The seed: job 2001's log already cached by an earlier status read.
        let row = json!({"ts": 1.0, "log": "line one\nline two\nfailing now\n"});
        std::fs::write(
            cache.path().join("job-Owner--Repo-2001.json"),
            row.to_string(),
        )
        .unwrap();
        std::env::set_var("FNO_PR_STATUS_CACHE_DIR", cache.path());
        let (code, stdout, _stderr) = run_logs_with(
            &fake,
            Path::new("/tmp"),
            "Owner/Repo",
            42,
            None,
            40,
            false,
            spool_root.path(),
        );
        assert_eq!(code, 1, "red");
        assert!(stdout.contains("6 failing check(s): "), "{stdout}");
        assert!(stdout.contains("fetching: smoke (job 2001)"), "{stdout}");
        assert!(stdout.contains("last 3 lines of smoke"), "{stdout}");
        assert!(stdout.contains("failing now"), "{stdout}");
        let spooled =
            std::fs::read_to_string(spool_root.path().join(".fno").join("last-ci.log")).unwrap();
        assert_eq!(spooled, "line one\nline two\nfailing now\n", "the spool");
    }
}
