//! `fno-agents pr-rebase` -- two-phase rebase with the conflict-delegation
//! protocol, ported from the Python `fno.pr._rebase` (which this replaces;
//! new code lands in crates, never cli/src/fno). The exit-code contract is
//! load-bearing for skill orchestration (a caller dispatches the
//! conflict-resolver agent on exit 42, then calls back `--continue`), so it
//! is preserved verbatim:
//!
//! PHASE A (default): fetch + rebase onto BASE.
//!     0  -> status "clean"          (no conflicts)
//!     1  -> status "failed"         (conflict_resolution=fail or non-conflict error)
//!           status "refused"        (guardrails blocked auto-resolve)
//!           status "fetch_failed"   (git fetch failed)
//!     2  -> status "dirty"          (working tree has uncommitted changes)
//!     3  -> status "refused"        (called from main/master/develop/dev)
//!     42 -> status "needs_resolver" (guardrails passed; caller invokes the agent)
//!
//! PHASE B (`--continue`): assume the agent staged + committed resolutions.
//!     0  -> status "resolved"
//!     42 -> status "needs_resolver" (more conflicts)
//!     1  -> status "failed"
//!
//! Output: one JSON line on stdout; all git output + human messages go to
//! stderr so stdout stays clean for the caller to parse.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Protected branches the verb refuses to rebase (exit 3).
const PROTECTED: [&str; 4] = ["main", "master", "develop", "dev"];

/// Environment for git calls with a NON-INTERACTIVE editor: `git rebase
/// --continue` finalises the current patch by committing the staged
/// resolution, and with the default editor it blocks waiting for the commit
/// message. Forcing both editors to a no-op makes git reuse the prefilled
/// message non-interactively.
fn git_env(cmd: &mut std::process::Command) {
    cmd.env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true");
}

/// Run git; when `echo`, pipe its stdout+stderr to the caller's stderr (the
/// bash `1>&2` idiom) so the JSON line on stdout stays clean. A spawn failure
/// means git itself is missing and reads as [`GIT_MISSING`].
fn run_git(
    git_bin: &str,
    args: &[&str],
    cwd: &Path,
    echo: bool,
) -> Result<(i32, String, String), String> {
    let mut cmd = std::process::Command::new(git_bin);
    cmd.args(args).current_dir(cwd);
    git_env(&mut cmd);
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if echo {
                if !stdout.is_empty() {
                    eprint!("{stdout}");
                }
                if !stderr.is_empty() {
                    eprint!("{stderr}");
                }
            }
            Ok((out.status.code().unwrap_or(1), stdout, stderr))
        }
        Err(e) => Err(format!("git spawn failed: {e}")),
    }
}

/// Currently-conflicting paths: `git diff --name-only --diff-filter=U`.
fn conflict_files(git_bin: &str, cwd: &Path) -> Vec<String> {
    let (_, out, _) = match run_git(
        git_bin,
        &["diff", "--name-only", "--diff-filter=U"],
        cwd,
        false,
    ) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Count `<<<<<<< ` lines in a file (bash `grep -c '^<<<<<<< '`).
fn count_conflict_markers(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|body| body.lines().filter(|l| l.starts_with("<<<<<<< ")).count())
        .unwrap_or(0)
}

