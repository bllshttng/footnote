//! Acceptance-evidence journey (x-d098): drives the production `probe-run`
//! entrypoint over temporary plans, proving the AC3-HP regression class: a
//! probe that checks PRESENCE cannot satisfy a criterion, while a run that
//! emits its own positive marker does. Payload-shape detail is unit-covered
//! in `src/acceptance_evidence.rs`; this journey asserts outcomes.

use fno_agents::acceptance_evidence::run_probe_run;
use std::path::PathBuf;

/// Write a plan declaring `probes` as close_probes plus a `bindings` block,
/// plus a ship.txt whose presence is real but whose behavior string is not.
/// Returns (cwd, plan_path); the tempdir is leaked so the files outlive this
/// function, reclaimed by the OS at process exit.
fn plan_with(probes: &str, bindings: &str) -> (PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let plan = tmp.path().join("plan.md");
    std::fs::write(
        &plan,
        format!("---\ntitle: t\n{probes}\n{bindings}\n---\n\n# doc\n"),
    )
    .unwrap();
    std::fs::write(tmp.path().join("ship.txt"), "nothing to see here\n").unwrap();
    let dir = tmp.path().to_path_buf();
    std::mem::forget(tmp);
    (dir, plan)
}

fn run_close(dir: &PathBuf, plan: &PathBuf) -> i32 {
    run_probe_run(&[
        "--plan".to_string(),
        plan.to_string_lossy().into_owned(),
        "--key".to_string(),
        "close_probes".to_string(),
        "--cwd".to_string(),
        dir.to_string_lossy().into_owned(),
        "--json".to_string(),
    ])
}

#[test]
fn presence_only_probe_cannot_satisfy_a_criterion() {
    let (dir, plan) = plan_with(
        "close_probes:\n  - \"test -f ship.txt\"",
        "acceptance_evidence:\n  bindings:\n    AC1-HP: close_probes[0]",
    );
    assert_eq!(
        run_close(&dir, &plan),
        1,
        "a silent presence check is SKIP and holds the close gate"
    );
}

#[test]
fn the_repaired_positive_control_satisfies() {
    let (dir, plan) = plan_with(
        "close_probes:\n  - \"test -f ship.txt && echo behavior-verified\"",
        "acceptance_evidence:\n  bindings:\n    AC1-HP: close_probes[0]",
    );
    assert_eq!(
        run_close(&dir, &plan),
        0,
        "exit 0 WITH a positive marker satisfies the bound criterion"
    );
}

#[test]
fn false_behavior_fails_even_when_the_file_is_present() {
    let (dir, plan) = plan_with(
        "close_probes:\n  - \"grep -q BEHAVIOR ship.txt && echo verified\"",
        "acceptance_evidence:\n  bindings:\n    AC1-HP: close_probes[0]",
    );
    assert_eq!(
        run_close(&dir, &plan),
        1,
        "presence without the declared behavior fails and names the criterion"
    );
}
