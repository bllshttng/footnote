//! `fno-agents pr-push` -- the one guarded push: fetch, rebase onto
//! origin/main (or merge it when the branch already holds merges), preflight, read the in-flight state, push exactly once,
//! print one receipt. Every push site in `skills/pr` calls this through
//! `fno do pr push`, so a branch integrates origin/main before it moves and a queued CI
//! run is never cancelled by a second push.
//!
//! The in-flight guard is heal's, promoted. heal used to hold a private copy
//! of the re-read-and-push decision; this module is now the shared push
//! layer, heal calls [`guarded_push`] instead, and heal.rs shrinks under the
//! file-budget ratchet. Exit codes are heal's, so one table covers both
//! verbs:
//!
//! * `0` pushed
//! * `1` preflight red (heal already uses 1 for escalations; the meanings
//!   are per-verb, the numbers shared)
//! * `2` a run in flight (nothing pushed)
//! * `3` a refusal the caller must fix (protected, dirty, conflict,
//!   remote-only commits)
//! * `4` a read error (fetch failed, check read failed, compare failed,
//!   push failed)
//!
//! A rebased branch is pushed with `--force-with-lease` pinned to the exact
//! remote sha this run fetched, and only after a patch-equivalence check
//! proves the remote branch holds no commit the branch lacks, so the lease
//! can never drop a commit another writer pushed. A merge commit on the
//! remote side counts only when its tree equals the automatic merge of its
//! parents: a merge carrying hand-resolved content is refused, never
//! leased over. The git pre-push hook is unaffected: it refuses protected
//! branches by destination, and this verb refuses them before anything
//! moves.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A `gh` read. Generous next to the stop gate's 30s because a paginated
/// check-runs read on a busy PR is slower than a single rollup read.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// A full preflight rehearsal. Opt-in policy makes this the push's gate, and
/// the repo's own doc prices the full rehearsal at 22 to 47 minutes.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(3600);

/// Branches the push verb refuses to move, same set the rebase leg refuses
/// to rebase.
const PROTECTED: [&str; 4] = ["main", "master", "develop", "dev"];

/// The hook's own debounce window (`hooks/git-protection.py`
/// PUSH_DEBOUNCE_SECONDS). One threshold, two enforcers.
const PUSH_DEBOUNCE_SECS: u64 = 120;

// ── shared push layer (moved here from heal.rs) ─────────────────────────────

/// A bounded external read with a caller label for the diagnostic.
pub(crate) fn run_labeled(
    label: &str,
    bin: &str,
    args: &[&str],
    cwd: &Path,
    timeout: Duration,
) -> Result<(bool, String, String), String> {
    match crate::loopcheck::bounded_read(bin.as_ref(), args, cwd, label, timeout) {
        Ok(out) => Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr_tail).into_owned(),
        )),
        Err(err) => Err(
            crate::loopcheck::bounded_read_diagnostic(label, &err).replacen("loop-check: ", "", 1),
        ),
    }
}

/// `gh api` against the current repo. `{owner}` unexpanded: gh resolves
/// owner/repo from the checkout. `--allow-escape-sequences` with a retry
/// without it covers an older gh, exactly as heal's twin did.
pub(crate) fn gh_api(
    gh_bin: &str,
    cwd: &Path,
    path: &str,
    extra: &[&str],
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["api", "--allow-escape-sequences", path];
    args.extend_from_slice(extra);
    let (ok, out, err) = run_labeled("pr-push", gh_bin, &args, cwd, READ_TIMEOUT)?;
    if ok {
        return Ok(out);
    }
    if err.to_lowercase().contains("unknown flag") {
        let mut plain: Vec<&str> = vec!["api", path];
        plain.extend_from_slice(extra);
        let (ok, out, err) = run_labeled("pr-push", gh_bin, &plain, cwd, READ_TIMEOUT)?;
        if ok {
            return Ok(out);
        }
        return Err(format!("gh api {path} failed: {}", err.trim()));
    }
    Err(format!("gh api {path} failed: {}", err.trim()))
}

/// Read a paginated `gh api` endpoint as one JSON array of PAGES (`--slurp`;
/// `--paginate` alone concatenates page bodies with no parseable boundary).
pub(crate) fn gh_api_pages(gh_bin: &str, cwd: &Path, path: &str) -> Result<Vec<Value>, String> {
    let raw = gh_api(gh_bin, cwd, path, &["--paginate", "--slurp"])?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Array(pages)) => Ok(pages),
        Ok(other) => Ok(vec![other]),
        Err(e) => Err(format!("gh api {path} returned unparseable pages: {e}")),
    }
}

