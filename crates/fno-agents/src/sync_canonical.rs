//! `sync-canonical`: the post-merge canonical sync, its catch-up sweep, and
//! its staleness alarm in one native verb. One JSON payload on stdin
//! (`{"action":"sync"|"catchup"|"staleness","cwd":...,"pr":N?,"fetch":bool?}`),
//! one JSON answer on stdout: always `exit`, `stdout` lines, `stderr` lines;
//! `catchup` adds `outcome`/`pr_number`/`swept`/`detail`/`stale`; `staleness`
//! adds `state`/`markerless`/`behind`/`detail`. The Python module
//! `cli/src/fno/pr/_sync_canonical.py` is the transport around this verb.
//!
//! Ported from `_sync_canonical.py` with receipt strings kept verbatim (tests
//! and operators grep them). The merged-PR list reads the REST endpoint, not
//! the GraphQL-backed `gh pr list --json`: the quota broker refuses GraphQL
//! reads at low budget, and that refusal must read as `unknown`, never as
//! health.

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

// Long enough to cover a slow sync (build + restart), short enough that a
// crashed holder's lock recovers within one coffee break.
const CLAIM_TTL_MS: i64 = 30 * 60 * 1000;
// Bounds every gh probe: this runs inside the reconcile sweep and the doctor
// report, and a hung gh must not wedge them.
const PROBE_TIMEOUT_SECS: u64 = 30;
// Backstop for a genuinely stuck sync_command; generous (pull + update +
// restart can be slow) and well inside the 30m claim TTL.
const SYNC_COMMAND_TIMEOUT_SECS: u64 = 600;
// gh page size. The window filter is what actually bounds the sweep; this
// only caps the wire network payload for a very busy week.
const CATCHUP_GH_LIMIT: usize = 50;
// Cap captured output echoed in a failure receipt: a pull/update/build can
// spew megabytes, and the receipt is a diagnostic line, not a log sink.
const CAPTURE_TAIL_CHARS: usize = 2000;
// Canonical-wide, NOT per-SHA: the claim's job is that two `fno agents
// restart`s never overlap in one checkout. Exactly-once-per-SHA is the
// marker's job.
const FLIGHT_KEY: &str = "post-merge-sync";

type GhRun = Rc<dyn Fn(&[String], &Path) -> Result<String, GhErr>>;
type GitOrigin = Rc<dyn Fn(&Path) -> Option<String>>;
type ShellRun = Rc<dyn Fn(&str, &Path) -> ShellOutcome>;
type Check = Rc<dyn Fn(&Value) -> Value>;
type Now = Rc<dyn Fn() -> DateTime<Utc>>;

/// The seams the ported logic runs through; `real()` wires the live
/// implementations. No trait hierarchy, five boxed closures.
pub(crate) struct Deps {
    pub gh: GhRun,
    pub git_origin: GitOrigin,
    pub shell: ShellRun,
    pub check: Check,
    pub now: Now,
}

pub(crate) enum GhErr {
    /// gh is not on PATH (spawn failure); the skip exit-0 contract.
    Missing,
    /// gh ran and failed; the detail names the exit and the stderr tail so a
    /// failure never reports under a misleading cause.
    Failed(String),
}

pub(crate) struct ShellOutcome {
    code: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

struct PostMergeCfg {
    sync_command: String,
    sync_paths: Vec<String>,
    auto_run: bool,
    catchup_window_days: i64,
    sync_stale_hours: f64,
}

#[derive(Clone)]
struct Markerless {
    number: u64,
    sha: String,
    merged_at: String,
}

struct Staleness {
    state: String,
    markerless: Vec<Markerless>,
    behind: Option<u64>,
    detail: String,
}

/// Read the `[post_merge]` config table, or the defaults when absent.
fn read_config(cwd: &Path) -> PostMergeCfg {
    let table = crate::agents_config::config_table_merged(cwd, &["post_merge"]);
    let get = |k: &str| table.as_ref().and_then(|t| t.get(k));
    PostMergeCfg {
        sync_command: get("sync_command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sync_paths: get("sync_paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        auto_run: get("auto_run").and_then(|v| v.as_bool()).unwrap_or(false),
        catchup_window_days: get("catchup_window_days")
            .and_then(|v| v.as_integer())
            .unwrap_or(3),
        sync_stale_hours: get("sync_stale_hours")
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
            .unwrap_or(24.0),
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn synced_marker(canonical: &Path, sha: &str) -> PathBuf {
    canonical.join(".fno").join("post-merge-synced").join(sha)
}

fn sha12(sha: &str) -> String {
    sha.chars().take(12).collect()
}

fn tail(text: &str) -> String {
    // A char boundary can sit inside the last window, so walk back to one
    // rather than panic on a multi-byte char split.
    let mut start = text.len().saturating_sub(CAPTURE_TAIL_CHARS);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    if start > 0 {
        format!("...{}", &text[start..])
    } else {
        text.to_string()
    }
}

// Port of Python fnmatch.fnmatch (POSIX case rules): `*` crosses `/`, `?`
// is one char, `[...]` a class with `!` negation and `-` ranges.
fn fnmatch(name: &str, pat: &str) -> bool {
    match_class(name.as_bytes(), pat.as_bytes())
}

fn match_class(n: &[u8], p: &[u8]) -> bool {
    match p.split_first() {
        None => n.is_empty(),
        Some((b'*', rest)) => {
            if rest.is_empty() {
                return true;
            }
            (0..=n.len()).any(|i| match_class(&n[i..], rest))
        }
        Some((b'?', rest)) => !n.is_empty() && match_class(&n[1..], rest),
        Some((b'[', _)) => match char_class(n.first().copied(), p) {
            // Unterminated class reads as a literal `[`.
            None => !n.is_empty() && n[0] == b'[' && match_class(&n[1..], &p[1..]),
            Some((hit, used)) => hit && !n.is_empty() && match_class(&n[1..], &p[used..]),
        },
        Some((c, rest)) => !n.is_empty() && n[0] == *c && match_class(&n[1..], rest),
    }
}

/// `[seq]` / `[!seq]` matcher; returns (matched, pattern bytes consumed).
/// None on an unterminated class, which the caller reads as a literal `[`.
fn char_class(c: Option<u8>, p: &[u8]) -> Option<(bool, usize)> {
    let negate = matches!(p.get(1), Some(b'!'));
    let mut i = if negate { 2 } else { 1 };
    let start = i;
    let mut hit = false;
    loop {
        match p.get(i) {
            None => return None,
            Some(b']') if i > start => {
                return Some((if negate { !hit } else { hit }, i + 1));
            }
            Some(&lo)
                if p.get(i + 1) == Some(&b'-')
                    && matches!(p.get(i + 2), Some(&hi) if hi != b']') =>
            {
                let hi = p[i + 2];
                if let Some(c) = c {
                    if c >= lo && c <= hi {
                        hit = true;
                    }
                }
                i += 3;
            }
            Some(&ch) => {
                if c == Some(ch) {
                    hit = true;
                }
                i += 1;
            }
        }
    }
}

fn write_marker(marker: &Path, stderr: &mut Vec<String>) {
    // Best-effort: the sync already ran, so a marker failure only makes the
    // next sweep re-run (safe direction). A failure is signalled so the
    // operator can see WHY the sync re-runs each sweep rather than it
    // looking like normal.
    if let Some(dir) = marker.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            push_marker_err(e, stderr);
            return;
        }
    }
    if let Err(e) = std::fs::File::create(marker) {
        push_marker_err(e, stderr);
    }
}

fn push_marker_err(e: std::io::Error, stderr: &mut Vec<String>) {
    stderr.push(format!(
        "post-merge sync: marker write failed ({e}); will re-run next sweep"
    ));
}

fn parse_iso(raw: &str) -> Option<DateTime<Utc>> {
    let s = raw.trim();
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        // An offset-less stamp would silently drop a merge from the sweep,
        // the exact lie the markerless read exists to catch; read it as UTC
        // the way the Python parser did.
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
}

fn fmt_ts(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "no expiry".to_string())
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn remote_slug(url: &str) -> Option<String> {
    // Handles both `git@github.com:owner/repo.git` and
    // `https://github.com/owner/repo(.git)`.
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?:github\.com[:/])([^/]+)/(.+?)(?:\.git)?/?$"#).unwrap())
        .captures(url)
        .map(|m| format!("{}/{}", &m[1], &m[2]))
}

fn slug_from_pr_url(url: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"github\.com/([^/]+)/([^/]+)/pull/\d+").unwrap())
        .captures(url)
        .map(|m| format!("{}/{}", &m[1], &m[2]))
}

