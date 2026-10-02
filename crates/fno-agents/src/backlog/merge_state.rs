//! The merge-state read behind `backlog reconcile` and the close path:
//! one PR's open/closed/merged state with merged-file evidence, served
//! first from the permanent gh-cache row (merged facts never change).
//! Ported from graph/_reconcile.py (`PrMergeState`, `query_pr_merge_state`)
//! on the merge_evidence reader plumbing and the gh_cache row store.

use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

use super::merge_evidence::PrReadError;
use super::pr_link;
use crate::gh_cache;
use crate::pr_push::run_labeled;

/// The Python reader's timeout (`GH_QUERY_TIMEOUT_S`): covers network hangs
/// and stuck auth prompts.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const FILES_PAGE_SIZE: usize = 100;
/// GitHub exposes at most 3,000 files on this endpoint and a full final
/// page has no positive tail marker, so fail closed rather than call an
/// incomplete list complete.
const FILES_MAX_PAGES: usize = 30;

/// One PR's GitHub-confirmed state (the PrMergeState twin).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PrMergeState {
    pub number: i64,
    pub state: String,
    pub url: Option<String>,
    pub merged_at: Option<String>,
    pub merge_sha: Option<String>,
    pub changed_files: Vec<String>,
    pub files_truncated: bool,
}

/// The production read: cache first, REST on a miss, write-back on a fresh
/// MERGED read. Tests go through [`fresh_read`] and [`paginate_files`] with
/// injected readers, so no test traffic reaches gh.
pub(crate) fn query_pr_merge_state(
    pr: i64,
    repo: Option<&str>,
    cwd: Option<&str>,
    include_files: bool,
) -> Result<PrMergeState, PrReadError> {
    let cwd_path = Path::new(cwd.unwrap_or("."));
    if let Some(row) = cached_row(pr, repo, cwd_path, include_files) {
        return Ok(row);
    }
    let fresh = fresh_read(pr, repo, cwd_path, include_files, read_info, read_files)?;
    if fresh.state == "MERGED" {
        write_cache_row(pr, repo, cwd_path, &fresh, include_files);
    }
    Ok(fresh)
}

/// The gh-cache hit: `{"info": {...}, "files": [...]?}` with a string
/// state, and the files list present exactly when this read asked for it.
fn cached_row(
    pr: i64,
    repo: Option<&str>,
    cwd: &Path,
    include_files: bool,
) -> Option<PrMergeState> {
    let slug = repo?;
    let root = gh_cache::rows_root(cwd)?;
    let now = gh_cache::now_secs();
    let row = gh_cache::read_row(&root, "merged", slug, Some(pr as u64), None, now);
    let row = row.get("row")?.as_object()?;
    let info = row.get("info")?.as_object()?;
    let state = info.get("state")?.as_str()?.to_string();
    if include_files && !row.get("files").map(Value::is_array).unwrap_or(false) {
        return None;
    }
    Some(PrMergeState {
        number: info.get("pr").and_then(Value::as_i64).unwrap_or(pr),
        state,
        url: str_field(info.get("url")),
        merged_at: str_field(info.get("merged_at")),
        merge_sha: str_field(info.get("merge_sha")),
        changed_files: row
            .get("files")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        files_truncated: false,
    })
}

fn write_cache_row(
    pr: i64,
    repo: Option<&str>,
    cwd: &Path,
    state: &PrMergeState,
    include_files: bool,
) {
    let Some(slug) = repo else { return };
    let Some(root) = gh_cache::rows_root(cwd) else {
        return;
    };
    let mut row = json!({
        "info": {
            "pr": state.number,
            "state": state.state,
            "url": state.url,
            "merged_at": state.merged_at,
            "merge_sha": state.merge_sha,
        }
    });
    if include_files {
        row["files"] = json!(state.changed_files);
    }
    gh_cache::write_row(&root, "merged", slug, Some(pr as u64), &row);
}

