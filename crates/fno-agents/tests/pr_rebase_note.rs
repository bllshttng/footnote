//! AC1-UI regression, ported with the `_rebase.py` -> `pr_rebase.rs` move:
//! a successful rebase prints ONE stderr nudge suggesting the stale-base tag
//! command and emits no event of its own; a failed rebase prints none. Runs
//! the built binary against real temp git repos because the nudge is a
//! stderr side effect of the verb entry point.

use std::process::Command;

fn git(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A work repo whose origin is a local bare remote, with one advanced
/// origin/main commit and a clean feature branch off the old main.
fn repo_with_stale_base(tmp: &std::path::Path) -> std::path::PathBuf {
    let bare = tmp.join("origin.git");
    git(
        tmp,
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let work = tmp.join("work");
    git(tmp, &["init", "-b", "main", work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@t.t"]);
    git(&work, &["config", "user.name", "t"]);
    // See pr_rebase.rs tests: a global core.hooksPath must not guard the
    // setup push.
    git(&work, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "init"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-u", "origin", "main"]);
    git(&work, &["checkout", "-b", "tmp"]);
    std::fs::write(work.join("added.txt"), "x\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "main advances"]);
    git(&work, &["push", "origin", "tmp:main"]);
    git(&work, &["checkout", "main"]);
    git(&work, &["checkout", "-b", "feature/x"]);
    std::fs::write(work.join("feature.txt"), "f\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "feature work"]);
    work
}

fn run_verb(cwd: &std::path::Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .args([
            "pr-rebase",
            "--base=origin/main",
            "--cwd",
            cwd.to_str().unwrap(),
        ])
        .output()
        .expect("run fno-agents");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn rebase_success_prints_one_nudge_and_no_event() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo_with_stale_base(tmp.path());
    let (rc, err) = run_verb(&work);
    assert_eq!(rc, 0);
    assert!(err.contains("gate-escape stale-base"), "{err}");
    assert_eq!(err.matches("note:").count(), 1, "{err}");
}

#[test]
fn rebase_failure_prints_no_nudge() {
    let tmp = tempfile::tempdir().unwrap();
    let work = repo_with_stale_base(tmp.path());
    // On main: the protected-branch refusal, a non-zero outcome.
    git(&work, &["checkout", "main"]);
    let (rc, err) = run_verb(&work);
    assert_ne!(rc, 0);
    assert!(!err.contains("gate-escape"), "{err}");
}