// ---------------------------------------------------------------------------
// Real deps
// ---------------------------------------------------------------------------

fn real_gh(args: &[String], cwd: &Path) -> Result<String, GhErr> {
    let mut cmd = std::process::Command::new("gh");
    cmd.args(args).current_dir(cwd);
    match crate::bounded_cmd::output_with_timeout(cmd, PROBE_TIMEOUT_SECS) {
        None => Err(GhErr::Missing),
        Some(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Some(out) => {
            let code = out.status.code().unwrap_or(-1);
            Err(GhErr::Failed(format!(
                "exit {code}: {}",
                tail(String::from_utf8_lossy(&out.stderr).trim())
            )))
        }
    }
}

fn real_git_origin(canonical: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(canonical)
        .args(["remote", "get-url", "origin"]);
    let out = crate::bounded_cmd::output_with_timeout(cmd, PROBE_TIMEOUT_SECS)?;
    if !out.status.success() {
        return None;
    }
    remote_slug(String::from_utf8_lossy(&out.stdout).trim())
}

fn real_shell(command: &str, cwd: &Path) -> ShellOutcome {
    // Output goes to temp FILES, never pipes: a sync_command ending in
    // `fno agents restart` detaches a daemon that inherits the child's
    // stdout/stderr and never closes them, and with pipes the parent blocks
    // on the EOF that live daemon never sends. A plain file has no
    // EOF-reader, so wait() returns as soon as the shell child exits; a
    // detached grandchild merely keeps appending to a file we have already
    // read.
    let dir = std::env::temp_dir().join(format!("fno-sync-{}-{}", std::process::id(), nanos()));
    if std::fs::create_dir_all(&dir).is_err() {
        return ShellOutcome {
            code: 127,
            stdout: String::new(),
            stderr: format!(
                "post-merge sync: cannot create capture dir {}",
                dir.display()
            ),
            timed_out: false,
        };
    }
    let out_path = dir.join("stdout");
    let err_path = dir.join("stderr");
    let outcome = (|| -> std::io::Result<ShellOutcome> {
        let outf = std::fs::File::create(&out_path)?;
        let errf = std::fs::File::create(&err_path)?;
        let mut child = std::process::Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(outf)
            .stderr(errf)
            .spawn()?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(SYNC_COMMAND_TIMEOUT_SECS);
        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(s) => break s,
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                None => {
                    let _ = child.kill();
                    let s = child.wait()?;
                    timed_out = true;
                    break s;
                }
            }
        };
        Ok(ShellOutcome {
            // A killed child reads as a signal (no code); the timed-out path
            // reports the 124 the receipt contract names.
            code: if timed_out {
                124
            } else {
                status.code().unwrap_or(-1)
            },
            stdout: read_tail_text(&out_path),
            stderr: read_tail_text(&err_path),
            timed_out,
        })
    })();
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap_or_else(|e| ShellOutcome {
        code: 127,
        stdout: String::new(),
        stderr: format!("post-merge sync: failed to run bash -lc: {e}"),
        timed_out: false,
    })
}

