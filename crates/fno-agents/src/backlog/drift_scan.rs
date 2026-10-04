//! The merge-drift scan: open nodes whose PR merged outside the ship gate,
//! plus the reverse map that closes ref-less nodes by merged branch name.
//! Ported from graph/_reconcile.py (`MergeDriftRecord`, `scan_merge_drift`,
//! `reverse_map_unstamped`, the REST listings and their per-run cache).

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::merge_evidence::PrReadError;
use super::merge_state::{query_pr_merge_state, PrMergeState};
use super::pr_link::repo_slug_from_url;
use crate::backlog_ready::node_is_open;
use crate::loop_dispatch::which_binary;

/// The reverse map and open-binding scans fire one gh call per distinct
/// repo in their scope; a wall-clock budget bounds the fan-out the same
/// way the Python sweep's REVERSE_MAP_BUDGET_S does.
const REVERSE_MAP_BUDGET: Duration = Duration::from_secs(60);

/// One open node carrying a PR whose GitHub state we resolved. `closeable`
/// records hold a MERGED PR and are safe to close. Records with a non-None
/// `error` could not be resolved and are surfaced but never closed.
#[derive(Debug, Clone)]
pub(crate) struct MergeDriftRecord {
    pub node_id: String,
    pub plan_path: Option<String>,
    pub pr_number: i64,
    pub pr_url: Option<String>,
    pub pr_state: String,
    pub merged_at: Option<String>,
    pub error: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// mergeCommit.oid for the closed PR: the exactly-once dedup key for
    /// the post-merge-ritual auto-dispatch. None on a reverse-mapped
    /// record (branch-name match has no SHA).
    pub merge_sha: Option<String>,
    pub changed_files: Vec<String>,
    pub files_truncated: bool,
    pub error_kind: Option<String>,
    pub remedy: Option<String>,
}

impl MergeDriftRecord {
    pub(crate) fn closeable(&self) -> bool {
        self.error.is_none() && self.pr_state == "MERGED"
    }
}

/// gh on PATH, else None (reconcile auto-fires on SessionStart; a gh-less
/// machine must stay quiet).
pub(crate) fn gh_executable() -> Option<PathBuf> {
    which_binary("gh")
}

fn map_pr_state(row: &Value) -> Result<String, String> {
    if row.get("merged").and_then(Value::as_bool).unwrap_or(false) {
        return Ok("MERGED".into());
    }
    match row
        .get("state")
        .and_then(Value::as_str)
        .map(|s| s.to_ascii_uppercase())
        .as_deref()
    {
        Some("OPEN") => Ok("OPEN".into()),
        Some("CLOSED") => Ok("CLOSED".into()),
        Some(other) => Err(format!("malformed PR state {other:?}")),
        None => Err("REST row carried no state".into()),
    }
}

/// One REST pulls listing with the detail rows the scans read:
/// `{number, state, title, headRefName, url, mergedAt, body}`. A malformed
/// head/title/url fails the whole page loudly, never an absent answer.
fn rest_pr_rows(cwd: &Path, state: &str, max_pages: usize) -> Result<Vec<Value>, PrReadError> {
    let Some(gh) = gh_executable() else {
        return Ok(Vec::new());
    };
    let slug = super::pr_link::resolve_current_repo_slug(cwd.to_str())
        .ok_or_else(|| PrReadError::new("could not resolve owner/repo from the checkout", ""))?;
    let mut rows: Vec<Value> = Vec::new();
    for page in 1..=max_pages {
        let path = format!("repos/{slug}/pulls?state={state}&per_page=100&page={page}");
        let (ok, out, err) = crate::pr_push::run_labeled(
            "backlog-evidence",
            gh.to_str().unwrap_or("gh"),
            &["api", "--allow-escape-sequences", &path],
            cwd,
            Duration::from_secs(30),
        )
        .map_err(|e| PrReadError::new(e, "availability"))?;
        if !ok {
            let message = if err.trim().is_empty() { out } else { err };
            return Err(PrReadError::new(
                format!("gh api pulls ({state}) failed: {message}"),
                "",
            ));
        }
        let payload: Value = serde_json::from_str(out.trim()).map_err(|e| {
            PrReadError::new(
                format!("gh api pulls list page {page} returned output that is not JSON: {e}"),
                "malformed",
            )
        })?;
        let Some(rows_page) = payload.as_array() else {
            return Err(PrReadError::new(
                format!("gh api pulls list page {page} was not a JSON array"),
                "malformed",
            ));
        };
        for row in rows_page {
            let Some(number) = row.get("number").and_then(Value::as_i64) else {
                continue;
            };
            let head_ref = row
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    PrReadError::new(
                        format!("gh api pulls list page {page} carried malformed head ref"),
                        "malformed",
                    )
                })?;
            if !row.get("title").map(Value::is_string).unwrap_or(false)
                || !row.get("html_url").map(Value::is_string).unwrap_or(false)
            {
                return Err(PrReadError::new(
                    format!("gh api pulls list page {page} carried malformed title/url"),
                    "malformed",
                ));
            }
            let state_word = map_pr_state(row).map_err(|e| PrReadError::new(e, "malformed"))?;
            rows.push(json!({
                "number": number,
                "state": state_word,
                "title": row.get("title"),
                "headRefName": head_ref,
                "url": row.get("html_url"),
                "mergedAt": row.get("merged_at"),
                "body": row.get("body").and_then(Value::as_str).unwrap_or(""),
            }));
        }
        if rows_page.len() < 100 {
            return Ok(rows);
        }
    }
    // Every page came back full: the ceiling cut a listing that had more
    // rows. Loud on purpose, like the Python twin's warning.
    eprintln!(
        "reconcile: gh api pulls list hit the max_pages={max_pages} ceiling with a full last page; listing is possibly truncated after {} rows",
        rows.len()
    );
    Ok(rows)
}