/// The REST read with no cache leg. The two reader seams keep tests
/// hermetic (the Python `info_reader`/`files_reader` contract); the
/// production seams are [`read_info`] and [`read_files`].
pub(crate) fn fresh_read(
    pr: i64,
    repo: Option<&str>,
    cwd: &Path,
    include_files: bool,
    info_reader: impl Fn(i64, Option<&str>, &Path) -> Result<Value, PrReadError>,
    files_reader: impl Fn(i64, Option<&str>, &Path) -> Result<Vec<String>, PrReadError>,
) -> Result<PrMergeState, PrReadError> {
    let info = info_reader(pr, repo, cwd)?;
    let map = info.as_object().ok_or_else(|| {
        PrReadError::new(
            "REST PR info reader returned a value that is not an object",
            "malformed",
        )
    })?;
    if !map.contains_key("pr") || !map.contains_key("state") {
        return Err(PrReadError::new(
            "REST PR info reader omitted required PR number or state",
            "malformed",
        ));
    }
    let state = match map.get("state") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(PrReadError::new(
                format!("REST PR info reader returned malformed state {other}"),
                "malformed",
            ))
        }
        None => unreachable!("state key checked above"),
    };
    if !matches!(state.as_str(), "OPEN" | "CLOSED" | "MERGED") {
        return Err(PrReadError::new(
            format!("REST PR info reader returned malformed state {state:?}"),
            "malformed",
        ));
    }
    let changed_files = if state == "MERGED" && include_files {
        files_reader(pr, repo, cwd)?
    } else {
        Vec::new()
    };
    let number = match map.get("pr") {
        None => pr,
        Some(v) => v.as_i64().ok_or_else(|| {
            PrReadError::new(
                "REST PR info reader returned a malformed PR number",
                "malformed",
            )
        })?,
    };
    Ok(PrMergeState {
        number,
        state,
        url: str_field(map.get("url")),
        merged_at: str_field(map.get("merged_at")),
        merge_sha: str_field(map.get("merge_sha")),
        changed_files,
        files_truncated: false,
    })
}

/// The production info seam: resolve the slug, one `gh api` pull read,
/// shape it into the info object the cache row and PrMergeState share.
fn read_info(pr: i64, repo: Option<&str>, cwd: &Path) -> Result<Value, PrReadError> {
    let slug = match repo {
        Some(r) => r.to_string(),
        None => pr_link::resolve_current_repo_slug(cwd.to_str()).ok_or_else(|| {
            PrReadError::new("could not resolve owner/repo from the checkout", "")
        })?,
    };
    let path = format!("repos/{slug}/pulls/{pr}");
    let data = gh_api_json(&path, cwd)?;
    let state = if data.get("merged").and_then(Value::as_bool).unwrap_or(false) {
        "MERGED".to_string()
    } else {
        let raw = data.get("state").and_then(Value::as_str).ok_or_else(|| {
            PrReadError::new(
                "REST PR info reader omitted required PR number or state",
                "malformed",
            )
        })?;
        match raw.to_ascii_uppercase().as_str() {
            "OPEN" => "OPEN".to_string(),
            "CLOSED" => "CLOSED".to_string(),
            // REST never says MERGED as a state string; a merged PR is
            // state "closed" with merged true (the flag wins above).
            other => {
                return Err(PrReadError::new(
                    format!("REST PR info reader returned malformed state {other:?}"),
                    "malformed",
                ))
            }
        }
    };
    Ok(json!({
        "pr": pr,
        "state": state,
        "url": data.get("html_url"),
        "merged_at": data.get("merged_at"),
        "merge_sha": data.get("merge_commit_sha"),
    }))
}

/// The production files seam: every changed-file path from the paginated
/// REST files endpoint.
fn read_files(pr: i64, repo: Option<&str>, cwd: &Path) -> Result<Vec<String>, PrReadError> {
    let slug = match repo {
        Some(r) => r.to_string(),
        None => pr_link::resolve_current_repo_slug(cwd.to_str()).ok_or_else(|| {
            PrReadError::new("could not resolve owner/repo from the checkout", "")
        })?,
    };
    paginate_files(|page| {
        let path = format!("repos/{slug}/pulls/{pr}/files?per_page={FILES_PAGE_SIZE}&page={page}");
        gh_api_json(&path, cwd)
    })
}

/// One `gh api` read returning the parsed JSON body.
fn gh_api_json(path: &str, cwd: &Path) -> Result<Value, PrReadError> {
    let (ok, out, err) = run_labeled(
        "backlog-evidence",
        "gh",
        &["api", "--allow-escape-sequences", path],
        cwd,
        QUERY_TIMEOUT,
    )
    .map_err(|e| PrReadError::new(e, "availability"))?;
    if !ok {
        let message = if err.trim().is_empty() { out } else { err };
        return Err(PrReadError::new(message, ""));
    }
    serde_json::from_str(out.trim()).map_err(|e| {
        PrReadError::new(
            format!("gh api returned output that is not JSON: {e}"),
            "malformed",
        )
    })
}