/// The head's check runs plus StatusContexts, in the `bucket`/`link` shape
/// [`crate::check_supersession::latest_per_name`] speaks. REST: `gh pr
/// checks` is GraphQL and the quota broker routes it away unconditionally.
/// MAY BE EMPTY: a commit GitHub holds no checks for (never pushed, or not
/// yet registered) is a legitimate answer, not an error. Callers that need
/// the old fail-closed-on-empty shape use [`read_checks`].
pub(crate) fn read_checks_rows(gh_bin: &str, cwd: &Path, head: &str) -> Result<Vec<Value>, String> {
    let pages = gh_api_pages(
        gh_bin,
        cwd,
        &format!("repos/{{owner}}/{{repo}}/commits/{head}/check-runs"),
    )?;
    let mut rows: Vec<Value> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();
    for page in pages {
        let Some(runs) = page.get("check_runs").and_then(|r| r.as_array()) else {
            continue;
        };
        for run in runs {
            raw.push(run.clone());
            let timeout = if run.get("conclusion").and_then(Value::as_str) == Some("cancelled")
                && run
                    .pointer("/output/annotations_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    > 0
            {
                run.get("id")
                    .and_then(Value::as_u64)
                    .and_then(|id| {
                        gh_api_pages(
                            gh_bin,
                            cwd,
                            &format!("repos/{{owner}}/{{repo}}/check-runs/{id}/annotations"),
                        )
                        .ok()
                    })
                    .and_then(|pages| {
                        crate::pr_status_facts::timeout_annotation(&Value::Array(pages))
                    })
            } else {
                None
            };
            let mut row = json!({
                "name": run.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "bucket": if timeout.is_some() { "fail" } else { rest_bucket(run) },
                "link": run.get("html_url").and_then(|v| v.as_str()).unwrap_or(""),
                "workflow": run
                    .pointer("/check_suite/id")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                "startedAt": run.get("started_at").and_then(|v| v.as_str()).unwrap_or(""),
                "completedAt": run.get("completed_at").and_then(|v| v.as_str()).unwrap_or(""),
            });
            if let Some(timeout) = timeout {
                row["timeout"] = json!(timeout);
            }
            rows.push(row);
        }
    }
    // A run that failed before minting a job owns no check run; the shared
    // rule adds its row so the failure cannot read green.
    rows.extend(zero_job_rows(gh_bin, cwd, head, &raw)?);
    // The check-runs endpoint returns ONLY check-runs. A commit StatusContext
    // lives on a different endpoint, and reading one without the other is a
    // false green; a failed read propagates, it never reads green.
    rows.extend(read_statuses(gh_bin, cwd, head)?);
    Ok(rows)
}

/// The rows for the Actions runs that failed before minting a job:
/// the head_sha-scoped runs listing, the shared rule in
/// `pr_status_facts::zero_job_failures`, and this module's row shape. A
/// failed runs read is an `Err`, like a failed check-runs read.
pub(crate) fn zero_job_rows(
    gh_bin: &str,
    cwd: &Path,
    head: &str,
    check_runs: &[Value],
) -> Result<Vec<Value>, String> {
    let pages = gh_api_pages(
        gh_bin,
        cwd,
        &format!("repos/{{owner}}/{{repo}}/actions/runs?head_sha={head}&per_page=100"),
    )?;
    let mut runs: Vec<Value> = Vec::new();
    for page in pages {
        if let Some(list) = page.get("workflow_runs").and_then(|r| r.as_array()) {
            runs.extend(list.iter().cloned());
        }
    }
    let jobs_total = |id: u64| -> Result<u64, String> {
        let raw = gh_api(
            gh_bin,
            cwd,
            &format!("repos/{{owner}}/{{repo}}/actions/runs/{id}/jobs?per_page=1"),
            &[],
        )?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|e| format!("the jobs read for run {id} was unparseable: {e}"))?
            .get("total_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("the jobs read for run {id} carried no total_count"))
    };
    let found = crate::pr_status_facts::zero_job_failures(&runs, check_runs, &jobs_total)?;
    Ok(found
        .iter()
        .map(|r| {
            json!({
                "name": r.path,
                "bucket": "fail",
                "link": r.url,
                "workflow": r.path,
                "startedAt": r.created_at,
                "completedAt": "",
            })
        })
        .collect())
}

/// The commit's StatusContexts, in the same row shape as the check-runs. A
/// failed read is an `Err`: a legacy status the read could not fetch must
/// never read as an absent-and-green one.
fn read_statuses(gh_bin: &str, cwd: &Path, head: &str) -> Result<Vec<Value>, String> {
    let raw = gh_api(
        gh_bin,
        cwd,
        &format!("repos/{{owner}}/{{repo}}/commits/{head}/status"),
        &[],
    )?;
    let v: Value =
        serde_json::from_str(&raw).map_err(|e| format!("the status read was unparseable: {e}"))?;
    let rows = v
        .get("statuses")
        .and_then(|s| s.as_array())
        .ok_or_else(|| "the status read carried malformed statuses".to_string())?;
    Ok(rows.iter()
        .map(|st| json!({
            "name": st.get("context").and_then(|v| v.as_str()).unwrap_or(""),
            "bucket": match st.get("state").and_then(|v| v.as_str()).unwrap_or("").to_lowercase().as_str() {
                "success" => "pass",
                "pending" => "pending",
                _ => "fail",
            },
            "link": st.get("target_url").and_then(|v| v.as_str()).unwrap_or(""),
            "workflow": "",
            "startedAt": st.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
            "completedAt": st.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""),
        }))
        .collect())
}

/// A REST check-run's `status`/`conclusion` folded to the bucket vocabulary.
/// An unrecognized conclusion buckets `fail` rather than `pass`: a bucket
/// this crate does not understand must never read green.
pub(crate) fn rest_bucket(run: &Value) -> &'static str {
    let status = run
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if status != "completed" {
        return "pending";
    }
    match run
        .get("conclusion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "success" => "pass",
        "skipped" | "neutral" => "skipping",
        "cancelled" => "cancel",
        _ => "fail",
    }
}

/// heal's caller shape: empty reads fail closed, because to heal a PR whose
/// checks are absent is indistinguishable from an unreadable one.
pub(crate) fn read_checks(gh_bin: &str, cwd: &Path, head: &str) -> Result<Value, String> {
    let rows = read_checks_rows(gh_bin, cwd, head)?;
    if rows.is_empty() {
        return Err("check-runs read named no checks".to_string());
    }
    Ok(Value::Array(rows))
}

/// True when any check is still running. Read AFTER a fix commit and before
/// the push: pushing over a run in flight cancels it, which is the exact
/// harm one session did seven times in one session.
pub(crate) fn any_pending(checks: &Value) -> bool {
    let deduped = crate::check_supersession::latest_per_name(checks);
    deduped
        .as_array()
        .map(|rows| {
            rows.iter().any(|row| {
                !matches!(
                    row.get("bucket")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_lowercase()
                        .as_str(),
                    "pass" | "fail" | "skipping" | "cancel"
                )
            })
        })
        .unwrap_or(false)
}

/// The job id out of a check's `link`
/// (`.../actions/runs/<run>/job/<job>`). Mirrors `_JOB_URL` in
/// `cli/src/fno/pr/_logs.py`; a check with no job link (a commit
/// StatusContext) has no log to read and answers `None`.
pub(crate) fn job_id(link: &str) -> Option<String> {
    let re = regex::Regex::new(r"^https?://[^/]+/[^/]+/[^/]+/actions/runs/\d+/job/(\d+)")
        .expect("static regex");
    re.captures(link).map(|c| c[1].to_string())
}