// Read only the tail off disk (bounded memory): a sync_command can emit for
// the full timeout window, and loading the whole capture would balloon the
// caller's memory for a receipt that discards all but the tail.
fn read_tail_text(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let max_bytes = CAPTURE_TAIL_CHARS * 4; // UTF-8 worst case covers the char budget
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let seek_back = (size as i64).min(max_bytes as i64);
    if f.seek(SeekFrom::End(-seek_back)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

impl Deps {
    pub(crate) fn real() -> Deps {
        Deps {
            gh: Rc::new(real_gh),
            git_origin: Rc::new(real_git_origin),
            shell: Rc::new(real_shell),
            check: Rc::new(|payload| crate::canonical_check::build_answer(payload)),
            now: Rc::new(chrono::Utc::now),
        }
    }
}

// ---------------------------------------------------------------------------
// sync: the merge-time sync for one PR
// ---------------------------------------------------------------------------

type SyncResult = (i32, Vec<String>, Vec<String>);

/// The `sync` action for one merged PR: guard chain, then the sync_command
/// under the canonical-wide lease. `shell` is a parameter (not `deps.shell`)
/// so the catch-up can wrap it and observe whether it ran.
fn run_sync_with_shell(deps: &Deps, cwd: &Path, pr: u64, shell: &ShellRun) -> SyncResult {
    let cfg = read_config(cwd);
    let mut stdout: Vec<String> = Vec::new();
    let mut stderr: Vec<String> = Vec::new();

    // 1. Unset command -> clean no-op (opt-in).
    if cfg.sync_command.trim().is_empty() {
        stdout.push("post-merge sync: not configured".to_string());
        return (0, stdout, stderr);
    }

    // 2. Resolve the canonical checkout (targets canonical even from a worktree).
    let canonical = crate::paths::canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());

    let Some(origin) = (deps.git_origin)(&canonical) else {
        stderr.push(format!(
            "post-merge sync: canonical {} has no resolvable origin; skipping",
            canonical.display()
        ));
        return (0, stdout, stderr);
    };

    // 3. Read merge SHA + files from GitHub (the canonical has not pulled yet).
    let args: Vec<String> = vec![
        "pr".into(),
        "view".into(),
        pr.to_string(),
        "--repo".into(),
        origin.clone(),
        "--json".into(),
        "state,mergeCommit,files,url".into(),
    ];
    let raw = match (deps.gh)(&args, &canonical) {
        Ok(raw) => raw,
        Err(GhErr::Missing) => {
            stderr.push("post-merge sync: gh not found on PATH; skipping".to_string());
            return (0, stdout, stderr);
        }
        Err(GhErr::Failed(d)) => {
            // No marker; next reconcile retries.
            stderr.push(format!("post-merge sync: gh pr view #{pr} failed: {d}"));
            return (1, stdout, stderr);
        }
    };
    let row: Value = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        Err(e) => {
            stderr.push(format!(
                "post-merge sync: gh pr view #{pr} failed: non-JSON gh output: {e}"
            ));
            return (1, stdout, stderr);
        }
    };

    let state = row.get("state").and_then(|v| v.as_str()).unwrap_or("");
    if state != "MERGED" {
        stdout.push(format!(
            "post-merge sync: PR #{pr} not merged (state={state}); skipping"
        ));
        return (0, stdout, stderr);
    }

    // Wrong-repo guard: the PR's own url must sit in the resolved canonical's
    // repo before we let sync_command run `git checkout main` there. GitHub
    // owner/repo are case-insensitive, so a casing mismatch is not a refusal.
    let pr_slug = row
        .get("url")
        .and_then(|v| v.as_str())
        .and_then(slug_from_pr_url);
    if let Some(s) = &pr_slug {
        if !s.eq_ignore_ascii_case(&origin) {
            stderr.push(format!(
                "post-merge sync: PR repo {s} != canonical origin {origin}; \
                 refusing to sync the wrong repo"
            ));
            return (0, stdout, stderr);
        }
    }

    let Some(sha) = row
        .pointer("/mergeCommit/oid")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        stdout.push(format!(
            "post-merge sync: PR #{pr} has no merge commit yet; skipping"
        ));
        return (0, stdout, stderr);
    };

    // 4. Dedup by merge SHA (cross-session).
    let marker = synced_marker(&canonical, &sha);
    if marker.exists() {
        stdout.push(format!("post-merge sync: already synced {}", sha12(&sha)));
        return (0, stdout, stderr);
    }

    // 5. Single-flight lock (canonical-scoped, TTL-live). The claim lives in
    // the name of THIS process: we are the one doing the work, so the lock
    // dies with us.
    let holder = format!("sync-canonical:{pr}:{}:{}", std::process::id(), nanos());
    let make_opts = || crate::claims::AcquireOpts {
        pid: Some(std::process::id()),
        pid_unavailable: false,
        ttl_ms: Some(CLAIM_TTL_MS),
        reason: Some("single-flight: post-merge canonical sync".into()),
        metadata: None,
        pid_provenance: Some(crate::claims::HOLDER_PROCESS.to_string()),
        root: Some(canonical.clone()),
        events_dir: None,
    };
    let by: Option<String> = match crate::claims::acquire(FLIGHT_KEY, &holder, make_opts()) {
        crate::claims::AcquireOutcome::Acquired(_) => None,
        crate::claims::AcquireOutcome::HeldByOther { holder: h, .. } => {
            let expires = crate::claims::status(FLIGHT_KEY, Some(&canonical))
                .1
                .and_then(|r| r.expires_at)
                .map(fmt_ts)
                .unwrap_or_else(|| "no expiry".to_string());
            Some(format!(" (held by {h}, expires {expires})"))
        }
        // An unavailable gate reads the same way as held: skip, fail-open.
        crate::claims::AcquireOutcome::Error(_) => Some(String::new()),
    };
    if let Some(by) = by {
        stdout.push(format!(
            "post-merge sync: in progress elsewhere for {}{by}; skipping",
            sha12(&sha)
        ));
        return (0, stdout, stderr);
    }

    let code = sync_under_lease(
        deps,
        &cfg,
        &canonical,
        pr,
        &sha,
        &row,
        shell,
        &mut stdout,
        &mut stderr,
    );
    let _ = crate::claims::release(FLIGHT_KEY, &holder, Some(&canonical), None);
    (code, stdout, stderr)
}

/// The work under the lease; the caller owns acquire + release.
fn sync_under_lease(
    deps: &Deps,
    cfg: &PostMergeCfg,
    canonical: &Path,
    pr: u64,
    sha: &str,
    row: &Value,
    shell: &ShellRun,
    stdout: &mut Vec<String>,
    stderr: &mut Vec<String>,
) -> i32 {
    // Re-check under the lock (double-checked): a loser that read the marker
    // as absent before the winner wrote it must not re-run sync_command
    // after the winner releases.
    let marker = synced_marker(canonical, sha);
    if marker.exists() {
        stdout.push(format!("post-merge sync: already synced {}", sha12(sha)));
        return 0;
    }

    // 6. Path-gate (globs computed from the GitHub file list, not local git).
    let files: Vec<String> = row
        .get("files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let globs = &cfg.sync_paths;
    if !globs.is_empty() && !files.iter().any(|f| globs.iter().any(|g| fnmatch(f, g))) {
        write_marker(&marker, stderr);
        stdout.push(format!(
            "post-merge sync: skipped - no buildable change ({} files, none matched [{}]); marked {}",
            files.len(),
            globs.iter().map(|g| format!("'{g}'")).collect::<Vec<_>>().join(", "),
            sha12(sha)
        ));
        return 0;
    }

    // 6.5 Divergence gate: `canonical-check` owns the read - the
    // dirty-overlap refusal and the ahead-of-origin refusal, the two shapes a
    // raw `git pull` failure used to hide behind "divergent branches".
    // Report, never auto-recover; an answer that cannot be had must not
    // refuse every sync (the in-process read is fail-open by construction).
    let answer = (deps.check)(&json!({
        "canonical": canonical.display().to_string(),
        "files": files,
        "pr": pr,
        "sha": sha,
    }));
    if let Some(refusal) = answer
        .get("refusal")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        stderr.push(refusal.to_string());
        return 1;
    }

    // 7. Run sync_command in the canonical via a login shell so uv/cargo/npm
    // on the shell-rc PATH resolve (a bare `bash -c` would miss them).
    stdout.push(format!(
        "post-merge sync: running in {} for {}",
        canonical.display(),
        sha12(sha)
    ));
    let out = shell(&cfg.sync_command, canonical);
    if out.timed_out {
        stderr.push(format!(
            "post-merge sync: sync_command timed out after {}s; marker withheld, will retry",
            SYNC_COMMAND_TIMEOUT_SECS
        ));
    }
    if out.code == 0 {
        write_marker(&marker, stderr);
        stdout.push(format!("post-merge sync: synced {}", sha12(sha)));
        return 0;
    }

    // Surface the command and its output, not just the exit code: a real
    // failure here was a one-word typo the receipt hid for days. The marker
    // stays withheld, so retry behaviour is unchanged.
    let mut parts = vec![
        format!(
            "post-merge sync: failed (exit {}); marker withheld, will retry",
            out.code
        ),
        format!("  command: {}", cfg.sync_command),
    ];
    if !out.stderr.trim().is_empty() {
        parts.push(format!("  stderr: {}", tail(&out.stderr)));
    }
    if !out.stdout.trim().is_empty() {
        parts.push(format!("  stdout: {}", tail(&out.stdout)));
    }
    stderr.extend(parts);
    out.code
}

