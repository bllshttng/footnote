//! The merge-gate probes the status read used to shell the Python CLI for,
//! in process: base lineage and the merge result. The verdict vocabulary is
//! the verbs' own (`ok|stale|unknown`, exit 0|3|4) so the walk's blockers
//! read the same either way, and the verbs stay for the operator CLI.

use crate::authorized_merge::{PrFacts, ProbeOutcome};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// One bounded subprocess run, `None` when it could not run at all. The
/// Python helpers' `_probe` contract: an unevaluated probe degrades to a
/// breadcrumb, never a traceback.
pub(crate) struct Run {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn read_all<R: std::io::Read>(mut pipe: R) -> String {
    let mut text = String::new();
    let _ = pipe.read_to_string(&mut text);
    text
}

fn probe_run(cmd: &mut Command, timeout: Duration) -> Option<Run> {
    let _ = cmd.stdin(std::process::Stdio::null());
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    // Both pipes drain while the child runs: a child blocked on a full pipe
    // never exits, so waiting for the exit before reading would stall every
    // oversized capture to the deadline.
    let mut handles = Vec::new();
    if let Some(s) = child.stdout.take() {
        handles.push((true, std::thread::spawn(move || read_all(s))));
    }
    if let Some(s) = child.stderr.take() {
        handles.push((false, std::thread::spawn(move || read_all(s))));
    }
    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                break None;
            }
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    for (is_stdout, handle) in handles {
        if is_stdout {
            stdout = handle.join().unwrap_or_default();
        } else {
            stderr = handle.join().unwrap_or_default();
        }
    }
    Some(Run {
        ok: code == Some(0),
        code,
        stdout,
        stderr,
    })
}

/// The one probe seam, the Python `_probe` shape: run argv, `None` when it
/// could not run (missing tool, timeout). `gh` and `git` both ride it, so
/// tests answer the verdict offline with canned rows.
pub(crate) type RunProbe<'a> = &'a dyn Fn(&[&str], &Path) -> Option<Run>;

fn gh_json(run: RunProbe, cwd: &Path, endpoint: &str) -> Option<Value> {
    let args = ["gh", "api", endpoint];
    let out = run(&args, cwd)?;
    if !out.ok {
        return None;
    }
    serde_json::from_str(&out.stdout).ok()
}

fn s_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

const PROBE_FAILED: i64 = -1;

/// `(verdict, reason)` for merging a PR whose declared base is `base_ref`,
/// the Python `lineage_verdict` in process. `base_ref` rides the facts the
/// caller already holds, so the base REST read the spawned verb paid for is
/// gone. `stale` honors the operator bypass env, recording the same
/// gate-escape the verb records.
pub(crate) fn base_lineage(cwd: &Path, facts: &PrFacts, run: RunProbe) -> ProbeOutcome {
    let (verdict, reason) = lineage(cwd, facts, run);
    match verdict.as_str() {
        "stale" if lineage_bypassed() => {
            emit_bypass_escape(facts.number, &reason);
            ProbeOutcome::Clear
        }
        "stale" => ProbeOutcome::Refused(reason),
        "ok" => ProbeOutcome::Clear,
        _ => ProbeOutcome::Inconclusive(reason),
    }
}

fn lineage_bypassed() -> bool {
    std::env::var("FNO_PR_BASE_LINEAGE_OK").ok().as_deref() == Some("stale-acknowledged")
}

