//! The guarded push against stub git/gh/fno executables, the same discipline
//! as heal's push-discipline tests: exactly one push, never over a run in
//! flight, refusals name their door. Process-level so the receipt line and
//! refusal text are asserted on real stdout/stderr.

use std::path::{Path, PathBuf};
use std::process::Command;

fn write_exec(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn log_of(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// Stub git. Behavior is file-flag driven: write `dirty` to make
/// `status --porcelain` report a change, write `conflict` to make the rebase
/// fail with two conflicting paths, drop `no-upstream` to make `@{u}` fail,
/// drop `no-remote-branch` to make the same-name remote branch absent (a
/// first push), write `remote-only` to make the cherry-pick log report one
/// remote-only commit, write `remote-only-merge` to make the merges log
/// report one merge with two parents, and write `dirty-merge` to make that
/// merge conflicted under `merge-tree` with a tree that differs from the
/// parent auto-merge. `rev-list --count` answers 3 on the first call and 0
/// after (the rebase happened), so the receipt reads behind-before=3
/// behind-after=0.
fn stub_git(dir: &Path) {
    write_exec(
        dir,
        "git",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "git $*" >> "$D/git.log"
case "$1" in
  rev-parse)
    case "$2" in
      --abbrev-ref)
        if [ "$3" = "@{u}" ]; then
          if [ -f "$D/no-upstream" ]; then exit 1; fi
          echo "refs/remotes/origin/feature/x"; exit 0
        fi
        echo "feature/x"; exit 0 ;;
      "@{u}") echo deadbeef0000000; exit 0 ;;
      --verify)
        if [ -f "$D/no-remote-branch" ]; then exit 1; fi
        echo deadbeef0000000; exit 0 ;;
      --short) echo abc1234; exit 0 ;;
      --show-toplevel) echo "$D"; exit 0 ;;
      *"^{tree}"*)
        if [ -f "$D/dirty-merge" ]; then echo handtree0000000; else echo autotree000000; fi
        exit 0 ;;
    esac
    exit 0 ;;
  status) if [ -f "$D/dirty" ]; then echo " M src/x.rs"; fi; exit 0 ;;
  rev-list)
    if [ -f "$D/counted" ]; then echo 0; else echo 3; touch "$D/counted"; fi
    exit 0 ;;
  fetch) exit 0 ;;
  rebase)
    if [ -f "$D/conflict" ]; then
      echo "CONFLICT (content): Merge conflict in src/a.rs" >&2
      exit 1
    fi
    exit 0 ;;
  diff) if [ -f "$D/conflict" ]; then printf "src/a.rs\nsrc/b.rs\n"; fi; exit 0 ;;
  log)
    case "$*" in
      *--merges*)
        if [ -f "$D/remote-only-merge" ]; then echo "m1234567 b0000000 m0000000"; fi
        exit 0 ;;
      *--cherry-pick*)
        if [ -f "$D/remote-only" ]; then echo abc1234; fi
        exit 0 ;;
      *--format=*)
        if [ -f "$D/bad-commit-msg" ]; then
          printf 'abc1234\037per d-deadbeef we ruled\036'
        elif [ -f "$D/live-commit-msg" ]; then
          printf 'abc1234\037per d-aaaa0001 we ruled\036'
        fi
        exit 0 ;;
    esac
    if [ -f "$D/remote-only" ]; then echo abc1234; fi
    exit 0 ;;
  merge-tree)
    if [ -f "$D/dirty-merge" ]; then echo conflicttree0000; exit 1; fi
    echo autotree000000; exit 0 ;;
esac
exit 0
"#,
    );
}

/// Stub gh: a settled read (completed failure buckets as not pending) by
/// default; write `pending` to make the check-runs read report a run in
/// flight with a job link.
fn stub_gh(dir: &Path) {
    write_exec(
        dir,
        "gh",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
if [ -f "$D/gh-error" ]; then
  echo "check read failed" >&2
  exit 1
fi
for a in "$@"; do case "$a" in
  */check-runs)
    if [ -f "$D/no-runs" ]; then
      echo '{"check_runs":[]}'
    elif [ -f "$D/pending" ]; then
      echo '{"check_runs":[{"name":"guards","status":"in_progress","conclusion":null,"html_url":"https://github.com/o/r/actions/runs/7/job/42"}]}'
    else
      echo '{"check_runs":[{"name":"guards","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/7/job/42"}]}'
    fi
    exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '{"check_runs":[]}'
exit 0
"#,
    );
}

