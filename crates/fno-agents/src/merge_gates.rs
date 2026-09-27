//! The merge gates that used to live in the Python merge verb, ported so
//! `authorized_merge::decide` is the one merge decision. Each gate
//! evaluates one input and answers `Option<Blocker>`: None clears. The pure
//! halves take their facts as arguments so unit tests need no filesystem or
//! network; the fetching halves ride the caller's `Probes` handle.

use crate::authorized_merge::{Blocker, Probes};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `fno do pr coverage-check <n> --recompute`'s exit contract (the verb's
/// own help text is the source of truth).
pub(crate) const COVERAGE_CLEAR: i32 = 0;
pub(crate) const COVERAGE_UNCOVERED: i32 = 3;
pub(crate) const COVERAGE_IMPOSSIBLE: i32 = 5;

/// The coverage gate, exit-code polarity kept. The verb runs the operator
/// waiver overlay inside itself: a waived PR exits 0 here, and the verb
/// prints its `override: ` note on stdout, so the waiver travels with the
/// gate's answer instead of a caller re-deriving it.
/// Returns (blocker, waiver).
pub(crate) fn coverage_gate<P: Probes>(
    probes: &P,
    cwd: &Path,
    pr: u64,
) -> (Option<Blocker>, Option<String>) {
    let args = vec![
        "do".to_string(),
        "pr".to_string(),
        "coverage-check".to_string(),
        pr.to_string(),
        "--recompute".to_string(),
    ];
    match probes.fno_shell(cwd, &args) {
        Ok((Some(code), stdout, stderr)) => {
            let detail = {
                let err = String::from_utf8_lossy(&stderr).trim().to_string();
                if err.is_empty() {
                    String::from_utf8_lossy(&stdout).trim().to_string()
                } else {
                    err
                }
            };
            match code {
                COVERAGE_CLEAR => {
                    let line = String::from_utf8_lossy(&stdout).trim().to_string();
                    // The prefix literal matches Python's OVERRIDE_NOTE_PREFIX.
                    let waiver = line.strip_prefix("override: ").map(str::to_string);
                    (None, waiver)
                }
                COVERAGE_UNCOVERED => (
                    Some(Blocker::held(
                        "review_coverage_uncovered",
                        if detail.is_empty() {
                            "unreviewed merge refused".to_string()
                        } else {
                            format!("unreviewed merge refused: {detail}")
                        },
                    )),
                    None,
                ),
                COVERAGE_IMPOSSIBLE => (
                    Some(Blocker::held(
                        "review_coverage_impossible",
                        if detail.is_empty() {
                            "review coverage impossible at this head".to_string()
                        } else {
                            format!("unreviewed merge refused: {detail}")
                        },
                    )),
                    None,
                ),
                other => (
                    Some(Blocker::unknown(
                        "review_coverage_unknown",
                        if detail.is_empty() {
                            format!("coverage probe failed, merge refused (exit {other})")
                        } else {
                            format!("coverage probe failed, merge refused: {detail}")
                        },
                    )),
                    None,
                ),
            }
        }
        Ok((None, stdout, stderr)) => {
            let detail = String::from_utf8_lossy(&stderr).trim().to_string();
            (
                Some(Blocker::unknown(
                    "review_coverage_unknown",
                    if detail.is_empty() {
                        format!(
                            "coverage probe failed, merge refused: {}",
                            String::from_utf8_lossy(&stdout).trim()
                        )
                    } else {
                        format!("coverage probe failed, merge refused: {detail}")
                    },
                )),
                None,
            )
        }
        Err(error) => (
            Some(Blocker::unknown(
                "review_coverage_unknown",
                format!("coverage probe failed, merge refused: {error}"),
            )),
            None,
        ),
    }
}

/// `<root>/.fno/stub-manifest-<node>.json` (stub_manifest.py's own layout).
fn stub_manifest_path(root: &Path, node_id: &str) -> PathBuf {
    root.join(".fno")
        .join(format!("stub-manifest-{node_id}.json"))
}