fn emit_bypass_escape(pr: u64, reason: &str) {
    let detail = format!("FNO_PR_BASE_LINEAGE_OK=stale-acknowledged (PR {pr}): {reason}");
    let _ = Command::new(crate::scrape::fno_bin())
        .args([
            "doctor",
            "event",
            "gate-escape",
            "other",
            "--pr",
            &pr.to_string(),
        ])
        .args(["--detail", &detail])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// `owner/repo` from a GitHub origin URL; None for anything else. The
/// post-merge scan's parser: lowercase, optional scheme and user@, host must
/// be github.com, exactly one slash in the path, `.git` stripped.
fn parse_origin_slug(url: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(?:[a-z][a-z0-9+.-]*://)?(?:[^/@]*@)?github\.com(?::[0-9]+)?[:/](.+)$")
            .unwrap()
    });
    let lowered = url.trim().to_lowercase();
    let rest = re.captures(&lowered)?.get(1)?.as_str().to_string();
    let mut rest = rest.trim_end_matches('/').to_string();
    if rest.ends_with(".git") {
        rest.truncate(rest.len() - 4);
    }
    if rest.matches('/').count() != 1 {
        return None;
    }
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(rest)
}

fn fetch_ref(cwd: &Path, ref_name: &str, run: RunProbe) -> bool {
    let spec = format!("+refs/heads/{ref_name}:refs/remotes/origin/{ref_name}");
    run(
        &["git", "fetch", "--no-write-fetch-head", "origin", &spec],
        cwd,
    )
    .map(|r| r.ok)
    .unwrap_or(false)
}

fn rev(cwd: &Path, ref_name: &str, run: RunProbe) -> String {
    run(&["git", "rev-parse", ref_name], cwd)
        .filter(|r| r.ok)
        .map(|r| r.stdout.trim().to_string())
        .unwrap_or_default()
}

/// `(pr_number, head_oid, failed)` of the newest MERGED PR whose head is the
/// base branch. `(0, "", false)` when none exists, failed on a probe error:
/// a failed probe must never read as a clean bill of health.
fn merged_pr_for_head(cwd: &Path, slug: &str, base: &str, run: RunProbe) -> (i64, String, bool) {
    let owner = slug.split('/').next().unwrap_or("");
    let head = format!("{owner}%3A{base}");
    for page in 1..=10 {
        let endpoint =
            format!("repos/{slug}/pulls?state=closed&head={head}&per_page=100&page={page}");
        let Some(payload) = gh_json(run, cwd, &endpoint) else {
            return (PROBE_FAILED, String::new(), true);
        };
        let Some(rows) = payload.as_array() else {
            return (PROBE_FAILED, String::new(), true);
        };
        for row in rows {
            let merged_at = row.get("merged_at").and_then(Value::as_str).unwrap_or("");
            if merged_at.is_empty() {
                continue;
            }
            let Some(number) = row.get("number").and_then(Value::as_i64) else {
                return (PROBE_FAILED, String::new(), true);
            };
            let Some(oid) = row
                .pointer("/head/sha")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && *s != "null")
            else {
                return (PROBE_FAILED, String::new(), true);
            };
            return (number, oid.to_string(), false);
        }
        if rows.len() < 100 {
            return (0, String::new(), false);
        }
    }
    (PROBE_FAILED, String::new(), true)
}

fn lineage(cwd: &Path, facts: &PrFacts, run: RunProbe) -> (String, String) {
    let base_ref = facts.base_ref.as_str();
    let slug = run(&["git", "remote", "get-url", "origin"], cwd)
        .filter(|r| r.ok)
        .and_then(|r| parse_origin_slug(r.stdout.trim()));
    let Some(slug) = slug else {
        return (
            "unknown".into(),
            "could not resolve owner/repo from the origin remote".into(),
        );
    };
    let default =
        gh_json(run, cwd, &format!("repos/{slug}")).and_then(|v| s_of(&v, "default_branch"));
    let Some(default) = default else {
        return (
            "unknown".into(),
            "could not read the repository default branch (REST read failed)".into(),
        );
    };
    if base_ref.is_empty() {
        return (
            "unknown".into(),
            "could not read the base ref of the PR (REST read failed)".into(),
        );
    }
    if base_ref == default {
        return (
            "ok".into(),
            format!("base is the default branch ({default})"),
        );
    }
    let (merged, merged_head, merged_failed) = merged_pr_for_head(cwd, &slug, base_ref, run);
    let default_ok = fetch_ref(cwd, &default, run);
    let git_ok = if fetch_ref(cwd, base_ref, run) {
        (default_ok, false)
    } else {
        let gone = run(
            &[
                "git",
                "ls-remote",
                "--heads",
                "origin",
                &format!("refs/heads/{base_ref}"),
            ],
            cwd,
        )
        .map(|r| r.ok && r.stdout.trim().is_empty())
        .unwrap_or(false);
        (default_ok, gone)
    };
    let (git_ok, base_gone) = git_ok;
    let base_tip = if git_ok {
        rev(cwd, &format!("origin/{base_ref}"), run)
    } else {
        String::new()
    };
    let contained = if git_ok {
        match run(
            &[
                "git",
                "merge-base",
                "--is-ancestor",
                &format!("origin/{base_ref}"),
                &format!("origin/{default}"),
            ],
            cwd,
        ) {
            None => None,
            Some(r) if r.code == Some(0) => Some(true),
            Some(r) if r.code == Some(1) => Some(false),
            Some(_) => None,
        }
    } else {
        None
    };
    lineage_decision(
        facts,
        &default,
        merged,
        &merged_head,
        merged_failed,
        git_ok,
        base_gone,
        &base_tip,
        contained,
    )
}