/// Stub fno for the bypass journal row.
fn stub_fno(dir: &Path) {
    write_exec(
        dir,
        "fno",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "fno $*" >> "$D/fno.log"
exit 0
"#,
    );
}

fn write_stubs(dir: &Path) {
    stub_git(dir);
    stub_gh(dir);
    stub_fno(dir);
}

fn run_verb(dir: &Path, extra: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .envs(fno_agents::test_run::self_owner_env())
        .args(["pr-push"])
        .arg("--cwd")
        .arg(dir)
        .arg("--git-bin")
        .arg(dir.join("git"))
        .arg("--gh-bin")
        .arg(dir.join("gh"))
        .arg("--fno-bin")
        .arg(dir.join("fno"))
        .arg("--stamps-dir")
        .arg(dir.join("stamps"))
        .args(extra)
        .output()
        .expect("run fno-agents");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn tmpdir() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().to_path_buf();
    write_stubs(&d);
    (t, d)
}

#[test]
fn pushes_once_with_the_behind_receipt() {
    let (_t, d) = tmpdir();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("behind-before=3 behind-after=0"), "{out}");
    assert!(out.contains("integrate=rebase"), "{out}");
    assert!(out.contains("preflight=absent"), "{out}");
    assert!(out.contains("ci=settled"), "{out}");
    assert!(out.contains("sha=abc1234"), "{out}");
    assert!(out.contains("pushed=1"), "{out}");
    assert_eq!(log_of(&d, "git.log").matches("git push").count(), 1);
    assert!(
        log_of(&d, "git.log").contains("--force-with-lease=refs/heads/feature/x:deadbeef0000000"),
        "the push is leased to the fetched remote sha: {:?}",
        log_of(&d, "git.log")
    );
    assert!(
        log_of(&d, "gh.log").contains("check-runs"),
        "the in-flight read ran"
    );
    assert!(
        d.join("stamps").join("feature_x.stamp").exists(),
        "the hook's stamp is written"
    );
}

#[test]
fn a_run_in_flight_refuses_with_exit_2_and_names_the_check() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("pending"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("guards"), "the check name: {err}");
    assert!(err.contains("42"), "the job id: {err}");
    assert!(
        !log_of(&d, "git.log").contains("git push"),
        "nothing pushed"
    );
}

#[test]
fn an_in_flight_run_refuses_before_preflight() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("pending"), "").unwrap();
    std::fs::create_dir_all(d.join("scripts/ci")).unwrap();
    write_exec(
        &d.join("scripts/ci"),
        "preflight.sh",
        "#!/bin/sh\necho ran > \"$(dirname \"$0\")/../../preflight.log\"\nexit 0\n",
    );
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(!d.join("preflight.log").exists(), "preflight did not run");
    assert!(!log_of(&d, "git.log").contains("git push"));
}

#[test]
fn the_in_flight_probe_emits_true_false_and_error_shapes_without_mutating_git() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("pending"), "").unwrap();
    let (code, out, err) = run_verb(&d, &["--in-flight", "feature/x"]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(out.contains("\"in_flight\":true"), "{out}");
    assert!(out.contains("\"check\":\"guards\""), "{out}");
    assert!(out.contains("\"job\":\"42\""), "{out}");
    assert!(!log_of(&d, "git.log").contains("git push"));
    assert!(!log_of(&d, "git.log").contains("fetch"));
    assert!(!log_of(&d, "git.log").contains("rebase"));

    std::fs::remove_file(d.join("pending")).unwrap();
    let (code, out, err) = run_verb(&d, &["--in-flight", "feature/x"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"in_flight\":false"), "{out}");

    std::fs::write(d.join("gh-error"), "").unwrap();
    let (code, out, err) = run_verb(&d, &["--in-flight", "feature/x"]);
    assert_eq!(code, 4, "{out}\n{err}");
    assert!(out.contains("\"error\""), "{out}");
    assert!(!out.contains("\"in_flight\""), "{out}");
}

#[test]
fn an_in_flight_probe_without_a_remote_ref_answers_false() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("no-remote-branch"), "").unwrap();
    let (code, out, err) = run_verb(&d, &["--in-flight", "feature/x"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"in_flight\":false"), "{out}");
    assert!(!log_of(&d, "gh.log").contains("check-runs"));
}

