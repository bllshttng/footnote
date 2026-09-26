//! The ruling-hold arm of DoneAwaitingMerge: a crown's `dispatch_hold`
//! terminates the loop on its own, without waiting on review.

use super::*;

/// Serialize FNO_HOME mutation across the parallel test threads in this
/// binary (mirrors `territory_parity.rs`'s `env_lock`). Only this module
/// touches FNO_HOME - it is the sole lever `ruling_hold`'s
/// `default_graph_path()` read respects, and there is no `--graph` CLI door
/// on `loop-check` to pass a per-test path through instead.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_hold_plan(dir: &Path, hold_frontmatter: &str) -> String {
    let plan = dir.join("plan.md");
    fs::write(
        &plan,
        format!("---\nstatus: ready\n{hold_frontmatter}---\n\n# Held\n"),
    )
    .unwrap();
    plan.display().to_string()
}

fn write_graph_with_hold_node(fno_home: &Path, node_id: &str, plan_path: &str) {
    let graph = fno_home.join("graph.json");
    fno_agents::graph_store::seed_rows(
        &graph,
        &[serde_json::json!({
            "id": node_id, "slug": node_id, "title": node_id, "type": "feature",
            "status": "ready", "priority": "p1", "plan_path": plan_path
        })],
    )
    .unwrap();
}

/// AC6-HP: a valid crown ruling on the session's node terminates
/// DoneAwaitingMerge on a CI-red, UNREVIEWED PR - the ruling is proof on its
/// own, so this does not wait on a review the crown's hold already outranks.
#[test]
fn ruling_hold_terminates_awaiting_merge_unreviewed() {
    let _env = env_lock();
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let fno_home = TempDir::new().unwrap();
    let plan_path = write_hold_plan(
        fno_home.path(),
        "dispatch_hold:\n  reason: waiting on legal\n  release_when: legal clears\n  review_on: 2099-01-01\n  set_by: operator\n",
    );
    write_graph_with_hold_node(fno_home.path(), "x-held", &plan_path);

    let manifest = format!(
        "{}graph_node_id: x-held\n",
        new_manifest("sess-ruling", "2026-06-05T00:00:00Z", true)
    );
    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, &manifest).unwrap();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_promise()).unwrap();

    let mock = MockBins::ci_red();
    std::env::set_var("FNO_HOME", fno_home.path());
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        cwd.join("transcript.jsonl").to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);
    std::env::remove_var("FNO_HOME");

    assert_eq!(d.decision, "allow", "ruling must terminate: {}", d.message);
    assert_eq!(d.termination_reason.as_deref(), Some("DoneAwaitingMerge"));
    assert!(
        d.message.contains("dispatch-hold:x-held"),
        "message must name the guard reason; got: {}",
        d.message
    );
}

/// AC7-EDGE: an invalid hold block (missing required field) is not proof -
/// the session falls through to today's ordinary CI-red hold.
#[test]
fn invalid_ruling_hold_falls_through_to_the_ordinary_block() {
    let _env = env_lock();
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(cwd);

    let fno_home = TempDir::new().unwrap();
    let plan_path = write_hold_plan(
        fno_home.path(),
        "dispatch_hold:\n  reason: waiting on legal\n",
    );
    write_graph_with_hold_node(fno_home.path(), "x-broken", &plan_path);

    let manifest = format!(
        "{}graph_node_id: x-broken\n",
        new_manifest("sess-broken-ruling", "2026-06-05T00:00:00Z", true)
    );
    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, &manifest).unwrap();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_promise()).unwrap();

    let mock = MockBins::ci_red();
    std::env::set_var("FNO_HOME", fno_home.path());
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest_path.to_str().unwrap(),
        "--transcript",
        cwd.join("transcript.jsonl").to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);
    std::env::remove_var("FNO_HOME");

    assert_eq!(d.decision, "block");
    assert!(d.termination_reason.is_none());
}