/// The verdict rules over already-probed inputs, in the Python order: the
/// landed-and-unmoved guard, the confirmed deletion, full containment, then
/// the unknown-vs-ok split that names which probe went blind.
#[allow(clippy::too_many_arguments)]
fn lineage_decision(
    facts: &PrFacts,
    default: &str,
    merged: i64,
    merged_head: &str,
    merged_failed: bool,
    git_ok: bool,
    base_gone: bool,
    base_tip: &str,
    contained: Option<bool>,
) -> (String, String) {
    let base_ref = facts.base_ref.as_str();
    let retarget = format!(
        "retarget it first: gh pr edit {} --base {default}",
        facts.number
    );
    if merged > 0 && !merged_head.is_empty() && !base_tip.is_empty() && merged_head == base_tip {
        return (
            "stale".into(),
            format!(
                "base branch '{base_ref}' already landed via merged PR #{merged} and has not \
                 moved since ({}), so merging would report MERGED while the commits never \
                 reach '{default}'; {retarget}",
                &base_tip[..8.min(base_tip.len())]
            ),
        );
    }
    if base_gone {
        let landed = if merged > 0 {
            format!(" (it landed via merged PR #{merged})")
        } else {
            String::new()
        };
        return (
            "stale".into(),
            format!(
                "base branch '{base_ref}' no longer exists on the remote{landed}, so merging \
                 would report MERGED while the commits never reach '{default}'; {retarget}"
            ),
        );
    }
    if contained == Some(true) {
        return (
            "stale".into(),
            format!(
                "base branch '{base_ref}' is already fully contained in '{default}' (nothing \
                 on it that '{default}' lacks), so merging would report MERGED while the \
                 commits never reach '{default}'; {retarget}. If '{base_ref}' is instead a \
                 new branch whose own commits are not pushed yet, push them first, or set \
                 FNO_PR_BASE_LINEAGE_OK=stale-acknowledged"
            ),
        );
    }
    let git_blind = !git_ok || contained.is_none() || base_tip.is_empty();
    if merged_failed && git_blind {
        return (
            "unknown".into(),
            format!("both lineage probes failed for base '{base_ref}' (gh and git)"),
        );
    }
    if merged_failed {
        return (
            "unknown".into(),
            format!("merged-PR probe failed for base '{base_ref}' (gh pr list)"),
        );
    }
    if git_blind {
        return (
            "unknown".into(),
            format!("ancestry probe failed for base '{base_ref}' (git fetch or merge-base)"),
        );
    }
    (
        "ok".into(),
        format!("base '{base_ref}' still leads to '{default}'"),
    )
}

