//! The open-PR reap guard families: a row whose node still carries an open
//! PR and whose session never recorded a termination is kept with a resume
//! task filed, and a reap that does go through on an open-PR node files the
//! alert naming the PR.

use super::*;

/// The guard, two-row marker: the quiet driver with no termination event
/// survives with `reap-keep` filed and `worker_reap_refused` emitted, while
/// the terminated session on an open-PR node still reaps with `reap-alert`
/// filed. Both nodes carry `pr_number` with no recorded merge, so the
/// recorded-graph PR read answers open for both.
#[test]
fn gc_sweep_keeps_the_open_pr_driver_without_a_termination_and_alerts_a_terminated_reap() {
    let sandbox = tmp_home("gc-open-pr-guard");
    let home = AgentsHome::at(sandbox.root().join("agents"));
    home.ensure_root().unwrap();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let keep_repo = home.root().join("keep-repo");
    let alert_repo = home.root().join("alert-repo");
    for repo in [&keep_repo, &alert_repo] {
        std::fs::create_dir_all(repo.join(".fno")).unwrap();
        // The termination journal resolves through the repo root, so each
        // fixture repo needs its own .git for the walk to stop here.
        assert!(crate::git_test_helpers::git_init(repo));
    }
    // The commit's graph read is the production resolver over the sandbox
    // state root, so the open-PR record must live there, not only in the
    // staged decision graph.
    stage_graph(
        sandbox.root(),
        json!([
            {
                "id": "x-2b1c", "status": "done", "pr_number": 4242,
                "merge_status": null,
                "sessions": [{"session_id": "keep-harness-uuid", "phase": "execute"}],
            },
            {
                "id": "x-2b1d", "status": "done", "pr_number": 4243,
                "merge_status": null,
                "sessions": [{"session_id": "alert-harness-uuid", "phase": "execute"}],
            },
        ]),
    );
    // The dispatch identity each repo's termination read keys on: the keep
    // repo records none (the guard's Absent case), the alert repo records
    // one below (the Found case).
    for (repo, fno_id, node) in [
        (&keep_repo, "target-run-x-2b1c", "x-2b1c"),
        (&alert_repo, "target-run-x-2b1d", "x-2b1d"),
    ] {
        std::fs::write(
            repo.join(".fno/target-state.md"),
            format!("---\nfno_id: {fno_id}\ninput: {node}\nplan_path: \"\"\n---\n"),
        )
        .unwrap();
    }

    let mut keep = bg_claude_row("target-x-2b1c-guard", "keep0001");
    keep.status = AgentStatus::Exited;
    keep.cwd = keep_repo.to_string_lossy().into_owned();
    keep.exited_at = Some("2020-01-01T00:00:00Z".into());
    keep.log_path = Some(stale_log(&keep_repo));
    keep.harness_session_id = Some("keep-harness-uuid".into());
    let mut alert = bg_claude_row("target-x-2b1d-alert", "alert0003");
    alert.status = AgentStatus::Exited;
    alert.cwd = alert_repo.to_string_lossy().into_owned();
    alert.exited_at = Some("2020-01-01T00:00:00Z".into());
    alert.log_path = Some(stale_log(&alert_repo));
    alert.harness_session_id = Some("alert-harness-uuid".into());
    // A helper row keyed to the open-PR node but with no dispatch name and
    // no target manifest: kept by the guard, never handed a resume task.
    let helper_repo = home.root().join("helper-repo");
    std::fs::create_dir_all(helper_repo.join(".fno")).unwrap();
    assert!(crate::git_test_helpers::git_init(&helper_repo));
    let mut helper = bg_claude_row("wake-abcd1234", "wake0009");
    helper.status = AgentStatus::Exited;
    helper.cwd = helper_repo.to_string_lossy().into_owned();
    helper.exited_at = Some("2020-01-01T00:00:00Z".into());
    helper.log_path = Some(stale_log(&helper_repo));
    helper.harness_session_id = Some("wake-harness-uuid".into());
    helper.node = Some("x-2b1c".into());
    state::update_registry(&home.registry_json(), |r| {
        r.entries.push(keep);
        r.entries.push(alert);
        r.entries.push(helper);
    })
    .unwrap();

    // The alert row recorded its dispatch termination in its repo's journal;
    // the keep row's repo holds none.
    std::fs::write(
        alert_repo.join(".fno/events.jsonl"),
        format!(
            r#"{{"ts":"2026-07-24T00:00:00Z","type":"termination","source":"loop","data":{{"session_id":"target-run-x-2b1d","reason":"DonePRGreen","message":"done"}}}}"#
        ),
    )
    .unwrap();

    let mut graph = graph_read(
        &[
            ("keep-harness-uuid", "x-2b1c", "done"),
            ("alert-harness-uuid", "x-2b1d", "done"),
            ("wake-harness-uuid", "x-2b1c", "done"),
        ],
        &[],
    )
    .unwrap();
    graph.pr_number.insert("x-2b1c".into(), Some(4242));
    graph.pr_number.insert("x-2b1d".into(), Some(4243));

    let summary = staged_sweep(
        &home,
        &emitter,
        0,
        Some(graph),
        &|e| {
            e.log_path
                .as_deref()
                .map(|p| vec![std::path::PathBuf::from(p)])
        },
        &|_| (None, None),
    );

    // The keep row never left the registry, and the refusal is named. The
    // retired ids are short_ids, so the keep row's absence reads keep0001.
    assert!(
        summary.retired.iter().all(|(id, _)| id != "keep0001"),
        "the open-PR driver must be kept: {summary:#?}"
    );
    assert!(
        summary
            .kept_no_receipt
            .iter()
            .any(|(id, reason)| id == "keep0001"
                && reason.contains("open pr 4242 with no termination event")),
        "the guard's keep must be named: {summary:#?}"
    );
    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert!(reg.entries.iter().any(|e| e.name == "target-x-2b1c-guard"));

    // The resume task and the mechanical trail exist.
    let store = crate::provider_cap::questions_path(&home);
    let tasks = crate::fleet_task::open_tasks(&store).unwrap();
    let keep_task = tasks
        .iter()
        .find(|t| t.lane == "reap-keep" && t.node == "x-2b1c")
        .expect("reap-keep task filed");
    assert!(keep_task.text.contains("PR 4242"), "{}", keep_task.text);
    assert_eq!(keep_task.run, "fno agents resume keep-harness-uuid");
    let reaps = read_events(&home);
    assert!(
        reaps.iter().any(|e| e["type"] == "worker_reap_refused"
            && e["data"]["pr"] == 4242
            && e["data"]["node"] == "x-2b1c"),
        "worker_reap_refused emitted: {reaps:?}"
    );

    // The helper row is kept under its own reason, with no resume task and
    // no refused event of its own: it has no dispatch loop to resume.
    assert!(
        summary
            .kept_no_receipt
            .iter()
            .any(|(id, reason)| id == "wake0009" && reason.contains("helper row kept")),
        "the helper row must be kept without a resume: {summary:#?}"
    );
    assert_eq!(
        tasks.iter().filter(|t| t.lane == "reap-keep").count(),
        1,
        "only the driver row files a resume task: {tasks:?}"
    );
    assert!(
        !reaps
            .iter()
            .any(|e| e["type"] == "worker_reap_refused" && e["data"]["name"] == "wake-abcd1234"),
        "the helper row emits no refused event: {reaps:?}"
    );

    // The terminated session still reaps, and the alert names its PR.
    assert!(
        summary.retired.iter().any(|(id, _)| id == "alert0003"),
        "the terminated row must retire: {summary:#?}"
    );
    let alert_task = tasks
        .iter()
        .find(|t| t.lane == "reap-alert" && t.node == "x-2b1d")
        .expect("reap-alert task filed");
    assert!(alert_task.text.contains("PR 4243"), "{}", alert_task.text);
    let reaped = reaps
        .iter()
        .find(|e| e["type"] == "agent_row_reaped" && e["data"]["short_id"] == "alert0003")
        .expect("terminated reap event");
    assert_eq!(reaped["data"]["termination_event"], true);
    std::fs::remove_dir_all(home.root()).ok();
}