/// The worktree's porcelain status, for the dirty guard.
pub(crate) fn porcelain(git_bin: &str, cwd: &Path) -> String {
    run_labeled(
        "pr-push",
        git_bin,
        &["status", "--porcelain"],
        cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| out.trim().to_string())
    .unwrap_or_default()
}

/// Whether the worktree carries uncommitted changes.
pub(crate) fn dirty(git_bin: &str, cwd: &Path) -> bool {
    !porcelain(git_bin, cwd).is_empty()
}

// ── the guarded push ────────────────────────────────────────────────────────

/// One bounded git read for the remote compare. `ok=false` means git ran
/// and reported failure, not that the read could not happen.
fn read_remote(git_bin: &str, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    run_labeled("pr-push", git_bin, args, cwd, READ_TIMEOUT).unwrap_or((
        false,
        String::new(),
        "compare read failed".to_string(),
    ))
}

/// The exit-4 refusal for a remote that cannot be compared; nothing pushed.
fn compare_fail(remote_ref: &str, err: &str) -> i32 {
    let err = if err.trim().is_empty() {
        "compare read failed".to_string()
    } else {
        err.to_string()
    };
    eprintln!("pr-push: could not compare with {remote_ref} ({err}); nothing pushed");
    4
}

/// The exit-3 refusal: the remote branch holds something this push would
/// drop, and `detail` names it. The text must not contain `not safely
/// rebasable`, which heal's conflict parser keys on.
fn remote_only_refusal(remote_ref: &str, branch: &str, detail: &str) -> i32 {
    eprintln!(
        "pr-push: {remote_ref} carries commits this branch lacks: {detail}. \
         Integrate them with git pull --rebase origin {branch}, then re-run \
         the push. Nothing pushed."
    );
    3
}

/// Everything a push needs. The `bin` seams exist for the same reason heal's
/// do: push discipline is provable against stub executables instead of a
/// real remote, without mutating the process PATH.
pub(crate) struct PushCtx {
    pub git_bin: String,
    pub gh_bin: String,
    /// The `fno` binary used for the `push_debounce_bypass` journal row.
    pub fno_bin: String,
    pub cwd: PathBuf,
    /// Where the last-push stamps live (the hook reads the same files).
    pub stamps_dir: PathBuf,
    /// `--force-ci-cancel`: skip the in-flight read and record the bypass.
    pub force: bool,
    /// The remote sha this push may replace, set only when the fetched
    /// remote branch carries no commit the local side lacks. `None` keeps
    /// the push plain: heal's callers never rebase, and a first push has no
    /// same-name remote to replace.
    pub lease: Option<String>,
}

/// What a push decision ended as.
pub(crate) enum PushOutcome {
    Pushed { sha: String },
    InFlight { check: String, job: Option<String> },
    Unreadable(String),
    PushFailed(String),
}

/// The in-flight decision + one push. heal calls this after its commit; the
/// verb adds the fetch/rebase/preflight legs around it. `head` is the
/// REMOTE head whose runs a push would cancel (heal knows it from read_pr;
/// the verb derives it from `@{u}`). Empty `head` skips the read: a branch
/// with no upstream has nothing in flight anywhere.
pub(crate) fn guarded_push(ctx: &PushCtx, head: &str) -> PushOutcome {
    if ctx.force {
        emit_bypass_row(ctx);
    } else {
        match in_flight(ctx, &current_branch_quoted(ctx), head) {
            Ok(Some((check, job))) => return PushOutcome::InFlight { check, job },
            Ok(None) => {}
            Err(msg) => return PushOutcome::Unreadable(msg),
        }
    }
    // Push exactly once, to the same-name remote branch, upstream or not.
    // A bare `git push` cannot be used here: push.default=simple refuses
    // when the branch's upstream name differs from its own name (a branch
    // born off origin/main carries exactly that tracking), and the create
    // path has no upstream at all. `--set-upstream origin HEAD:<branch>`
    // covers both and pins correct tracking on the first push.
    let branch = run_labeled(
        "pr-push",
        &ctx.git_bin,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &ctx.cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| out.trim().to_string())
    .unwrap_or_default();
    let refspec = format!("HEAD:{branch}");
    // The lease flag rides after the positionals: git's option parser
    // accepts it there, and the `push --set-upstream origin HEAD:<branch>`
    // prefix stays byte-identical for `a_first_push_sets_the_upstream`.
    let mut push_args: Vec<&str> = vec!["push", "--set-upstream", "origin", refspec.as_str()];
    let lease_flag;
    if let Some(sha) = &ctx.lease {
        lease_flag = format!("--force-with-lease=refs/heads/{branch}:{sha}");
        push_args.push(lease_flag.as_str());
    }
    let (ok, _, err) =
        crate::pr_push::run_labeled("pr-push", &ctx.git_bin, &push_args, &ctx.cwd, READ_TIMEOUT)
            .unwrap_or((false, String::new(), "push spawn failed".to_string()));
    if !ok {
        return PushOutcome::PushFailed(err);
    }
    stamp_push(ctx);
    PushOutcome::Pushed {
        sha: run_labeled(
            "pr-push",
            &ctx.git_bin,
            &["rev-parse", "--short", "HEAD"],
            &ctx.cwd,
            READ_TIMEOUT,
        )
        .map(|(_, out, _)| out.trim().to_string())
        .unwrap_or_default(),
    }
}

/// Record the push in the hook's stamp dir, so a hand push that follows
/// within the debounce window still sees a coherent clock. Best-effort.
fn stamp_push(ctx: &PushCtx) {
    let branch = run_labeled(
        "pr-push",
        &ctx.git_bin,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &ctx.cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| {
        out.trim().replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_' && c != '-',
            "_",
        )
    })
    .map(|b| if b.is_empty() { "HEAD".to_string() } else { b })
    .unwrap_or_else(|_| "HEAD".to_string());
    let dir = &ctx.stamps_dir;
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join(format!("{branch}.stamp")), b"");
}

