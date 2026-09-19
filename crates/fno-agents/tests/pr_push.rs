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
/// fail with two conflicting paths, drop `no-upstream` to make `@{u}` fail.
/// `rev-list --count` answers 3 on the first call and 0 after (the rebase
/// happened), so the receipt reads behind-before=3 behind-after=0.
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
      --verify) echo deadbeef0000000; exit 0 ;;
      --short) echo abc1234; exit 0 ;;
      --show-toplevel) echo "$D"; exit 0 ;;
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
    assert!(out.contains("preflight=absent"), "{out}");
    assert!(out.contains("ci=settled"), "{out}");
    assert!(out.contains("sha=abc1234"), "{out}");
    assert!(out.contains("pushed=1"), "{out}");
    assert_eq!(log_of(&d, "git.log").matches("git push").count(), 1);
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
    let (code, out, err) = run_verb(&d, &[]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        log_of(&d, "git.log").contains("git push --set-upstream origin HEAD:feature/x"),
        "{:?}",
        log_of(&d, "git.log")
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