#[test]
fn a_conflict_refuses_with_exit_3_and_names_the_rebase_door() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("conflict"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(err.contains("fno do pr rebase"), "the door: {err}");
    assert!(
        err.contains("src/a.rs") && err.contains("src/b.rs"),
        "the paths: {err}"
    );
    assert!(
        !log_of(&d, "git.log").contains("git push"),
        "nothing pushed"
    );
}

#[test]
fn a_dirty_tree_refuses_before_anything_moves() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("dirty"), "").unwrap();
    let (code, _out, err) = run_verb(&d, &[]);
    assert_eq!(code, 3, "{err}");
    assert!(
        !log_of(&d, "git.log").contains("fetch"),
        "no fetch after a dirty refusal"
    );
    assert!(!log_of(&d, "git.log").contains("git push"));
}

#[test]
fn force_ci_cancel_pushes_and_records_the_bypass() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("pending"), "").unwrap();
    let (code, out, err) = run_verb(&d, &["--force-ci-cancel"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("ci=bypassed"), "{out}");
    assert!(
        log_of(&d, "fno.log").contains("push_debounce_bypass"),
        "the journal row"
    );
    assert_eq!(log_of(&d, "git.log").matches("git push").count(), 1);
}

#[test]
fn no_preflight_is_recorded_in_the_receipt() {
    let (_t, d) = tmpdir();
    let (code, out, err) = run_verb(&d, &["--no-preflight"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("preflight=skipped"), "{out}");
}

#[test]
fn a_red_preflight_refuses_the_push() {
    let (_t, d) = tmpdir();
    std::fs::create_dir_all(d.join("scripts/ci")).unwrap();
    write_exec(
        &d.join("scripts/ci"),
        "preflight.sh",
        "#!/bin/sh\necho red > \"$(dirname \"$0\")/../../preflight.log\"\nexit 3\n",
    );
    let (code, _out, err) = run_verb(&d, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(d.join("preflight.log").exists(), "preflight ran");
    assert!(
        !log_of(&d, "git.log").contains("git push"),
        "nothing pushed"
    );
}

#[test]
fn a_first_push_sets_the_upstream() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("no-upstream"), "").unwrap();
    std::fs::write(d.join("no-remote-branch"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "{out}\n{err}");
    let log = log_of(&d, "git.log");
    assert!(
        log.contains("git push --set-upstream origin HEAD:feature/x\n"),
        "the push args keep their prefix: {log:?}"
    );
    assert!(
        !log.contains("--force-with-lease"),
        "a first push carries no lease: {log:?}"
    );
}

#[test]
fn a_second_push_inside_the_registration_window_is_refused() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("no-runs"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "first push lands: {out}\n{err}");
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 2, "the window holds: {out}\n{err}");
    assert!(
        err.contains("may not be registered yet"),
        "the refusal names the window: {err}"
    );
    assert_eq!(
        log_of(&d, "git.log").matches("git push").count(),
        1,
        "exactly one push across both runs"
    );
}

#[test]
fn a_recent_stamp_with_registered_settled_rows_still_pushes() {
    let (_t, d) = tmpdir();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "{out}\n{err}");
    // The stamp is now young, but the rows are registered and settled: the
    // live read outranks the clock.
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "settled rows outrank the stamp: {out}\n{err}");
    assert_eq!(log_of(&d, "git.log").matches("git push").count(), 2);
}

#[test]
fn force_ci_cancel_pushes_through_the_registration_window() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("no-runs"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "{out}\n{err}");
    let (code, out, err) = run_verb(&d, &["--force-ci-cancel"]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(log_of(&d, "fno.log").contains("push_debounce_bypass"));
    assert_eq!(log_of(&d, "git.log").matches("git push").count(), 2);
}

#[test]
fn a_remote_only_commit_refuses_with_exit_3_before_preflight() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("remote-only"), "").unwrap();
    // A passing preflight runner must never run: the refusal fires before
    // the rehearsal, not after it.
    std::fs::create_dir_all(d.join("scripts/ci")).unwrap();
    write_exec(
        &d.join("scripts/ci"),
        "preflight.sh",
        "#!/bin/sh\necho ran > \"$(dirname \"$0\")/../../preflight.log\"\nexit 0\n",
    );
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(err.contains("abc1234"), "the remote-only sha: {err}");
    assert!(
        err.contains("git pull --rebase origin feature/x"),
        "the door: {err}"
    );
    assert!(err.contains("Nothing pushed"), "{err}");
    assert!(
        !err.contains("not safely rebasable"),
        "the refusal must not key heal's conflict phrase: {err}"
    );
    assert!(
        !d.join("preflight.log").exists(),
        "the refusal fired before preflight"
    );
    assert!(
        !log_of(&d, "git.log").contains("git push"),
        "nothing pushed"
    );
}