/// Merged PRs (number/url/headRefName/mergedAt) for reverse-mapping, run in
/// `cwd` so gh resolves the repo from that dir's origin remote. Empty when
/// gh is absent; a real gh failure is a typed refusal so the caller degrades
/// with one advisory per repo.
pub(crate) fn list_merged_pr_branches(cwd: &str, limit: usize) -> Result<Vec<Value>, PrReadError> {
    if gh_executable().is_none() {
        return Ok(Vec::new());
    }
    let rows: Vec<Value> = rest_pr_rows(Path::new(cwd), "closed", 1)?
        .into_iter()
        .filter(|r| r.get("state").and_then(Value::as_str) == Some("MERGED"))
        .take(limit)
        .collect();
    Ok(rows)
}

/// Open PRs (number/url/headRefName) for the open-binding heal. Same
/// contract as the merged listing; the listing itself is capped at two
/// pages and the row count at `limit`.
pub(crate) fn list_open_pr_branches(cwd: &str, limit: usize) -> Result<Vec<Value>, PrReadError> {
    if gh_executable().is_none() {
        return Ok(Vec::new());
    }
    let rows = rest_pr_rows(Path::new(cwd), "open", 2)?;
    if rows.len() > limit {
        return Err(PrReadError::new(
            format!("open PR listing hit its {limit}-row limit; refusing a unique binding"),
            "",
        ));
    }
    Ok(rows)
}

/// The git common dir of `cwd`, or `cwd` when git can't say. `.git` comes
/// back relative on a main checkout, so the answer is anchored and
/// canonicalized for the keys to agree across worktrees.
pub(crate) fn repo_group_key(cwd: &str, memo: &mut HashMap<String, String>) -> String {
    if let Some(hit) = memo.get(cwd) {
        return hit.clone();
    }
    let probe = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .output();
    let hit = match probe {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
            std::fs::canonicalize(Path::new(cwd).join(rel))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| cwd.to_string())
        }
        _ => cwd.to_string(),
    };
    memo.insert(cwd.to_string(), hit.clone());
    hit
}

/// One gh listing per repo per run, shared by the scans. A failed fetch
/// caches the error so later asks re-raise, not re-hit gh.
#[derive(Default)]
pub(crate) struct ListingCache {
    repo_keys: RefCell<HashMap<String, String>>,
    store: RefCell<HashMap<(String, String), Vec<Value>>>,
    errors: RefCell<HashMap<(String, String), String>>,
}

impl ListingCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cached rows for `kind` ("open" | "merged") of `cwd`'s repo.
    pub(crate) fn rows_for(&self, kind: &str, cwd: &str) -> Result<Vec<Value>, PrReadError> {
        self.rows_for_with(kind, cwd, |cwd| match kind {
            "open" => list_open_pr_branches(cwd, 100),
            _ => list_merged_pr_branches(cwd, 100),
        })
    }

    /// The seam: tests pass a stub fetch, production uses the closures above.
    pub(crate) fn rows_for_with(
        &self,
        kind: &str,
        cwd: &str,
        fetch: impl Fn(&str) -> Result<Vec<Value>, PrReadError>,
    ) -> Result<Vec<Value>, PrReadError> {
        let mut keys = self.repo_keys.borrow_mut();
        let key = (kind.to_string(), repo_group_key(cwd, &mut keys));
        drop(keys);
        if let Some(err) = self.errors.borrow().get(&key) {
            return Err(PrReadError::new(err.clone(), ""));
        }
        if let Some(rows) = self.store.borrow().get(&key) {
            return Ok(rows.clone());
        }
        match fetch(cwd) {
            Ok(rows) => {
                self.store.borrow_mut().insert(key, rows.clone());
                Ok(rows)
            }
            Err(e) => {
                self.errors.borrow_mut().insert(key, e.message.clone());
                Err(e)
            }
        }
    }
}

/// `cwd` is a dir inside some git checkout (its own `.git` or an ancestor's).
fn in_checkout(cwd: &Path) -> bool {
    if !cwd.is_dir() {
        return false;
    }
    let mut probe = Some(cwd);
    while let Some(dir) = probe {
        if dir.join(".git").exists() {
            return true;
        }
        probe = dir.parent();
    }
    false
}

