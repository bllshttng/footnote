//! The ruling-hold arm of DoneAwaitingMerge: a team's `dispatch_hold`
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

/// AC6-HP: a valid team ruling on the session's node terminates
/// DoneAwaitingMerge on a CI-red, UNREVIEWED PR - the ruling is proof on its
/// own, so this does not wait on a review the team's hold already outranks.
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

/// A stub `fno` whose `do plan fidelity` answer is always REFUSED - the
/// shape a `carveouts: forbidden` plan shows while any deliverable is
/// unjoined (fidelity.py:39: forbidden plans never cover before merge).
/// Every other argv is also refused, so a fire that consults the stub for
/// anything else fails loudly in the assertions below.
fn refusing_fidelity_stub(dir: &Path) -> PathBuf {
    make_script(
        dir,
        "fno",
        r#"echo '{"refused": true, "reason": "unjoined deliverables, carveouts forbidden"}' ; exit 0"#,
    )
}

fn delegated_fixture(
    session: &str,
    node_id: &str,
    hold_frontmatter: &str,
    manifest_extra: &str,
) -> (TempDir, PathBuf, PathBuf, TempDir, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().to_path_buf();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    isolate_settings(&cwd);

    let fno_home = TempDir::new().unwrap();
    let plan_path = write_hold_plan(fno_home.path(), hold_frontmatter);
    write_graph_with_hold_node(fno_home.path(), node_id, &plan_path);

    let manifest = format!(
        "---\nsession_id: {session}\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\nplan_path: {plan}\n---\ngraph_node_id: {node_id}\n{extra}",
        session = session,
        plan = plan_path,
        node_id = node_id,
        extra = manifest_extra
    );
    let manifest_path = cwd.join("target-state.md");
    fs::write(&manifest_path, &manifest).unwrap();
    fs::write(cwd.join("transcript.jsonl"), transcript_with_promise()).unwrap();
    let stub_dir = TempDir::new().unwrap();
    (tmp, manifest_path, cwd, fno_home, stub_dir)
}

fn fire_with_fno_stub(cwd: &Path, manifest: &Path, mock: &MockBins, stub: &Path) -> Decision {
    std::env::set_var("FNO_LOOPCHECK_FNO_BIN", stub);
    let (_code, d) = fire(&[
        "loop-check",
        "--state",
        manifest.to_str().unwrap(),
        "--transcript",
        cwd.join("transcript.jsonl").to_str().unwrap(),
        "--cwd",
        cwd.to_str().unwrap(),
        "--now",
        "2026-06-05T00:30:00Z",
        &format!("--gh-bin={}", mock.gh.display()),
        &format!("--git-bin={}", mock.git.display()),
    ]);
    std::env::remove_var("FNO_LOOPCHECK_FNO_BIN");
    d
}

/// AC1-HP: a green, reviewed, covered PR under a per-run no-merge manifest
/// with a refusing fidelity gate parks ONCE on DonePRGreen, naming the merge
/// owner and the exact attended grant command - the worker is never
/// re-invoked to loop on a fidelity block the merge gate will enforce again
/// at merge time anyway.
#[test]
fn a_green_pr_under_per_run_no_merge_parks_on_doneprgreen_naming_the_owner() {
    let (_tmp, manifest_path, cwd, fno_home, stub_dir) = delegated_fixture(
        "sess-delegated",
        "x-delegated",
        "",
        "auto_merge_approved: false\nauto_merge_source: flag-no-merge\n",
    );
    let mock = MockBins::green();
    let stub = refusing_fidelity_stub(stub_dir.path());
    let _lock = env_lock();
    std::env::set_var("FNO_HOME", fno_home.path());
    let d = fire_with_fno_stub(&cwd, &manifest_path, &mock, &stub);
    std::env::remove_var("FNO_HOME");

    assert_eq!(
        d.decision, "allow",
        "delegated merge must park: {}",
        d.message
    );
    assert_eq!(
        d.termination_reason.as_deref(),
        Some("DonePRGreen"),
        "{}",
        d.message
    );
    assert!(
        d.message.contains("merge owned by"),
        "the owner line must name who merges: {}",
        d.message
    );
    assert!(d.message.contains("flag-no-merge"), "{}", d.message);
    assert!(
        d.message.contains("merge-grant:"),
        "the message must carry the attended grant command: {}",
        d.message
    );
    assert!(d.message.contains("--authority operator"), "{}", d.message);
}

/// AC1-ERR: the same park under a valid team dispatch hold on a GREEN PR -
/// the red-CI DoneAwaitingMerge arm cannot fire (it requires !ci_ok), so the
/// delegated park is the only terminal. Two fires on one head agree.
#[test]
fn a_green_pr_under_a_team_hold_parks_on_doneprgreen_naming_the_ruling() {
    let (_tmp, manifest_path, cwd, fno_home, stub_dir) = delegated_fixture(
        "sess-held-green",
        "x-held-green",
        "dispatch_hold:\n  reason: waiting on legal\n  release_when: legal clears\n  review_on: 2099-01-01\n  set_by: operator\n",
        "",
    );
    let mock = MockBins::green();
    let stub = refusing_fidelity_stub(stub_dir.path());
    let _lock = env_lock();
    std::env::set_var("FNO_HOME", fno_home.path());
    let d1 = fire_with_fno_stub(&cwd, &manifest_path, &mock, &stub);
    let d2 = fire_with_fno_stub(&cwd, &manifest_path, &mock, &stub);
    std::env::remove_var("FNO_HOME");

    for (n, d) in [("first", &d1), ("second", &d2)] {
        assert_eq!(d.decision, "allow", "{n} fire: {}", d.message);
        assert_eq!(
            d.termination_reason.as_deref(),
            Some("DonePRGreen"),
            "{n} fire: {}",
            d.message
        );
        assert!(
            d.message.contains("the ruling"),
            "{n} fire must name the ruling owner: {}",
            d.message
        );
        assert!(
            d.message.contains("dispatch-hold:x-held-green"),
            "{n} fire must name the guard reason: {}",
            d.message
        );
        assert!(
            !d.message.contains("CI is running"),
            "{n} fire must not claim CI is running: {}",
            d.message
        );
    }
}

/// AC1-EDGE: a SELF-merging run (auto_merge_approved not false, no hold)
/// with the same refusing fidelity stub still BLOCKS - the stop-time gate is
/// unchanged for runs that own their merge.
#[test]
fn a_self_merging_run_still_blocks_on_the_refusing_fidelity_gate() {
    let (_tmp, manifest_path, cwd, fno_home, stub_dir) =
        delegated_fixture("sess-self-merge", "x-self-merge", "", "");
    let mock = MockBins::green();
    let stub = refusing_fidelity_stub(stub_dir.path());
    let _lock = env_lock();
    std::env::set_var("FNO_HOME", fno_home.path());
    let d = fire_with_fno_stub(&cwd, &manifest_path, &mock, &stub);
    std::env::remove_var("FNO_HOME");

    assert_eq!(
        d.decision, "block",
        "self-merge fidelity gate must hold: {}",
        d.message
    );
    assert!(d.termination_reason.is_none());
    assert!(
        d.message.contains("carveouts forbidden"),
        "the block must name the fidelity reason: {}",
        d.message
    );
}