/// Age in seconds of this branch's last-push stamp, the same file the hook
/// writes and reads. An absent or unreadable stamp answers None: the clock
/// is a proxy and, like the hook's, it fails open.
fn stamp_age_secs(ctx: &PushCtx, branch: &str) -> Option<u64> {
    let safe: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stamp = std::fs::metadata(ctx.stamps_dir.join(format!("{safe}.stamp"))).ok()?;
    let modified = stamp.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(modified).ok()?;
    Some(age.as_secs())
}

/// Read whether a remote head would be cancelled by a push. `None` means the
/// head has no registered pending check; errors stay distinct so callers can
/// fail open only at the hand-push hook, never in the guarded verb.
pub(crate) fn in_flight(
    ctx: &PushCtx,
    branch: &str,
    head: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    if head.is_empty() {
        return Ok(None);
    }
    let rows = read_checks_rows(&ctx.gh_bin, &ctx.cwd, head)?;
    if rows.is_empty() {
        if let Some(age) = stamp_age_secs(ctx, branch) {
            if age < PUSH_DEBOUNCE_SECS {
                return Ok(Some((
                    format!("last push {age}s ago, its run may not be registered yet"),
                    None,
                )));
            }
        }
    }
    let arr = Value::Array(rows);
    if !any_pending(&arr) {
        return Ok(None);
    }
    let row = crate::check_supersession::latest_per_name(&arr)
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|row| {
                !matches!(
                    row.get("bucket").and_then(|v| v.as_str()).unwrap_or(""),
                    "pass" | "fail" | "skipping" | "cancel"
                )
            })
        })
        .cloned()
        .unwrap_or(Value::Null);
    let check = row
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let job = job_id(row.get("link").and_then(|v| v.as_str()).unwrap_or(""));
    Ok(Some((check, job)))
}

fn print_in_flight(branch: &str, head: &str, check: &str, job: Option<&str>) -> i32 {
    println!(
        "{}",
        json!({
            "branch": branch,
            "head": head,
            "in_flight": true,
            "check": check,
            "job": job,
        })
    );
    2
}

/// The `push_debounce_bypass` journal row, one per --force-ci-cancel.
/// Best-effort by contract.
fn emit_bypass_row(ctx: &PushCtx) {
    let _ = crate::loopcheck::bounded_read(
        ctx.fno_bin.as_ref(),
        &[
            "doctor",
            "event",
            "emit",
            "push_debounce_bypass",
            "--json",
            // serde builds the payload: a branch name is caller-controlled
            // text and may carry quotes or backslashes (both legal in git
            // refs), which a format! would emit as broken JSON.
            &json!({ "branch": current_branch_quoted(ctx) }).to_string(),
        ],
        &ctx.cwd,
        "pr-push-bypass",
        Duration::from_secs(10),
    );
}

/// The current branch, for the bypass journal row.
fn current_branch_quoted(ctx: &PushCtx) -> String {
    run_labeled(
        "pr-push",
        &ctx.git_bin,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &ctx.cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| out.trim().to_string())
    .unwrap_or_else(|_| "HEAD".to_string())
}

// ── the verb ────────────────────────────────────────────────────────────────

/// Parsed verb arguments.
struct VerbArgs {
    force: bool,
    in_flight: Option<String>,
    preflight: bool,
    git_bin: String,
    gh_bin: String,
    fno_bin: String,
    cwd: PathBuf,
    stamps_dir: PathBuf,
}

fn parse_verb_args(argv: &[String]) -> Result<VerbArgs, String> {
    let mut a = VerbArgs {
        force: false,
        in_flight: None,
        preflight: false,
        git_bin: "git".to_string(),
        gh_bin: "gh".to_string(),
        fno_bin: "fno".to_string(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        // The hook's own stamp dir (git-protection.py PUSH_STAMP_DIR is
        // $FNO_HOME/push-stamps = ~/.fno/push-stamps). Rooting this at
        // AgentsHome would split the clock: the verb writes stamps the hook
        // never reads, and the registration-window guard never fires.
        stamps_dir: default_stamps_dir(),
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let take = |name: &str| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "--force-ci-cancel" => a.force = true,
            "--in-flight" => {
                a.in_flight = Some(take("--in-flight")?);
                i += 1;
            }
            "--preflight" => a.preflight = true,
            "--no-preflight" => {}
            "--git-bin" => {
                a.git_bin = take("--git-bin")?;
                i += 1;
            }
            "--gh-bin" => {
                a.gh_bin = take("--gh-bin")?;
                i += 1;
            }
            "--fno-bin" => {
                a.fno_bin = take("--fno-bin")?;
                i += 1;
            }
            "--cwd" => {
                a.cwd = PathBuf::from(take("--cwd")?);
                i += 1;
            }
            "--stamps-dir" => {
                a.stamps_dir = PathBuf::from(take("--stamps-dir")?);
                i += 1;
            }
            other if other.starts_with('-') => return Err(unknown_flag(other)),
            _ => {}
        }
        i += 1;
    }
    Ok(a)
}

fn unknown_flag(other: &str) -> String {
    format!("unknown flag: {other}")
}

/// `$FNO_HOME/push-stamps` (else `$HOME/.fno/push-stamps`), matching the
/// hook's own resolution (git-protection.py: `FNO_HOME = env FNO_HOME or
/// ~/.fno`). The hook reads the stamps this verb writes, so both sides must
/// resolve the dir the same way or the registration window never fires.
pub(crate) fn default_stamps_dir() -> PathBuf {
    std::env::var_os("FNO_HOME")
        .map(|h| PathBuf::from(h).join("push-stamps"))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno").join("push-stamps"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp").join(".fno-push-stamps"))
}

/// Preflight modes for the receipt.
#[derive(Clone, Copy)]
enum PreflightMode {
    Full,
    Skipped,
    Absent,
}

impl PreflightMode {
    fn label(self) -> &'static str {
        match self {
            PreflightMode::Full => "full",
            PreflightMode::Skipped => "skipped",
            PreflightMode::Absent => "absent",
        }
    }
}