/// The dir reconcile should run a node's gh query / post-close routing in:
/// the recorded cwd (a worktree) when it is a live checkout, else the
/// node's own project checkout when that exists, else the original cwd so
/// the existing degrade is strictly unchanged.
pub(crate) fn effective_reconcile_cwd(cwd: &str, project: Option<&str>) -> String {
    if !cwd.is_empty() && !in_checkout(Path::new(cwd)) {
        if let Some(project) = project.filter(|p| !p.is_empty()) {
            if let Some(root) = super::settings::project_root(project) {
                if Path::new(&root).is_dir() {
                    return root;
                }
            }
        }
    }
    cwd.to_string()
}

/// True when `node_id` is a full delimiter-bounded segment of `head_ref`.
/// A bare substring must NOT match: fixed-width hex ids make a short id a
/// prefix of a longer one, so an unbounded match would close the wrong node.
pub(crate) fn branch_matches_node(head_ref: &str, node_id: &str) -> bool {
    if head_ref.is_empty() || node_id.is_empty() {
        return false;
    }
    let escaped = regex::escape(node_id);
    let re = regex::Regex::new(&format!(r"(^|[/-]){escaped}([/-]|$)")).expect("static pattern");
    re.is_match(head_ref)
}

/// Group open ref-less candidates by repo: (groups, cwd_by_nid, skipped).
/// Shared eligibility of both listing scans; the first member's cwd runs
/// the gh call so gh still resolves the repo from that dir's origin
/// remote. Groups carry entry indices so the callers can re-read rows.
fn group_refless_by_repo(
    entries: &[Value],
    scope: Option<&BTreeSet<String>>,
    memo: &mut HashMap<String, String>,
    with_refs: bool,
) -> (
    Vec<(String, Vec<usize>)>,
    HashMap<String, String>,
    Vec<String>,
) {
    let mut by_repo: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut cwd_by_nid: HashMap<String, String> = HashMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for (index, node) in entries.iter().enumerate() {
        let Some(nid) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(scope) = scope {
            if !scope.contains(nid) {
                continue;
            }
        }
        if !node_is_open(node) || (!with_refs && !node_pr_refs_of(node).is_empty()) {
            continue;
        }
        let raw = node.get("cwd").and_then(Value::as_str).unwrap_or("");
        if raw.is_empty() {
            continue;
        }
        let cwd = effective_reconcile_cwd(raw, node.get("project").and_then(Value::as_str));
        if !Path::new(&cwd).is_dir() || repo_group_key(&cwd, memo) == cwd {
            skipped.push(nid.to_string());
            continue;
        }
        cwd_by_nid.insert(nid.to_string(), cwd.clone());
        let key = repo_group_key(&cwd, memo);
        by_repo.entry(key).or_default().push(index);
    }
    (by_repo.into_iter().collect(), cwd_by_nid, skipped)
}

fn node_pr_refs_of(node: &Value) -> Vec<(i64, Option<String>)> {
    super::merge_evidence::node_pr_refs(node)
}

/// First row carrying this number whose repo may answer for the ref.
fn listing_answer<'a>(rows: &'a [Value], number: i64, ref_repo: Option<&str>) -> Option<&'a Value> {
    rows.iter().find(|row| {
        row.get("number").and_then(Value::as_i64) == Some(number)
            && match (
                repo_slug_from_url(row.get("url").and_then(Value::as_str)),
                ref_repo,
            ) {
                (None, _) | (_, None) => true,
                (Some(slug), Some(repo)) => slug.eq_ignore_ascii_case(repo),
            }
    })
}

