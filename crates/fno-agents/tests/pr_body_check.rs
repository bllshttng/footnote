//! `pr-body-check` against a real temp git repo and stub guard scripts, the
//! same process-level discipline as tests/pr_push.rs: the built binary runs
//! as its own process, so each test gets a clean environment and the
//! receipt/refusal text is asserted on real stdout/stderr.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A work repo whose `origin` is a local bare remote with a main branch, so
/// `git merge-base origin/main HEAD` resolves (pr_rebase's pattern).
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
    git(&work, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(work.join("base.txt"), "base\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "init"]);
    git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&work, &["push", "-u", "origin", "main"]);
    work
}

/// An executable stub guard under `<root>/scripts/ci/`.
fn write_guard(root: &Path, name: &str, body: &str) {
    let dir = root.join("scripts").join("ci");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

const PASS_STUB: &str = "#!/bin/sh\nexit 0\n";
const FAIL_STUB: &str = "#!/bin/sh\necho 'stub violation in the body' >&2\nexit 1\n";

fn body_file(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("body.md");
    std::fs::write(&p, body).unwrap();
    p
}

fn run_verb(cwd: &Path, extra: &[&str]) -> (i32, String, String) {
    run_verb_with_stdin(cwd, extra, None)
}

fn run_verb_with_stdin(cwd: &Path, extra: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fno-agents"));
    cmd.envs(fno_agents::test_run::self_owner_env())
        .args(["pr-body-check"])
        .arg("--cwd")
        .arg(cwd)
        .args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(input) = stdin {
        use std::io::Write;
        let mut child = cmd.stdin(Stdio::piped()).spawn().expect("run fno-agents");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("wait fno-agents");
        return (
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
    }
    let out = cmd.output().expect("run fno-agents");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn all_three_pass_exit_0() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    for name in [
        "check-no-session-urls.sh",
        "check-pr-node-closure.sh",
        "check-oos-tracked.sh",
    ] {
        write_guard(&work, name, PASS_STUB);
    }
    let body = body_file(&work, "clean body\n\nBacklog-Closure: x-3830\n");
    let (code, out, err) = run_verb(
        &work,
        &[
            "--body-file",
            body.to_str().unwrap(),
            "--title",
            "t",
            "--head",
            "feature/x-3830",
        ],
    );
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(out.matches("pass check-").count(), 3, "{out}");
    assert!(out.contains("3 ran, 0 failed"), "{out}");
}

#[test]
fn a_failing_guard_carries_its_own_output_and_exits_1() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    write_guard(&work, "check-no-session-urls.sh", FAIL_STUB);
    write_guard(&work, "check-pr-node-closure.sh", PASS_STUB);
    write_guard(&work, "check-oos-tracked.sh", PASS_STUB);
    let body = body_file(&work, "body\n");
    let (code, out, err) = run_verb(&work, &["--body-file", body.to_str().unwrap()]);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.contains("fail check-no-session-urls.sh"), "{out}");
    assert!(out.contains("pass check-pr-node-closure.sh"), "{out}");
    assert!(err.contains("stub violation in the body"), "{err}");
    assert!(out.contains("3 ran, 1 failed"), "{out}");
    assert!(
        err.contains("the PR body is not a commit"),
        "the remedy line: {err}"
    );
}

#[test]
fn all_guards_run_even_after_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    write_guard(&work, "check-no-session-urls.sh", FAIL_STUB);
    write_guard(&work, "check-pr-node-closure.sh", PASS_STUB);
    write_guard(&work, "check-oos-tracked.sh", FAIL_STUB);
    let body = body_file(&work, "body\n");
    let (code, out, err) = run_verb(&work, &["--body-file", body.to_str().unwrap()]);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.contains("fail check-no-session-urls.sh"), "{out}");
    assert!(out.contains("fail check-oos-tracked.sh"), "{out}");
    assert!(out.contains("3 ran, 2 failed"), "{out}");
}

#[test]
fn a_repo_with_no_guards_skips_all_three() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    let body = body_file(&work, "body\n");
    let (code, out, err) = run_verb(&work, &["--body-file", body.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(out.matches("skip check-").count(), 3, "{out}");
    assert!(out.contains("0 ran, 0 failed"), "{out}");
}

#[test]
fn a_missing_body_file_names_the_path_and_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    let (code, out, err) = run_verb(&work, &["--body-file", "/nonexistent/b.md"]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("/nonexistent/b.md"), "{err}");
}

#[test]
fn an_unresolvable_merge_base_is_a_read_error_naming_the_fetch() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    git(
        &work.parent().unwrap(),
        &["init", "-b", "main", work.to_str().unwrap()],
    );
    git(&work, &["config", "user.email", "t@t.t"]);
    git(&work, &["config", "user.name", "t"]);
    std::fs::write(work.join("f.txt"), "x\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "init"]);
    let body = body_file(&work, "body\n");
    let (code, out, err) = run_verb(&work, &["--body-file", body.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("git fetch origin main"), "{err}");
}

#[test]
fn every_guard_gets_the_same_env_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    let log = tmp.path().join("env.log");
    write_guard(
        &work,
        "check-no-session-urls.sh",
        &format!(
            "#!/bin/sh\nprintf 'BODY=%s\\nTITLE=%s\\nHEADREF=%s\\nHEADSHA=%s\\nBASESHA=%s\\n' \
             \"$PR_BODY\" \"$PR_TITLE\" \"$PR_HEAD_REF\" \"$PR_HEAD_SHA\" \"$PR_BASE_SHA\" > {}\nexit 0\n",
            log.to_str().unwrap().escape_default()
        ),
    );
    write_guard(&work, "check-pr-node-closure.sh", PASS_STUB);
    write_guard(&work, "check-oos-tracked.sh", PASS_STUB);
    let body = body_file(&work, "the body text\n");
    let (code, out, err) = run_verb(
        &work,
        &[
            "--body-file",
            body.to_str().unwrap(),
            "--title",
            "the title",
            "--head",
            "feature/x-3830",
        ],
    );
    assert_eq!(code, 0, "{out}\n{err}");
    let env = std::fs::read_to_string(&log).unwrap();
    assert!(env.contains("BODY=the body text\n"), "{env}");
    assert!(env.contains("TITLE=the title\n"), "{env}");
    assert!(env.contains("HEADREF=feature/x-3830\n"), "{env}");
    assert!(env.contains("HEADSHA=HEAD\n"), "{env}");
    let expected_base = git(&work, &["merge-base", "origin/main", "HEAD"]);
    assert!(
        env.contains(&format!("BASESHA={}", expected_base.trim())),
        "{env}"
    );
}

#[test]
fn dash_reads_the_body_from_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    write_guard(&work, "check-no-session-urls.sh", PASS_STUB);
    write_guard(&work, "check-pr-node-closure.sh", PASS_STUB);
    write_guard(&work, "check-oos-tracked.sh", PASS_STUB);
    let (code, out, err) = run_verb_with_stdin(
        &work,
        &["--body-file", "-", "--head", "feature/x-3830"],
        Some("stdin body\n"),
    );
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("3 ran, 0 failed"), "{out}");
}

#[test]
fn not_a_git_repo_is_a_read_error() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let body = body_file(&plain, "body\n");
    let (code, out, err) = run_verb(&plain, &["--body-file", body.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("not a git repository"), "{err}");
}

#[test]
fn a_missing_body_file_flag_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let work = init_repo_with_origin(tmp.path());
    let (code, out, err) = run_verb(&work, &[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("--body-file is required"), "{err}");
}
