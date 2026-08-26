use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_fno-agents");

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn manifest(root: &Path, session_id: &str) -> PathBuf {
    let path = root.join(".fno/target-state.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            "---\nfno_id: run-{session_id}\nharness: codex\nharness_session_id: {session_id}\nowner_cwd: \"{}\"\n---\n",
            root.display()
        ),
    )
    .unwrap();
    path
}

fn two_worktrees() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let other = temp.path().join("other");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("seed"), "seed\n").unwrap();
    git(&repo, &["add", "seed"]);
    git(&repo, &["commit", "-qm", "seed"]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            other.to_str().unwrap(),
            "-b",
            "other",
        ],
    );
    (temp, repo, other)
}

fn run(cwd: &Path, session_id: &str) -> Output {
    Command::new(BINARY)
        .current_dir(cwd)
        .args(["manifest-for-session", "--harness-session-id", session_id])
        .output()
        .expect("manifest-for-session runs")
}

#[test]
fn ac3_hp_resolves_the_matching_manifest_from_another_worktree() {
    let (_temp, repo, other) = two_worktrees();
    let expected = manifest(&repo, "session-a");
    manifest(&other, "session-b");

    let output = run(&other, "session-a");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        expected.canonicalize().unwrap().to_string_lossy()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn ac3_err_returns_one_without_output_for_an_unknown_session() {
    let (_temp, repo, other) = two_worktrees();
    manifest(&repo, "session-a");
    manifest(&other, "session-b");

    let output = run(&other, "missing-session");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn unreadable_worktree_list_returns_two() {
    let temp = tempfile::tempdir().unwrap();

    let output = run(temp.path(), "session-a");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