/// Close open nodes with NO PR refs by matching the id in a merged branch.
pub(crate) fn reverse_map_unstamped(
    entries: &[Value],
    scope: Option<&BTreeSet<String>>,
    listings: Option<&ListingCache>,
) -> Vec<MergeDriftRecord> {
    let mut fallback = HashMap::new();
    let (groups, cwd_by_nid, skipped_dead_cwd) = {
        let mut cache_memo;
        let memo: &mut HashMap<String, String> = match listings {
            Some(cache) => {
                cache_memo = cache.repo_keys.borrow_mut();
                &mut cache_memo
            }
            None => &mut fallback,
        };
        group_refless_by_repo(entries, scope, memo, false)
    };

    if !skipped_dead_cwd.is_empty() {
        // Name EVERY skipped id: the id is the only handle an operator has
        // to heal a genuinely-merged-but-archived node.
        eprintln!(
            "reverse-map: skipped {} ref-less node(s) with missing or non-checkout cwd: {} (heal with: fno backlog update <id> --project <p> --cwd <path>)",
            skipped_dead_cwd.len(),
            skipped_dead_cwd.join(" ")
        );
    }

    let mut records: Vec<MergeDriftRecord> = Vec::new();
    let deadline = Instant::now() + REVERSE_MAP_BUDGET;
    for (_key, nodes) in &groups {
        if Instant::now() >= deadline {
            let deferred: Vec<String> = groups
                .iter()
                .flat_map(|(_, ns)| ns.iter())
                .filter_map(|i| entries[*i].get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            eprintln!(
                "reverse-map: stopped at its {}s budget (gh is slow or degraded); deferred {} node(s) to a later sweep: {}",
                REVERSE_MAP_BUDGET.as_secs(),
                deferred.len(),
                deferred.join(" ")
            );
            break;
        }
        let Some(first) = nodes.first() else { continue };
        let nid0 = entries[*first]
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let gh_cwd = cwd_by_nid
            .get(&nid0)
            .map(String::as_str)
            .unwrap_or_default();
        let merged_rows = match listings {
            Some(cache) => cache.rows_for("merged", gh_cwd),
            None => list_merged_pr_branches(gh_cwd, 100),
        };
        let merged_rows = match merged_rows {
            Ok(rows) => rows,
            Err(e) => {
                for node_index in nodes {
                    let node = &entries[*node_index];
                    records.push(MergeDriftRecord {
                        node_id: node
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        plan_path: str_field(node.get("plan_path")),
                        pr_number: 0,
                        pr_url: None,
                        pr_state: "UNKNOWN".into(),
                        merged_at: None,
                        error: Some(format!("reverse-map gh query failed: {}", e.message)),
                        session_id: str_field(node.get("session_id")),
                        cwd: cwd_by_nid
                            .get(node.get("id").and_then(Value::as_str).unwrap_or_default())
                            .cloned(),
                        merge_sha: None,
                        changed_files: Vec::new(),
                        files_truncated: false,
                        error_kind: Some(e.kind.clone()),
                        remedy: None,
                    });
                }
                continue;
            }
        };
        for node_index in nodes {
            let node = &entries[*node_index];
            let nid = node.get("id").and_then(Value::as_str).unwrap_or_default();
            let hits: Vec<&Value> = merged_rows
                .iter()
                .filter(|row| {
                    branch_matches_node(
                        row.get("headRefName").and_then(Value::as_str).unwrap_or(""),
                        nid,
                    )
                })
                .collect();
            if hits.is_empty() {
                continue;
            }
            let mut numbers: Vec<i64> = hits
                .iter()
                .filter_map(|r| r.get("number").and_then(Value::as_i64))
                .collect();
            numbers.sort_unstable();
            numbers.dedup();
            if numbers.len() > 1 {
                let nums = numbers
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                records.push(MergeDriftRecord {
                    node_id: nid.to_string(),
                    plan_path: str_field(node.get("plan_path")),
                    pr_number: 0,
                    pr_url: None,
                    pr_state: "UNKNOWN".into(),
                    merged_at: None,
                    error: Some(format!(
                        "reverse-map ambiguous: {nums} both match branch id {nid}"
                    )),
                    session_id: str_field(node.get("session_id")),
                    cwd: cwd_by_nid.get(nid).cloned(),
                    merge_sha: None,
                    changed_files: Vec::new(),
                    files_truncated: false,
                    error_kind: None,
                    remedy: None,
                });
                continue;
            }
            let row = hits[0];
            records.push(MergeDriftRecord {
                node_id: nid.to_string(),
                plan_path: str_field(node.get("plan_path")),
                pr_number: row.get("number").and_then(Value::as_i64).unwrap_or(0),
                pr_url: str_field(row.get("url")),
                pr_state: "MERGED".into(),
                merged_at: str_field(row.get("mergedAt")),
                error: None,
                session_id: str_field(node.get("session_id")),
                cwd: cwd_by_nid.get(nid).cloned(),
                merge_sha: None,
                changed_files: Vec::new(),
                files_truncated: false,
                error_kind: None,
                remedy: None,
            });
        }
    }
    records
}

/// (node_id, revert_pr_number) pairs to stamp `reverted: true`.
///
/// A GitHub revert PR titles itself `Revert "..."` and auto-writes
/// `Reverts owner/repo#N` in the body; a hand-written one usually keeps the
/// git subject. Misses are a documented limitation with the manual
/// `fno backlog update --reverted` fallback.
pub(crate) fn detect_reverted_nodes(merged_prs: &[Value], entries: &[Value]) -> Vec<(String, i64)> {
    let title_re = regex::Regex::new(r"^\s*Revert\b").expect("static pattern");
    let body_re =
        regex::Regex::new(r"(?i)\breverts\s+(?:([\w.-]+/[\w.-]+))?#(\d+)").expect("static pattern");
    // pr_number -> [(entry index, that ref's repo slug)]; refs without a
    // parseable pr_url are indexed with slug None and never match
    // (conservative).
    let mut by_pr: HashMap<i64, Vec<(usize, Option<String>)>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        for (num, url) in node_pr_refs_of(entry) {
            let slug = repo_slug_from_url(url.as_deref());
            by_pr.entry(num).or_default().push((index, slug));
        }
    }

    let mut out: Vec<(String, i64)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for pr in merged_prs {
        let Some(number) = pr.get("number").and_then(Value::as_i64) else {
            continue;
        };
        if number <= 0 {
            continue;
        }
        let title = pr.get("title").and_then(Value::as_str).unwrap_or("");
        if !title_re.is_match(title) {
            continue;
        }
        let Some(revert_slug) = repo_slug_from_url(pr.get("url").and_then(Value::as_str)) else {
            // No repo context for the revert PR itself: refuse to match a
            // bare number against the multi-project graph.
            continue;
        };
        let body = pr.get("body").and_then(Value::as_str).unwrap_or("");
        for caps in body_re.captures_iter(body) {
            let qualifier = caps.get(1).map(|m| m.as_str());
            let Ok(target) = caps[2].parse::<i64>() else {
                continue;
            };
            if qualifier.is_some_and(|q| !q.eq_ignore_ascii_case(&revert_slug)) {
                continue; // explicit cross-repo reference: not this repo's PR
            }
            let matches: Vec<usize> = by_pr
                .get(&target)
                .map(|rows| {
                    rows.iter()
                        .filter(|(index, slug)| {
                            slug.as_ref()
                                .is_some_and(|s| s.eq_ignore_ascii_case(&revert_slug))
                                && !entries[*index]
                                    .get("reverted")
                                    .map(|v| !v.is_null() && v.as_bool().unwrap_or(false))
                                    .unwrap_or(false)
                        })
                        .map(|(index, _)| *index)
                        .collect()
                })
                .unwrap_or_default();
            let ids: BTreeSet<&str> = matches
                .iter()
                .filter_map(|i| entries[*i].get("id").and_then(Value::as_str))
                .collect();
            if ids.len() != 1 {
                continue; // zero or ambiguous: stamp nothing
            }
            let nid = ids.into_iter().next().expect("exactly one");
            if seen.insert(nid.to_string()) {
                out.push((nid.to_string(), number));
            }
        }
    }
    out
}

