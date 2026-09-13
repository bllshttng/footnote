//! `canonical-check` (x-a150): the divergence read for the post-merge
//! canonical sync. One JSON payload on stdin (`canonical` required; optional
//! `files`, `pr`, `sha`, `fetch`), one JSON answer on stdout naming the
//! dirty-overlap refusal and the ahead-of-origin refusal - the two shapes a
//! raw `git pull` failure used to hide behind "divergent branches". The
//! Python sync keeps its orchestration and calls this verb through
//! `rust_binary.verb_call`; a refusal is reported, never auto-recovered.

use serde_json::{json, Value};
use std::path::Path;

use crate::daemon::output_with_timeout;

const FETCH_TIMEOUT_SECS: u64 = 30;
const PROBE_TIMEOUT_SECS: u64 = 10;
const SHOW_CAP: usize = 5;

fn git_out(dir: &Path, args: &[&str], secs: u64) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = output_with_timeout(cmd, secs)?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// The remote's default branch: `origin/HEAD` minus the remote prefix, `main`
/// when git cannot answer (port of the Python rule
/// `test_behind_count_ignores_empty_symbolic_ref` pins: an empty read is not
/// an answer).
pub(crate) fn default_branch(cwd: &Path) -> String {
    git_out(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        PROBE_TIMEOUT_SECS,
    )
    .filter(|s| !s.is_empty())
    .map(|s| match s.split_once('/') {
        Some((_, local)) => local.to_string(),
        None => s,
    })
    .unwrap_or_else(|| "main".to_string())
}

/// True when `cwd` IS the canonical checkout. An unprovable checkout (bare
/// repo, separate git dir) takes the safe side and reads as canonical.
pub(crate) fn is_canonical_checkout(cwd: &Path) -> bool {
    match crate::paths::canonical_repo_root(cwd) {
        None => true,
        Some(canonical) => canonical == crate::paths::worktree_repo_root(cwd),
    }
}

pub(crate) struct Probe {
    pub default: String,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    /// `"%h %s"` lines, newest first, capped at SHOW_CAP.
    pub ahead_commits: Vec<String>,
    /// Local tip of the default branch; names the rescue branch.
    pub tip_sha: Option<String>,
    /// `None` on a detached HEAD.
    pub head_branch: Option<String>,
    pub dirty: Vec<String>,
}

/// A stale tracking ref can under-report `behind`, but it cannot invent an
/// `ahead` commit: only a local commit puts one there, so the ahead gate needs
/// no fetch. `fetch` only refreshes the behind answer for human-facing
/// callers, and a failed fetch just means a staler answer.
pub(crate) fn probe(canonical: &Path, fetch: bool) -> Probe {
    if fetch {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(canonical)
            .args(["fetch", "--quiet", "origin"]);
        let _ = output_with_timeout(cmd, FETCH_TIMEOUT_SECS);
    }
    let default = default_branch(canonical);
    let (ahead, behind) = git_out(
        canonical,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{default}...origin/{default}"),
        ],
        PROBE_TIMEOUT_SECS,
    )
    .and_then(|s| {
        let mut it = s.split_whitespace();
        let ahead = it.next()?.parse::<u64>().ok()?;
        let behind = it.next()?.parse::<u64>().ok()?;
        Some((Some(ahead), Some(behind)))
    })
    .unwrap_or((None, None));
    let ahead_commits: Vec<String> = git_out(
        canonical,
        &[
            "--no-pager",
            "log",
            "--format=%h %s",
            &format!("origin/{default}..{default}"),
        ],
        PROBE_TIMEOUT_SECS,
    )
    .map(|s| s.lines().take(SHOW_CAP).map(str::to_string).collect())
    .unwrap_or_default();
    let tip_sha = git_out(canonical, &["rev-parse", &default], PROBE_TIMEOUT_SECS);
    let head_branch = git_out(
        canonical,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        PROBE_TIMEOUT_SECS,
    );
    let dirty = git_out(canonical, &["status", "--porcelain"], PROBE_TIMEOUT_SECS)
        .map(|s| {
            s.lines()
                .filter(|l| l.len() >= 4)
                .map(|l| match l[3..].split_once(" -> ") {
                    Some((_, new)) => c_unquote(new),
                    None => c_unquote(&l[3..]),
                })
                .collect()
        })
        .unwrap_or_default();
    Probe {
        default,
        ahead,
        behind,
        ahead_commits,
        tip_sha,
        head_branch,
        dirty,
    }
}