/// `<repo root>/scripts/ci/preflight.sh`, present and executable. The root
/// comes from `git rev-parse --show-toplevel`, the resolution
/// `_preflight.py` performs.
fn resolve_preflight_runner(git_bin: &str, cwd: &Path) -> Option<PathBuf> {
    let root = run_labeled(
        "pr-push",
        git_bin,
        &["rev-parse", "--show-toplevel"],
        cwd,
        READ_TIMEOUT,
    )
    .ok()
    .filter(|(ok, _, _)| *ok)
    .map(|(_, out, _)| out.trim().to_string())?;
    let runner = PathBuf::from(&root)
        .join("scripts")
        .join("ci")
        .join("preflight.sh");
    let meta = std::fs::metadata(&runner).ok()?;
    if meta.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        Some(runner)
    } else {
        None
    }
}

/// Measure `HEAD..origin/main`. A non-digit answer records
/// `behind=unmeasured:<why>`, never a silent zero - the receipt convention
/// `cli/src/fno/target_cli.py` established.
fn behind(git_bin: &str, cwd: &Path) -> String {
    match run_labeled(
        "pr-push",
        git_bin,
        &["rev-list", "--count", "HEAD..origin/main"],
        cwd,
        READ_TIMEOUT,
    ) {
        Ok((true, out, _))
            if out.trim().chars().all(|c| c.is_ascii_digit()) && !out.trim().is_empty() =>
        {
            out.trim().to_string()
        }
        Ok((true, out, _)) => format!("unmeasured:non-numeric-output:{}", out.trim()),
        Ok((false, _, err)) => format!("unmeasured:git-failed:{}", err.trim()),
        Err(e) => format!("unmeasured:read-failed:{e}"),
    }
}

/// One refusal line per commit in a `git log --format=%h%x1f%B%x1e` capture
/// whose message cites a decision id no ruling carries. Records split on
/// \x1e, the short sha joins its message on \x1f.
fn commit_citation_failures(log: &str) -> Vec<String> {
    let mut failures = Vec::new();
    // One store read serves the whole scan: the known set loads on the
    // first id-citing record and every later record reuses it. A failed
    // load yields the same refusal a single-message scan produces.
    let mut known: Option<Result<std::collections::HashSet<String>, String>> = None;
    for record in log.split('\u{1e}').map(str::trim).filter(|r| !r.is_empty()) {
        let mut parts = record.splitn(2, '\u{1f}');
        let short = parts.next().unwrap_or("?").trim();
        // The whole record is scanned, not only the message half: a body
        // carrying a literal \x1e would otherwise put its tail in the short
        // field, which no real short sha can (hex only, never a hyphen), so
        // scanning it cannot fabricate a hit but can catch a hidden one.
        let bad = if crate::evidence::cites_decision_id(record) {
            if known.is_none() {
                known = Some(crate::evidence::known_decision_ids());
            }
            match known.as_ref().unwrap() {
                Ok(set) => crate::evidence::check_decision_citations_against(record, set),
                Err(reason) => vec![format!(
                    "the decision store could not be read ({reason}); the citation \
                     cannot be checked, and an unchecked citation is not a pass"
                )],
            }
        } else {
            Vec::new()
        };
        if !bad.is_empty() {
            failures.push(format!(
                "commit {short} cites a decision id no ruling carries: {}. \
                 Reword that commit message, then re-run the push.",
                bad.join("; ")
            ));
        }
    }
    failures
}