/// Find open nodes whose PR has merged outside the ship gate. With
/// `listings`, a ref resolves against the repo's listings first; the
/// per-node query fires only for a number in neither listing.
///
/// The query seam keeps tests hermetic: production passes
/// [`query_seam`]. Successors of a pending supersession need changed-file
/// evidence, which only the per-node query fetches: they route around the
/// listings.
pub(crate) fn scan_merge_drift(
    entries: &[Value],
    scope: Option<&BTreeSet<String>>,
    listings: Option<&ListingCache>,
    query: impl Fn(i64, Option<&str>, Option<&str>, bool) -> Result<PrMergeState, PrReadError>,
) -> Vec<MergeDriftRecord> {
    let pending_supersede: BTreeSet<&str> = entries
        .iter()
        .filter(|e| {
            e.get("supersession")
                .map(|r| r.is_object() && r.get("verified_at").is_none())
                .unwrap_or(false)
        })
        .filter_map(|e| e.get("superseded_by").and_then(Value::as_str))
        .collect();

    let mut records: Vec<MergeDriftRecord> = Vec::new();

    for node in entries {
        let Some(nid) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(scope) = scope {
            if !scope.contains(nid) {
                continue;
            }
        }
        if !node_is_open(node) {
            continue;
        }
        let refs = node_pr_refs_of(node);
        if refs.is_empty() {
            continue;
        }

        // Same dead-cwd defect as the reverse path: a subprocess cwd of a
        // missing dir fails at launch even when --repo was parsed from
        // pr_url. Resolve through the project-root fallback; if still gone,
        // degrade to None so a repo-scoped query still succeeds via --repo.
        let raw_cwd = node.get("cwd").and_then(Value::as_str).unwrap_or("");
        let cwd = if raw_cwd.is_empty() {
            None
        } else {
            let resolved =
                effective_reconcile_cwd(raw_cwd, node.get("project").and_then(Value::as_str));
            if Path::new(&resolved).is_dir() {
                Some(resolved)
            } else {
                None
            }
        };

        let mut merged_rows: Vec<Value> = Vec::new();
        let mut open_rows: Vec<Value> = Vec::new();
        if let Some(cache) = listings {
            if cwd.is_some() && !pending_supersede.contains(nid) {
                if let Ok(rows) = cache.rows_for("merged", cwd.as_deref().unwrap_or("")) {
                    merged_rows = rows;
                }
                if let Ok(rows) = cache.rows_for("open", cwd.as_deref().unwrap_or("")) {
                    open_rows = rows;
                }
            }
        }

        let mut merged: Option<PrMergeState> = None;
        let mut first_error: Option<String> = None;
        let mut first_error_kind: Option<String> = None;
        let mut first_remedy: Option<String> = None;

        for (number, url) in &refs {
            // Prefer an explicit repo parsed from the PR URL so we never
            // resolve a PR number against the wrong repository. With
            // neither URL nor cwd we cannot safely identify the repo:
            // record a failure rather than risk closing a node off a
            // same-numbered PR elsewhere.
            let repo = repo_slug_from_url(url.as_deref());
            if let Some(hit) = listing_answer(&merged_rows, *number, repo.as_deref()) {
                // No mergeCommit oid/files on a listing row - like a
                // reverse-mapped record.
                merged = Some(PrMergeState {
                    number: *number,
                    state: "MERGED".into(),
                    url: str_field(hit.get("url")),
                    merged_at: str_field(hit.get("mergedAt")),
                    merge_sha: None,
                    changed_files: Vec::new(),
                    files_truncated: false,
                });
                break;
            }
            if listing_answer(&open_rows, *number, repo.as_deref()).is_some() {
                continue; // still open on GitHub: no drift, and no query owed
            }
            if repo.is_none() && cwd.is_none() {
                if first_error.is_none() {
                    first_error = Some(format!(
                        "PR #{number}: no repo context (pr_url unparseable and cwd unset); refusing to query to avoid a wrong-repo match"
                    ));
                    first_error_kind = Some("repository_context".to_string());
                }
                continue;
            }
            match query(*number, repo.as_deref(), cwd.as_deref(), true) {
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e.message.clone());
                        first_error_kind = Some(e.kind.clone());
                        first_remedy = Some(e.remedy_for(*number, repo.as_deref()));
                    }
                    continue;
                }
                Ok(state) => {
                    if state.state == "MERGED" {
                        merged = Some(state);
                        break;
                    }
                }
            }
        }

        if let Some(state) = merged {
            records.push(MergeDriftRecord {
                node_id: nid.to_string(),
                plan_path: str_field(node.get("plan_path")),
                pr_number: state.number,
                pr_url: state.url,
                pr_state: "MERGED".into(),
                merged_at: state.merged_at,
                error: None,
                session_id: str_field(node.get("session_id")),
                cwd,
                merge_sha: state.merge_sha,
                changed_files: state.changed_files,
                files_truncated: state.files_truncated,
                error_kind: None,
                remedy: None,
            });
        } else if let Some(error) = first_error {
            // Could not resolve any PR for this open node. Surface it so
            // the caller reports a query failure rather than silently
            // dropping.
            let (number, url) = &refs[0];
            records.push(MergeDriftRecord {
                node_id: nid.to_string(),
                plan_path: str_field(node.get("plan_path")),
                pr_number: *number,
                pr_url: url.clone(),
                pr_state: "UNKNOWN".into(),
                merged_at: None,
                error: Some(error),
                session_id: str_field(node.get("session_id")),
                cwd,
                merge_sha: None,
                changed_files: Vec::new(),
                files_truncated: false,
                error_kind: first_error_kind,
                remedy: first_remedy,
            });
        }
    }

    records.extend(reverse_map_unstamped(entries, scope, listings));
    records
}