// ---------------------------------------------------------------------------
// Catch-up sweep + staleness alarm
// ---------------------------------------------------------------------------

/// Merged PRs in the window, newest-first, or the unknown detail when gh
/// cannot answer. REST, not the GraphQL-backed `gh pr list --json` (the
/// quota broker refuses GraphQL reads at low budget and that must read as
/// `unknown`, never as health).
fn merged_rows(
    deps: &Deps,
    canonical: &Path,
    window_days: i64,
    now: DateTime<Utc>,
) -> Result<Vec<Markerless>, String> {
    let path = match (deps.git_origin)(canonical) {
        Some(o) => format!("repos/{o}/pulls?state=closed&sort=updated&direction=desc&per_page={CATCHUP_GH_LIMIT}"),
        // gh expands the placeholders from the cwd's own repo, preserving the
        // old `gh pr list` behaviour when the slug cannot be pre-resolved.
        None => format!("repos/{{owner}}/{{repo}}/pulls?state=closed&sort=updated&direction=desc&per_page={CATCHUP_GH_LIMIT}"),
    };
    let args = vec!["api".to_string(), path];
    let raw = match (deps.gh)(&args, canonical) {
        Ok(raw) => raw,
        Err(GhErr::Missing) => return Err("gh unavailable or unauthenticated".to_string()),
        Err(GhErr::Failed(d)) => return Err(format!("gh unavailable or unauthenticated (gh {d})")),
    };
    let rows: Value = serde_json::from_str(raw.trim())
        .map_err(|e| format!("gh unavailable or unauthenticated (non-JSON gh output: {e})"))?;
    let arr = rows
        .as_array()
        .ok_or_else(|| "gh unavailable or unauthenticated (non-list gh output)".to_string())?;
    let cutoff = now - chrono::Duration::days(window_days.max(0));
    let mut out: Vec<Markerless> = Vec::new();
    for row in arr {
        let Some(number) = row.get("number").and_then(|v| v.as_u64()) else {
            continue;
        };
        let Some(sha) = row
            .get("merge_commit_sha")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Some(merged_at_raw) = row.get("merged_at").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(merged_at) = parse_iso(merged_at_raw) else {
            continue;
        };
        if merged_at < cutoff {
            continue;
        }
        out.push(Markerless {
            number,
            sha,
            merged_at: merged_at_raw.to_string(),
        });
    }
    out.sort_by(|a, b| {
        let ka = parse_iso(&a.merged_at);
        let kb = parse_iso(&b.merged_at);
        kb.cmp(&ka)
    });
    Ok(out)
}