/// Port of the bash `check_guardrails`. Returns `None` when every file is
/// safe to auto-resolve, else the refused `(reason, files)`. The LAST refused
/// reason wins, matching the bash, which overwrites `refuse_reason` per file.
pub(crate) fn check_guardrails(
    conflict_files: &[String],
    cwd: &Path,
) -> Option<(String, Vec<String>)> {
    let mut refused: Vec<String> = Vec::new();
    let mut reason = String::new();
    for f in conflict_files {
        // Migration files.
        if f.contains("/migrations/")
            || f.starts_with("migrations/")
            || f == "schema.prisma"
            || f.ends_with("/schema.prisma")
            || f.starts_with("supabase/migrations/")
            || f.contains("/supabase/migrations/")
            || (f.ends_with(".sql") && f.contains("migration"))
        {
            refused.push(f.clone());
            reason = "migration file in conflict".to_string();
            continue;
        }
        // Secret / env files.
        if f == ".env"
            || f.starts_with(".env.")
            || f.contains(".env.")
            || f.contains("/secrets/")
            || f.starts_with("secrets/")
            || f.contains("/config/secrets/")
        {
            refused.push(f.clone());
            reason = "secret or env file in conflict".to_string();
            continue;
        }
        // Lock files and git config files (by basename).
        let base = basename(f);
        if matches!(
            base,
            "package-lock.json"
                | "yarn.lock"
                | "Cargo.lock"
                | "Gemfile.lock"
                | "uv.lock"
                | "poetry.lock"
        ) {
            refused.push(f.clone());
            reason = "lock file in conflict - hand-resolution required".to_string();
            continue;
        }
        if matches!(base, ".gitattributes" | ".gitignore") {
            refused.push(f.clone());
            reason = "git config file in conflict".to_string();
            continue;
        }
        // Mass conflicts: more than 3 conflict markers in a single file.
        let marker_count = count_conflict_markers(&cwd.join(f));
        if marker_count > 3 {
            refused.push(f.clone());
            reason = format!(
                "mass conflicts ({marker_count} hunks) in {f} - design problem, not merge problem"
            );
        }
    }
    if refused.is_empty() {
        None
    } else {
        Some((reason, refused))
    }
}

fn needs_resolver_extra(git_bin: &str, conflict_files: &[String], cwd: &Path) -> Value {
    let diff = run_git(git_bin, &["diff", "--cc"], cwd, false)
        .map(|(_, out, _)| out)
        .unwrap_or_default();
    let preview: Vec<&str> = diff.lines().take(200).collect();
    json!({
        "files": conflict_files,
        "diff_preview": preview.join("\n"),
    })
}

/// Resolve `config.auto_merge.conflict_resolution` ("opus"|"fail"); unreadable
/// settings read as the bash default "opus" (auto-resolve attempted).
fn conflict_resolution(repo: &Path) -> String {
    crate::agents_config::config_lookup(repo, &["auto_merge", "conflict_resolution"])
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "opus".to_string())
}