/// The production query seam: the cache-first merge-state read.
pub(crate) fn query_seam(
    pr: i64,
    repo: Option<&str>,
    cwd: Option<&str>,
    include_files: bool,
) -> Result<PrMergeState, PrReadError> {
    query_pr_merge_state(pr, repo, cwd, include_files)
}

fn str_field(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_node(extra: Value) -> Value {
        let mut base = json!({"id": "x-aaaa"});
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn pr_row(number: i64, head: &str, state: &str) -> Value {
        json!({
            "number": number,
            "state": state,
            "title": format!("work {number}"),
            "headRefName": head,
            "url": format!("https://github.com/o/r/pull/{number}"),
            "mergedAt": "2026-10-01T00:00:00Z",
            "body": "",
        })
    }

    #[test]
    fn branch_match_is_delimiter_bounded() {
        assert!(branch_matches_node("feature/x-aaaa", "x-aaaa"));
        assert!(branch_matches_node("target/some-slug-x-aaaa", "x-aaaa"));
        assert!(branch_matches_node("x-aaaa", "x-aaaa"));
        assert!(!branch_matches_node("feature/x-aaaab", "x-aaaa"));
        assert!(!branch_matches_node("", "x-aaaa"));
        assert!(!branch_matches_node("feature/x-aaaa", ""));
    }

    #[test]
    fn listing_answer_scopes_by_repo_when_both_sides_name_one() {
        let rows = vec![
            json!({"number": 7, "url": "https://github.com/o/other/pull/7"}),
            json!({"number": 7, "url": "https://github.com/o/r/pull/7"}),
        ];
        let hit = listing_answer(&rows, 7, Some("o/r")).expect("scoped hit");
        assert_eq!(hit["url"], "https://github.com/o/r/pull/7");
        // A ref-less number answers from the first row carrying it.
        assert!(listing_answer(&rows, 7, None).is_some());
    }

    #[test]
    fn scan_records_a_merged_listing_hit_without_owing_the_query() {
        let entries = vec![open_node(json!({
            "pr_number": 7,
            "pr_url": "https://github.com/o/r/pull/7",
            "cwd": ".",
        }))];
        let cache = ListingCache::new();
        let merged_rows = vec![pr_row(7, "feature/x-aaaa", "MERGED")];
        cache
            .rows_for_with("merged", ".", |_| Ok(merged_rows.clone()))
            .expect("seed merged");
        cache
            .rows_for_with("open", ".", |_| Ok(Vec::new()))
            .expect("seed open");
        let records = scan_merge_drift(&entries, None, Some(&cache), |pr, _repo, _cwd, _files| {
            panic!("query owed nothing: listing answered for #{pr}")
        });
        // The stamped-ref record rides the listing: no per-node query, and
        // like a reverse-mapped record it carries no merge sha.
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].pr_state, "MERGED");
        assert_eq!(records[0].pr_number, 7);
        assert_eq!(records[0].merge_sha, None);
        assert!(records[0].closeable());

        // No listing: the query seam answers and carries the sha.
        let records = scan_merge_drift(&entries, None, None, |pr, _repo, _cwd, _files| {
            Ok(PrMergeState {
                number: pr,
                state: "MERGED".into(),
                url: None,
                merged_at: Some("2026-10-01T00:00:00Z".into()),
                merge_sha: Some("abc".into()),
                changed_files: vec!["a.rs".into()],
                files_truncated: false,
            })
        });
        assert_eq!(records[0].pr_state, "MERGED");
        assert_eq!(records[0].merge_sha.as_deref(), Some("abc"));
        assert!(records[0].closeable());

        // An open listing row for the same PR means no drift and no query.
        let cache = ListingCache::new();
        let open_rows = vec![pr_row(7, "feature/x-aaaa", "OPEN")];
        cache
            .rows_for_with("open", ".", |_| Ok(open_rows.clone()))
            .expect("seed open");
        let records = scan_merge_drift(&entries, None, Some(&cache), |_, _, _, _| {
            panic!("an open listing row means no drift")
        });
        assert!(records.is_empty(), "{records:?}");
    }

    #[test]
    fn scan_skips_done_nodes_and_refless_nodes() {
        let entries = vec![
            open_node(
                json!({"id": "x-done", "completed_at": "2026-10-01T00:00:00Z", "pr_number": 7}),
            ),
            open_node(json!({"id": "x-bare"})),
        ];
        let records = scan_merge_drift(&entries, None, None, |_, _, _, _| {
            panic!("nothing open-with-refs remains")
        });
        assert!(records.is_empty(), "{records:?}");
    }

    #[test]
    fn scan_surfacing_refuses_a_repoless_query() {
        let entries = vec![open_node(json!({"pr_number": 7}))];
        let records = scan_merge_drift(&entries, None, None, |_, _, _, _| {
            Err(PrReadError::new("gh down", "availability"))
        });
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pr_state, "UNKNOWN");
        let error = records[0].error.as_deref().unwrap_or_default();
        assert!(error.contains("no repo context"), "{error}");
        assert_eq!(records[0].error_kind.as_deref(), Some("repository_context"));
        assert!(!records[0].closeable());
    }

    #[test]
    fn open_binding_scan_releases_the_repo_keys_borrow() {
        // Regression for the daemon merge_close exit-101: the grouping step
        // held the repo_keys RefMut across the loop, and rows_for's
        // borrow_mut panicked the moment one ref-less open-bound node gave
        // the loop a group to walk. Seeding first keeps the run hermetic:
        // the loop's rows_for hits the store, never gh.
        let cache = ListingCache::new();
        cache
            .rows_for_with("open", ".", |_| Ok(Vec::new()))
            .expect("seed open");
        let entries = vec![open_node(json!({"cwd": "."}))];
        let (heals, advisories) =
            collect_open_binding_heals(&entries, None, Some(&cache), |_, _, _, _| {
                panic!("no PR read is owed: the listing is empty")
            });
        assert!(heals.is_empty(), "{heals:?}");
        assert!(advisories.is_empty(), "{advisories:?}");
    }

    #[test]
    fn reverse_map_matches_a_merged_branch_and_names_ambiguity() {
        let entries = vec![open_node(json!({"cwd": "."}))];
        let cache = ListingCache::new();
        let merged_rows = vec![
            pr_row(11, "feature/x-aaaa", "MERGED"),
            pr_row(12, "chore/x-aaaa", "MERGED"),
        ];
        cache
            .rows_for_with("merged", ".", |_| Ok(merged_rows.clone()))
            .expect("seed merged");
        let records = reverse_map_unstamped(&entries, None, Some(&cache));
        assert_eq!(records.len(), 1);
        assert!(records[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("ambiguous"));
        assert_eq!(records[0].pr_state, "UNKNOWN");

        let single = vec![pr_row(11, "feature/x-aaaa", "MERGED")];
        let cache2 = ListingCache::new();
        cache2
            .rows_for_with("merged", ".", |_| Ok(single.clone()))
            .expect("seed merged");
        let records = reverse_map_unstamped(&entries, None, Some(&cache2));
        assert_eq!(records.len(), 1);
        assert!(records[0].closeable());
        assert_eq!(records[0].pr_number, 11);
        assert_eq!(records[0].cwd.as_deref(), Some("."));
    }

    #[test]
    fn revert_detection_needs_a_titled_pr_in_the_same_repo() {
        let entries = vec![open_node(json!({
            "id": "x-aaaa",
            "pr_number": 5,
            "pr_url": "https://github.com/o/r/pull/5",
        }))];
        let revert = json!({
            "number": 9,
            "title": "Revert \"work 5\"",
            "url": "https://github.com/o/r/pull/9",
            "body": "This reverts commit x.\nReverts o/r#5",
        });
        let hits = detect_reverted_nodes(&[revert], &entries);
        assert_eq!(hits, vec![("x-aaaa".to_string(), 9i64)]);
        // A cross-repo qualifier never matches this repo's node.
        let cross = json!({
            "number": 9,
            "title": "Revert \"work 5\"",
            "url": "https://github.com/o/r/pull/9",
            "body": "Reverts other/repo#5",
        });
        assert!(detect_reverted_nodes(&[cross], &entries).is_empty());
        // An untitled revert is invisible: documented limitation.
        let untitled = json!({
            "number": 9,
            "title": "roll back 5",
            "url": "https://github.com/o/r/pull/9",
            "body": "Reverts o/r#5",
        });
        assert!(detect_reverted_nodes(&[untitled], &entries).is_empty());
    }

    #[test]
    fn effective_cwd_keeps_a_live_dir_and_falls_back_to_the_project_root() {
        assert_eq!(effective_reconcile_cwd("", None), "");
        // A dir that exists but is not a checkout falls back; the cwd that
        // IS this repo stays.
        let here = std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .into_owned();
        assert_eq!(effective_reconcile_cwd(&here, None), here);
    }
}