fn compute_staleness(deps: &Deps, cwd: &Path, fetch: bool) -> Staleness {
    let cfg = read_config(cwd);
    let canonical = crate::paths::canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let now = (deps.now)();
    let markerless = match merged_rows(deps, &canonical, cfg.catchup_window_days, now) {
        Ok(rows) => rows
            .into_iter()
            .filter(|r| !synced_marker(&canonical, &r.sha).exists())
            .collect::<Vec<Markerless>>(),
        Err(detail) => {
            return Staleness {
                state: "unknown".to_string(),
                markerless: Vec::new(),
                behind: None,
                detail,
            }
        }
    };

    let answer = (deps.check)(&json!({
        "canonical": canonical.display().to_string(),
        "fetch": fetch,
    }));
    let behind = answer.get("behind").and_then(|v| v.as_u64());
    let ahead = answer.get("ahead").and_then(|v| v.as_u64());

    // ANY markerless merge past the threshold is stale, not just the newest
    // one: a marker does not imply a pull (a merge missing the sync_paths
    // globs is marked without one), so the newest being marked proves nothing
    // about the merges behind it.
    let overdue: Vec<&Markerless> = markerless
        .iter()
        .filter(|r| {
            parse_iso(&r.merged_at)
                .map(|t| (now - t).num_seconds() as f64 / 3600.0 > cfg.sync_stale_hours)
                .unwrap_or(false)
        })
        .collect();
    let mut detail = String::new();
    let mut stale = !overdue.is_empty();
    if let Some(oldest) = overdue.last() {
        let age_h = parse_iso(&oldest.merged_at)
            .map(|t| (now - t).num_seconds() as f64 / 3600.0)
            .unwrap_or(0.0);
        detail = format!("PR #{} merged {age_h:.0}h ago, never synced", oldest.number);
        if overdue.len() > 1 {
            detail.push_str(&format!(" (+{} more)", overdue.len() - 1));
        }
    }
    if behind.unwrap_or(0) > 0 || ahead.unwrap_or(0) > 0 {
        stale = true;
    }
    // The verb's notes ride even when the verdict stays fresh (naming the
    // dirt is doctor's job); they never flip it by themselves.
    let notes: Vec<&str> = answer
        .get("notes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str())
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !notes.is_empty() {
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        detail.push_str(&notes.join("; "));
    }

    Staleness {
        state: if stale { "stale" } else { "fresh" }.to_string(),
        markerless,
        behind,
        detail,
    }
}

struct CatchupOut {
    exit: i32,
    stdout: Vec<String>,
    stderr: Vec<String>,
    outcome: String,
    pr_number: Option<u64>,
    swept: u64,
    detail: String,
    stale: bool,
}

impl CatchupOut {
    fn bare(outcome: &str) -> CatchupOut {
        CatchupOut {
            exit: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            outcome: outcome.to_string(),
            pr_number: None,
            swept: 0,
            detail: String::new(),
            stale: false,
        }
    }
}

/// Sync the canonical for any merge the event-time triggers missed.
/// Newest-only: one sync regardless of how many merges piled up, because a
/// single pull brings HEAD current for all of them. The older swept SHAs are
/// marker-stamped afterwards so they stop reading as stale - but ONLY once
/// the newest SHA's marker proves the sync actually landed, so a claim-held
/// skip or a failed sync can never backdate a lie.
fn run_catchup(deps: &Deps, cwd: &Path) -> CatchupOut {
    let cfg = read_config(cwd);
    if !cfg.auto_run {
        return CatchupOut::bare("disabled");
    }
    let canonical = crate::paths::canonical_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());

    let st = compute_staleness(deps, cwd, false);
    if st.state == "unknown" {
        let mut out = CatchupOut::bare("unknown");
        out.detail = st.detail.clone();
        out.stderr
            .push(format!("post-merge sync catch-up: {}; skipping", st.detail));
        return out;
    }
    if st.markerless.is_empty() {
        // Carry the staleness detail even here. Every marker can be present
        // while the canonical is still behind origin - that is what "the
        // markers lie" looks like, and it is the one state the sweep cannot
        // act on, so the least it can do is say so rather than report a flat
        // "fresh".
        let mut out = CatchupOut::bare("fresh");
        out.detail = st.detail;
        out.stale = st.state == "stale";
        return out;
    }

    let newest = st.markerless[0].clone();
    // Stamping the older merges is only sound if the newest one actually
    // PULLED, and neither the exit code nor the marker proves that:
    // run_sync_canonical returns 0 and writes a marker for a merge that
    // misses the sync_paths globs (a docs-only merge needs no build), having
    // run nothing. The proof is whether the shell was entered, observed
    // through this wrapper.
    let pulled = Rc::new(RefCell::new(false));
    let inner = Rc::clone(&deps.shell);
    let flag = Rc::clone(&pulled);
    let tracking: ShellRun = Rc::new(move |command, dir| {
        *flag.borrow_mut() = true;
        inner(command, dir)
    });

    // Catch-up runs inline during `reconcile --json`; every progress line
    // from the wrapped sync must land on stderr, never stdout, or the JSON
    // caller's stdout stops parsing. The direct `sync` action keeps its own
    // stdout untouched - only this wrapper redirects.
    let (rc, so, se) = run_sync_with_shell(deps, cwd, newest.number, &tracking);
    if rc != 0 {
        let mut out = CatchupOut::bare("failed");
        out.exit = rc;
        out.stderr = so.into_iter().chain(se).collect();
        out.stderr.push(format!(
            "post-merge sync catch-up: sync of PR #{} failed (exit {rc}); markers withheld, will retry",
            newest.number
        ));
        out.pr_number = Some(newest.number);
        out.detail = format!("exit {rc}");
        out.stale = st.state == "stale";
        return out;
    }
    let stdout: Vec<String> = Vec::new();
    let mut stderr: Vec<String> = so.into_iter().chain(se).collect();

    if !synced_marker(&canonical, &newest.sha).exists() {
        let mut out = CatchupOut::bare("skipped");
        out.stdout = stdout;
        out.stderr = stderr;
        out.pr_number = Some(newest.number);
        out.detail = "sync declined (claim held or out of scope)".to_string();
        out.stale = st.state == "stale";
        return out;
    }

    if !*pulled.borrow() {
        // The newest merge needed no sync, so it is marked but nothing was
        // pulled. The older merges keep their claim on the next sweep, which
        // will pick the newest REMAINING one and pull for real.
        let mut out = CatchupOut::bare("marked");
        out.stdout = stdout;
        out.stderr = stderr;
        out.pr_number = Some(newest.number);
        out.detail = "no buildable change; older merges still pending".to_string();
        out.stale = st.state == "stale";
        return out;
    }

    let mut swept: u64 = 0;
    for row in &st.markerless[1..] {
        let marker = synced_marker(&canonical, &row.sha);
        if !marker.exists() {
            write_marker(&marker, &mut stderr);
            swept += 1;
        }
    }
    stderr.push(format!(
        "post-merge sync catch-up: synced PR #{}{}",
        newest.number,
        if swept > 0 {
            format!(", stamped {swept} older merge(s)")
        } else {
            String::new()
        }
    ));
    let mut out = CatchupOut::bare("synced");
    out.stdout = stdout;
    out.stderr = stderr;
    out.pr_number = Some(newest.number);
    out.swept = swept;
    out
}

// ---------------------------------------------------------------------------
// Verb entry
// ---------------------------------------------------------------------------