/// The merge-result probe in process: the cheap ancestry answer first (the
/// head already contains its base, so CI on the head IS the merge result),
/// then the race-guarded pull-head fetch and the repo-wide script, exactly
/// the spawned verb's order.
pub(crate) fn merge_result(cwd: &Path, facts: &PrFacts, run: RunProbe) -> ProbeOutcome {
    let base = facts.base_ref.as_str();
    let head_oid = facts.head_sha.as_str();
    if base.is_empty() || head_oid.is_empty() {
        return ProbeOutcome::Inconclusive(
            "merge-result probe could not read the PR base/head".into(),
        );
    }
    if !fetch_ref(cwd, base, run) {
        return ProbeOutcome::Inconclusive(format!("could not fetch base branch '{base}'"));
    }
    let ancestor = run(
        &[
            "git",
            "merge-base",
            "--is-ancestor",
            &format!("origin/{base}"),
            head_oid,
        ],
        cwd,
    );
    if ancestor.map(|r| r.code == Some(0)).unwrap_or(false) {
        return ProbeOutcome::Clear;
    }
    let spec = format!("refs/pull/{}/head:refs/fno/merge-result/head", facts.number);
    let fetched = run(
        &[
            "git",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "origin",
            &spec,
        ],
        cwd,
    )
    .filter(|r| r.ok)
    .map(|_| rev(cwd, "refs/fno/merge-result/head", run))
    .unwrap_or_default();
    if fetched != head_oid {
        return ProbeOutcome::Inconclusive(format!(
            "PR head moved during the probe ({} != {})",
            &fetched[..8.min(fetched.len())],
            &head_oid[..8.min(head_oid.len())]
        ));
    }
    let Some(top) =
        crate::paths::canonical_repo_root(cwd).map(|p| p.to_string_lossy().into_owned())
    else {
        return ProbeOutcome::Inconclusive(
            "could not resolve the canonical repo root for the merge-result script".into(),
        );
    };
    let (verdict, reason) = run_merge_script(&top, &format!("origin/{base}"), head_oid, cwd);
    match verdict.as_str() {
        "ok" => ProbeOutcome::Clear,
        "red" => ProbeOutcome::Refused(reason),
        _ => ProbeOutcome::Inconclusive(reason),
    }
}

/// The static step: the repo's own `check-merge-result.sh` (merge-tree +
/// repo-wide ruff + mypy on the merged tree), bounded at the verb's 180s and
/// resolved against the canonical checkout's `cli/` the way the verb does.
fn run_merge_script(top: &str, base_rev: &str, head_oid: &str, cwd: &Path) -> (String, String) {
    let script = PathBuf::from(top)
        .join("scripts")
        .join("ci")
        .join("check-merge-result.sh");
    let on_path = |tool: &str| {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).is_file()))
            .unwrap_or(false)
    };
    let cli = PathBuf::from(top).join("cli");
    let tool = |name: &str| {
        if on_path(name) {
            name.to_string()
        } else {
            format!("uv run --project {} {name}", cli.display())
        }
    };
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .args([top, base_rev, head_oid])
        .current_dir(cwd)
        .env("RUFF", tool("ruff"))
        .env("MYPY", tool("mypy"));
    match probe_run(&mut cmd, Duration::from_secs(180)) {
        None => ("unknown".into(), "merge-result probe did not run".into()),
        Some(r) if r.code == Some(0) => {
            let out = r.stdout.trim();
            (
                "ok".into(),
                out.strip_prefix("merge-result: ok - ")
                    .unwrap_or(out)
                    .to_string(),
            )
        }
        Some(r) if r.code == Some(3) => {
            let out = r.stdout.trim();
            (
                "red".into(),
                out.strip_prefix("merge-result: red - ")
                    .unwrap_or(out)
                    .to_string(),
            )
        }
        Some(r) => {
            let detail = format!("{}{}", r.stderr.trim(), r.stdout.trim());
            let detail = detail.trim().to_string();
            let mut end = 200.min(detail.len());
            while end > 0 && !detail.is_char_boundary(end) {
                end -= 1;
            }
            (
                "unknown".into(),
                format!("merge-result probe failed: {}", &detail[..end]),
            )
        }
    }
}