/// One open-PR binding heal: a missing verdict whose node is open with no
/// PR refs - the exact repair a human did by hand with `update --pr-number`.
#[derive(Debug, Clone)]
pub(crate) struct OpenBindingHeal {
    pub node_id: String,
    pub pr_number: i64,
    pub pr_url: Option<String>,
    /// true when the node already carries refs and the primary PR closed:
    /// the heal REBINDS instead of filling.
    pub rebind: bool,
}

/// Discover open PRs that uniquely name an open, ref-less node. One gh
/// listing per repo (same-repo worktrees share the call), under the same
/// wall-clock budget as the merged reverse map. Returns (heals,
/// advisories): advisories name ambiguity and gh read failures without
/// mutating anything. Persisting the fills is the CALLER's job.
pub(crate) fn collect_open_binding_heals(
    entries: &[Value],
    scope: Option<&BTreeSet<String>>,
    listings: Option<&ListingCache>,
    query: impl Fn(i64, Option<&str>, Option<&str>, bool) -> Result<PrMergeState, PrReadError>,
) -> (Vec<OpenBindingHeal>, Vec<String>) {
    // The repo_keys RefMut must drop before the loop: rows_for re-borrows
    // it, and a borrow held across the loop panicked (exit 101) on every
    // daemon sweep once the graph held a ref-less open-bound node.
    let (groups, cwd_by_nid, _skipped) = {
        let mut fallback = HashMap::new();
        let mut cache_memo;
        let memo: &mut HashMap<String, String> = match listings {
            Some(cache) => {
                cache_memo = cache.repo_keys.borrow_mut();
                &mut cache_memo
            }
            None => &mut fallback,
        };
        group_refless_by_repo(entries, scope, memo, true)
    };

    let mut heals: Vec<OpenBindingHeal> = Vec::new();
    let mut advisories: Vec<String> = Vec::new();
    let deadline = Instant::now() + REVERSE_MAP_BUDGET;
    for (_key, nodes) in &groups {
        if Instant::now() >= deadline {
            let deferred: Vec<String> = groups
                .iter()
                .flat_map(|(_, ns)| ns.iter())
                .filter_map(|i| entries[*i].get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            advisories.push(format!(
                "open-binding scan stopped at its {}s budget (gh is slow or degraded); deferred {} node(s) to a later sweep: {}",
                REVERSE_MAP_BUDGET.as_secs(),
                deferred.len(),
                deferred.join(" ")
            ));
            break;
        }
        let Some(first) = nodes.first() else { continue };
        let nid0 = entries[*first]
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let gh_cwd = cwd_by_nid
            .get(&nid0)
            .map(String::as_str)
            .unwrap_or_default();
        let rows = match listings {
            Some(cache) => cache.rows_for("open", gh_cwd),
            None => list_open_pr_branches(gh_cwd, 100),
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                advisories.push(format!(
                    "open-binding gh query failed ({gh_cwd}): {}",
                    e.message
                ));
                continue;
            }
        };
        for verdict in crate::org_board::prs::pr_binding_verdicts(&rows, entries) {
            match verdict.verdict {
                "ambiguous" => {
                    if let Some(detail) = &verdict.detail {
                        advisories.push(format!(
                            "open PR #{} binding ambiguous: {detail}",
                            verdict.number
                        ));
                    }
                }
                "missing" => {
                    let Some(heal_nid) = &verdict.node_id else {
                        continue;
                    };
                    let Some(node) = nodes.iter().find(|i| {
                        entries[**i].get("id").and_then(Value::as_str) == Some(heal_nid.as_str())
                    }) else {
                        continue;
                    };
                    let node = &entries[*node];
                    let refs = node_pr_refs_of(node);
                    if !refs.is_empty() {
                        let closed = primary_closed(node, gh_cwd, &query, &mut advisories);
                        if !closed {
                            continue;
                        }
                    }
                    heals.push(OpenBindingHeal {
                        node_id: heal_nid.clone(),
                        pr_number: verdict.number,
                        pr_url: verdict.pr_url().map(str::to_string),
                        rebind: !refs.is_empty(),
                    });
                }
                _ => {}
            }
        }
    }
    (heals, advisories)
}

/// The rebind gate: the node's primary PR must have CLOSED on GitHub
/// before an open PR may rebind it. A read failure is an advisory, never
/// a bind.
fn primary_closed(
    node: &Value,
    cwd: &str,
    query: &impl Fn(i64, Option<&str>, Option<&str>, bool) -> Result<PrMergeState, PrReadError>,
    advisories: &mut Vec<String>,
) -> bool {
    let Some(number) = node.get("pr_number").and_then(Value::as_i64) else {
        return false;
    };
    match query(number, None, Some(cwd), false) {
        Ok(state) => state.state == "CLOSED",
        Err(e) => {
            let nid = node.get("id").and_then(Value::as_str).unwrap_or("?");
            advisories.push(format!(
                "open-binding rebind: {nid} PR #{number} unreadable ({})",
                e.message
            ));
            false
        }
    }
}