/// A worker rebound to a new node is judged by that node. The session's row
/// on the old node ended and the old node reads merged; its open row on the
/// new node drives an open PR. Graph order lists the old node first, the
/// order that once released the row through the old node's recorded merge.
#[test]
fn gc_sweep_keeps_a_rebound_worker_on_its_current_node_open_pr() {
    let sandbox = tmp_home("gc-rebound-worker");
    let home = AgentsHome::at(sandbox.root().join("agents"));
    home.ensure_root().unwrap();
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
    let repo = home.root().join("rebound-repo");
    std::fs::create_dir_all(repo.join(".fno")).unwrap();
    assert!(crate::git_test_helpers::git_init(&repo));
    stage_graph(
        sandbox.root(),
        json!([
            {
                "id": "x-0ld1", "status": "in_review", "pr_number": 4300,
                "merge_status": "merged",
                "sessions": [{"session_id": "rebound-uuid", "phase": "execute",
                    "harness": "claude", "started_at": "2026-10-01T01:02:37Z",
                    "ended_at": "2026-10-01T03:35:52Z"}],
            },
            {
                "id": "x-0n01", "status": "in_review", "pr_number": 4301,
                "merge_status": null,
                "sessions": [{"session_id": "rebound-uuid", "phase": "execute",
                    "harness": "claude", "started_at": "2026-10-01T04:02:26Z"}],
            },
        ]),
    );
    let mut row = bg_claude_row("t-x0ld1-fix", "rebd0001");
    row.cwd = repo.to_string_lossy().into_owned();
    row.log_path = Some(stale_log(&repo));
    row.harness_session_id = Some("rebound-uuid".into());
    state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();

    let mut graph = gc_sweep::read_graph_entries(&home).expect("staged graph reads");
    graph
        .pr_reads
        .insert((repo.to_string_lossy().into_owned(), 4301), Some(true));
    let summary = staged_sweep(
        &home,
        &emitter,
        0,
        Some(graph),
        &|e| {
            e.log_path
                .as_deref()
                .map(|p| vec![std::path::PathBuf::from(p)])
        },
        &|_| (None, None),
    );

    assert!(
        summary.retired.iter().all(|(id, _)| id != "rebd0001"),
        "the rebound worker must not retire on its old node's merge: {summary:#?}"
    );
    assert!(
        summary
            .kept_open_pr
            .iter()
            .any(|(id, node)| id == "rebd0001" && node.contains("x-0n01")),
        "the keep names the current node: {summary:#?}"
    );
    std::fs::remove_dir_all(home.root()).ok();
}