/// The pagination loop over the files endpoint: stop on the first short
/// page, fail closed at the 3,000-file cap, refuse a malformed row.
fn paginate_files(
    mut page_fetch: impl FnMut(usize) -> Result<Value, PrReadError>,
) -> Result<Vec<String>, PrReadError> {
    let mut paths: Vec<String> = Vec::new();
    for page in 1..=FILES_MAX_PAGES {
        let payload = page_fetch(page)?;
        let rows = payload.as_array().ok_or_else(|| {
            PrReadError::new(
                format!("gh api pull files page {page} returned a value that is not an array"),
                "malformed",
            )
        })?;
        for (index, row) in rows.iter().enumerate() {
            let name = row
                .get("filename")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    PrReadError::new(
                        format!("gh api pull files page {page} carried malformed row {index}"),
                        "malformed",
                    )
                })?;
            paths.push(name.to_string());
        }
        if rows.len() < FILES_PAGE_SIZE {
            return Ok(paths);
        }
    }
    Err(PrReadError::new(
        "gh api pull files reached GitHub's 3,000-file cap without a short page",
        "",
    ))
}

pub(crate) fn str_field(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}


/// One PR's closure context: body + state from a single `gh pr view`
/// (the query-once contract: the trailer the gate reads and the state it
/// is read against must come from one fetch).
#[derive(Debug, Clone)]
pub(crate) struct ClosureContext {
    pub number: i64,
    pub body: String,
    pub url: Option<String>,
    pub state: String,
    pub merged_at: Option<String>,
    pub changed_files: Vec<String>,
}

