//! `fno-agents pr-push` -- the one guarded push: fetch, rebase onto
//! origin/main, preflight, read the in-flight state, push exactly once,
//! print one receipt. Every push site in `skills/pr` calls this through
//! `fno do pr push`, so a branch is rebased before it moves and a queued CI
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
//! can never drop a commit another writer pushed. The git pre-push hook is
//! unaffected: it refuses protected branches by destination, and this verb
//! refuses them before anything moves.

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
            rows.push(json!({
                "name": run.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "bucket": rest_bucket(run),
                "link": run.get("html_url").and_then(|v| v.as_str()).unwrap_or(""),
                "workflow": run
                    .pointer("/check_suite/id")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                "startedAt": run.get("started_at").and_then(|v| v.as_str()).unwrap_or(""),
                "completedAt": run.get("completed_at").and_then(|v| v.as_str()).unwrap_or(""),
            }));
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
    if !ctx.force && !head.is_empty() {
        match crate::pr_push::read_checks_rows(&ctx.gh_bin, &ctx.cwd, head) {
            Ok(rows) => {
                // Empty rows read two ways: nothing was ever queued, or
                // GitHub has not registered the last push's run yet. The
                // stamp clock covers the second window, the same instrument
                // and threshold the hook uses for hand pushes; registered
                // and settled rows always outrank the clock.
                if rows.is_empty() {
                    if let Some(age) = stamp_age_secs(ctx) {
                        if age < PUSH_DEBOUNCE_SECS {
                            return PushOutcome::InFlight {
                                check: format!(
                                    "last push {age}s ago, its run may not be registered yet"
                                ),
                                job: None,
                            };
                        }
                    }
                }
                let arr = Value::Array(rows);
                if any_pending(&arr) {
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
                    return PushOutcome::InFlight { check, job };
                }
            }
            Err(msg) => return PushOutcome::Unreadable(msg),
        }
    } else if ctx.force {
        emit_bypass_row(ctx);
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
fn stamp_age_secs(ctx: &PushCtx) -> Option<u64> {
    let branch = run_labeled(
        "pr-push",
        &ctx.git_bin,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        &ctx.cwd,
        READ_TIMEOUT,
    )
    .map(|(_, out, _)| out.trim().to_string())
    .ok()?;
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
    no_preflight: bool,
    git_bin: String,
    gh_bin: String,
    fno_bin: String,
    cwd: PathBuf,
    stamps_dir: PathBuf,
}

fn parse_verb_args(argv: &[String]) -> Result<VerbArgs, String> {
    let mut a = VerbArgs {
        force: false,
        no_preflight: false,
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
            "--no-preflight" => a.no_preflight = true,
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

/// The guarded push, verb entry. Sequence: refuse protected/dirty, fetch,
/// measure behind-before, rebase onto origin/main (refuse on conflict,
/// naming the rebase verb as the resolver door), measure behind-after,
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

    // (4) Rebase onto origin/main; a non-clean result is the caller's door.
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

    // (5) behind-after.
    let after = behind(&git, &cwd);

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
        // Run after the rebase, so main's commits are already ancestors of
        // HEAD and a GitHub "Update branch" merge is handled too.
        match run_labeled(
            "pr-push",
            &git,
            &[
                "log",
                "--cherry-pick",
                "--right-only",
                "--no-merges",
                "--format=%h",
                &format!("HEAD...{remote_head}"),
            ],
            &cwd,
            READ_TIMEOUT,
        ) {
            Ok((true, out, _)) => {
                let remote_only = out
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !remote_only.is_empty() {
                    eprintln!(
                        "pr-push: {remote_ref} carries commits this branch lacks: \
                         {remote_only}. Integrate them with git pull --rebase origin \
                         {branch}, then re-run the push. Nothing pushed."
                    );
                    return 3;
                }
                lease = Some(remote_head.clone());
            }
            // Fail closed, the same as the fetch: an incomparable remote is
            // never pushed over.
            Ok((false, _, err)) => {
                let err = if err.trim().is_empty() {
                    "git log failed".to_string()
                } else {
                    err
                };
                eprintln!("pr-push: could not compare with {remote_ref} ({err}); nothing pushed");
                return 4;
            }
            Err(err) => {
                eprintln!("pr-push: could not compare with {remote_ref} ({err}); nothing pushed");
                return 4;
            }
        }
    }

    // (7) Preflight.
    let (mode, preflight_ok) = if a.no_preflight {
        (PreflightMode::Skipped, true)
    } else {
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
    };
    if !preflight_ok {
        return 1;
    }

    // (8) In-flight read + the push, leased against the fetched remote sha
    // when the branch was rebased.
    let ctx = PushCtx {
        git_bin: git.clone(),
        gh_bin: a.gh_bin.clone(),
        fno_bin: a.fno_bin.clone(),
        cwd: cwd.clone(),
        stamps_dir: a.stamps_dir.clone(),
        force: a.force,
        lease,
    };
    let outcome = guarded_push(&ctx, &remote_head);
    match outcome {
        PushOutcome::Pushed { sha } => {
            println!(
                "pr-push: origin/main behind-before={before} behind-after={after} \
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
  */check-runs)
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
}
