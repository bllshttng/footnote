//! The heal -> fleet-task seam, driven through the public verb: a rebase
//! conflict files one deduped fleet task beside the events journal, a clean
//! rebase closes it with reason `rebased`, and two roots with the same
//! conflict file two tasks distinguished by cwd (one heal process serves
//! several repos and PR numbers repeat across them, so identity is
//! lane + key + cwd).
//!
//! These live here rather than beside the rest of the heal drive tests
//! because heal.rs sits at the file budget; the stubs replicate the in-file
//! ones against common::make_script.

use common::make_script;
use std::path::Path;

mod common;

fn args_for(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut v = vec![
        "1".to_string(),
        "--gh-bin".to_string(),
        dir.join("gh").to_string_lossy().into_owned(),
        "--git-bin".to_string(),
        dir.join("git").to_string_lossy().into_owned(),
        "--claims-root".to_string(),
        dir.to_string_lossy().into_owned(),
        "--cwd".to_string(),
        dir.to_string_lossy().into_owned(),
    ];
    v.push("--bin-dir".to_string());
    v.push(dir.to_string_lossy().into_owned());
    // The journal rides the same test seam as the in-file drive_args: the
    // fleet-task store is questions.jsonl beside it.
    v.push("--events-file".to_string());
    v.push(dir.join("events.jsonl").to_string_lossy().into_owned());
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

/// PR 1 carries mergeable=MERGEABLE (the test names the value) and PR 2 has
/// no worktree. Its one red check is rustfmt-drift, so if the loop ever
/// reached the heal path the remedy would run and cargo.log would name it.
fn stub_gh_rebase(dir: &Path, pr1_mergeable: &str) {
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "gh $*" >> "$D/gh.log"
for a in "$@"; do case "$a" in
  *'pulls?state=open'*)
     echo '[{"number":1,"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":MERGEABLE,"body":"b"},{"number":2,"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null,"body":"b"}]'
     exit 0 ;;
  *pulls/1*) echo '{"head":{"sha":"aaa1","ref":"feature/x-1111"},"base":{"ref":"main"},"mergeable":MERGEABLE,"body":"b"}'; exit 0 ;;
  *pulls/2*) echo '{"head":{"sha":"bbb2","ref":"feature/x-2222"},"base":{"ref":"main"},"mergeable":null,"body":"b"}'; exit 0 ;;
  *check-runs) echo '{"check_runs":[{"name":"cargo fmt --check (pinned)","status":"completed","conclusion":"failure","html_url":"https://github.com/o/r/actions/runs/1/job/9"}]}'; exit 0 ;;
  */logs) echo "Diff in /w/w/crates/fno-agents/src/x.rs:1:"; exit 0 ;;
  */status) echo '{"statuses":[]}'; exit 0 ;;
esac; done
echo '[]'
"#
    .replace("MERGEABLE", pr1_mergeable);
    make_script(dir, "gh", &body);
}

/// A stub `git` that lists one worktree (on feature/x-1111) and answers
/// run_one's branch/porcelain questions inside it.
fn stub_git_drive(dir: &Path) {
    let wt = dir.join("wt").to_string_lossy().into_owned();
    make_script(
        dir,
        "git",
        &format!(
            r#"#!/bin/sh
D="$(dirname "$0")"
echo "git $*" >> "$D/git.log"
case "$1 $2" in
  "worktree list") printf 'worktree {wt}\nHEAD aaa\nbranch refs/heads/feature/x-1111\n\n'; exit 0 ;;
  "rev-parse --abbrev-ref") echo feature/x-1111; exit 0 ;;
  "status --porcelain") exit 0 ;;
esac
exit 0
"#
        ),
    );
}

fn stub_cargo(dir: &Path) {
    make_script(
        dir,
        "cargo",
        r#"#!/bin/sh
D="$(dirname "$0")"
echo "cargo $*" >> "$D/cargo.log"
exit 0
"#,
    );
}