/// The bounded runner the Probes closures call: one Command, one timeout.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn run_probe(mut cmd: Command) -> Option<Run> {
    probe_run(&mut cmd, PROBE_TIMEOUT)
}

/// The dispatch-hold probe in process: the PR's bound nodes (branch name,
/// closure trailer, graph back-references), each walked up the hold ladder,
/// first positive wins. The spawned verb's fail-closed contract: a graph
/// read failure is `dispatch-hold-invalid` prose, never an assumed unheld.
pub(crate) fn dispatch_hold(cwd: &Path, facts: &PrFacts) -> ProbeOutcome {
    let graph_path = crate::org_board::scope::graph_json_path(cwd);
    let store = crate::backlog::api::Store::new(&graph_path);
    match crate::graph_store::read_rows_where(
        &store.graph,
        &crate::backlog::RowQuery {
            fields: Some(
                [
                    "id",
                    "slug",
                    "parent",
                    "contained_in",
                    "plan_path",
                    "cwd",
                    "dispatch_hold",
                    "project",
                    "pr_number",
                    "pr_url",
                    "additional_prs",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ),
            with_blockers: true,
            ..Default::default()
        },
    )
    .map_err(|error| crate::backlog::api::ApiError(error.to_string()))
    {
        Ok(entries) => dispatch_hold_rows(cwd, facts, &entries),
        Err(e) => ProbeOutcome::Inconclusive(format!(
            "dispatch-hold-invalid: backlog graph unreadable ({e:?}); refusing to assume unheld"
        )),
    }
}

/// The pure half over already-read rows, so the candidate selection and the
/// ladder walk are unit-testable without a graph on disk.
pub(crate) fn dispatch_hold_rows(root: &Path, facts: &PrFacts, entries: &[Value]) -> ProbeOutcome {
    if entries.is_empty() {
        return ProbeOutcome::Clear;
    }
    let mut by_id: std::collections::BTreeMap<String, Value> = Default::default();
    for e in entries {
        if let Some(id) = crate::graph_store::entry_id(e) {
            by_id.insert(id.to_string(), e.clone());
        }
    }
    let Some(project) = crate::backlog_ready::detect_project(entries, &root.to_string_lossy())
    else {
        return ProbeOutcome::Clear;
    };
    let keys = crate::org_board::prs::pr_binding_keys(
        facts.number as i64,
        &facts.head_ref,
        Some(&facts.url),
        facts.body.as_deref(),
        &entries,
    );
    let mut seen: Vec<String> = Vec::new();
    for id in keys
        .branch
        .iter()
        .chain(keys.trailer.iter())
        .chain(keys.backrefs.iter())
    {
        if seen.contains(id) {
            continue;
        }
        seen.push(id.clone());
        let Some(entry) = by_id.get(id.as_str()) else {
            continue;
        };
        if !crate::backlog_ready::row_matches_project(entry, Some(&project)) {
            continue;
        }
        if let Some(v) = crate::backlog_ready::hold_verdict_receipt(entry, &by_id) {
            return hold_outcome(facts.number, &v);
        }
    }
    // The backref key needs a comparable URL; a url-less node bound by bare
    // PR number is the one shape the keys miss, and the spawned verb found
    // it. Same repo scope, so a cross-repo same-numbered node stays invisible.
    // Several same-number nodes with no url discriminating them must refuse
    // to assume unheld; the spawned verb failed closed there too.
    let mut bare: Vec<&Value> = Vec::new();
    for e in entries {
        let Some(id) = crate::graph_store::entry_id(e) else {
            continue;
        };
        if seen.contains(&id.to_string()) {
            continue;
        }
        let number_ok = e.get("pr_number").and_then(Value::as_u64) == Some(facts.number);
        let url = e.get("pr_url").and_then(Value::as_str).unwrap_or("");
        if number_ok
            && url.is_empty()
            && crate::backlog_ready::row_matches_project(e, Some(&project))
        {
            bare.push(e);
        }
    }
    if keys.backrefs.is_empty() && bare.len() > 1 {
        return ProbeOutcome::Inconclusive(
            "dispatch-hold-ambiguous: several nodes bind this PR number with no discriminating pr_url; refusing to assume unheld"
                .to_string(),
        );
    }
    for e in bare {
        if let Some(v) = crate::backlog_ready::hold_verdict_receipt(e, &by_id) {
            return hold_outcome(facts.number, &v);
        }
    }
    ProbeOutcome::Clear
}

/// One hold-verdict receipt as the spawned `merge_hold_reason` worded it: a
/// held block names its fields, an INVALID read refuses to assume unheld,
/// and a positive verdict also disarms an armed server-side queue.
fn hold_outcome(pr_number: u64, v: &crate::backlog_ready::HoldVerdictReceipt) -> ProbeOutcome {
    if !v.held {
        return ProbeOutcome::Inconclusive(format!(
            "{}: {}; refusing to assume unheld",
            v.guard_reason, v.detail
        ));
    }
    let note = crate::merge_hold::disarm_automerge(pr_number);
    eprintln!("hold: auto-merge disarm for PR {pr_number}: {note}");
    ProbeOutcome::Refused(format!(
        "{}: {}; set_by={}; release_when={}; review_on={}",
        v.guard_reason, v.reason, v.set_by, v.release_when, v.review_on
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorized_merge::facts_from_pulls;
    use serde_json::json;

    fn facts(n: u64) -> PrFacts {
        PrFacts {
            number: n,
            head_sha: "a".repeat(40),
            head_ref: format!("feature/x-{n}"),
            base_ref: "main".into(),
            url: format!("https://github.com/o/r/pull/{n}"),
            body: Some(String::new()),
            state: "OPEN".into(),
            armed: false,
        }
    }

    #[test]
    fn facts_parse_from_the_status_projection() {
        let payload = serde_json::json!({
            "number": 9,
            "headRefOid": "h",
            "headRefName": "feature/x",
            "baseRefName": "main",
            "state": "OPEN",
            "html_url": "https://github.com/o/r/pull/9",
            "body": "b",
            "auto_merge": Value::Null,
        });
        let f = facts_from_pulls(&payload, None).unwrap();
        assert_eq!(f.number, 9);
        assert_eq!(f.head_sha, "h");
        assert_eq!(f.head_ref, "feature/x");
        assert_eq!(f.base_ref, "main");
        assert_eq!(f.state, "OPEN");
        assert!(!f.armed);
        // A projection older than the url/body fields refuses instead of
        // guessing.
        let old = serde_json::json!({"number": 9, "headRefOid": "h"});
        assert!(facts_from_pulls(&old, None).is_err());
    }

    #[test]
    fn origin_slug_parses_the_github_forms() {
        assert_eq!(
            parse_origin_slug("git@github.com:o/r.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            parse_origin_slug("https://github.com/o/r.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(parse_origin_slug("gitlab.com/o/r").as_deref(), None);
    }

    #[test]
    fn lineage_reads_stale() {
        let f = facts_with_base(&facts(9), "release");
        let (verdict, reason) = lineage_decision(
            &f,
            "main",
            7,
            "c0ffee",
            false,
            true,
            false,
            "c0ffee",
            Some(false),
        );
        assert_eq!(verdict, "stale");
        assert!(reason.contains("already landed via merged PR #7"));
        let f = facts_with_base(&facts(9), "live");
        let (verdict, _) = lineage_decision(&f, "main", 0, "", false, true, true, "", Some(false));
        assert_eq!(verdict, "stale")
    }

    fn facts_with_base(f: &PrFacts, base: &str) -> PrFacts {
        let mut f = f.clone();
        f.base_ref = base.into();
        f
    }

    #[test]
    fn lineage_contained_vs_uncontained() {
        let f = facts_with_base(&facts(9), "release");
        let (verdict, reason) = lineage_decision(
            &f,
            "main",
            0,
            "",
            false,
            true,
            false,
            "deadbeef",
            Some(true),
        );
        assert_eq!(verdict, "stale");
        assert!(reason.contains("already fully contained"));
        let (verdict, _) = lineage_decision(
            &f,
            "main",
            0,
            "",
            false,
            true,
            false,
            "deadbeef",
            Some(false),
        );
        assert_eq!(verdict, "ok");
    }

    #[test]
    fn hold_via_url_backref_refuses_with_fields() {
        let entries = vec![json!({
            "id": "x-h1",
            "project": "fno",
            "cwd": "/repo",
            "pr_number": 9,
            "pr_url": "https://github.com/o/r/pull/9",
            "dispatch_hold": {
                "held": true, "reason": "awaiting ops",
                "set_by": "lead", "release_when": "ops done", "review_on": "2026-10-06",
            },
        })];
        let root = Path::new("/repo");
        let out = dispatch_hold_rows(root, &facts(9), &entries);
        match out {
            ProbeOutcome::Refused(text) => {
                assert!(text.contains("dispatch-hold:x-h1"));
                assert!(text.contains("set_by=lead"));
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn hold_ignores_a_url_only_backref() {
        let held = json!({
            "held": true, "reason": "awaiting ops",
            "set_by": "lead", "release_when": "ops done", "review_on": "2026-10-06",
        });
        // A url-only row: the stamped gate needs a pr_number, so the spawned
        // verb's ladder never saw this row either.
        let url_only = json!({
            "id": "x-h4", "project": "fno", "cwd": "/repo",
            "pr_url": "https://github.com/o/r/pull/9",
            "dispatch_hold": held.clone(),
        });
        // A cross-project backref: another project's hold is not ours.
        let cross = json!({
            "id": "x-h2", "project": "other", "cwd": "/elsewhere",
            "pr_url": "https://github.com/o/r/pull/9",
            "dispatch_hold": held,
        });
        let entries = vec![url_only, cross];
        let out = dispatch_hold_rows(Path::new("/repo"), &facts(9), &entries);
        assert_eq!(out, ProbeOutcome::Clear);
    }

    #[test]
    fn hold_refuses_when_bare_number_binding_is_ambiguous() {
        let hold = json!({
            "reason": "awaiting ops", "set_by": "lead",
            "release_when": "ops done", "review_on": "2026-10-06",
        });
        let entries = vec![
            json!({
                "id": "x-h5", "project": "fno", "cwd": "/repo",
                "pr_number": 9, "dispatch_hold": hold.clone(),
            }),
            json!({
                "id": "x-h6", "project": "fno", "cwd": "/repo",
                "pr_number": 9, "dispatch_hold": hold,
            }),
        ];
        let out = dispatch_hold_rows(Path::new("/repo"), &facts(9), &entries);
        match out {
            ProbeOutcome::Inconclusive(text) => {
                assert!(text.contains("dispatch-hold-ambiguous"));
                assert!(text.contains("refusing to assume unheld"));
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    #[test]
    fn hold_finds_a_urlless_number_bound_node() {
        let entries = vec![json!({
            "id": "x-h3",
            "project": "fno",
            "cwd": "/repo",
            "pr_number": 9,
            "dispatch_hold": {
                "reason": "awaiting review", "release_when": "review lands",
                "set_by": "lead", "review_on": "2026-10-06",
            },
        })];
        let out = dispatch_hold_rows(Path::new("/repo"), &facts(9), &entries);
        match out {
            ProbeOutcome::Refused(text) => assert!(text.contains("dispatch-hold:x-h3")),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn hold_clear_when_no_candidate_is_held() {
        let entries = vec![json!({"id": "x-ok", "project": "fno", "cwd": "/repo"})];
        let out = dispatch_hold_rows(Path::new("/repo"), &facts(9), &entries);
        assert_eq!(out, ProbeOutcome::Clear);
    }
}