/// The node-side half of the stub-manifest gate, pure over graph entries: the
/// `dep=contract` node this PR closes, or None. The graph read degrades to
/// None (the default hard merge path), exactly as the Python `dep` did.
pub(crate) fn contract_node_for_pr(entries: &[Value], pr: u64) -> Option<String> {
    for entry in entries {
        let dep = entry.get("dep").and_then(Value::as_str);
        if dep != Some("contract") {
            continue;
        }
        let numbers = pr_numbers_of(entry);
        if numbers.contains(&pr) {
            return entry.get("id").and_then(Value::as_str).map(str::to_owned);
        }
    }
    None
}

/// Every PR number a graph row carries: the persisted `pr_number` field plus
/// the `additional_prs` entries (typed rows carry `{number}` or `{url}`
/// objects; legacy rows carry bare ints or `/pull/<n>` URL strings). A
/// contract dependent whose PR is recorded only in `additional_prs` must
/// still be found or the gate is bypassed.
fn pr_numbers_of(entry: &Value) -> Vec<u64> {
    let mut out = Vec::new();
    if let Some(n) = entry.get("pr_number").and_then(Value::as_u64) {
        out.push(n);
    }
    if let Some(list) = entry.get("additional_prs").and_then(Value::as_array) {
        for raw in list {
            match raw {
                Value::Number(n) => {
                    if let Some(n) = n.as_u64() {
                        out.push(n);
                    }
                }
                Value::Object(o) => {
                    if let Some(n) = o.get("number").and_then(Value::as_u64) {
                        out.push(n);
                    } else if let Some(url) = o.get("url").and_then(Value::as_str) {
                        if let Some(n) = pull_number_from_url(url) {
                            out.push(n);
                        }
                    }
                }
                Value::String(url) => {
                    if let Some(n) = pull_number_from_url(url) {
                        out.push(n);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The PR number a `/pull/<n>` URL names, if it names one.
fn pull_number_from_url(url: &str) -> Option<u64> {
    let rest = url.split("/pull/").nth(1)?;
    rest.split('/').next()?.parse::<u64>().ok()
}

/// The stub-manifest gate, pure over an already-read manifest value: a
/// contract dependent whose manifest is not `reconciled: true` holds the
/// merge (mocks would ship). An unreadable manifest of a known contract node
/// fails CLOSED, as the Python did.
pub(crate) fn stub_manifest_blocker(root: &Path, node_id: &str) -> Option<Blocker> {
    let path = stub_manifest_path(root, node_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            return Some(Blocker::held(
                "stub_manifest_unreconciled",
                format!(
                    "contract dependent {node_id} carries a malformed stub-manifest \
                     (cannot prove stubs are gone): {e}; reconcile before merge"
                ),
            ))
        }
    };
    let manifest: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(e) => {
            return Some(Blocker::held(
                "stub_manifest_unreconciled",
                format!(
                    "contract dependent {node_id} carries a malformed stub-manifest \
                     (cannot prove stubs are gone): {e}; reconcile before merge"
                ),
            ))
        }
    };
    if manifest.get("reconciled").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let stubs = manifest
        .get("stubs")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    Some(Blocker::held(
        "stub_manifest_unreconciled",
        format!(
            "contract dependent {node_id} carries a unreconciled stub-manifest \
             ({stubs} stub(s)); reconcile before merge"
        ),
    ))
}

/// The whole stub-manifest gate over graph entries. An unreadable store
/// degrades to clear (never block a normal merge on our own read).
pub(crate) fn stub_manifest_gate(root: &Path, entries: &[Value], pr: u64) -> Option<Blocker> {
    contract_node_for_pr(entries, pr).and_then(|node| stub_manifest_blocker(root, &node))
}

/// Whether a path is documentation (loopcheck's own classifier; one copy).
fn is_documentation_path(path: &str) -> bool {
    crate::loopcheck::is_documentation_path(path)
}

/// The plan-fidelity gate: a PR bound to a plan whose declared deliverables
/// did not all ship refuses, unless the PR payload is documentation-only.
/// Fail-open on a degraded probe (the Python polarity), fail-refused only
/// when the fidelity verdict itself says so.
pub(crate) fn plan_fidelity_blocker<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Option<Blocker> {
    let plan_path = ledger_plan_path(cwd, pr)?;
    let plan_path = plan_path.trim();
    if plan_path.is_empty() {
        return None;
    }
    if !pr_payload_is_code(probes, cwd, pr) {
        return None;
    }
    let args = vec![
        "plan".to_string(),
        "fidelity".to_string(),
        plan_path.to_string(),
        "--json".to_string(),
    ];
    match probes.fno_shell(cwd, &args) {
        Ok((Some(0), stdout, _)) => {
            let verdict: Value = serde_json::from_slice(&stdout).ok()?;
            if verdict.get("refused").and_then(Value::as_bool) == Some(true) {
                let reason = verdict
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("uncovered shortfall");
                Some(Blocker::refused(
                    "plan_fidelity_refused",
                    format!("plan fidelity refused: {reason}"),
                ))
            } else {
                None
            }
        }
        Ok(_) => {
            breadcrumb("plan fidelity probe answered nonzero; proceeding (fail-open)");
            None
        }
        Err(error) => {
            breadcrumb(&format!(
                "plan fidelity probe unavailable ({error}); proceeding (fail-open)"
            ));
            None
        }
    }
}

/// The plan_path bound to this PR's delivery row in the ledger, or None.
/// PR numbers are per-repo and the ledger is global, so when several rows
/// match, the one whose pr_url names this checkout's remote wins; otherwise
/// the first match answers (fail-open, as the Python did). A missing or
/// broken ledger is None: no signal, no gate.
fn ledger_plan_path(cwd: &Path, pr: u64) -> Option<String> {
    use std::process::Command;

    // Read-only: the optional resolver skips the checkout migration
    // `ledger_path` performs and answers `None` with no declared root (a
    // hermetic test), the same no-signal shape as a missing ledger.
    let path = crate::paths::worktree_space_dir_opt(cwd)?.join("ledger.json");
    let text = std::fs::read_to_string(path).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let rows = match data {
        Value::Array(list) => list,
        Value::Object(map) => map.get("entries")?.as_array()?.clone(),
        _ => return None,
    };
    let origin = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    // The pr_url names `owner/repo` inside its path; the remote url names the
    // same pair in ssh, https, and .git-suffixed spellings. Compare the
    // normalized pair, never the raw url: a substring test on the raw form
    // never matches and every scoping election falls to first-match.
    let slug = repo_slug_from_origin(&origin).unwrap_or_default();
    let matches: Vec<&Value> = rows
        .iter()
        .filter(|row| row.get("pr_number").and_then(Value::as_u64) == Some(pr))
        .collect();
    let picked = matches
        .iter()
        .copied()
        .find(|row| {
            !slug.is_empty()
                && row
                    .get("pr_url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.contains(&slug))
        })
        .or_else(|| matches.first().copied());
    picked.and_then(|row| {
        row.get("plan_path")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

/// The PR carries a code payload (at least one non-documentation changed
/// file). A read miss answers false, which skips the gate (fail-open: the
/// fidelity guard never wedges a merge because gh hiccuped).
fn pr_payload_is_code<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> bool {
    let args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr.to_string(),
        "--json".to_string(),
        "files".to_string(),
    ];
    let Ok((true, stdout)) = probes.run_gh(cwd, &args) else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&stdout) else {
        return false;
    };
    let Some(files) = payload.get("files").and_then(Value::as_array) else {
        return false;
    };
    files.iter().any(|file| {
        file.get("path")
            .or_else(|| file.get("filename"))
            .and_then(Value::as_str)
            .is_some_and(|path| !is_documentation_path(path))
    })
}

fn breadcrumb(message: &str) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "pr-merge: {message}");
}

/// The overlap gate (parallel-mode G4, LD#9), polarity kept: only arms while
/// live lanes run; `_behind_by` miss disarms; base-move and PR-file misses
/// HOLD (fail closed) because the base already moved.
pub(crate) fn overlap_blocker<P: Probes>(
    probes: &P,
    cwd: &Path,
    facts_number: u64,
) -> Option<Blocker> {
    if probes.live_lanes(cwd) == 0 {
        return None;
    }
    let behind = behind_by(probes, cwd, facts_number);
    if behind == 0 {
        return None;
    }
    // The probes already know the base moved, so a read that fails holds:
    // an under-reported move must never read as a clear. This is the Python
    // miss contract, kept verbatim.
    let base_paths = match base_move_paths(probes, cwd, facts_number) {
        Some(paths) => paths,
        None => {
            return Some(Blocker::held(
                "base_overlap",
                "stale base: overlap probe unavailable (base moved; could not \
                 compare file sets); run fno do pr rebase, then retry",
            ));
        }
    };
    let pr_paths = match pr_file_paths(probes, cwd, facts_number) {
        Some(paths) => paths,
        None => {
            return Some(Blocker::held(
                "base_overlap",
                "stale base: overlap probe unavailable (the PR's own file \
                 read failed); run fno do pr rebase, then retry",
            ));
        }
    };
    let overlap = overlaps(&base_paths, &pr_paths);
    if overlap.is_empty() {
        return None;
    }
    let shown = overlap
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let extra = if overlap.len() > 3 {
        format!(" and {} more", overlap.len() - 3)
    } else {
        String::new()
    };
    Some(Blocker::held(
        "base_overlap",
        format!(
            "stale base: base move touches files this PR also changes ({shown}{extra}); \
             run fno do pr rebase, then retry"
        ),
    ))
}

/// (base, head) ref names for a PR, or None on any read miss.
fn pr_base_head_refs<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Option<(String, String)> {
    let args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr.to_string(),
        "--json".to_string(),
        "baseRefName,headRefName".to_string(),
    ];
    let (ok, stdout) = probes.run_gh(cwd, &args).ok()?;
    if !ok {
        return None;
    }
    let refs: Value = serde_json::from_str(&stdout).ok()?;
    let base = refs.get("baseRefName").and_then(Value::as_str)?;
    let head = refs.get("headRefName").and_then(Value::as_str)?;
    if base.is_empty() || head.is_empty() {
        return None;
    }
    Some((base.to_string(), head.to_string()))
}

/// Commits the PR head is behind its base. 0 on any probe miss (never block
/// a merge because our own read failed).
fn behind_by<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> u64 {
    let Some((base, head)) = pr_base_head_refs(probes, cwd, pr) else {
        breadcrumb(
            "stale-base probe unavailable (pr refs unreadable); merging without freshness hold",
        );
        return 0;
    };
    let args = vec![
        "api".to_string(),
        format!("repos/{{owner}}/{{repo}}/compare/{base}...{head}"),
        "-q".to_string(),
        ".behind_by".to_string(),
    ];
    match probes.run_gh(cwd, &args) {
        Ok((true, stdout)) => stdout.trim().parse::<u64>().unwrap_or_else(|_| {
            breadcrumb(
                "stale-base probe unavailable (gh compare failed); merging without freshness hold",
            );
            0
        }),
        _ => {
            breadcrumb(
                "stale-base probe unavailable (gh compare failed); merging without freshness hold",
            );
            0
        }
    }
}

/// Files the BASE branch gained since the PR head diverged, or None (HOLD).
/// Truncation is a miss: an under-reported move fails in the merging direction.
fn base_move_paths<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Option<Vec<String>> {
    let Some((base, head)) = pr_base_head_refs(probes, cwd, pr) else {
        breadcrumb("overlap probe unavailable (pr refs unreadable); holding for a rebase");
        return None;
    };
    let args = vec![
        "api".to_string(),
        format!("repos/{{owner}}/{{repo}}/compare/{head}...{base}"),
        "--jq".to_string(),
        "{truncated: .truncated, names: [.files[] | ((.filename // empty), (.previous_filename // empty))]}"
            .to_string(),
    ];
    let (ok, stdout) = probes.run_gh(cwd, &args).ok()?;
    if !ok {
        breadcrumb("overlap probe unavailable (gh reverse compare failed); holding for a rebase");
        return None;
    }
    let payload: Value = serde_json::from_str(&stdout).ok()?;
    let names = payload.get("names").and_then(Value::as_array)?;
    let mut paths = Vec::with_capacity(names.len());
    for name in names {
        match name.as_str() {
            Some(path) if !path.is_empty() => paths.push(path.to_string()),
            _ => {
                breadcrumb("overlap probe unavailable (compare file list carries non-string entries); holding for a rebase");
                return None;
            }
        }
    }
    if payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || paths.len() >= 300
    {
        breadcrumb(&format!(
            "overlap probe unavailable (compare truncated, {} files; caps at 300); holding for a rebase",
            paths.len()
        ));
        return None;
    }
    Some(paths)
}

/// The PR's own changed file paths, or None (HOLD). An EMPTY list is a real
/// answer: a PR with no diff cannot overlap anything.
fn pr_file_paths<P: Probes>(probes: &P, cwd: &Path, pr: u64) -> Option<Vec<String>> {
    let args = vec![
        "api".to_string(),
        format!("repos/{{owner}}/{{repo}}/pulls/{pr}/files"),
        "--paginate".to_string(),
        "--jq".to_string(),
        ".[] | .filename // empty".to_string(),
    ];
    let (ok, stdout) = probes.run_gh(cwd, &args).ok()?;
    if !ok {
        breadcrumb("overlap probe unavailable (gh pr files read failed); holding for a rebase");
        return None;
    }
    Some(
        stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Sorted intersection of two changed-file lists, documentation paths dropped
/// from both sides first. Empty: no semantic conflict the merge could carry.
fn overlaps(base_paths: &[String], pr_paths: &[String]) -> Vec<String> {
    let base: std::collections::BTreeSet<&str> = base_paths
        .iter()
        .map(String::as_str)
        .filter(|p| !is_documentation_path(p))
        .collect();
    let pr: std::collections::BTreeSet<&str> = pr_paths
        .iter()
        .map(String::as_str)
        .filter(|p| !is_documentation_path(p))
        .collect();
    base.intersection(&pr).map(|p| (*p).to_string()).collect()
}

/// Sorted intersection with no documentation filter: tests can read markdown
/// files, so a shared documentation path can invalidate a run too.
pub(crate) fn shared_paths(a: &[String], b: &[String]) -> Vec<String> {
    let left: std::collections::BTreeSet<&str> = a.iter().map(String::as_str).collect();
    let right: std::collections::BTreeSet<&str> = b.iter().map(String::as_str).collect();
    left.intersection(&right)
        .map(|path| (*path).to_string())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaleOverlap {
    pub(crate) ci_base_sha: String,
    pub(crate) landed: usize,
    pub(crate) shared: Vec<String>,
}

pub(crate) fn stale_overlap(
    cwd: &Path,
    base_rev: &str,
    head_sha: &str,
    since: &str,
) -> Result<StaleOverlap, String> {
    let head_object = format!("{head_sha}^{{commit}}");
    let head_check = std::process::Command::new("git")
        .args(["cat-file", "-e", &head_object])
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("PR head check failed: {error}"))?;
    if !head_check.status.success() {
        return Err(format!(
            "PR head {} not present locally",
            head_sha.chars().take(8).collect::<String>()
        ));
    }

    let ci_base_sha = stale_git_read(
        cwd,
        &[
            "rev-list".to_string(),
            "-1".to_string(),
            "--first-parent".to_string(),
            format!("--before={since}"),
            base_rev.to_string(),
        ],
        "CI base lookup",
    )?;
    if ci_base_sha.is_empty() {
        return Err(format!("no {base_rev} commit at or before {since}"));
    }

    let landed_output = stale_git_read(
        cwd,
        &[
            "diff".to_string(),
            "--name-only".to_string(),
            "--no-renames".to_string(),
            ci_base_sha.clone(),
            base_rev.to_string(),
        ],
        "landed file diff",
    )?;
    let pr_output = stale_git_read(
        cwd,
        &[
            "diff".to_string(),
            "--name-only".to_string(),
            "--no-renames".to_string(),
            format!("{base_rev}...{head_sha}"),
        ],
        "PR file diff",
    )?;
    let landed_paths = diff_paths(&landed_output);
    let pr_paths = diff_paths(&pr_output);
    Ok(StaleOverlap {
        ci_base_sha,
        landed: landed_paths.len(),
        shared: shared_paths(&landed_paths, &pr_paths),
    })
}

fn stale_git_read(cwd: &Path, args: &[String], step: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("{step} failed: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        return Err(format!("{step} failed: {detail}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn diff_paths(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `owner/name` pair a remote url names, normalized across the ssh
/// (`git@host:owner/repo.git`), https (`https://host/owner/repo.git`), and
/// trailing-slash spellings, so the ledger scoping test reads the same
/// repository whatever form the checkout's origin takes.
pub(crate) fn repo_slug_from_origin(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts: Vec<&str> = trimmed
        .split(['/', ':'])
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return None;
    }
    let name = parts.pop()?;
    let owner = parts.pop()?;
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_at(repo: &Path, date: &str, message: &str) -> String {
        git(repo, &["add", "-A"]);
        let output = std::process::Command::new("git")
            .args(["commit", "-q", "-m", message])
            .current_dir(repo)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        git(repo, &["rev-parse", "HEAD"])
    }

    fn init_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "--initial-branch=main"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        repo
    }

    #[test]
    fn stale_overlap_uses_the_first_parent_ci_base_and_counts_landed_files() {
        let repo = init_repo();
        std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
        let base = commit_at(repo.path(), "2026-01-01T00:00:00Z", "base");

        git(repo.path(), &["checkout", "-q", "-b", "topic"]);
        std::fs::write(repo.path().join("landed.txt"), "landed\n").unwrap();
        commit_at(repo.path(), "2026-01-02T00:00:00Z", "topic change");
        git(repo.path(), &["checkout", "-q", "main"]);
        std::fs::write(repo.path().join("main.txt"), "main\n").unwrap();
        let ci_base = commit_at(repo.path(), "2026-01-01T12:00:00Z", "main change");
        let merge = std::process::Command::new("git")
            .args(["merge", "--no-ff", "--no-edit", "topic"])
            .current_dir(repo.path())
            .env("GIT_AUTHOR_DATE", "2026-01-04T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-04T00:00:00Z")
            .output()
            .unwrap();
        assert!(
            merge.status.success(),
            "git merge failed: {}",
            String::from_utf8_lossy(&merge.stderr)
        );

        git(
            repo.path(),
            &["checkout", "-q", "-b", "pull-request", &base],
        );
        std::fs::write(repo.path().join("pr.txt"), "pr\n").unwrap();
        let head = commit_at(repo.path(), "2026-01-02T12:00:00Z", "pr change");

        let result = stale_overlap(repo.path(), "main", &head, "2026-01-03T00:00:00Z");
        assert!(
            result.is_ok(),
            "stale overlap should be readable: {result:?}"
        );
        let result = result.unwrap();
        assert_eq!(result.ci_base_sha, ci_base);
        assert_eq!(result.landed, 1);
        assert!(result.shared.is_empty());
    }

    #[test]
    fn stale_overlap_keeps_both_rename_paths_for_shared_files() {
        let repo = init_repo();
        std::fs::write(repo.path().join("a.rs"), "base\n").unwrap();
        commit_at(repo.path(), "2026-01-01T00:00:00Z", "base");

        git(repo.path(), &["checkout", "-q", "-b", "pull-request"]);
        git(repo.path(), &["mv", "a.rs", "b.rs"]);
        let head = commit_at(repo.path(), "2026-01-02T00:00:00Z", "rename");
        git(repo.path(), &["checkout", "-q", "main"]);
        std::fs::write(repo.path().join("a.rs"), "main\n").unwrap();
        commit_at(repo.path(), "2026-01-04T00:00:00Z", "main change");

        let result = stale_overlap(repo.path(), "main", &head, "2026-01-03T00:00:00Z");
        assert!(
            result.is_ok(),
            "stale overlap should be readable: {result:?}"
        );
        assert_eq!(result.unwrap().shared, vec!["a.rs".to_string()]);
    }

    #[test]
    fn stale_overlap_fails_when_the_pr_head_is_missing() {
        let repo = init_repo();
        std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
        commit_at(repo.path(), "2026-01-01T00:00:00Z", "base");

        let result = stale_overlap(repo.path(), "main", "deadbeef", "2026-01-03T00:00:00Z");
        let error = result.unwrap_err();
        assert!(
            error.contains("PR head deadbeef not present locally"),
            "{error}"
        );
    }

    #[test]
    fn stale_overlap_reports_a_malformed_missing_head_without_panicking() {
        let repo = init_repo();
        std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
        commit_at(repo.path(), "2026-01-01T00:00:00Z", "base");

        let result = stale_overlap(repo.path(), "main", "abcdefgé", "2026-01-03T00:00:00Z");
        assert!(matches!(result, Err(error) if error == "PR head abcdefgé not present locally"));
    }

    #[test]
    fn overlaps_drops_documentation_paths_from_both_sides() {
        let base = vec!["src/a.rs".to_string(), "docs/guide.md".to_string()];
        let pr = vec!["src/a.rs".to_string(), "docs/guide.md".to_string()];
        assert_eq!(overlaps(&base, &pr), vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn an_unreconciled_manifest_of_a_contract_node_holds() {
        // AC6's gate half: mocks would ship; the merge holds by code.
        let root = std::env::temp_dir().join(format!("x53c5-stub-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".fno")).unwrap();
        let node = "x-test";
        std::fs::write(
            root.join(".fno").join(format!("stub-manifest-{node}.json")),
            r#"{"reconciled": false, "stubs": [{"stub_id": "a"}]}"#,
        )
        .unwrap();
        let blocker = stub_manifest_blocker(&root, node).expect("held");
        assert_eq!(blocker.code, "stub_manifest_unreconciled");
        assert!(blocker.detail.contains("1 stub(s)"), "{}", blocker.detail);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_reconciled_manifest_clears_and_a_missing_one_never_holds() {
        let root = std::env::temp_dir().join(format!("x53c5-stub2-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".fno")).unwrap();
        let node = "x-test";
        std::fs::write(
            root.join(".fno").join(format!("stub-manifest-{node}.json")),
            r#"{"reconciled": true, "stubs": []}"#,
        )
        .unwrap();
        assert!(stub_manifest_blocker(&root, node).is_none());
        // No manifest carried: nothing to hold against.
        assert!(stub_manifest_blocker(&root, "x-never-wrote").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn contract_node_matches_dep_and_pr() {
        let entries = vec![
            json!({"id": "x-1111", "dep": "hard", "pr_number": 7}),
            json!({"id": "x-2222", "dep": "contract", "pr_number": 8}),
        ];
        assert_eq!(contract_node_for_pr(&entries, 8).as_deref(), Some("x-2222"));
        assert_eq!(contract_node_for_pr(&entries, 7), None);
    }

    #[test]
    fn contract_node_reads_the_persisted_pr_fields() {
        // The store persists `pr_number` + `additional_prs`; a dependent
        // recorded only in the additional list must still be found.
        let entries = vec![json!({
            "id": "x-3333",
            "dep": "contract",
            "pr_number": 7,
            "additional_prs": [
                {"number": 42},
                {"url": "https://github.com/o/r/pull/99"},
                105,
            ],
        })];
        for pr in [7u64, 42, 99, 105] {
            assert_eq!(
                contract_node_for_pr(&entries, pr).as_deref(),
                Some("x-3333"),
                "pr {pr} must match"
            );
        }
        assert_eq!(contract_node_for_pr(&entries, 8), None);
    }

    #[test]
    fn repo_slug_normalizes_the_remote_spellings() {
        assert_eq!(
            repo_slug_from_origin("git@github.com:o/r.git"),
            Some("o/r".to_string())
        );
        assert_eq!(
            repo_slug_from_origin("https://github.com/o/r.git/"),
            Some("o/r".to_string())
        );
        assert_eq!(
            repo_slug_from_origin("ssh://git@host:2222/o/r"),
            Some("o/r".to_string())
        );
        assert_eq!(repo_slug_from_origin("o"), None);
    }
}
