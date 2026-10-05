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

/// The visual-approval gate: a PR whose changed files touch a configured
/// paint path holds until an ANSWERED question page names the PR. The user's
/// look is the only clear, mechanizing the prose rulings this gate replaces
/// (rebrand and splash PRs merged with no user look). `merge.visual_paint_paths`
/// lists the lines the gate watches; empty (the default everywhere) disarms it.
pub(crate) fn visual_approval_blocker<P: Probes>(
    probes: &P,
    cwd: &Path,
    pr: u64,
) -> Option<Blocker> {
    let paint_paths = crate::agents_config::visual_paint_paths(cwd);
    if paint_paths.is_empty() {
        return None;
    }
    let Some(files) = pr_file_paths(probes, cwd, pr) else {
        // The files list is this gate's one instrument; an unreadable list
        // must not release a paint PR, so it reads unknown (never a verdict),
        // the same class the pin uses for an unreadable covered head.
        return Some(Blocker::unknown(
            "visual_paint_paths_unknown",
            format!(
                "PR {pr}: the changed-file list was unreadable, so the visual-approval gate cannot answer"
            ),
        ));
    };
    let touched: Vec<&str> = files
        .iter()
        .filter(|f| path_matches_paint(f, &paint_paths))
        .map(String::as_str)
        .collect();
    if touched.is_empty() {
        return None;
    }
    if answered_question_names_pr(cwd, pr) {
        return None;
    }
    Some(Blocker::held(
        "visual_approval",
        format!(
            "PR {pr} touches the paint surface the config lists ({}) and no answered \
             question page names it; the user's look is the only clear. Ask via \
             `fno inbox outstanding ask`, then the user answers the page.",
            touched.join(", ")
        ),
    ))
}

/// Does one changed file match the paint-path list? Two pattern shapes: a
/// `dir/**` subtree and a bare file name (any directory). `ponytail:` no glob
/// engine; a mid-pattern `**` or `*` wildcard is added when a config needs it.
fn path_matches_paint(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = p.trim();
        if let Some(prefix) = p.strip_suffix("/**") {
            let prefix = prefix.trim_end_matches('/');
            return path.starts_with(prefix)
                && (path.len() == prefix.len() || path[prefix.len()..].starts_with('/'));
        }
        if !p.contains('/') {
            return path.rsplit('/').next() == Some(p);
        }
        path == p
    })
}

/// True when any ANSWERED question page in this project's questions directory
/// names the PR: the pages the lead check-in reads, parsed the same way.
fn answered_question_names_pr(cwd: &Path, pr: u64) -> bool {
    let dir = crate::escalation::questions_dir(cwd);
    // Answered pages are archived into done/ after the fact; an approval
    // must not lapse because its page moved there.
    let mut dirs = vec![dir.clone()];
    dirs.push(dir.join("done"));
    dirs.iter().any(|d| {
        crate::lead_answers::read_question_pages(d).is_ok_and(|pages| {
            pages.iter().any(|(_stem, text)| {
                crate::attention_file::parse_page(text)
                    .map(|(front, _)| front.status == "answered")
                    .unwrap_or(false)
                    && page_names_pr(text, pr)
            })
        })
    })
}

/// The page text names the PR: a word-start `pr` (any case), an optional
/// `#`/`-`/`/`/`:`/space separator run, then the PR number, not followed by
/// another digit. Prose like "the PR is 3058" or "PRs 3058" never matches.
fn page_names_pr(text: &str, pr: u64) -> bool {
    let needle = pr.to_string();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i + 2 <= bytes.len() {
        let is_pr =
            bytes[i].to_ascii_lowercase() == b'p' && bytes[i + 1].to_ascii_lowercase() == b'r';
        if !is_pr {
            i += 1;
            continue;
        }
        let word_start = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        i += 1;
        if !word_start {
            continue;
        }
        let mut j = i + 1; // first byte after "pr"
                           // One separator run, spaces and symbols in any order, so "PR #3036"
                           // and "pr - 3036" read like "PR 3036".
        while matches!(
            bytes.get(j),
            Some(b'#') | Some(b'-') | Some(b'/') | Some(b':') | Some(b' ')
        ) {
            j += 1;
        }
        let rest = &text[j.min(text.len())..];
        if rest.starts_with(&needle)
            && !rest[needle.len()..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
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
    #[test]
    fn a_paint_pattern_matches_its_subtree_and_a_bare_name_anywhere() {
        let patterns = vec![
            "crates/fno/src/client/**".to_string(),
            "theme.rs".to_string(),
            "docs/brand.md".to_string(),
        ];
        let m = |p: &str| path_matches_paint(p, &patterns);
        assert!(m("crates/fno/src/client/pane_paint.rs"));
        assert!(!m("crates/fno/src/client.rs"));
        assert!(m("crates/fno/src/theme.rs"));
        assert!(m("theme.rs"));
        assert!(m("docs/brand.md"));
        assert!(!m("docs/brand.md.bak"));
    }
    #[test]
    fn a_page_names_the_pr_only_at_a_word_start_with_the_number() {
        assert!(page_names_pr("May PR 3036 merge?", 3036));
        assert!(page_names_pr("pr-3036 ships the splash", 3036));
        assert!(page_names_pr("(PR #3036)", 3036));
        assert!(!page_names_pr("PRs 3036 land", 3036));
        assert!(!page_names_pr("the PR is 3036", 3036));
        assert!(!page_names_pr("PR 30365", 3036));
        assert!(!page_names_pr("plain 3036", 3036));
    }
}