/// PHASE B (`--continue`): resume from git's native in-progress rebase state.
///
/// Conflicts are checked BEFORE the exit code: in a multi-commit rebase
/// `git rebase --continue` exits non-zero (1) when it pauses on NEW conflicts
/// in a SUBSEQUENT commit, so gating the needs_resolver path on `rc == 0`
/// would misreport "more conflicts remain" as a hard failure and break the
/// caller's resolve-loop.
fn phase_b_continue(base: &str, cwd: &Path, git_bin: &str) -> (i32, Value) {
    eprintln!("Running git rebase --continue...");
    let rc = run_git(git_bin, &["rebase", "--continue"], cwd, true)
        .map(|(c, _, _)| c)
        .unwrap_or(1);
    let conflicts = conflict_files(git_bin, cwd);
    if !conflicts.is_empty() {
        if let Some((reason, files)) = check_guardrails(&conflicts, cwd) {
            eprintln!("Guardrails refused during --continue phase");
            let v = json!({"status": "refused", "base": base, "reason": reason, "files": files});
            let _ = run_git(git_bin, &["rebase", "--abort"], cwd, true);
            return (1, v);
        }
        let extra = needs_resolver_extra(git_bin, &conflicts, cwd);
        let mut v = json!({"status": "needs_resolver", "base": base});
        if let (Some(obj), Some(files), Some(prev)) = (
            v.as_object_mut(),
            extra.get("files"),
            extra.get("diff_preview"),
        ) {
            obj.insert("files".into(), files.clone());
            obj.insert("diff_preview".into(), prev.clone());
        }
        return (42, v);
    }
    if rc == 0 {
        let commits: Vec<String> =
            run_git(git_bin, &["log", "--oneline", "--not", base], cwd, false)
                .map(|(_, out, _)| {
                    out.lines()
                        .filter(|l| !l.trim().is_empty())
                        .take(20)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
        return (
            0,
            json!({"status": "resolved", "base": base, "resolution_commits": commits}),
        );
    }
    (
        1,
        json!({"status": "failed", "base": base, "reason": "git rebase --continue failed"}),
    )
}

/// Initial rebase onto `base`.
pub(crate) fn phase_a(base: &str, cwd: &Path, git_bin: &str) -> (i32, Value) {
    // (1) Refuse on protected branches.
    let curr = run_git(git_bin, &["rev-parse", "--abbrev-ref", "HEAD"], cwd, false)
        .map(|(_, out, _)| out.trim().to_string())
        .unwrap_or_default();
    if PROTECTED.contains(&curr.as_str()) {
        eprintln!("Error: refusing to rebase on protected branch '{curr}'");
        return (
            3,
            json!({"status": "refused", "base": base, "reason": format!("on protected branch '{curr}'")}),
        );
    }

    // (2) Dirty tree guard.
    let dirty = run_git(git_bin, &["status", "--porcelain"], cwd, false)
        .map(|(_, out, _)| !out.trim().is_empty())
        .unwrap_or(true);
    if dirty {
        eprintln!("Error: working tree has uncommitted changes");
        return (
            2,
            json!({"status": "dirty", "base": base, "reason": "working tree has uncommitted changes"}),
        );
    }

    // (3) Attempt the rebase. The fetch already happened (phase A's own, or
    // the caller's, as in pr-push); a rebase against a stale origin is the
    // caller's choice to have made, never this function's.
    let rc = run_git(git_bin, &["rebase", base], cwd, true)
        .map(|(c, _, _)| c)
        .unwrap_or(1);
    if rc == 0 {
        return (0, json!({"status": "clean", "base": base}));
    }

    // (4) Collect conflicts.
    let conflicts = conflict_files(git_bin, cwd);
    if conflicts.is_empty() {
        let _ = run_git(git_bin, &["rebase", "--abort"], cwd, true);
        return (
            1,
            json!({"status": "failed", "base": base, "reason": "rebase failed (non-conflict error)"}),
        );
    }

    // (5) conflict_resolution policy.
    if conflict_resolution(cwd) == "fail" {
        let _ = run_git(git_bin, &["rebase", "--abort"], cwd, true);
        eprintln!("conflict_resolution=fail; aborting rebase");
        return (
            1,
            json!({
                "status": "failed",
                "base": base,
                "reason": "conflicts detected, conflict_resolution=fail",
                "files": conflicts,
            }),
        );
    }

    // (6) Guardrails.
    if let Some((reason, files)) = check_guardrails(&conflicts, cwd) {
        eprintln!("Guardrails refused auto-resolution");
        let v = json!({"status": "refused", "base": base, "reason": reason, "files": files});
        let _ = run_git(git_bin, &["rebase", "--abort"], cwd, true);
        return (1, v);
    }

    // (7) Guardrails passed - leave the rebase in-progress for the agent.
    eprintln!("Conflicts detected; guardrails passed. Caller must invoke conflict-resolver agent.");
    let extra = needs_resolver_extra(git_bin, &conflicts, cwd);
    let mut v = json!({"status": "needs_resolver", "base": base});
    if let (Some(obj), Some(files), Some(prev)) = (
        v.as_object_mut(),
        extra.get("files"),
        extra.get("diff_preview"),
    ) {
        obj.insert("files".into(), files.clone());
        obj.insert("diff_preview".into(), prev.clone());
    }
    (42, v)
}

/// Fetch origin, fail loudly so nobody rebases against a stale origin.
/// Separate from [`phase_a`] so `pr-push` fetches ONCE and still measures
/// `behind` against the fresh `origin/main` before rebasing; bare `pr-rebase`
/// runs fetch + phase_a, exactly the old Python sequence.
pub(crate) fn fetch_origin(cwd: &Path, git_bin: &str) -> Result<(), String> {
    match run_git(git_bin, &["fetch", "origin", "--quiet"], cwd, false) {
        Ok((0, _, _)) => Ok(()),
        Ok((_, _, ferr)) => {
            let first = ferr.lines().next().unwrap_or("").to_string();
            Err(first)
        }
        Err(e) => Err(e),
    }
}

/// The full PHASE A: fetch, then rebase. Bare `pr-rebase` entry path.
fn phase_a_full(base: &str, cwd: &Path, git_bin: &str) -> (i32, Value) {
    if let Err(first) = fetch_origin(cwd, git_bin) {
        return (
            1,
            json!({"status": "fetch_failed", "base": base, "reason": first}),
        );
    }
    phase_a(base, cwd, git_bin)
}

/// Entry point. Parses `--base=` / `--continue` plus the test seams
/// `--cwd` / `--git-bin`, runs the phase, prints the one JSON line.
pub fn run_rebase(argv: &[String]) -> i32 {
    let mut base = "origin/main".to_string();
    let mut phase_continue = false;
    let mut cwd: PathBuf = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut git_bin = "git".to_string();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if let Some(rest) = arg.strip_prefix("--base=") {
            base = rest.to_string();
        } else if arg == "--continue" {
            phase_continue = true;
        } else if arg == "--cwd" {
            if let Some(v) = argv.get(i + 1) {
                cwd = PathBuf::from(v);
                i += 1;
            }
        } else if arg == "--git-bin" {
            if let Some(v) = argv.get(i + 1) {
                git_bin = v.clone();
                i += 1;
            }
        }
        i += 1;
    }
    let (rc, v) = if phase_continue {
        phase_b_continue(&base, &cwd, &git_bin)
    } else {
        phase_a_full(&base, &cwd, &git_bin)
    };
    println!("{v}");
    if rc == 0 {
        // Advisory-only nudge: a hand-rebase is a common stale-base escape
        // the loop should have caught. This never emits on its own.
        eprintln!(
            "note: if this rebase resolved a stale base the loop should have \
             caught, tag it: fno doctor event gate-escape stale-base --detail \"hand-rebase\""
        );
    }
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run git in `cwd`, asserting success, returning stdout.
    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A work repo whose `origin` is a local bare remote with a main branch.
    /// `.fno/` is ignored so a test's project config never dirties the tree.
    fn init_repo_with_origin(tmp: &Path) -> PathBuf {
        let bare = tmp.join("origin.git");
        git(
            tmp,
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        let work = tmp.join("work");
        git(tmp, &["init", "-b", "main", work.to_str().unwrap()]);
        git(&work, &["config", "user.email", "t@t.t"]);
        git(&work, &["config", "user.name", "t"]);
        // A global core.hooksPath on a dev machine injects a pre-push guard
        // into every fresh repo, refusing the setup push. Point the test
        // repos' hooks at /dev/null; CI is clean either way.
        git(&work, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(work.join(".gitignore"), ".fno/\n").unwrap();
        std::fs::write(work.join("base.txt"), "base\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "init"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "-u", "origin", "main"]);
        work
    }

    /// Build a feature branch that conflicts with an advanced origin/main.
    fn make_conflict(tmp: &Path, conflict_file: &str) -> PathBuf {
        let work = init_repo_with_origin(tmp);
        git(&work, &["checkout", "-b", "tmp"]);
        std::fs::write(work.join(conflict_file), "main side\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "main edit"]);
        git(&work, &["push", "origin", "tmp:main"]);
        git(&work, &["checkout", "main"]);
        git(&work, &["checkout", "-b", "feature/conflict"]);
        std::fs::write(work.join(conflict_file), "feature side\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "feature edit"]);
        work
    }

    fn str_field(v: &Value, key: &str) -> String {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn clean_rebase_exits_0() {
        let tmp = tempfile::tempdir().unwrap();
        let work = init_repo_with_origin(tmp.path());
        // Advance origin/main with a non-conflicting file.
        git(&work, &["checkout", "-b", "tmp"]);
        std::fs::write(work.join("added-on-main.txt"), "x\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "main advances"]);
        git(&work, &["push", "origin", "tmp:main"]);
        // Feature branch off the old main, touching a different file.
        git(&work, &["checkout", "main"]);
        git(&work, &["checkout", "-b", "feature/x"]);
        std::fs::write(work.join("feature.txt"), "f\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "feature work"]);

        let (rc, v) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 0);
        assert_eq!(str_field(&v, "status"), "clean");
        assert_eq!(str_field(&v, "base"), "origin/main");
    }

    #[test]
    fn protected_branch_exits_3() {
        let tmp = tempfile::tempdir().unwrap();
        let work = init_repo_with_origin(tmp.path()); // on main
        let (rc, v) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 3);
        assert_eq!(str_field(&v, "status"), "refused");
    }

    #[test]
    fn dirty_tree_exits_2() {
        let tmp = tempfile::tempdir().unwrap();
        let work = init_repo_with_origin(tmp.path());
        git(&work, &["checkout", "-b", "feature/dirty"]);
        std::fs::write(work.join("base.txt"), "uncommitted change\n").unwrap();
        let (rc, v) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 2);
        assert_eq!(str_field(&v, "status"), "dirty");
    }

    #[test]
    fn conflict_needs_resolver_exits_42_and_leaves_rebase_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let work = make_conflict(tmp.path(), "base.txt");
        let (rc, v) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 42);
        assert_eq!(str_field(&v, "status"), "needs_resolver");
        let files = v
            .get("files")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            files.iter().any(|f| f.as_str() == Some("base.txt")),
            "{files:?}"
        );
        // The rebase is left in-progress for the agent (not aborted).
        assert!(work.join(".git/rebase-merge").exists() || work.join(".git/rebase-apply").exists());
        let _ = git(&work, &["rebase", "--abort"]);
    }

    #[test]
    fn conflict_resolution_fail_exits_1() {
        let tmp = tempfile::tempdir().unwrap();
        let work = make_conflict(tmp.path(), "base.txt");
        std::fs::create_dir_all(work.join(".fno")).unwrap();
        std::fs::write(
            work.join(".fno/config.toml"),
            "[auto_merge]\nconflict_resolution = \"fail\"\n",
        )
        .unwrap();
        let (rc, v) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 1);
        assert_eq!(str_field(&v, "status"), "failed");
        assert!(str_field(&v, "reason").contains("conflict_resolution=fail"));
    }

    #[test]
    fn guardrail_lockfile_refuses_exit_1() {
        let tmp = tempfile::tempdir().unwrap();
        let work = make_conflict(tmp.path(), "uv.lock");
        let (rc, v) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 1);
        assert_eq!(str_field(&v, "status"), "refused");
        assert!(str_field(&v, "reason").contains("lock file"));
    }

    #[test]
    fn phase_b_continue_resolves_exit_0() {
        let tmp = tempfile::tempdir().unwrap();
        let work = make_conflict(tmp.path(), "base.txt");
        // Phase A leaves an in-progress rebase at exit 42.
        let (rc, _) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 42);
        // The "agent" resolves + stages the conflict (stage-only: --continue
        // must finalise the commit non-interactively, the headless-editor
        // regression).
        std::fs::write(work.join("base.txt"), "resolved\n").unwrap();
        git(&work, &["add", "base.txt"]);
        let (rc, v) = phase_b_continue("origin/main", &work, "git");
        assert_eq!(rc, 0);
        assert_eq!(str_field(&v, "status"), "resolved");
    }

    #[test]
    fn phase_b_continue_more_conflicts_exits_42_not_1() {
        // In a multi-commit rebase, --continue exits non-zero when it pauses
        // on a NEW conflict in a later commit; the port must report
        // needs_resolver (42), not failed (1).
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("origin.git");
        git(
            tmp.path(),
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        let work = tmp.path().join("work");
        git(tmp.path(), &["init", "-b", "main", work.to_str().unwrap()]);
        git(&work, &["config", "user.email", "t@t.t"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "core.hooksPath", "/dev/null"]);
        std::fs::write(work.join("f1.txt"), "base1\n").unwrap();
        std::fs::write(work.join("f2.txt"), "base2\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "init"]);
        git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&work, &["push", "-u", "origin", "main"]);
        // origin/main edits BOTH files.
        git(&work, &["checkout", "-b", "tmp"]);
        std::fs::write(work.join("f1.txt"), "main1\n").unwrap();
        std::fs::write(work.join("f2.txt"), "main2\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "main edits both"]);
        git(&work, &["push", "origin", "tmp:main"]);
        // feature: commit A edits f1, commit B edits f2 (each conflicts).
        git(&work, &["checkout", "main"]);
        git(&work, &["checkout", "-b", "feature/multi"]);
        std::fs::write(work.join("f1.txt"), "feat1\n").unwrap();
        git(&work, &["add", "f1.txt"]);
        git(&work, &["commit", "-m", "feature A: f1"]);
        std::fs::write(work.join("f2.txt"), "feat2\n").unwrap();
        git(&work, &["add", "f2.txt"]);
        git(&work, &["commit", "-m", "feature B: f2"]);
        // Phase A: commit A conflicts -> 42.
        let (rc, _) = phase_a_full("origin/main", &work, "git");
        assert_eq!(rc, 42);
        // Resolve f1, stage, --continue -> applies B -> f2 conflict -> 42 (not 1).
        std::fs::write(work.join("f1.txt"), "resolved1\n").unwrap();
        git(&work, &["add", "f1.txt"]);
        let (rc, v) = phase_b_continue("origin/main", &work, "git");
        assert_eq!(
            rc, 42,
            "the B-conflict pause is needs_resolver, never failed"
        );
        assert_eq!(str_field(&v, "status"), "needs_resolver");
        let files = v
            .get("files")
            .and_then(|f| f.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            files.iter().any(|f| f.as_str() == Some("f2.txt")),
            "{files:?}"
        );
        let _ = git(&work, &["rebase", "--abort"]);
    }

    // ---- guardrail unit table ----

    #[test]
    fn guardrail_classification() {
        let tmp = tempfile::tempdir().unwrap();
        let cases: Vec<(&str, bool)> = vec![
            ("supabase/migrations/001_x.sql", true),
            ("app/migrations/0001.py", true),
            ("schema.prisma", true),
            (".env", true),
            (".env.local", true),
            ("config/secrets/key.pem", true),
            ("package-lock.json", true),
            ("Cargo.lock", true),
            (".gitignore", true),
            (".gitattributes", true),
            ("src/app/main.py", false),
            ("README.md", false),
        ];
        for (path, expect_refused) in cases {
            let refused = check_guardrails(&[path.to_string()], tmp.path());
            assert_eq!(refused.is_some(), expect_refused, "{path}");
        }
    }

    #[test]
    fn guardrail_mass_conflict_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("big.py"), "<<<<<<< HEAD\n".repeat(4)).unwrap();
        let (reason, _) = check_guardrails(&["big.py".to_string()], tmp.path()).expect("refused");
        assert!(reason.contains("mass conflicts"), "{reason}");
    }

    #[test]
    fn guardrail_three_markers_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("small.py"), "<<<<<<< HEAD\n".repeat(3)).unwrap();
        assert!(check_guardrails(&["small.py".to_string()], tmp.path()).is_none());
    }
}