#[test]
fn a_remote_hand_resolved_merge_refuses_with_exit_3() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("remote-only-merge"), "").unwrap();
    std::fs::write(d.join("dirty-merge"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(err.contains("m1234567"), "the merge sha: {err}");
    assert!(
        err.contains("git pull --rebase origin feature/x"),
        "the door: {err}"
    );
    assert!(
        !err.contains("not safely rebasable"),
        "the refusal must not key heal's conflict phrase: {err}"
    );
    assert!(
        !log_of(&d, "git.log").contains("git push"),
        "nothing pushed"
    );
}

#[test]
fn a_clean_remote_merge_still_leases_and_pushes() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("remote-only-merge"), "").unwrap();
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("pushed=1"), "{out}");
    assert!(
        log_of(&d, "git.log").contains("--force-with-lease=refs/heads/feature/x:deadbeef0000000"),
        "the validated merge leaves the lease on: {:?}",
        log_of(&d, "git.log")
    );
}

// ── real-git regression: the second push against a moved base ───────────────

/// Real git, severed from the user's global config and hooks: a global
/// pre-push hook that refuses protected branches must never fire here.
fn git_in(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit_file(repo: &Path, name: &str, body: &str, msg: &str) {
    std::fs::write(repo.join(name), body).unwrap();
    git_in(repo, &["add", name]);
    git_in(repo, &["commit", "-m", msg]);
}

/// Bare remote + two clones. main is seeded from clone A, feature/x carries
/// one commit, and the stub gh/fno ride in the root. The first verb push is
/// each test's own act: it is the act under test.
fn real_repo() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().to_path_buf();
    let a = root.join("a");
    let b = root.join("b");
    stub_gh(&root);
    stub_fno(&root);
    git_in(&root, &["init", "--bare", "-b", "main", "remote.git"]);
    git_in(
        &root,
        &["-c", "init.defaultBranch=main", "clone", "remote.git", "a"],
    );
    git_in(&a, &["config", "user.email", "a@example.test"]);
    git_in(&a, &["config", "user.name", "Clone A"]);
    commit_file(&a, "README.md", "seed\n", "seed");
    git_in(&a, &["push", "origin", "main"]);
    git_in(&a, &["checkout", "-b", "feature/x"]);
    commit_file(&a, "one.txt", "one\n", "one");
    git_in(
        &root,
        &["-c", "init.defaultBranch=main", "clone", "remote.git", "b"],
    );
    git_in(&b, &["config", "user.email", "b@example.test"]);
    git_in(&b, &["config", "user.name", "Clone B"]);
    (t, root, a, b)
}

/// The verb against the real repo, global git config severed the same way.
fn run_verb_real(a: &Path, root: &Path) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .envs(fno_agents::test_run::self_owner_env())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["pr-push"])
        .arg("--cwd")
        .arg(a)
        .arg("--git-bin")
        .arg("/usr/bin/git")
        .arg("--gh-bin")
        .arg(root.join("gh"))
        .arg("--fno-bin")
        .arg(root.join("fno"))
        .arg("--stamps-dir")
        .arg(root.join("stamps"))
        .arg("--no-preflight")
        .output()
        .expect("run fno-agents");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_second_push_after_main_moved_lands() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    // main moves from clone B, then clone A commits again.
    commit_file(&b, "main.txt", "m\n", "main moves");
    git_in(&b, &["push", "origin", "main"]);
    commit_file(&a, "two.txt", "2\n", "two");
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "the leased second push: {out}\n{err}");
    assert!(out.contains("pushed=1"), "{out}");
    let remote = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    let head = git_in(&a, &["rev-parse", "HEAD"]);
    assert_eq!(
        remote.trim(),
        head.trim(),
        "the remote branch equals the rebased HEAD"
    );
}

#[test]
fn a_second_push_with_main_unchanged_still_fast_forwards() {
    let (_t, root, a, _b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    commit_file(&a, "two.txt", "2\n", "two");
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "{out}\n{err}");
    let remote = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    let head = git_in(&a, &["rev-parse", "HEAD"]);
    assert_eq!(remote.trim(), head.trim());
}