/// The verb entry: one JSON payload on stdin, one JSON answer on stdout, exit
/// 0 (the answer carries the real `exit`); malformed stdin prints
/// `sync-canonical: bad payload: <e>` and exits 2 (same contract as
/// `canonical-check`).
pub fn run_sync_canonical_verb(_args: &[String]) -> i32 {
    use std::io::Read;
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        eprint!("sync-canonical: cannot read payload\n");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("sync-canonical: bad payload: {e}\n");
            return 2;
        }
    };
    let deps = Deps::real();
    let cwd = PathBuf::from(parsed.get("cwd").and_then(|v| v.as_str()).unwrap_or("."));
    let action = parsed.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let answer = match action {
        "sync" => {
            let Some(pr) = parsed.get("pr").and_then(|v| v.as_u64()) else {
                eprint!("sync-canonical: sync requires an integer pr\n");
                return 2;
            };
            let (exit, stdout, stderr) = run_sync_with_shell(&deps, &cwd, pr, &deps.shell);
            json!({"exit": exit, "stdout": stdout, "stderr": stderr})
        }
        "catchup" => {
            let out = run_catchup(&deps, &cwd);
            json!({
                "exit": out.exit,
                "stdout": out.stdout,
                "stderr": out.stderr,
                "outcome": out.outcome,
                "pr_number": out.pr_number,
                "swept": out.swept,
                "detail": out.detail,
                "stale": out.stale,
            })
        }
        "staleness" => {
            let fetch = parsed
                .get("fetch")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let st = compute_staleness(&deps, &cwd, fetch);
            json!({
                "exit": 0,
                "stdout": [],
                "stderr": [],
                "state": st.state,
                "markerless": st.markerless.iter().map(|r| json!({
                    "number": r.number, "sha": r.sha, "merged_at": r.merged_at,
                })).collect::<Vec<_>>(),
                "behind": st.behind,
                "detail": st.detail,
            })
        }
        _ => {
            eprint!("sync-canonical: unknown action {action:?}\n");
            return 2;
        }
    };
    println!("{answer}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const CFG_BODY: &str = "[post_merge]\n\
        sync_command = 'echo hi'\n\
        sync_paths = ['cli/**']\n\
        auto_run = true\n\
        catchup_window_days = 3\n\
        sync_stale_hours = 24\n";

    fn write_cfg(cwd: &Path, body: &str) {
        std::fs::create_dir_all(cwd.join(".fno")).unwrap();
        std::fs::write(cwd.join(".fno/config.toml"), body).unwrap();
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, 12, 0, 0).unwrap()
    }

    fn iso_at_hours_before(hours: i64) -> String {
        (fixed_now() - chrono::Duration::hours(hours))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn view_row(state: &str, sha: &str, files: &[&str], url: &str) -> String {
        json!({
            "state": state,
            "mergeCommit": {"oid": sha},
            "files": files.iter().map(|f| json!({"path": f})).collect::<Vec<_>>(),
            "url": url,
        })
        .to_string()
    }

    fn list_rows(rows: &[(u64, &str, &str)]) -> String {
        Value::Array(
            rows.iter()
                .map(|(n, m, s)| json!({"number": n, "merged_at": m, "merge_commit_sha": s}))
                .collect(),
        )
        .to_string()
    }

    /// What the stub's `api` (REST list) branch answers.
    enum ListStub {
        Rows(String),
        Missing,
        Failed(String),
    }

    /// gh stub serving `pr view` and `api` answers.
    fn gh_stub(view: String, list: ListStub) -> GhRun {
        Rc::new(move |args: &[String], _cwd: &Path| {
            if args.first().map(String::as_str) == Some("pr") {
                return Ok(view.clone());
            }
            match &list {
                ListStub::Rows(raw) => Ok(raw.clone()),
                ListStub::Missing => Err(GhErr::Missing),
                ListStub::Failed(d) => Err(GhErr::Failed(d.clone())),
            }
        })
    }

    fn ok_shell() -> ShellRun {
        Rc::new(|_c, _d| ShellOutcome {
            code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            timed_out: false,
        })
    }

    /// Records whether it ran, then succeeds (or fails with the given code).
    fn spy_shell(code: i32) -> (ShellRun, Rc<RefCell<bool>>) {
        let ran = Rc::new(RefCell::new(false));
        let flag = Rc::clone(&ran);
        let shell: ShellRun = Rc::new(move |_c, _d| {
            *flag.borrow_mut() = true;
            ShellOutcome {
                code,
                stdout: if code == 0 {
                    "out".to_string()
                } else {
                    String::new()
                },
                stderr: if code != 0 {
                    "boom".to_string()
                } else {
                    String::new()
                },
                timed_out: false,
            }
        });
        (shell, ran)
    }

    fn deps_with(gh: GhRun, shell: ShellRun, check: Check, now: Now) -> Deps {
        Deps {
            gh,
            git_origin: Rc::new(|_| Some("owner/repo".to_string())),
            shell,
            check,
            now,
        }
    }

    const SHA: &str = "abcdef1234567890abcdef1234567890abcdef12";
    const PR_URL: &str = "https://github.com/owner/repo/pull/5";

    fn sync_deps(view: String, shell: ShellRun, check: Check) -> Deps {
        deps_with(
            gh_stub(view, ListStub::Rows("[]".to_string())),
            shell,
            check,
            Rc::new(fixed_now),
        )
    }

    #[test]
    fn fnmatch_matches_python_semantics() {
        assert!(fnmatch("cli/x/y.py", "cli/**"));
        assert!(fnmatch("a.md", "*.md"));
        assert!(fnmatch("d/b.md", "*.md")); // `*` crosses `/` like fnmatch
        assert!(fnmatch("abc", "a?c"));
        assert!(!fnmatch("ab", "a?c"));
        assert!(fnmatch("b", "[abc]"));
        assert!(!fnmatch("d", "[abc]"));
        assert!(fnmatch("d", "[!abc]"));
        assert!(fnmatch("x2", "x[0-9]"));
        assert!(!fnmatch("x9x", "x[0-9]"));
    }

    #[test]
    fn synced_runs_shell_writes_marker_exits_zero() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(stdout
            .last()
            .unwrap()
            .ends_with(&format!("post-merge sync: synced {}", &SHA[..12])));
        assert!(*ran.borrow());
        assert!(synced_marker(tmp.path(), SHA).exists());
    }

    #[test]
    fn wrong_repo_refuses_and_never_shells() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row(
                "MERGED",
                SHA,
                &["cli/x.py"],
                "https://github.com/other/repo/pull/5",
            ),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stderr
            .join("\n")
            .contains("refusing to sync the wrong repo"));
    }

    #[test]
    fn wrong_repo_is_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row(
                "MERGED",
                SHA,
                &["cli/x.py"],
                "https://github.com/Owner/Repo/pull/5",
            ),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(
            stderr.is_empty(),
            "casing mismatch must not refuse: {stderr:?}"
        );
        assert!(*ran.borrow(), "a casing-only mismatch must proceed to sync");
    }

    #[test]
    fn held_lease_skips_without_shelling() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        // A live other holder (our pid proves live).
        let opts = crate::claims::AcquireOpts {
            pid: Some(std::process::id()),
            pid_unavailable: false,
            ttl_ms: Some(600_000),
            reason: Some("test".into()),
            metadata: None,
            pid_provenance: Some(crate::claims::HOLDER_PROCESS.to_string()),
            root: Some(tmp.path().to_path_buf()),
            events_dir: None,
        };
        assert!(matches!(
            crate::claims::acquire(FLIGHT_KEY, "someone-else", opts),
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        let line = stdout.join("\n");
        assert!(line.contains("in progress elsewhere"), "{line}");
        assert!(line.contains("held by someone-else"));
        crate::claims::release(FLIGHT_KEY, "someone-else", Some(tmp.path()), None).unwrap();
    }

    #[test]
    fn path_gate_skips_and_marks_without_a_shell() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row("MERGED", SHA, &["docs/x.md"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stdout.join("\n").contains("skipped - no buildable change"));
        assert!(synced_marker(tmp.path(), SHA).exists());
    }

    #[test]
    fn not_merged_state_skips() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row("OPEN", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stdout.join("\n").contains("not merged (state=OPEN)"));
    }

    #[test]
    fn already_synced_marker_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        write_marker(&synced_marker(tmp.path(), SHA), &mut Vec::new());
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stdout.join("\n").contains("already synced"));
    }

    #[test]
    fn missing_merge_commit_skips() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let view = json!({
            "state": "MERGED",
            "mergeCommit": Value::Null,
            "files": [],
            "url": PR_URL,
        })
        .to_string();
        let deps = sync_deps(view, shell, Rc::new(|_| json!({})));
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stdout.join("\n").contains("no merge commit yet"));
    }

    #[test]
    fn unresolvable_origin_skips() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let gh = gh_stub(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            ListStub::Rows("[]".to_string()),
        );
        let deps = Deps {
            git_origin: Rc::new(|_| None),
            gh,
            shell,
            check: Rc::new(|_| json!({})),
            now: Rc::new(fixed_now),
        };
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stderr.join("\n").contains("no resolvable origin"));
    }

    #[test]
    fn gh_missing_on_view_skips_exit_zero() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let gh: GhRun = Rc::new(|_a, _c| Err(GhErr::Missing));
        let deps = Deps {
            git_origin: Rc::new(|_| Some("owner/repo".to_string())),
            gh,
            shell,
            check: Rc::new(|_| json!({})),
            now: Rc::new(fixed_now),
        };
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(!*ran.borrow());
        assert!(stderr.join("\n").contains("gh not found on PATH"));
    }

    #[test]
    fn gh_view_failure_exits_one_and_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let gh: GhRun = Rc::new(|_a, _c| Err(GhErr::Failed("exit 1: boom".to_string())));
        let deps = Deps {
            git_origin: Rc::new(|_| Some("owner/repo".to_string())),
            gh,
            shell,
            check: Rc::new(|_| json!({})),
            now: Rc::new(fixed_now),
        };
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 1);
        assert!(!*ran.borrow());
        let line = stderr.join("\n");
        assert!(line.contains("gh pr view #5 failed"), "{line}");
        assert!(line.contains("boom"));
    }

    #[test]
    fn divergence_refusal_exits_one() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({"refusal": "post-merge sync: canonical checkout is dirty"})),
        );
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 1);
        assert!(!*ran.borrow());
        assert!(stderr.join("\n").contains("canonical checkout is dirty"));
    }

    #[test]
    fn shell_failure_receipt_surfaces_command_and_output() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(3);
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 3);
        assert!(*ran.borrow());
        assert!(!synced_marker(tmp.path(), SHA).exists());
        let lines = stderr.join("\n");
        assert!(lines.contains("post-merge sync: failed (exit 3); marker withheld, will retry"));
        assert!(lines.contains("  command: echo hi"));
        assert!(lines.contains("  stderr: boom"));
    }

    #[test]
    fn shell_timeout_exits_124_with_the_timeout_line() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let ran = Rc::new(RefCell::new(false));
        let flag = Rc::clone(&ran);
        let shell: ShellRun = Rc::new(move |_c, _d| {
            *flag.borrow_mut() = true;
            ShellOutcome {
                code: 124,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            }
        });
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            Rc::new(|_| json!({})),
        );
        let (exit, _, stderr) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 124);
        let lines = stderr.join("\n");
        assert!(lines.contains("sync_command timed out after 600s; marker withheld, will retry"));
        assert!(lines.contains("post-merge sync: failed (exit 124); marker withheld, will retry"));
    }

    #[test]
    fn unconfigured_command_is_a_clean_noop() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), "[post_merge]\nauto_run = false\n");
        let deps = sync_deps(
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            ok_shell(),
            Rc::new(|_| json!({})),
        );
        let (exit, stdout, _) = run_sync_with_shell(&deps, tmp.path(), 5, &deps.shell);
        assert_eq!(exit, 0);
        assert!(stdout.join("\n").contains("not configured"));
    }

    // -- catch-up -----------------------------------------------------------

    fn catchup_deps(list: ListStub, shell: ShellRun, check: Check) -> Deps {
        catchup_deps_with_view(
            list,
            view_row("MERGED", SHA, &["cli/x.py"], PR_URL),
            shell,
            check,
        )
    }

    #[test]
    fn catchup_disabled_without_auto_run() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            "[post_merge]\nsync_command = 'echo hi'\nauto_run = false\n",
        );
        let deps = catchup_deps(
            ListStub::Rows(list_rows(&[])),
            ok_shell(),
            Rc::new(|_| json!({})),
        );
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "disabled");
    }

    #[test]
    fn catchup_unknown_when_gh_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let deps = catchup_deps(ListStub::Missing, ok_shell(), Rc::new(|_| json!({})));
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "unknown");
        assert_eq!(out.detail, "gh unavailable or unauthenticated");
        let st = compute_staleness(&deps, tmp.path(), false);
        assert_eq!(st.state, "unknown");
    }

    #[test]
    fn catchup_unknown_names_the_gh_exit() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let deps = catchup_deps(
            ListStub::Failed("exit 4: auth fail".into()),
            ok_shell(),
            Rc::new(|_| json!({})),
        );
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "unknown");
        assert!(out.detail.contains("exit 4"), "{}", out.detail);
        assert!(out.detail.contains("auth fail"));
    }

    #[test]
    fn catchup_synced_stamps_two_older_merges() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        // markerless: #7 (1h ago), #6 (2h ago), #5 (3h ago) - all in window.
        let rows = list_rows(&[
            (7, &iso_at_hours_before(1), SHA),
            (
                6,
                &iso_at_hours_before(2),
                "1111111111111111111111111111111111111111",
            ),
            (
                5,
                &iso_at_hours_before(3),
                "2222222222222222222222222222222222222222",
            ),
        ]);
        let deps = catchup_deps(ListStub::Rows(rows), shell, Rc::new(|_| json!({})));
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "synced");
        assert_eq!(out.pr_number, Some(7));
        assert_eq!(out.swept, 2);
        assert!(*ran.borrow());
        for sha in [
            SHA,
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
        ] {
            assert!(synced_marker(tmp.path(), sha).exists(), "{sha}");
        }
    }

    #[test]
    fn catchup_progress_goes_to_stderr_not_stdout() {
        // Catch-up runs inline during `reconcile --json`; its own progress
        // and the wrapped sync's progress must both land on stderr, or the
        // JSON caller's stdout stops parsing (the bug main fixed for the old
        // Python implementation in 1c4a346d55).
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, _ran) = spy_shell(0);
        let rows = list_rows(&[(7, &iso_at_hours_before(1), SHA)]);
        let deps = catchup_deps(ListStub::Rows(rows), shell, Rc::new(|_| json!({})));
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "synced");
        assert!(out.stdout.is_empty(), "{:?}", out.stdout);
        let stderr = out.stderr.join("\n");
        assert!(stderr.contains("post-merge sync: running in"), "{stderr}");
        assert!(
            stderr.contains("post-merge sync catch-up: synced PR #7"),
            "{stderr}"
        );
    }

    #[test]
    fn catchup_marked_keeps_older_merges_pending() {
        let tmp = tempfile::tempdir().unwrap();
        // Newest only path-gated: its files miss the cli/** glob.
        write_cfg(tmp.path(), CFG_BODY);
        let (shell, ran) = spy_shell(0);
        // Give the newest merge docs-only files: the gh view stub always
        // returns cli/x.py, so list the newest with a sha the VIEW row
        // matches... instead swap: make the newest merge's files docs/*.
        let rows = list_rows(&[
            (
                7,
                &iso_at_hours_before(1),
                "9999999999999999999999999999999999999999",
            ),
            (
                6,
                &iso_at_hours_before(2),
                "1111111111111111111111111111111111111111",
            ),
        ]);
        let deps = catchup_deps_with_view(
            ListStub::Rows(rows),
            view_row(
                "MERGED",
                "9999999999999999999999999999999999999999",
                &["docs/n.md"],
                PR_URL,
            ),
            shell,
            Rc::new(|_| json!({})),
        );
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "marked", "{}", out.detail);
        assert_eq!(out.pr_number, Some(7));
        assert_eq!(
            out.detail,
            "no buildable change; older merges still pending"
        );
        assert!(!*ran.borrow());
        // Older merge stays markerless.
        assert!(!synced_marker(tmp.path(), "1111111111111111111111111111111111111111").exists());
    }

    /// Like [`catchup_deps`] but the view row is decoupled from the list.
    fn catchup_deps_with_view(list: ListStub, view: String, shell: ShellRun, check: Check) -> Deps {
        deps_with(gh_stub(view, list), shell, check, Rc::new(fixed_now))
    }

    #[test]
    fn catchup_fresh_with_all_markers_and_carries_stale() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        // All merged SHAs marked; the divergence read says behind 2.
        write_marker(&synced_marker(tmp.path(), SHA), &mut Vec::new());
        let deps = catchup_deps(
            ListStub::Rows(list_rows(&[(7, &iso_at_hours_before(1), SHA)])),
            ok_shell(),
            Rc::new(|_| json!({"behind": 2})),
        );
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "fresh");
        assert!(out.stale, "behind 2 must flip stale");
    }

    #[test]
    fn catchup_skipped_when_the_newest_sync_declined() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        // Hold the lease so the newest sync reads declined.
        let opts = crate::claims::AcquireOpts {
            pid: Some(std::process::id()),
            pid_unavailable: false,
            ttl_ms: Some(600_000),
            reason: Some("test".into()),
            metadata: None,
            pid_provenance: Some(crate::claims::HOLDER_PROCESS.to_string()),
            root: Some(tmp.path().to_path_buf()),
            events_dir: None,
        };
        crate::claims::acquire(FLIGHT_KEY, "someone-else", opts);
        let rows = list_rows(&[(7, &iso_at_hours_before(1), SHA)]);
        let deps = catchup_deps(ListStub::Rows(rows), ok_shell(), Rc::new(|_| json!({})));
        let out = run_catchup(&deps, tmp.path());
        assert_eq!(out.outcome, "skipped");
        assert_eq!(out.detail, "sync declined (claim held or out of scope)");
        crate::claims::release(FLIGHT_KEY, "someone-else", Some(tmp.path()), None).unwrap();
    }

    // -- staleness ----------------------------------------------------------

    #[test]
    fn staleness_stale_names_the_oldest_overdue_merge() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        // 30h and 25h old, both past the 24h threshold; a third one fresh.
        let rows = list_rows(&[
            (
                8,
                &iso_at_hours_before(5),
                "8888888888888888888888888888888888888888",
            ),
            (7, &iso_at_hours_before(25), SHA),
            (
                6,
                &iso_at_hours_before(30),
                "6666666666666666666666666666666666666666",
            ),
        ]);
        let deps = catchup_deps(ListStub::Rows(rows), ok_shell(), Rc::new(|_| json!({})));
        let st = compute_staleness(&deps, tmp.path(), false);
        assert_eq!(st.state, "stale");
        assert_eq!(st.behind, None);
        assert!(
            st.detail.starts_with("PR #6 merged 30h ago, never synced"),
            "{}",
            st.detail
        );
        assert!(st.detail.contains("(+1 more)"));
        // Newest-first ordering: #8, #7, #6.
        assert_eq!(st.markerless.len(), 3);
        assert_eq!(st.markerless[0].number, 8);
        assert_eq!(st.markerless[2].number, 6);
    }

    #[test]
    fn staleness_fresh_when_everything_marked_and_current() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        write_marker(&synced_marker(tmp.path(), SHA), &mut Vec::new());
        let rows = list_rows(&[(7, &iso_at_hours_before(1), SHA)]);
        let deps = catchup_deps(
            ListStub::Rows(rows),
            ok_shell(),
            Rc::new(|_| json!({"behind": 0})),
        );
        let st = compute_staleness(&deps, tmp.path(), false);
        assert_eq!(st.state, "fresh");
        assert!(st.markerless.is_empty());
        assert_eq!(st.behind, Some(0));
    }

    #[test]
    fn staleness_unknown_keeps_the_verbatim_detail_when_gh_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        let deps = catchup_deps(ListStub::Missing, ok_shell(), Rc::new(|_| json!({})));
        let st = compute_staleness(&deps, tmp.path(), false);
        assert_eq!(st.state, "unknown");
        assert_eq!(st.detail, "gh unavailable or unauthenticated");
    }

    #[test]
    fn staleness_notes_never_flip_the_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), CFG_BODY);
        write_marker(&synced_marker(tmp.path(), SHA), &mut Vec::new());
        let rows = list_rows(&[(7, &iso_at_hours_before(1), SHA)]);
        let deps = catchup_deps(
            ListStub::Rows(rows),
            ok_shell(),
            Rc::new(|_| json!({"notes": ["canonical dirty: f1"], "behind": 0})),
        );
        let st = compute_staleness(&deps, tmp.path(), false);
        assert_eq!(st.state, "fresh");
        assert!(st.detail.contains("canonical dirty: f1"));
    }

    #[test]
    fn merged_rows_window_filters_old_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = catchup_deps(
            ListStub::Rows(list_rows(&[
                (7, &iso_at_hours_before(1), SHA),
                (
                    2,
                    &iso_at_hours_before(24 * 10),
                    "2222222222222222222222222222222222222222",
                ),
            ])),
            ok_shell(),
            Rc::new(|_| json!({})),
        );
        let rows = merged_rows(&deps, tmp.path(), 3, fixed_now()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 7);
    }

    // -- real shell runner (a detached child must not wedge the parent) -----

    #[test]
    fn real_shell_returns_while_a_detached_child_holds_the_capture_files() {
        let tmp = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let out = real_shell("sleep 5 & disown; echo ok", tmp.path());
        let elapsed = started.elapsed();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("ok"));
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "elapsed {elapsed:?}"
        );
    }

    #[test]
    fn real_shell_failure_carries_the_stderr_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let out = real_shell(
            "echo captured-out; echo captured-err >&2; exit 7",
            tmp.path(),
        );
        assert_eq!(out.code, 7);
        assert!(out.stdout.contains("captured-out"));
        assert!(out.stderr.contains("captured-err"));
    }

    #[test]
    fn parse_iso_reads_offset_less_stamps_as_utc() {
        let t = parse_iso("2026-09-18T07:00:00").expect("naive stamp must parse");
        assert_eq!(t, fixed_now() - chrono::Duration::hours(5));
        let z = parse_iso("2026-09-18T07:00:00Z").expect("rfc3339 must parse");
        assert_eq!(z, t);
    }

    #[test]
    fn slugs_parse_both_remote_forms_and_pr_urls() {
        assert_eq!(
            remote_slug("git@github.com:owner/repo.git"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            remote_slug("https://github.com/owner/repo"),
            Some("owner/repo".to_string())
        );
        assert_eq!(remote_slug("https://gitlab.com/owner/repo.git"), None);
        assert_eq!(
            slug_from_pr_url("https://github.com/owner/repo/pull/123"),
            Some("owner/repo".to_string())
        );
        assert_eq!(
            slug_from_pr_url("https://github.com/owner/repo/issue/9"),
            None
        );
    }
}