/// De-quote a `git status --porcelain` path. Git C-quotes paths holding
/// spaces, quotes or non-ASCII bytes; the merge's file list from GitHub is
/// raw, so the quoted form never string-matched and silently read as
/// non-blocking (Python's `_dirty_paths` documented that as the fail-safe
/// direction; here the honest match is cheap, so take it).
fn c_unquote(raw: &str) -> String {
    if !(raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"')) {
        return raw.to_string();
    }
    let bytes = raw.as_bytes();
    let inner = &bytes[1..bytes.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        if inner[i] != b'\\' {
            out.push(inner[i]);
            i += 1;
            continue;
        }
        match inner.get(i + 1) {
            Some(b'"') | Some(b'\\') => {
                out.push(inner[i + 1]);
                i += 2;
            }
            Some(b'n') => {
                out.push(b'\n');
                i += 2;
            }
            Some(b't') => {
                out.push(b'\t');
                i += 2;
            }
            Some(c) if (b'0'..=b'7').contains(c) => {
                let mut val: u32 = 0;
                let mut n = 0;
                while n < 3 {
                    match inner.get(i + 1 + n).and_then(|d| (*d as char).to_digit(8)) {
                        Some(d) => {
                            val = val * 8 + d;
                            n += 1;
                        }
                        None => break,
                    }
                }
                out.push(val as u8);
                i += 1 + n;
            }
            _ => {
                out.push(inner[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The recovery line saves the local commits BEFORE it moves the branch.
/// `reset --keep` keeps uncommitted work and refuses when that work would be
/// overwritten; on any other head the branch is moved without a checkout.
fn ahead_recovery(canonical: &Path, p: &Probe) -> String {
    let canonical = canonical.display();
    let tip12 = p
        .tip_sha
        .as_deref()
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_else(|| "unknown".to_string());
    let default = &p.default;
    let second = if p.head_branch.as_deref() == Some(default.as_str()) {
        format!("git -C {canonical} reset --keep origin/{default}")
    } else {
        format!("git -C {canonical} branch -f {default} origin/{default}")
    };
    format!("git -C {canonical} branch rescue/canonical-{tip12} {default} && {second}")
}

fn build_refusal(
    canonical: &Path,
    files: &[String],
    pr: Option<u64>,
    sha: Option<&str>,
    p: &Probe,
) -> Option<String> {
    let mut blocking: Vec<String> = files
        .iter()
        .filter(|f| p.dirty.contains(f))
        .cloned()
        .collect();
    blocking.sort();
    blocking.dedup();
    let canonical_display = canonical.display();
    let mut blocks: Vec<String> = Vec::new();

    if !blocking.is_empty() {
        let shown = match blocking.len() {
            n if n > SHOW_CAP => {
                format!(
                    "{}, (+{} more)",
                    blocking[..SHOW_CAP].join(", "),
                    n - SHOW_CAP
                )
            }
            _ => blocking.join(", "),
        };
        let date = chrono::Utc::now().format("%Y-%m-%d");
        let pr = pr.map(|n| n.to_string()).unwrap_or_else(|| "0".into());
        let sha12: String = sha
            .map(|s| s.chars().take(12).collect())
            .unwrap_or_else(|| "unknown".into());
        let paths = blocking
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(" ");
        blocks.push(format!(
            "post-merge sync: canonical checkout is dirty - the pull would refuse:\n\
             \x20 checkout: {canonical_display}\n\
             \x20 blocking (uncommitted + touched by this merge): {shown}\n\
             \x20 recovery: git -C {canonical_display} stash push -u -m \"fno post-merge sync {date} PR #{pr} {sha12}\" -- {paths}\n\
             \x20 marker withheld, will retry once the blocking paths are committed or stashed"
        ));
    }

    if p.ahead.unwrap_or(0) > 0 {
        let ahead = p.ahead.unwrap_or(0);
        let mut shown = p.ahead_commits.join("; ");
        if ahead > SHOW_CAP as u64 {
            shown.push_str(&format!(" (+{} more)", ahead - SHOW_CAP as u64));
        }
        blocks.push(format!(
            "post-merge sync: canonical {} is {ahead} ahead of origin/{} - the pull cannot fast-forward:\n\
             \x20 checkout: {canonical_display}\n\
             \x20 local commits: {shown}\n\
             \x20 recovery: {}\n\
             \x20 marker withheld, will retry once the local commits are saved to a branch and {} matches origin",
            p.default,
            p.default,
            ahead_recovery(canonical, p),
            p.default,
        ));
    }

    if blocks.is_empty() {
        return None;
    }
    Some(blocks.join("\n"))
}

fn build_answer(payload: &Value) -> Value {
    let canonical = Path::new(
        payload
            .get("canonical")
            .and_then(|v| v.as_str())
            .unwrap_or("."),
    );
    let files: Vec<String> = payload
        .get("files")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let pr = payload.get("pr").and_then(|v| v.as_u64());
    let sha = payload.get("sha").and_then(|v| v.as_str());
    let fetch = payload
        .get("fetch")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let p = probe(canonical, fetch);
    let refusal = build_refusal(canonical, &files, pr, sha, &p);

    let mut notes: Vec<String> = Vec::new();
    if let Some(behind) = p.behind.filter(|b| *b > 0) {
        notes.push(format!("local default branch {behind} behind origin"));
    }
    if let Some(ahead) = p.ahead.filter(|a| *a > 0) {
        let first = p.ahead_commits.first().cloned().unwrap_or_default();
        notes.push(format!(
            "local default branch {ahead} ahead of origin: {first}"
        ));
    }
    if !p.dirty.is_empty() {
        let shown = match p.dirty.len() {
            n if n > SHOW_CAP => {
                format!(
                    "{}, (+{} more)",
                    p.dirty[..SHOW_CAP].join(", "),
                    n - SHOW_CAP
                )
            }
            _ => p.dirty.join(", "),
        };
        notes.push(format!("canonical dirty: {shown}"));
    }

    let mut blocking: Vec<String> = files
        .iter()
        .filter(|f| p.dirty.contains(f))
        .cloned()
        .collect();
    blocking.sort();
    blocking.dedup();

    json!({
        "default_branch": p.default,
        "head_branch": p.head_branch,
        "ahead": p.ahead,
        "behind": p.behind,
        "ahead_commits": p.ahead_commits,
        "dirty": p.dirty,
        "blocking": blocking,
        "refusal": refusal,
        "notes": notes,
    })
}

/// The verb entry: one JSON payload on stdin, one JSON answer on stdout,
/// exit 0; malformed stdin prints `canonical-check: bad payload: <e>` and
/// exits 2 (same contract as `publish-review`).
pub fn run_canonical_check(_args: &[String]) -> i32 {
    use std::io::Read;

    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        eprint!("canonical-check: cannot read payload\n");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("canonical-check: bad payload: {e}\n");
            return 2;
        }
    };
    println!("{}", build_answer(&parsed));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed in {}", dir.display());
    }

    fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) {
        std::fs::write(dir.join(name), body).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-q", "-m", msg]);
    }

    /// bare origin + `canonical` clone pinned to `main`. Returns the temp dir
    /// (must outlive the paths) and the canonical path.
    struct Repo {
        _tmp: tempfile::TempDir,
        canonical: PathBuf,
    }

    fn repo_with_origin() -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        git(tmp.path(), &["init", "--bare", "-q", "origin.git"]);
        let st = Command::new("git")
            .arg("--git-dir")
            .arg(&origin)
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .status()
            .unwrap();
        assert!(st.success());
        let seed = tmp.path().join("seed");
        git(
            tmp.path(),
            &["clone", "-q", &origin.to_string_lossy(), "seed"],
        );
        git(&seed, &["config", "user.email", "t@t"]);
        git(&seed, &["config", "user.name", "t"]);
        commit_file(&seed, "base.md", "base", "base");
        git(&seed, &["push", "-q", "origin", "main"]);
        let canonical = tmp.path().join("canonical");
        git(
            tmp.path(),
            &["clone", "-q", &origin.to_string_lossy(), "canonical"],
        );
        git(&canonical, &["config", "user.email", "t@t"]);
        git(&canonical, &["config", "user.name", "t"]);
        Repo {
            _tmp: tmp,
            canonical,
        }
    }

    fn payload_files(files: &[&str]) -> Vec<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn default_branch_falls_back_to_main_without_a_remote() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        assert_eq!(default_branch(tmp.path()), "main");
    }

    #[test]
    fn a_plain_repo_is_its_own_canonical_and_a_worktree_is_not() {
        let r = repo_with_origin();
        assert!(is_canonical_checkout(&r.canonical));
        let wt = r.canonical.parent().unwrap().join("wt");
        let wt_str = wt.to_string_lossy().into_owned();
        git(
            &r.canonical,
            &["worktree", "add", "-q", "-b", "feature/x", &wt_str],
        );
        assert!(!is_canonical_checkout(&wt));
    }

    #[test]
    fn ahead_counts_a_local_commit_a_fetch_cannot_explain() {
        let r = repo_with_origin();
        commit_file(&r.canonical, "local.md", "x", "local work");
        let p = probe(&r.canonical, false);
        assert_eq!(p.ahead, Some(1));
        assert_eq!(p.behind, Some(0));
        assert_eq!(p.ahead_commits.len(), 1);
        assert!(p.ahead_commits[0].contains("local work"));
    }

    #[test]
    fn overlap_refuses_with_the_attributed_stash_line() {
        let r = repo_with_origin();
        commit_file(&r.canonical, "AGENTS.md", "old", "agents");
        std::fs::write(r.canonical.join("AGENTS.md"), "edited").unwrap();
        std::fs::write(r.canonical.join(".candidate.50371"), "").unwrap();
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(
            &r.canonical,
            &payload_files(&["AGENTS.md"]),
            Some(1924),
            Some(&"a".repeat(40)),
            &p,
        );
        let refusal = refusal.unwrap();
        assert!(refusal.contains("canonical checkout is dirty"));
        assert!(refusal.contains("-- 'AGENTS.md'"));
        assert!(!refusal.contains(".candidate.50371"));
        assert!(refusal.contains("stash push -u -m \"fno post-merge sync"));
        assert!(refusal.contains("PR #1924"));
    }

    #[test]
    fn rename_new_side_blocks() {
        let r = repo_with_origin();
        commit_file(&r.canonical, "docs-old.md", "x", "old");
        git(&r.canonical, &["mv", "docs-old.md", "AGENTS.md"]);
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(
            &r.canonical,
            &payload_files(&["AGENTS.md"]),
            Some(7),
            Some(&"a".repeat(40)),
            &p,
        );
        assert!(refusal.unwrap().contains("-- 'AGENTS.md'"));
    }

    #[test]
    fn dirt_the_merge_does_not_touch_passes() {
        let r = repo_with_origin();
        std::fs::write(r.canonical.join("OTHER.md"), "wip").unwrap();
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(
            &r.canonical,
            &payload_files(&["AGENTS.md"]),
            Some(7),
            Some(&"a".repeat(40)),
            &p,
        );
        assert_eq!(refusal, None);
    }

    #[test]
    fn display_caps_at_five_but_the_pathspec_keeps_every_path() {
        let r = repo_with_origin();
        for i in 0..7 {
            std::fs::write(r.canonical.join(format!("merge{i}.py")), "x").unwrap();
        }
        let files: Vec<String> = (0..7).map(|i| format!("merge{i}.py")).collect();
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(&r.canonical, &files, Some(7), Some(&"a".repeat(40)), &p);
        let refusal = refusal.unwrap();
        assert!(refusal.contains("merge4.py, (+2 more)"));
        assert!(refusal.contains(
            "-- 'merge0.py' 'merge1.py' 'merge2.py' 'merge3.py' 'merge4.py' 'merge5.py' 'merge6.py'"
        ));
    }

    #[test]
    fn a_path_with_a_space_stays_paste_ready() {
        let r = repo_with_origin();
        std::fs::create_dir_all(r.canonical.join("docs")).unwrap();
        commit_file(&r.canonical, "docs/my notes.md", "x", "notes");
        std::fs::write(r.canonical.join("docs/my notes.md"), "edited").unwrap();
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(
            &r.canonical,
            &payload_files(&["docs/my notes.md"]),
            Some(7),
            Some(&"a".repeat(40)),
            &p,
        );
        assert!(refusal.unwrap().contains("'docs/my notes.md'"));
    }

    #[test]
    fn ahead_refusal_names_the_commit_and_a_reset_keep_recovery() {
        let r = repo_with_origin();
        commit_file(&r.canonical, "local.md", "x", "local work");
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(&r.canonical, &[], Some(1924), Some(&"a".repeat(40)), &p);
        let refusal = refusal.unwrap();
        assert!(refusal.contains("is 1 ahead of origin/main - the pull cannot fast-forward"));
        assert!(refusal.contains("local commits: "));
        assert!(refusal.contains("local work"));
        assert!(refusal.contains("branch rescue/canonical-"));
        assert!(refusal.contains("reset --keep origin/main"));
    }

    #[test]
    fn both_refusals_carry_the_dirty_block_first() {
        let r = repo_with_origin();
        commit_file(&r.canonical, "AGENTS.md", "old", "agents");
        std::fs::write(r.canonical.join("AGENTS.md"), "edited").unwrap();
        commit_file(&r.canonical, "local.md", "x", "local work");
        let p = probe(&r.canonical, false);
        let refusal = build_refusal(
            &r.canonical,
            &payload_files(&["AGENTS.md"]),
            Some(7),
            Some(&"a".repeat(40)),
            &p,
        );
        let refusal = refusal.unwrap();
        let dirty_at = refusal.find("canonical checkout is dirty").unwrap();
        let ahead_at = refusal.find("ahead of origin/main").unwrap();
        assert!(dirty_at < ahead_at);
    }

    #[test]
    fn a_detached_head_recovery_uses_branch_force_not_a_checkout() {
        let r = repo_with_origin();
        let base = git_out(&r.canonical, &["rev-parse", "HEAD"], 10).unwrap();
        git(&r.canonical, &["checkout", "-q", "--detach", &base]);
        let p = probe(&r.canonical, false);
        assert_eq!(p.head_branch, None);
        let recovery = ahead_recovery(&r.canonical, &p);
        assert!(recovery.contains("branch -f main origin/main"));
        assert!(!recovery.contains("reset --keep"));
    }

    #[test]
    fn the_answer_carries_the_contract_keys() {
        let r = repo_with_origin();
        let answer = build_answer(&json!({ "canonical": r.canonical.to_string_lossy() }));
        for key in [
            "default_branch",
            "head_branch",
            "ahead",
            "behind",
            "ahead_commits",
            "dirty",
            "blocking",
            "refusal",
            "notes",
        ] {
            assert!(answer.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(answer["refusal"], Value::Null);
        assert_eq!(answer["default_branch"], "main");
    }
}