#[test]
fn a_merge_bearing_branch_merges_main_instead_of_rebasing() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    commit_file(&b, "main.txt", "m\n", "main moves");
    git_in(&b, &["push", "origin", "main"]);
    git_in(&a, &["fetch", "origin"]);
    git_in(&a, &["merge", "--no-edit", "origin/main"]);
    git_in(&a, &["push", "origin", "feature/x"]);
    let old_remote = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    commit_file(&b, "second-main.txt", "m2\n", "main moves again");
    git_in(&b, &["push", "origin", "main"]);
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "the merge-bearing push: {out}\n{err}");
    assert!(out.contains("integrate=merge"), "{out}");
    let new_remote = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    git_in(
        &root.join("remote.git"),
        &[
            "merge-base",
            "--is-ancestor",
            old_remote.trim(),
            new_remote.trim(),
        ],
    );
    assert_eq!(
        git_in(
            &a,
            &["rev-list", "--merges", "--count", "origin/main..HEAD"]
        )
        .trim(),
        "2"
    );
}

#[test]
fn a_merge_bearing_branch_at_behind_zero_is_not_rewritten() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    commit_file(&b, "main.txt", "m\n", "main moves");
    git_in(&b, &["push", "origin", "main"]);
    git_in(&a, &["fetch", "origin"]);
    git_in(&a, &["merge", "--no-edit", "origin/main"]);
    git_in(&a, &["push", "origin", "feature/x"]);
    let before = git_in(&a, &["rev-parse", "HEAD"]);
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "the up-to-date merge-bearing push: {out}\n{err}");
    assert!(out.contains("integrate=merge"), "{out}");
    let after = git_in(&a, &["rev-parse", "HEAD"]);
    assert_eq!(before.trim(), after.trim());
    assert_eq!(
        git_in(
            &root.join("remote.git"),
            &["rev-parse", "refs/heads/feature/x"],
        )
        .trim(),
        before.trim()
    );
}

#[test]
fn a_merge_conflict_aborts_and_names_the_merge_door() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    commit_file(&b, "main.txt", "m\n", "main moves");
    git_in(&b, &["push", "origin", "main"]);
    git_in(&a, &["fetch", "origin"]);
    git_in(&a, &["merge", "--no-edit", "origin/main"]);
    git_in(&a, &["push", "origin", "feature/x"]);
    commit_file(&b, "one.txt", "main\n", "main conflicts");
    git_in(&b, &["push", "origin", "main"]);
    let before = git_in(&a, &["rev-parse", "HEAD"]);
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 3, "the merge conflict: {out}\n{err}");
    assert!(err.contains("not safely rebasable"), "{err}");
    assert!(err.contains("status merge_conflict"), "{err}");
    assert!(err.contains("files: one.txt"), "{err}");
    assert!(err.contains("Merge origin/main by hand"), "{err}");
    assert_eq!(before.trim(), git_in(&a, &["rev-parse", "HEAD"]).trim());
    assert!(!a.join(".git/MERGE_HEAD").exists());
}

#[test]
fn a_remote_only_commit_is_refused_and_kept() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    // Another writer pushes a NEW commit to feature/x, then main moves.
    git_in(&b, &["fetch", "origin"]);
    git_in(&b, &["checkout", "feature/x"]);
    commit_file(&b, "theirs.txt", "t\n", "theirs");
    git_in(&b, &["push", "origin", "feature/x"]);
    git_in(&b, &["checkout", "main"]);
    commit_file(&b, "main.txt", "m\n", "main moves");
    git_in(&b, &["push", "origin", "main"]);
    commit_file(&a, "two.txt", "2\n", "two");
    let theirs = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 3, "the remote-only refusal: {out}\n{err}");
    assert!(
        err.contains("git pull --rebase origin feature/x"),
        "the door: {err}"
    );
    assert!(
        !err.contains("not safely rebasable"),
        "the refusal must not key heal's conflict phrase: {err}"
    );
    let after = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    assert_eq!(
        theirs.trim(),
        after.trim(),
        "the remote branch still holds the other writer's commit"
    );
}