/// A stub `fno` whose `do pr push` seam answers a caller-named
/// stdout/exit/stderr triple; the backlog lane logs and answers.
fn stub_fno_push(dir: &Path, push_stdout: &str, push_exit: u8, push_stderr: &str) {
    let body = r#"#!/bin/sh
D="$(dirname "$0")"
echo "fno $*" >> "$D/fno.log"
case "$*" in
  *"do pr push"*) printf '%s' 'PUSH_STDOUT' ; printf '%s' 'PUSH_STDERR' >&2; exit PUSH_EXIT ;;
  *"backlog idea"*) echo "backlog node fno-abc9 created"; exit 0 ;;
esac
exit 0
"#
    .replace("PUSH_STDOUT", push_stdout)
    .replace("PUSH_EXIT", &push_exit.to_string())
    .replace("PUSH_STDERR", push_stderr);
    make_script(dir, "fno", &body);
}

fn log_of(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

/// Writers commit to the event store, so the projection reads committed
/// rows plus the live tail, never the raw file alone.
fn store_text(dir: &Path) -> String {
    fno_agents::event_store::journal_text(&dir.join("questions.jsonl"), &[])
}

const CONFLICT_PUSH_STDERR: &str = "pr-push: the branch is not safely rebasable onto origin/main (status needs_resolver; files: crates/fno-agents/src/a.rs, crates/fno/src/b.rs). Resolve the conflicts, then run `fno do pr rebase --continue`.";

#[test]
fn a_rebase_conflict_files_one_fleet_task_and_a_clean_rebase_closes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    stub_gh_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    stub_fno_push(d, "", 3, CONFLICT_PUSH_STDERR);
    fno_agents::heal::run_heal(&args_for(d, &["--all", "--apply"]));
    let store = store_text(d);
    assert_eq!(
        store.matches(r#""type":"fleet_task""#).count(),
        1,
        "one open task: {store}"
    );
    assert!(
        store.contains("rebase conflict")
            && store.contains("crates/fno-agents/src/a.rs")
            && store.contains("crates/fno/src/b.rs"),
        "{store}"
    );
    assert!(store.contains(r#""run":"fno do pr rebase 1""#), "{store}");
    assert!(
        !log_of(d, "fno.log").contains("outstanding ask"),
        "no question was filed"
    );
    // The skip receipt itself stays asserted in-file: the events journal is
    // store-committed, and the committed read is crate-private.
    // The next run rebases PR 1 cleanly, and the task closes.
    stub_fno_push(
        d,
        "pr-push: origin/main behind-before=9 behind-after=0 preflight=full ci=settled sha=abc pushed=1",
        0,
        "",
    );
    fno_agents::heal::run_heal(&args_for(d, &["--all", "--apply"]));
    let store = store_text(d);
    assert!(
        store.contains(r#""type":"fleet_task_closed""#) && store.contains(r#""reason":"rebased""#),
        "the clean rebase closed the task: {store}"
    );
}

#[test]
fn two_roots_with_the_same_conflict_file_two_tasks_distinguished_by_cwd() {
    // One heal process serves several roots and PR numbers repeat across
    // repos, so identity is lane + key + cwd.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let root1 = d.join("root-1");
    std::fs::create_dir_all(&root1).unwrap();
    stub_gh_rebase(d, "false");
    stub_git_drive(d);
    stub_cargo(d);
    stub_fno_push(d, "", 3, CONFLICT_PUSH_STDERR);
    // The push seam spawns from the PR's worktree; a missing cwd is a
    // spawn NotFound even when the stub binaries exist.
    std::fs::create_dir_all(d.join("wt/crates/fno-agents")).unwrap();
    let mut argv = args_for(d, &["--all", "--apply"]);
    argv.push("--cwd".to_string());
    argv.push(root1.to_string_lossy().into_owned());
    fno_agents::heal::run_heal(&argv);
    let store = store_text(d);
    let tasks: Vec<&str> = store
        .lines()
        .filter(|l| l.contains(r#""type":"fleet_task""#))
        .collect();
    assert_eq!(tasks.len(), 2, "one task per root: {store}");
    assert!(
        tasks[0].contains(r#""key":"PR 1 rebase conflict""#)
            && tasks[1].contains(r#""key":"PR 1 rebase conflict""#)
            && tasks
                .iter()
                .any(|t| t.contains(&format!(r#""cwd":"{}""#, d.display())))
            && tasks
                .iter()
                .any(|t| t.contains(&format!(r#""cwd":"{}""#, root1.display()))),
        "same key, different cwd: {store}"
    );
}