/// `gh pr view` once for body + merge state + files. A typed error on any
/// gh failure; blank exit-0 output refuses (never read a bare exit 0 as
/// permission).
pub(crate) fn fetch_pr_closure_context(
    pr_number: i64,
    repo: Option<&str>,
) -> Result<ClosureContext, PrReadError> {
    use std::process::Command;
    let Some(gh) = gh_executable_for_merge_state() else {
        return Err(PrReadError::new("gh CLI not found on PATH", "availability"));
    };
    let mut cmd = Command::new(gh);
    cmd.args(["pr", "view", &pr_number.to_string(), "--json"]);
    cmd.arg("number,body,url,state,mergedAt,files");
    if let Some(repo) = repo {
        cmd.args(["--repo", repo]);
    }
    let out = cmd
        .output()
        .map_err(|e| PrReadError::new(format!("gh subprocess failed to launch: {e}"), ""))?;
    if !out.status.success() {
        return Err(PrReadError::new(
            format!(
                "gh pr view #{pr_number} failed (rc={}): {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            "",
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        return Err(PrReadError::new(
            format!("gh pr view #{pr_number} returned no output (exit 0)"),
            "",
        ));
    }
    let row: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| PrReadError::new(format!("gh stdout was not JSON: {e}"), "malformed"))?;
    let changed_files: Vec<String> = row
        .get("files")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|item| match item {
                    Value::String(path) => Some(path.clone()),
                    Value::Object(map) => map.get("path").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                })
                .filter(|path| !path.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(ClosureContext {
        number: row.get("number").and_then(Value::as_i64).unwrap_or(pr_number),
        body: row.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        url: str_field(row.get("url")),
        state: row
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_string(),
        merged_at: str_field(row.get("mergedAt")),
        changed_files,
    })
}

/// gh on PATH for this module's shellouts.
fn gh_executable_for_merge_state() -> Option<std::path::PathBuf> {
    crate::loop_dispatch::which_binary("gh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// FNO_GH_FACTS_DIR is process-global, so cache-leg tests serialize.
    fn cache_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn temp_cache_dir(tag: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("fno-ms-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        std::env::set_var("FNO_GH_FACTS_DIR", &dir);
        Some(dir)
    }

    fn merged_info_row(pr: i64, files: Value) -> Value {
        json!({
            "info": {
                "pr": pr,
                "state": "MERGED",
                "url": format!("https://github.com/o/r/pull/{pr}"),
                "merged_at": "2026-10-01T00:00:00Z",
                "merge_sha": "abc123",
            },
            "files": files,
        })
    }

    #[test]
    fn cache_hit_serves_the_merged_row_without_gh() {
        let _guard = cache_env_lock();
        let dir = temp_cache_dir("hit").expect("temp dir");
        let pr = 4242i64;
        gh_cache::write_row(
            &dir,
            "merged",
            "o/r",
            Some(pr as u64),
            &merged_info_row(pr, json!(["a.rs", "b.rs"])),
        );
        let state = query_pr_merge_state(pr, Some("o/r"), None, true).expect("cache hit");
        assert_eq!(state.state, "MERGED");
        assert_eq!(state.number, pr);
        assert_eq!(state.merge_sha.as_deref(), Some("abc123"));
        assert_eq!(state.changed_files, vec!["a.rs", "b.rs"]);
        assert!(!state.files_truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_hit_without_files_stays_a_miss_when_files_are_asked() {
        let _guard = cache_env_lock();
        let dir = temp_cache_dir("nofiles").expect("temp dir");
        let pr = 4243i64;
        gh_cache::write_row(
            &dir,
            "merged",
            "o/r",
            Some(pr as u64),
            merged_info_row(pr, json!(null)),
        );
        // Without files in the row a files-asking read misses; with
        // include_files false the same row serves.
        let miss = query_pr_merge_state(pr, Some("o/r"), None, true);
        assert!(
            miss.is_err(),
            "expected the fresh read to fail with no gh in tests"
        );
        let hit = query_pr_merge_state(pr, Some("o/r"), None, false).expect("serves");
        assert_eq!(hit.state, "MERGED");
        assert!(hit.changed_files.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_read_refuses_a_state_outside_the_three_values() {
        let err = fresh_read(
            7,
            Some("o/r"),
            Path::new("."),
            false,
            |_| Ok(json!({"pr": 7, "state": "WIP"})),
            |_, _, _| unreachable!("files never read for a refused state"),
        )
        .expect_err("malformed state");
        assert_eq!(err.kind, "malformed");
        assert!(err.message.contains("malformed state"), "{err.message}");
    }

    #[test]
    fn fresh_read_refuses_a_missing_pr_or_state_key() {
        for info in [json!({"state": "OPEN"}), json!({"pr": 7})] {
            let err = fresh_read(
                7,
                Some("o/r"),
                Path::new("."),
                false,
                |_| Ok(info.clone()),
                |_, _, _| unreachable!(),
            )
            .expect_err("omitted key");
            assert_eq!(err.kind, "malformed");
            assert!(err.message.contains("omitted required"), "{err.message}");
        }
    }

    #[test]
    fn fresh_read_reads_files_only_for_a_merged_pr_that_was_asked() {
        let info = |state: &str| json!({"pr": 9, "state": state, "merge_sha": "deadbeef"});
        let open = fresh_read(
            9,
            Some("o/r"),
            Path::new("."),
            true,
            |_| Ok(info("OPEN")),
            |_, _, _| panic!("files must not be read for an open PR"),
        )
        .expect("open read");
        assert!(open.changed_files.is_empty());
        let merged = fresh_read(
            9,
            Some("o/r"),
            Path::new("."),
            true,
            |_| Ok(info("MERGED")),
            |_, _, _| Ok(vec!["x.py".into()]),
        )
        .expect("merged read");
        assert_eq!(merged.changed_files, vec!["x.py"]);
        // Not asked: no files leg even on a merged PR.
        let merged = fresh_read(
            9,
            Some("o/r"),
            Path::new("."),
            false,
            |_| Ok(info("MERGED")),
            |_, _, _| panic!("files must not be read when not asked"),
        )
        .expect("merged read");
        assert!(merged.changed_files.is_empty());
    }

    #[test]
    fn files_pagination_stops_on_the_short_page_and_fails_the_cap() {
        let full_page: Value = (0..FILES_PAGE_SIZE)
            .map(|i| json!({"filename": format!("f{i}.rs")}))
            .collect();
        let short: Value = (0..3)
            .map(|i| json!({"filename": format!("tail{i}.rs")}))
            .collect();
        let pages = [full_page.clone(), short];
        let mut calls = 0usize;
        let paths = paginate_files(|page| {
            calls += 1;
            Ok(pages.get(page - 1).cloned().unwrap_or_default())
        })
        .expect("paginated");
        assert_eq!(calls, 2, "stops on the first short page");
        assert_eq!(paths.len(), FILES_PAGE_SIZE + 3);
        let cap = paginate_files(|_| Ok(full_page.clone())).expect_err("cap fails closed");
        assert_eq!(cap.kind, "evidence_incomplete", "{}", cap.kind);
        assert!(cap.message.contains("3,000-file cap"), "{cap.message}");
    }

    #[test]
    fn a_malformed_files_row_names_the_page_and_row() {
        let err = paginate_files(|_| Ok(json!([{"filename": "ok.rs"}, {"filename": ""}])))
            .expect_err("malformed row");
        assert_eq!(err.kind, "malformed");
        assert!(
            err.message.contains("page 1 carried malformed row 1"),
            "{err.message}"
        );
    }

    #[test]
    fn a_write_back_row_is_readable_by_the_next_cache_read() {
        let _guard = cache_env_lock();
        let dir = temp_cache_dir("roundtrip").expect("temp dir");
        let fresh = PrMergeState {
            number: 80,
            state: "MERGED".into(),
            url: Some("https://github.com/o/r/pull/80".into()),
            merged_at: Some("2026-10-02T00:00:00Z".into()),
            merge_sha: Some("cafe123".into()),
            changed_files: vec!["one.rs".into()],
            files_truncated: false,
        };
        write_cache_row(80, Some("o/r"), &dir, &fresh, true);
        let hit = cached_row(80, Some("o/r"), &dir, true).expect("round trip");
        assert_eq!(hit, fresh);
        std::fs::remove_dir_all(&dir).ok();
    }
}