#[test]
fn a_remote_merge_with_manual_resolution_is_refused() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    // Clone B folds content into a merge commit itself: no non-merge
    // commit carries the hand.txt content, only the merge's tree does.
    git_in(&b, &["fetch", "origin"]);
    git_in(&b, &["checkout", "main"]);
    commit_file(&b, "main.txt", "m\n", "main moves");
    git_in(&b, &["push", "origin", "main"]);
    git_in(&b, &["checkout", "feature/x"]);
    git_in(&b, &["merge", "--no-commit", "origin/main"]);
    std::fs::write(b.join("hand.txt"), "hand\n").unwrap();
    git_in(&b, &["add", "hand.txt"]);
    git_in(&b, &["commit", "-m", "merge with a hand edit"]);
    git_in(&b, &["push", "origin", "feature/x"]);
    commit_file(&a, "two.txt", "2\n", "two");
    let merged = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 3, "the hand-resolved merge refusal: {out}\n{err}");
    assert!(
        err.contains("the hand-resolved merge"),
        "the refusal names the merge: {err}"
    );
    assert!(
        err.contains("git pull --rebase origin feature/x"),
        "the door: {err}"
    );
    let after = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    assert_eq!(
        merged.trim(),
        after.trim(),
        "the remote branch still holds the merge and its hand content"
    );
}

#[test]
fn a_clean_remote_merge_still_lands() {
    let (_t, root, a, b) = real_repo();
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "first push: {out}\n{err}");
    // Clone B merges the moved main into feature/x cleanly (GitHub's
    // "Update branch" shape): the merge adds nothing, so the lease lands.
    git_in(&b, &["fetch", "origin"]);
    git_in(&b, &["checkout", "feature/x"]);
    git_in(&b, &["merge", "origin/main"]);
    git_in(&b, &["push", "origin", "feature/x"]);
    commit_file(&a, "two.txt", "2\n", "two");
    let (code, out, err) = run_verb_real(&a, &root);
    assert_eq!(code, 0, "the leased second push: {out}\n{err}");
    assert!(out.contains("pushed=1"), "{out}");
    let remote = git_in(
        &root.join("remote.git"),
        &["rev-parse", "refs/heads/feature/x"],
    );
    let head = git_in(&a, &["rev-parse", "HEAD"]);
    assert_eq!(
        remote.trim(),
        head.trim(),
        "the remote branch equals the rebased HEAD"
    );
}

/// A seeded FNO_HOME so the commit-message scan never touches the machine
/// store: one live id, d-aaaa0001.
fn seed_fno_home(root: &Path) -> PathBuf {
    let home = root.join("fno-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("decisions.jsonl"),
        "{\"type\":\"operator_decision\",\"ts\":\"2026-09-12T00:00:00Z\",\"data\":\
         {\"decision_id\":\"d-aaaa0001\",\"subject\":\"s\",\"decision\":\"R.\",\
         \"text\":\"R.\",\"authority_source\":\"operator\"}}\n",
    )
    .unwrap();
    home
}

fn run_verb_with_fno_home(dir: &Path, fno_home: &Path) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_fno-agents"))
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_HOME", fno_home)
        .args(["pr-push"])
        .arg("--cwd")
        .arg(dir)
        .arg("--git-bin")
        .arg(dir.join("git"))
        .arg("--gh-bin")
        .arg(dir.join("gh"))
        .arg("--fno-bin")
        .arg(dir.join("fno"))
        .arg("--stamps-dir")
        .arg(dir.join("stamps"))
        .output()
        .expect("run fno-agents");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// AC4-ERR: the scan refuses before anything moves and names commit + id.
#[test]
fn a_commit_message_citing_an_unknown_id_refuses_the_push() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("bad-commit-msg"), "").unwrap();
    let home = seed_fno_home(&d);
    let (code, out, err) = run_verb_with_fno_home(&d, &home);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(err.contains("commit abc1234"), "{err}");
    assert!(err.contains("d-deadbeef"), "{err}");
    assert!(err.contains("Reword that commit message"), "{err}");
    assert!(
        !log_of(&d, "git.log").contains("git push"),
        "nothing pushed"
    );
}

// AC4-HP: live ids read clean and the push path continues unchanged.
#[test]
fn commit_messages_citing_only_live_ids_push_unchanged() {
    let (_t, d) = tmpdir();
    std::fs::write(d.join("live-commit-msg"), "").unwrap();
    let home = seed_fno_home(&d);
    let (code, out, err) = run_verb_with_fno_home(&d, &home);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("pushed=1"), "{out}");
    assert_eq!(log_of(&d, "git.log").matches("git push").count(), 1);
}