/// The guarded push, verb entry. Sequence: refuse protected/dirty, fetch,
/// measure behind-before, rebase onto origin/main (or merge it when the branch
/// already holds merges; refuse on conflict, naming the resolver door), measure behind-after,
/// compare against the fetched remote branch (refuse remote-only commits),
/// preflight, in-flight read on the remote head, push exactly once (leased
/// when the branch was rebased), stamp, receipt. Exit codes: 0 pushed, 1
/// preflight red, 2 in flight, 3 refusal, 4 read error.
pub fn run_push(argv: &[String]) -> i32 {
    let a = match parse_verb_args(argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("pr-push: {msg}");
            return 4;
        }
    };
    let cwd = a.cwd.clone();
    let git = a.git_bin.clone();

    if let Some(probe_branch) = a.in_flight.as_deref() {
        let remote_ref = format!("refs/remotes/origin/{probe_branch}");
        let (ok, head, _err) = match run_labeled(
            "pr-push-in-flight",
            &git,
            &["rev-parse", "--verify", "--quiet", remote_ref.as_str()],
            &cwd,
            READ_TIMEOUT,
        ) {
            Ok(result) => result,
            Err(err) => {
                println!(
                    "{}",
                    json!({"branch": probe_branch, "head": "", "error": err})
                );
                return 4;
            }
        };
        let head = if ok {
            head.trim().to_string()
        } else {
            String::new()
        };
        if head.is_empty() {
            println!(
                "{}",
                json!({"branch": probe_branch, "head": head, "in_flight": false})
            );
            return 0;
        }
        let ctx = PushCtx {
            git_bin: git,
            gh_bin: a.gh_bin,
            fno_bin: a.fno_bin,
            cwd,
            stamps_dir: a.stamps_dir,
            force: false,
            lease: None,
        };
        return match in_flight(&ctx, probe_branch, &head) {
            Ok(Some((check, job))) => print_in_flight(probe_branch, &head, &check, job.as_deref()),
            Ok(None) => {
                println!(
                    "{}",
                    json!({"branch": probe_branch, "head": head, "in_flight": false})
                );
                0
            }
            Err(error) => {
                println!(
                    "{}",
                    json!({"branch": probe_branch, "head": head, "error": error})
                );
                4
            }
        };
    }

    // (1) Protected-branch and dirty-tree refusals, exit 3.
    let branch = run_labeled(
        "pr-push",
        &git,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| out.trim().to_string())
    .unwrap_or_default();
    if PROTECTED.contains(&branch.as_str()) {
        eprintln!("pr-push: refusing to push the protected branch '{branch}'");
        return 3;
    }
    let dirty = dirty(&git, &cwd);
    if dirty {
        eprintln!("pr-push: refusing: working tree has uncommitted changes");
        return 3;
    }

    // (2) Fetch. A failed fetch exits 4: never rebase against a stale origin.
    if let Err(e) = crate::pr_rebase::fetch_origin(&cwd, &git) {
        eprintln!("pr-push: git fetch origin failed: {e}");
        return 4;
    }

    // (3) behind-before.
    let before = behind(&git, &cwd);

    // (4) A plain rebase drops every merge commit, so preserve a branch that
    // already merges origin/main by merging the fetched base instead.
    let merge_count = match run_labeled(
        "pr-push",
        &git,
        &["rev-list", "--merges", "--count", "origin/main..HEAD"],
        &cwd,
        READ_TIMEOUT,
    ) {
        Ok((true, out, _)) => match out.trim().parse::<u64>() {
            Ok(count) => count,
            Err(_) => {
                eprintln!(
                    "pr-push: could not count merge commits on the branch (non-numeric output: {}); nothing moved",
                    out.trim()
                );
                return 4;
            }
        },
        Ok((false, _, err)) => {
            eprintln!(
                "pr-push: could not count merge commits on the branch (git failed: {}); nothing moved",
                err.trim()
            );
            return 4;
        }
        Err(err) => {
            eprintln!(
                "pr-push: could not count merge commits on the branch ({err}); nothing moved"
            );
            return 4;
        }
    };
    let integrate = if merge_count == 0 {
        // The door depends on the status: needs_resolver LEFT the rebase
        // in-progress (plain `fno do pr rebase` would dead-end on the dirty
        // guard or abort the caller's resolutions), refused/failed aborted it.
        let (rc, v) = crate::pr_rebase::phase_a("origin/main", &cwd, &git);
        if rc != 0 {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
            let files = v
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let door = match status {
                "needs_resolver" => {
                    "Resolve the conflicts, then run `fno do pr rebase --continue`, \
                     and re-run the push."
                }
                "refused" => {
                    "The rebase was aborted (a guardrail refused auto-resolution); \
                     resolve by hand, then re-run the push."
                }
                "dirty" => "Commit or stash the working-tree changes, then re-run the push.",
                _ => "The rebase was aborted; rebase by hand, then re-run the push.",
            };
            eprintln!(
                "pr-push: the branch is not safely rebasable onto origin/main \
                 (status {status}{}). {door}",
                if files.is_empty() {
                    String::new()
                } else {
                    format!("; files: {files}")
                }
            );
            return 3;
        }
        "rebase"
    } else {
        let (ok, _, _err) = match run_labeled(
            "pr-push",
            &git,
            &["merge", "--no-edit", "origin/main"],
            &cwd,
            READ_TIMEOUT,
        ) {
            Ok(result) => result,
            Err(err) => {
                eprintln!(
                    "pr-push: the branch is not safely rebasable onto origin/main \
                     (status merge_failed). The branch already merges origin/main, so the verb merged instead of rebasing; the merge was aborted. Merge origin/main by hand, resolve, commit, then re-run the push. ({err})"
                );
                return 3;
            }
        };
        if !ok {
            let files = crate::pr_rebase::conflict_files(&git, &cwd);
            let _ = run_labeled("pr-push", &git, &["merge", "--abort"], &cwd, READ_TIMEOUT);
            if files.is_empty() {
                eprintln!(
                    "pr-push: the branch is not safely rebasable onto origin/main \
                     (status merge_failed). The branch already merges origin/main, so the verb merged instead of rebasing; the merge was aborted. Merge origin/main by hand, resolve, commit, then re-run the push."
                );
            } else {
                eprintln!(
                    "pr-push: the branch is not safely rebasable onto origin/main \
                     (status merge_conflict; files: {}). The branch already merges origin/main, so the verb merged instead of rebasing; the merge was aborted. Merge origin/main by hand, resolve, commit, then re-run the push.",
                    files.join(", ")
                );
            }
            return 3;
        }
        "merge"
    };

    // (5) behind-after.
    let after = behind(&git, &cwd);

    // (5b) Ruling citations in the commit messages of exactly the commits
    // this push sends: the rebase has landed, so origin/main..HEAD is the
    // range the push sends. A fabricated ruling converts a refused gate
    // into a claimed pass, and the commit message is where such a claim
    // reaches every downstream reader.
    let (ok, out, err) = match run_labeled(
        "pr-push",
        &git,
        &["log", "--format=%h%x1f%B%x1e", "origin/main..HEAD"],
        &cwd,
        READ_TIMEOUT,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pr-push: the commit-log read failed: {e}");
            return 4;
        }
    };
    if !ok {
        eprintln!("pr-push: the commit-log read failed: {}", err.trim());
        return 4;
    }
    let failures = commit_citation_failures(&out);
    if !failures.is_empty() {
        for line in &failures {
            eprintln!("pr-push: refusing: {line}");
        }
        return 3;
    }

    // (6) The fetched remote head of the SAME-NAME branch, read BEFORE
    // preflight: a doomed push must not first spend a rehearsal of up to an
    // hour. This is the head both the lease and the in-flight read pin to;
    // `@{u}` is wrong for both - a branch born off origin/main tracks main
    // until its first verb push re-points the upstream, and main's checks
    // would read as this branch's runs. A branch with no same-name remote
    // has an empty head: first push, nothing in flight, no lease.
    let remote_ref = format!("origin/{branch}");
    let remote_head = run_labeled(
        "pr-push",
        &git,
        &["rev-parse", "--verify", "--quiet", remote_ref.as_str()],
        &cwd,
        READ_TIMEOUT,
    )
    .map(|(ok, out, _)| {
        if ok {
            out.trim().to_string()
        } else {
            String::new()
        }
    })
    .unwrap_or_default();
    let mut lease = None;
    if !remote_head.is_empty() {
        // Patch equivalence: the remote branch may only be replaced when
        // every commit on it has a patch-equivalent in the rebased HEAD
        // (--cherry-pick drops those pairs from the right-only listing).
        // Run before the lease, so main's commits are already ancestors of
        // HEAD and a GitHub "Update branch" merge is handled too.
        let (ok, out, err) = read_remote(
            &git,
            &cwd,
            &[
                "log",
                "--cherry-pick",
                "--right-only",
                "--no-merges",
                "--format=%h",
                &format!("HEAD...{remote_head}"),
            ],
        );
        if !ok {
            return compare_fail(&remote_ref, &err);
        }
        let remote_only = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !remote_only.is_empty() {
            return remote_only_refusal(&remote_ref, &branch, &remote_only);
        }
        // A merge commit has no patch to pair: --no-merges skips it, so a
        // merge whose tree holds hand-resolved content (a web-UI conflict
        // resolution, manual edits folded into the merge) reads as empty
        // and the lease would delete that content. A merge survives only
        // when its tree equals the automatic merge of its parents, which
        // proves it introduces nothing of its own.
        let (ok, out, err) = read_remote(
            &git,
            &cwd,
            &[
                "log",
                "--right-only",
                "--format=%H %P",
                "--merges",
                &format!("HEAD...{remote_head}"),
            ],
        );
        if !ok {
            return compare_fail(&remote_ref, &err);
        }
        for row in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let mut fields = row.split_whitespace();
            let sha = match fields.next() {
                Some(sha) => sha.to_string(),
                None => continue,
            };
            let parents: Vec<&str> = fields.collect();
            if parents.len() != 2 {
                return remote_only_refusal(&remote_ref, &branch, &format!("merge {sha}"));
            }
            let (ok, mout, merr) = read_remote(
                &git,
                &cwd,
                &["merge-tree", "--write-tree", parents[0], parents[1]],
            );
            let auto = mout.lines().next().unwrap_or("").trim().to_string();
            if auto.is_empty() {
                // No tree answer at all (an old git without --write-tree):
                // the merge cannot be validated, so nothing is pushed.
                return compare_fail(&remote_ref, &merr);
            }
            if !ok {
                return remote_only_refusal(
                    &remote_ref,
                    &branch,
                    &format!("the hand-resolved merge {sha}"),
                );
            }
            let (ok, tout, terr) =
                read_remote(&git, &cwd, &["rev-parse", &format!("{sha}^{{tree}}")]);
            if !ok {
                return compare_fail(&remote_ref, &terr);
            }
            if tout.trim() != auto {
                return remote_only_refusal(
                    &remote_ref,
                    &branch,
                    &format!("the hand-resolved merge {sha}"),
                );
            }
        }
        lease = Some(remote_head.clone());
    }

    let ctx = PushCtx {
        git_bin: git.clone(),
        gh_bin: a.gh_bin.clone(),
        fno_bin: a.fno_bin.clone(),
        cwd: cwd.clone(),
        stamps_dir: a.stamps_dir.clone(),
        force: a.force,
        lease,
    };

    // (7) Read the in-flight state before preflight: a doomed push must not
    // first spend a rehearsal of up to an hour.
    if !a.force {
        match in_flight(&ctx, &branch, &remote_head) {
            Ok(Some((check, job))) => {
                eprintln!(
                    "pr-push: a run is in flight on the remote head, nothing pushed: \
                     check '{check}' job {}.",
                    job.as_deref().unwrap_or("?")
                );
                return 2;
            }
            Ok(None) => {}
            Err(msg) => {
                eprintln!("pr-push: could not read checks ({msg}); nothing pushed");
                return 4;
            }
        }
    }

    // (8) Preflight.
    let (mode, preflight_ok) = if a.preflight {
        match resolve_preflight_runner(&git, &cwd) {
            Some(runner) => {
                let runner_str = runner.to_string_lossy().into_owned();
                let script = runner_str.as_str();
                let (ok, _, err) =
                    run_labeled("pr-push-preflight", script, &[], &cwd, PREFLIGHT_TIMEOUT)
                        .unwrap_or((false, String::new(), "preflight spawn failed".to_string()));
                if ok {
                    (PreflightMode::Full, true)
                } else {
                    eprintln!("pr-push: preflight is red; nothing pushed");
                    if !err.is_empty() {
                        eprint!("{err}");
                    }
                    (PreflightMode::Full, false)
                }
            }
            None => (PreflightMode::Absent, true),
        }
    } else {
        (PreflightMode::Skipped, true)
    };
    if !preflight_ok {
        return 1;
    }

    // (9) In-flight read + the push, leased against the fetched remote sha
    // when the branch was rebased.
    let outcome = guarded_push(&ctx, &remote_head);
    match outcome {
        PushOutcome::Pushed { sha } => {
            println!(
                "pr-push: origin/main behind-before={before} behind-after={after} integrate={integrate} \
                 preflight={} ci={} sha={sha} pushed=1",
                mode.label(),
                if a.force { "bypassed" } else { "settled" }
            );
            0
        }
        PushOutcome::InFlight { check, job } => {
            eprintln!(
                "pr-push: a run is in flight on the remote head, nothing pushed: \
                 check '{check}' job {}.",
                job.as_deref().unwrap_or("?")
            );
            2
        }
        PushOutcome::Unreadable(msg) => {
            eprintln!("pr-push: could not re-read checks ({msg}); nothing pushed");
            4
        }
        PushOutcome::PushFailed(err) => {
            eprintln!("pr-push: the push failed: {}", err.trim());
            4
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_exec(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// A fake gh: green rust-ci check runs, an empty status read, a failed
    /// cli-ci run with no check-run link, and a jobs read answering zero.
    /// A `fail-runs` flag file makes the runs listing exit non-zero.
    fn stub_gh(dir: &std::path::Path) -> PathBuf {
        write_exec(
            dir,
            "gh",
            r#"#!/bin/sh
D="$(dirname "$0")"
for a in "$@"; do case "$a" in
  */check-runs/*/annotations)
    if [ -f "$D/fail-timeout-annotations" ]; then
      echo "gh: annotation read failed" >&2
      exit 1
    fi
    echo '[{"annotation_level":"failure","message":"The job has exceeded the maximum execution time of 35m0s"}]'
    exit 0 ;;
  */check-runs)
    if [ -f "$D/timeout-run" ]; then
      echo '{"check_runs":[{"id":123,"name":"stress","status":"completed","conclusion":"cancelled","output":{"annotations_count":1},"started_at":"2026-09-19T06:00:00Z","completed_at":"2026-09-19T06:40:00Z","html_url":"https://github.com/o/r/actions/runs/123/job/456","check_suite":{"id":88}}]}'
      exit 0
    fi
    if [ -f "$D/cancel-run" ]; then
      echo '{"check_runs":[{"id":124,"name":"cancelled","status":"completed","conclusion":"cancelled","output":{"annotations_count":0},"started_at":"2026-09-19T06:00:00Z","completed_at":"2026-09-19T06:10:00Z","html_url":"https://github.com/o/r/actions/runs/124/job/457","check_suite":{"id":89}}]}'
      exit 0
    fi
    echo '{"check_runs":[{"name":"rust-ci","status":"completed","conclusion":"success","started_at":"2026-09-19T06:00:00Z","completed_at":"2026-09-19T06:05:00Z","html_url":"https://github.com/o/r/actions/runs/35344488345/job/99","check_suite":{"id":7}}]}'
    exit 0 ;;
  */status)
    if [ -f "$D/fail-status" ]; then
      echo "gh: status read failed" >&2
      exit 1
    fi
    echo '{"statuses":[]}'
    exit 0 ;;
  *actions/runs?head_sha=*)
    if [ -f "$D/fail-runs" ]; then
      echo "gh: runs read failed" >&2
      exit 1
    fi
    echo '{"total_count":1,"workflow_runs":[{"id":35366958901,"path":".github/workflows/cli-ci.yml","status":"completed","conclusion":"failure","created_at":"2026-09-19T07:00:00Z","html_url":"https://github.com/o/r/actions/runs/35366958901"}]}'
    exit 0 ;;
  *jobs?per_page=1)
    echo '{"total_count":0}'
    exit 0 ;;
esac; done
echo '{"check_runs":[]}'
exit 1
"#,
        )
    }

    #[test]
    fn ac2_hp_the_zero_job_failure_reads_fail() {
        let dir = tempfile::tempdir().unwrap();
        let gh = stub_gh(dir.path());
        let rows = read_checks_rows(gh.to_str().unwrap(), dir.path(), "abc123").unwrap();
        let hit = rows
            .iter()
            .find(|r| r["name"] == ".github/workflows/cli-ci.yml")
            .expect("the zero-job failure row");
        assert_eq!(hit["bucket"], "fail");
        assert_eq!(
            hit["link"],
            "https://github.com/o/r/actions/runs/35366958901"
        );
        assert_eq!(hit["workflow"], ".github/workflows/cli-ci.yml");
    }

    #[test]
    fn timeout_annotations_escalate_while_unannotated_and_unreadable_cancels_rerun() {
        let message = "The job has exceeded the maximum execution time of 35m0s";
        for (marker, check_name, expected_bucket, expected_signature) in [
            ("timeout-run", "stress", "fail", "timed_out"),
            ("cancel-run", "cancelled", "cancel", "cancelled"),
            (
                "timeout-run fail-timeout-annotations",
                "stress",
                "cancel",
                "cancelled",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let gh = stub_gh(dir.path());
            for flag in marker.split_whitespace() {
                std::fs::write(dir.path().join(flag), b"").unwrap();
            }
            let rows = read_checks_rows(gh.to_str().unwrap(), dir.path(), "abc123").unwrap();
            let row = rows
                .iter()
                .find(|row| row["name"] == check_name)
                .expect("the check run row");
            assert_eq!(row["bucket"], expected_bucket);
            let log = row.get("timeout").and_then(Value::as_str).unwrap_or("");
            if expected_signature == "timed_out" {
                assert_eq!(row["timeout"], message);
            } else {
                assert!(row.get("timeout").is_none());
            }
            let check = row["name"].as_str().unwrap_or("");
            let bucket = row["bucket"].as_str().unwrap_or("");
            let link = row["link"].as_str().unwrap_or("");
            let finding = crate::heal::classify(
                &crate::heal::Ctx {
                    check,
                    log,
                    bucket,
                    link,
                },
                false,
            );
            assert_eq!(finding.signature, expected_signature);
            match expected_signature {
                "timed_out" => assert!(matches!(
                    finding.remedy,
                    crate::heal::Remedy::Escalate { .. }
                )),
                _ => assert!(matches!(
                    finding.remedy,
                    crate::heal::Remedy::Rerun { ref run_id } if run_id == "124"
                )),
            }
        }
    }

    #[test]
    fn ac2_err_a_failed_runs_read_is_err_never_green_rows() {
        let dir = tempfile::tempdir().unwrap();
        let gh = stub_gh(dir.path());
        std::fs::write(dir.path().join("fail-runs"), b"").unwrap();
        let err = read_checks_rows(gh.to_str().unwrap(), dir.path(), "abc123")
            .expect_err("the runs read failed");
        assert!(
            err.contains("actions/runs"),
            "err names the runs read: {err}"
        );
    }

    #[test]
    fn a_failed_status_read_is_err_never_green_rows() {
        let dir = tempfile::tempdir().unwrap();
        let gh = stub_gh(dir.path());
        std::fs::write(dir.path().join("fail-status"), b"").unwrap();
        let err = read_checks_rows(gh.to_str().unwrap(), dir.path(), "abc123")
            .expect_err("the status read failed");
        assert!(err.contains("status"), "err names the status read: {err}");
    }

    // The record parser: clean and torn captures read empty without touching
    // the store (no id, no read); the unknown-id path is the integration
    // test's, which seeds FNO_HOME.
    #[test]
    fn commit_citation_failures_split_records_and_pass_clean_messages() {
        let log = "abc1234\u{1f}clean one\u{1e}\ndef5678\u{1f}clean two\u{1e}";
        assert!(commit_citation_failures(log).is_empty());
        assert!(commit_citation_failures("").is_empty());
        assert!(commit_citation_failures("abc1234\u{1f}no separator").is_empty());
    }
}
